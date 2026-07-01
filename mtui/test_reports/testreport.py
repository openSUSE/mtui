"""The `TestReport` abstract base class."""

import concurrent.futures
import os
import random
import re
import threading
from abc import ABC, abstractmethod
from collections.abc import Iterable
from contextlib import suppress
from json import loads
from json.decoder import JSONDecodeError
from logging import getLogger
from pathlib import Path
from traceback import format_exc
from typing import TYPE_CHECKING, Any, Literal

from ..hosts.refhost import Attributes, RefhostsFactory, RefhostsResolveFailedError
from ..hosts.refhost import verify as product_verify
from ..hosts.target import Target, TargetLockedError
from ..hosts.target.hostgroup import HostsGroup
from ..support.concurrency import ContextExecutor
from ..support.config import Config
from ..support.exceptions import InvalidGiteaHashError, UpdateError
from ..support.fileops import ensure_dir_exists
from ..support.messages import MetadataNotLoadedError
from ..types import OpenQAResults, Product, TargetMeta, Workflow
from .metadata_parsers import ReducedMetadataParser, patchinfo_titles
from .svn_io import (
    TemplateFormatError,
    TemplateIOError,
    TestReportAlreadyLoadedError,
)

if TYPE_CHECKING:
    from ..cli.prompter import Prompter
    from ..hosts.host_arbiter import HostArbiter

logger = getLogger("mtui.template.testreport")


re_ver = re.compile(r"(\S+)\s+(\S+)")


# Matches the "Test Plan Reviewer:" metadata line in a testreport template.
# Group 1 captures the literal label so only the value after the colon is
# replaced. Older templates used the "Suggested Test Plan Reviewer(s):"
# phrasing; both are matched and normalized to "Test Plan Reviewer:".
_reviewer_line = re.compile(
    r"^(?:Suggested )?Test Plan Reviewers?:.*$",
    re.MULTILINE,
)

# Matches the "Slack Review: <channel>/<ts>" marker line recording the Slack
# message a review was requested on. Unlike the reviewer line this does not
# pre-exist in the template; ``set_slack_review`` inserts it on first write.
# ``get_slack_review`` parses the value back with the same regex the load-time
# metadata parser uses (``ReducedMetadataParser.slack_review``), so the two
# views of the marker can never diverge.
_slack_review_line = re.compile(
    r"^Slack Review:.*$",
    re.MULTILINE,
)


class TestReport(ABC):
    """An abstract base class for all test report implementations."""

    # FIXME: the code around read() (_open_and_parse, _parse and factory
    # _factory_md5) is weird a lot.
    # Firstly, it might clear some things up to change the open/read
    # things to file-like interface.

    refhostsFactory = RefhostsFactory

    # -- Attributes set dynamically by UpdateID subclasses --
    incident: Any

    def __init__(
        self,
        config: Config,
        prompter: "Prompter | None" = None,
    ) -> None:
        """Initializes the `TestReport` object.

        Args:
            config: The application configuration.
            prompter: Optional :class:`mtui.cli.prompter.Prompter` forwarded
                to every :class:`mtui.target.Target` built by
                :meth:`connect_target` and :meth:`add_target`. ``None``
                means "no prompt"; SSH command timeouts will silently
                wait. See :class:`mtui.connection.Connection`.

        """
        self.config: Config = config
        self._prompter = prompter

        # Per-report workflow mode (previously global config.auto / config.kernel).
        self.workflow: Workflow = Workflow.MANUAL

        self.directory: Path = config.template_dir

        # Note: the default values here are unchanged from the previous
        # class Metadata for backward compaibility purposes, so we don't
        # have to modify every user of this class at the same time as
        # refactoring the internals.
        self.path: Path | None = None
        """
        :param path: path to the testreport file if loaded, otherwise None
        """
        self.systems: dict[str, str] = {}
        """
        :type systems: dict str -> str
        :param systems: hostname -> system
        """
        self.targets = HostsGroup([])
        """
        :type  targets: dict(hostname = L{Target})
            where hostname = str
        """
        self.update_repos: dict[Product, str] = {}
        """
        :type update_repos dict(Product = repository)
           where Product = namedtuple
                 repository = str
        """
        self.hostnames: set[str] = set()
        # When non-empty, newly connected reference hosts are locked with
        # this comment (set while a PI assignment is active). See
        # ``mtui.commands.apicall`` and ``lock_pi_autolock``.
        self.lock_comment: str = ""
        # Host-arbitration wiring (RFC §5.7), set by TemplateRegistry.add().
        # ``_owner`` is the composite ``(registry_id, RRID)`` ownership key;
        # ``_arbiter`` is the process-global HostArbiter. Both stay ``None``
        # for directly-constructed reports, which fall back to the legacy
        # remote-lock-only connect path.
        self._arbiter: HostArbiter | None = None
        self._owner: tuple[str, str] | None = None
        # Hosts this report has claimed through the arbiter (for release).
        self._pool_claims: set[str] = set()
        # Per-slot ordered candidate hostnames captured during pool selection,
        # so connect_targets can fall back to a sibling host when the primary
        # claim fails to connect (RFC §5.7 backup-refhost).
        self._slot_candidates: dict[tuple, list[str]] = {}
        # Set by ``make_testreport`` when a load asked for autoconnect; the
        # actual connect is deferred to :meth:`autoconnect` so it runs *after*
        # ``TemplateRegistry.add`` has wired the host arbiter (otherwise the
        # legacy search() path connects every candidate instead of one per
        # slot).
        self._autoconnect_pending = False
        self.bugs: dict[str, str] = {}
        self.jira: dict[str, str] = {}
        self.testplatforms: list[str] = []
        self.products: list[str] = []
        self.category: str = ""
        self.packager: str = ""
        self.reviewer: str = ""
        self.repository: str = ""
        self.packages = {}
        # Slack review reference (channel ID, message ts) recorded by
        # ``request_review`` and re-checked by the approve/reject gate.
        # ``None`` until a review has been requested and persisted.
        self.slack_review: tuple[str, str] | None = None

        self._attrs = [
            "products",
            "category",
            "packager",
            "reviewer",
            "packages",
            "bugs",
            "repository",
        ]
        """
        :type attrs: [str]
        :param attrs: attributes expected to exist on `self` after
            parsing the template
        """

        self.openqa: OpenQAResults = OpenQAResults()

        # hostname -> product-drift warning lines from the last connect
        # (see _verify_target_products); commands print these so they
        # reach MCP clients, which only see command stdout (not logs).
        self.product_warnings: dict[str, list[str]] = {}
        # Lazily-built, cached refhosts store for the product check, with
        # a lock because connect_targets runs connect_target concurrently.
        self._refhosts_store: Any = None
        self._refhosts_store_built = False
        self._refhosts_store_lock = threading.Lock()

    @property
    @abstractmethod
    def id(self) -> str:
        """Returns the ID of the test report."""
        ...

    def _open_and_parse(self, path: Path) -> None:
        """Opens and parses a test report file.

        Args:
            path: The path to the test report file.

        """
        metadata = path.parent / "metadata.json"
        try:
            tpl = path.read_text(errors="replace")
        except FileNotFoundError as e:
            args = [*list(e.args), e.filename]
            e_new = TemplateIOError(*args)
            raise e_new from e

        data = None
        if metadata.exists() and metadata.is_file():
            data = metadata.read_text()
            try:
                data = loads(data)
            except JSONDecodeError:
                raise MetadataNotLoadedError from None
        else:
            raise MetadataNotLoadedError

        self._parse_json(data, tpl)
        self._enrich_issue_titles(path.parent)

    def _enrich_issue_titles(self, directory: Path) -> None:
        """Fill in real bug/jira titles from the checkout's ``patchinfo.xml``.

        The JSON metadata only carries ids, so :class:`JSONParser` leaves a
        placeholder description. ``patchinfo.xml`` (in the same checkout) has
        the titles; here we overlay them onto the ids we already know about,
        leaving the id set authoritative and untouched when no title is found.
        """
        titles = patchinfo_titles(directory)
        if not titles:
            return
        for iid, title in titles.items():
            if iid in self.bugs:
                self.bugs[iid] = title
            elif iid in self.jira:
                self.jira[iid] = title

    def read(self, path: Path) -> None:
        """Reads a test report file.

        Args:
            path: The path to the test report file.

        """
        self._open_and_parse(path)
        self.path = path.resolve()
        self._update_repos_parse()
        if self.config.chdir_to_template_dir:
            os.chdir(path.parent)

        result = self.check_hash()
        if not result[0]:
            raise InvalidGiteaHashError(self.id, result[1], result[2])

    def set_reviewer(self, name: str) -> None:
        """Records the reviewer in the loaded testreport template on disk.

        Replaces the value of the ``Test Plan Reviewer:`` metadata line with
        ``name`` and rewrites the template file atomically. The line is always
        normalized to ``Test Plan Reviewer: <name>`` (older ``Suggested ...``
        phrasings are replaced). The in-memory :attr:`reviewer` attribute is
        updated only after the file is written.

        Args:
            name: The reviewer to record. Surrounding whitespace is stripped.

        Raises:
            ValueError: If ``name`` is empty or whitespace only.
            RuntimeError: If no template is loaded (``self.path`` is ``None``).
            TemplateFormatError: If the template has no ``Test Plan Reviewer:``
                line to replace.

        """
        name = name.strip()
        if not name:
            raise ValueError("reviewer must be a non-empty string")
        if not self.path:
            raise RuntimeError("Called while missing path")

        text = self.path.read_text(errors="replace")
        new_text, count = _reviewer_line.subn(
            f"Test Plan Reviewer: {name}", text, count=1
        )
        if count == 0:
            raise TemplateFormatError(
                f"no 'Test Plan Reviewer:' line found in {self.path}"
            )

        tmp = self.path.with_name(self.path.name + ".tmp")
        tmp.write_text(new_text)
        os.replace(tmp, self.path)
        self.reviewer = name

    def set_slack_review(self, channel: str, ts: str) -> None:
        """Records the Slack review reference in the loaded template on disk.

        Writes a ``Slack Review: <channel>/<ts>`` marker line so the ack state
        survives a reload and can be re-checked by the approve/reject gate. If
        the marker already exists it is replaced; otherwise it is inserted right
        after the ``Test Plan Reviewer:`` line (a stable anchor that always
        pre-exists in the template). The file is rewritten atomically and the
        in-memory :attr:`slack_review` attribute is updated only afterwards.

        Args:
            channel: The Slack channel ID the review message was posted to.
            ts: The Slack message timestamp identifying the review message.

        Raises:
            RuntimeError: If no template is loaded (``self.path`` is ``None``).
            TemplateFormatError: If the marker is missing and there is no
                ``Test Plan Reviewer:`` line to anchor the insert.

        """
        if not self.path:
            raise RuntimeError("Called while missing path")

        marker = f"Slack Review: {channel}/{ts}"
        text = self.path.read_text(errors="replace")
        new_text, count = _slack_review_line.subn(marker, text, count=1)
        if count == 0:
            # Marker absent: insert it right after the reviewer line.
            new_text, count = _reviewer_line.subn(
                lambda m: f"{m.group(0)}\n{marker}", text, count=1
            )
            if count == 0:
                raise TemplateFormatError(
                    f"no 'Test Plan Reviewer:' line to anchor Slack Review in {self.path}"
                )

        tmp = self.path.with_name(self.path.name + ".tmp")
        tmp.write_text(new_text)
        os.replace(tmp, self.path)
        self.slack_review = (channel, ts)

    def has_slack_review_anchor(self) -> bool:
        """Whether :meth:`set_slack_review` could record its marker.

        True when the loaded template already carries a ``Slack Review:``
        marker or has a ``Test Plan Reviewer:`` line to anchor the insert.
        ``request_review`` checks this **before** posting to Slack, so a
        malformed template aborts the request instead of leaving a dangling
        review message whose ack can never be recorded.
        """
        if not self.path:
            return False
        try:
            text = self.path.read_text(errors="replace")
        except OSError:
            # The template can vanish underneath the session (an ``svn up``
            # that removed a withdrawn report); no file, no anchor.
            return False
        return bool(_slack_review_line.search(text) or _reviewer_line.search(text))

    def get_slack_review(self) -> tuple[str, str] | None:
        """The ``Slack Review:`` marker as currently recorded **on disk**.

        Unlike :attr:`slack_review` (a load-time snapshot plus this session's
        own writes), this re-parses the template file, so it reflects marker
        changes pulled in underneath the session — every testreport commit
        runs ``svn up``, which can bring in a colleague's newer, reposted, or
        removed marker. Used by ``request_review`` for its resume-vs-post
        decision and its pre-approve supersede guard, and by the
        ``approve``/``reject`` review gate. A missing file reads as "no
        marker".
        """
        if not self.path:
            return None
        try:
            text = self.path.read_text(errors="replace")
        except OSError:
            return None
        # Match per line with the load-time parser's own regex (its ``$`` is
        # not MULTILINE-aware, so it must see one line at a time).
        for line in text.splitlines():
            if match := ReducedMetadataParser.slack_review.search(line):
                return (match.group(1), match.group(2))
        return None

    @abstractmethod
    def check_hash(self) -> tuple[bool, str, str]:
        """An abstract method for checking git hash of gitea based testreports,.

        return: bool
                True if hash is same or if it isn't supported
                False for different hashes
        """

    @abstractmethod
    def _parser(self) -> dict[str, Any]:
        """An abstract method for getting the parser for the testreport."""

    def _parse_json(self, data, tpl: str) -> None:
        """Parses a testreport from a JSON object.

        Args:
            data: The JSON object to parse.
            tpl: The test report string to parse.

        """
        if self.path:
            raise TestReportAlreadyLoadedError(self.path)

        parser_json = self._parser()["json"]
        parser_hosts = self._parser()["hosts"]

        for line in tpl.splitlines():
            parser_hosts.parse(self, line)

        parser_json.parse(self, data)

        self._warn_missing_fields()

    def _warn_missing_fields(self) -> None:
        """Warns about missing fields in the test report."""
        missing = {x for x in self._attrs if not getattr(self, x)}

        if missing:
            msg = f"TestReport: missing fields: {missing}"
            logger.warning(msg)

    def get_package_list(self):
        """Gets a list of all packages in the test report.

        Returns:
            A list of all packages in the test report.

        """
        ret = []
        for key in self.packages:
            ret.extend(self.packages[key])
        # deduplicate list
        ret = list(set(ret))

        return ret

    @abstractmethod
    def list_update_commands(self, targets: HostsGroup, display) -> None:
        """An abstract method for listing the update commands."""
        ...

    def perform_get(self, targets: HostsGroup, remote: Path):
        """Performs a `get` operation.

        Args:
            targets: The targets to perform the operation on.
            remote: The remote path to get.

        """
        local = self.report_wd("downloads", remote.name, filepath=True)

        targets.sftp_get(remote, local)

    def perform_prepare(self, targets: HostsGroup, **kw) -> None:
        """Performs a `prepare` operation.

        Args:
            targets: The targets to perform the operation on.
            **kw: Additional keyword arguments.

        """
        targets.perform_prepare(self.get_package_list(), self, **kw)

    def perform_update(self, targets: HostsGroup, params: list[str]) -> None:
        """Performs an `update` operation.

        Args:
            targets: The targets to perform the operation on.
            params: A list of update parameters.

        """
        targets.add_history(["update", str(self.id), " ".join(self.get_package_list())])

        try:
            targets.perform_update(self, params)
        except UpdateError:
            logger.error("Update failed")
            logger.warning("Error while updating. Rolling back changes")
            try:
                self.perform_downgrade(targets)
            except Exception:
                # A failure during rollback must not bury the original update
                # error: log it and still re-raise the UpdateError below so the
                # real reason (e.g. a dependency error) is what the user sees.
                logger.error(
                    "Rollback after the failed update did not complete cleanly"
                )
                logger.debug(format_exc())
            # Surface the original update failure to the caller — the update did
            # not apply, even though we attempted to roll back.
            raise

    def perform_downgrade(self, targets):
        """Performs a `downgrade` operation.

        Args:
            targets: The targets to perform the operation on.

        """
        targets.add_history(
            ["downgrade", str(self.id), " ".join(self.get_package_list())]
        )
        targets.perform_downgrade(self.get_package_list(), self)

    def perform_install(self, targets: HostsGroup, packages) -> None:
        """Performs an `install` operation.

        Args:
            targets: The targets to perform the operation on.
            packages: The packages to install.

        """
        targets.add_history(["install", packages])

        targets.perform_install(packages)

    def perform_uninstall(self, targets: HostsGroup, packages) -> None:
        """Performs an `uninstall` operation.

        Args:
            targets: The targets to perform the operation on.
            packages: The packages to uninstall.

        """
        targets.add_history(["uninstall", packages])
        targets.perform_uninstall(packages)

    def _autolock_new_target(self, target: Target) -> None:
        """Locks a freshly connected target if a PI lock is active.

        When :attr:`lock_comment` is set (a PI assignment is in progress),
        newly connected reference hosts are locked with that comment so a
        host added via ``add_host`` after ``assign`` is covered too. A host
        already locked by someone else is left as-is.

        Args:
            target: The freshly connected target to lock.

        """
        if not self.lock_comment:
            return
        with suppress(TargetLockedError):
            target.lock(self.lock_comment)

    def _get_refhosts_store(self):
        """Build (once, thread-safe) the refhosts store, or None on failure.

        connect_targets fans connect_target out across threads, so the
        first lookup may race; guard the one-time build with a lock and
        cache the (possibly ``None``) result.
        """
        if self._refhosts_store_built:
            return self._refhosts_store
        with self._refhosts_store_lock:
            if not self._refhosts_store_built:
                try:
                    self._refhosts_store = self.refhostsFactory(self.config)
                except Exception:
                    logger.debug(
                        "refhosts store unavailable for product check:\n%s",
                        format_exc(),
                    )
                    self._refhosts_store = None
                self._refhosts_store_built = True
        return self._refhosts_store

    def _verify_target_products(self, target) -> None:
        """Warn if a connected host's products drift from ``refhosts.yml``.

        Compares the host's detected :class:`~mtui.types.systems.System`
        against its ``refhosts.yml`` :class:`Host` row (wrong/wrong-version
        base, wrong arch, missing/extra/mismatched addons, dangling
        baseproduct symlink). Drift is recorded in
        :attr:`product_warnings` and logged at WARNING; the host is kept.

        Best-effort: any failure is swallowed (logged at DEBUG) so the
        check never breaks a connect. Hosts absent from ``refhosts.yml``
        are skipped silently (DEBUG).
        """
        try:
            system = getattr(target, "system", None)
            if system is None:
                return
            store = self._get_refhosts_store()
            if store is None:
                return
            meta = store.host_by_name(target.hostname)
            if meta is None:
                logger.debug(
                    "refhosts.yml has no entry for %s; skipping product check",
                    target.hostname,
                )
                self.product_warnings.pop(target.hostname, None)
                return
            diff = product_verify.compare(system, meta)
            if diff.ok:
                self.product_warnings.pop(target.hostname, None)
                return
            lines = diff.warnings()
            self.product_warnings[target.hostname] = lines
            for line in lines:
                logger.warning(
                    "%s: products differ from refhosts.yml metadata: %s",
                    target.hostname,
                    line,
                )
        except Exception:
            logger.debug(
                "product verification failed for %s:\n%s",
                getattr(target, "hostname", "?"),
                format_exc(),
            )

    def connect_target(
        self, host
    ) -> tuple[Target, str] | tuple[Literal[False], Literal[False]]:
        """Connects to a single target.

        Args:
            host: The hostname of the target to connect to.

        Returns:
            A tuple containing the `Target` object and the system
            string, or `(False, False)` if the connection fails.

        """
        try:
            target = Target(
                self.config,
                host,
                self.packages,
                timeout=self.config.connection_timeout,
                prompter=self._prompter,
                interactive=self._prompter is not None,
                rrid=str(self.id),
            )
            target.connect()
            new_system = str(target.system)
            self._verify_target_products(target)
            if host in self._pool_claims and self._pool_selection_active():
                comment = f"mtui pool {self.id} [{self._pool_owner_label()}]"
                if not target.try_claim(comment):
                    # Lost the race to another process holding the remote lock;
                    # free our in-process slot so a sibling slot host can be
                    # tried and don't add this dead connection.
                    logger.warning(
                        "%s claimed in-process but busy remotely; skipping", host
                    )
                    self._pool_claims.discard(host)
                    if self._arbiter is not None and self._owner is not None:
                        self._arbiter.release(host, self._owner)
                    with suppress(Exception):
                        target.close()
                    return False, False
            else:
                self._autolock_new_target(target)
        except KeyboardInterrupt:
            logger.warning("Connection to %s canceled by user", host)
            return False, False
        except Exception:
            logger.debug(format_exc())
            msg = f"failed to add host {host} to target list"
            logger.warning(msg)
            return False, False
        else:
            return target, new_system

    def connect_targets(self) -> None:
        """Connects to all targets."""
        targets: dict[str, Target] = {}
        new_systems: dict[str, str] = {}
        executor = ContextExecutor()
        # Once a testplatform/pool selection has run (``_slot_candidates`` set),
        # only the arbiter-chosen hosts (one per slot) should be connected.
        # ``hostnames`` may also hold the full set of template ``reference host:``
        # candidates (autoconnect pre-loads them), so connecting all of it would
        # drag in every duplicate per slot -- the regression this guards against.
        # Before any pool selection (plain template autoconnect / ``add_host
        # --target``) every requested host in ``hostnames`` connects as before.
        connectable = (
            self._pool_claims
            if self._pool_selection_active() and self._slot_candidates
            else self.hostnames
        )
        hosts: set[str] = {host for host in connectable if host not in self.targets}

        if hosts:
            logger.info("Adding %s", hosts)
        else:
            logger.info("No refhosts to add")

        connections = {}
        try:
            connections = {
                executor.submit(self.connect_target, host): host for host in hosts
            }
            done, _ = concurrent.futures.wait(connections)
            for future in done:
                host = connections[future]
                target, new_system = future.result()
                # connect_target returns (False, False) on failure; skip those
                # so the typed dicts only ever hold successful connections.
                if target is False or new_system is False:
                    continue
                targets[host], new_systems[host] = target, new_system
        except KeyboardInterrupt:
            for future in connections:
                future.cancel()
            logger.debug("CTRL-C .. ...")

            # explicitly call del over Target instances
            for host in list(targets.keys()):
                del targets[host]
            targets = {}
            logger.warning("Connection to refhosts cancelled by user")
        finally:
            executor.shutdown(wait=False)
            del connections
            del executor

        # For pool slots whose chosen host did not connect, fall back to a
        # sibling candidate in the same slot (RFC §5.7 backup-refhost).
        self._connect_pool_backups(hosts, targets, new_systems)

        # We need to be sure that only the system property only have the  connected hosts
        self.systems = {host: system for host, system in new_systems.items() if system}
        for t in self.targets.copy():
            if not self.targets[t].connection.is_active():
                del self.targets[t]

        self.targets.update(
            {host: target for host, target in targets.items() if target}
        )

    def _connect_pool_backups(
        self,
        attempted: set[str],
        targets: dict[str, Target],
        new_systems: dict[str, str],
    ) -> None:
        """Retry failed pool slots against their remaining candidates.

        A slot whose primary claim failed to connect (and which we did not
        already connect via another candidate) is retried sequentially: the
        next free candidate in the slot is claimed through the arbiter and
        connected, until one succeeds or the slot is exhausted. Mutates
        ``targets`` / ``new_systems`` in place for any backup that connects.

        Best-effort and a no-op when pool selection is inactive. Failures are
        rare, so the retry runs serially rather than re-entering the fan-out.
        """
        if not self._pool_selection_active() or not self._slot_candidates:
            return
        arbiter = self._arbiter
        owner = self._owner
        if arbiter is None or owner is None:
            return

        for slot, candidates in self._slot_candidates.items():
            # Already have a live connection for this slot? Nothing to do.
            if any(c in targets for c in candidates):
                continue
            # Drop the dead primary claim(s) so a sibling can be tried and the
            # exhausted-pool wait below reflects real availability.
            for c in candidates:
                if c in self._pool_claims and c not in targets:
                    self._pool_claims.discard(c)
                    arbiter.release(c, owner)

            remaining = [c for c in candidates if c not in attempted]
            connected = False
            while remaining:
                chosen = arbiter.acquire_any(
                    remaining,
                    owner,
                    wait=self.config.lock_wait,
                    poll=self.config.lock_wait_poll,
                )
                if chosen is None:
                    break
                attempted.add(chosen)
                remaining.remove(chosen)
                self._pool_claims.add(chosen)
                self.hostnames.add(chosen)
                logger.info("Trying backup refhost %s for slot %s", chosen, slot)
                target, new_system = self.connect_target(chosen)
                if target is False or new_system is False:
                    # connect_target already released the in-process claim on a
                    # remote-lock race; drop it unconditionally so the next
                    # candidate is free to try.
                    self._pool_claims.discard(chosen)
                    arbiter.release(chosen, owner)
                    continue
                targets[chosen], new_systems[chosen] = target, new_system
                connected = True
                break
            if not connected:
                logger.warning(
                    "no connectable pool host for slot %s (tried %d candidates)",
                    slot,
                    len(candidates),
                )

    def add_target(self, hostname: str) -> None:
        """Adds a target to the test report.

        Args:
            hostname: The hostname of the target to add.

        """
        if hostname in self.targets:
            logger.warning(
                "already connected to %s, skipping.", self.targets[hostname].hostname
            )
            return
        try:
            self.targets[hostname] = Target(
                self.config,
                hostname,
                self.packages,
                prompter=self._prompter,
                interactive=self._prompter is not None,
                rrid=str(self.id),
            )
            self.targets[hostname].connect()

            if self:
                self.systems[hostname] = str(self.targets[hostname].system)

            self._verify_target_products(self.targets[hostname])
            self._autolock_new_target(self.targets[hostname])

        except Exception:
            if hostname in self.targets:
                del self.targets[hostname]
            if hostname in self.systems:
                del self.systems[hostname]
            logger.warning("failed to add host %s to target list", hostname)
            logger.debug(format_exc())

    def refhosts_from_tp(self, testplatform) -> None:
        """Gets reference hosts from a test platform.

        Args:
            testplatform: The test platform to get reference hosts from.

        """
        try:
            refhosts = self.refhostsFactory(self.config)
        except RefhostsResolveFailedError:
            return

        try:
            attributes = Attributes.from_testplatform(testplatform)
        except (ValueError, KeyError):
            logger.warning("failed to parse testplatform %r", testplatform)
            return

        if self._pool_selection_active():
            self._pool_select_from_tp(refhosts, attributes, testplatform)
            return

        hostnames = refhosts.search(attributes)
        if not hostnames:
            logger.warning("nothing found for testplatform %r", testplatform)
        self.hostnames.update(set(hostnames))

    def _pool_selection_active(self) -> bool:
        """True when refhost-pool arbitration should drive host selection.

        Active whenever an in-process arbiter and owner are wired up (the
        fan-out / ``mtui-mcp`` runtime path). When they are absent — e.g. a
        direct ``add_host`` — selection falls back to the legacy
        single-host-per-arch ``search()`` path.
        """
        return self._arbiter is not None and self._owner is not None

    def _pool_owner_label(self) -> str:
        """Human owner stamp for the remote pool lock comment."""
        return self._owner[1] if self._owner else str(self.id)

    def _pool_select_from_tp(self, refhosts, attributes, testplatform) -> None:
        """Pick one distinct free host per test-target slot via the arbiter.

        Candidates are searched and grouped by the *requested* slot
        (base product + version + arch + the testplatform's requested addons),
        so hosts that satisfy the same testplatform collapse to one slot
        regardless of which extra modules they happen to have installed. For
        each slot the in-process arbiter hands out one host not already claimed
        by another owner, queueing up to ``[lock] wait`` seconds when every
        candidate is busy. The remote lock is taken later at connect time
        (``connect_target``).
        """
        arbiter = self._arbiter
        owner = self._owner
        if arbiter is None or owner is None:
            return
        pairs = refhosts.search_pool_by_query(attributes)
        if not pairs:
            logger.warning("nothing found for testplatform %r", testplatform)
            return
        by_slot: dict[tuple, list[str]] = {}
        for host, slot in pairs:
            by_slot.setdefault(slot, []).append(host.name)
        for slot, candidates in by_slot.items():
            # Pick (and fall back) randomly within a slot so load is spread
            # across the interchangeable refhosts instead of always hammering
            # the first one in refhosts.yml order.
            random.shuffle(candidates)
            # Remember the (shuffled) candidate list so connect_targets can fall
            # back to a sibling host if the chosen one fails to connect.
            self._slot_candidates[slot] = list(candidates)
            # Skip slots we already hold a host for (across testplatforms).
            if any(arbiter.owner_of(c) == owner for c in candidates):
                continue
            chosen = arbiter.acquire_any(
                candidates,
                owner,
                wait=self.config.lock_wait,
                poll=self.config.lock_wait_poll,
            )
            if chosen is None:
                logger.warning(
                    "no free pool host for slot %s (all %d candidates busy)",
                    slot,
                    len(candidates),
                )
                continue
            self._pool_claims.add(chosen)
            self.hostnames.add(chosen)

    def release_pool_claims(self) -> None:
        """Drop arbiter ownership and remove this report's remote pool locks.

        Idempotent and safe when pool selection was never used. Called from
        :meth:`TemplateRegistry.release_claims` (``remove`` / ``unload`` /
        ``quit`` / ``McpSession.close``).
        """
        for host in list(self._pool_claims):
            target = self.targets.get(host)
            if target is not None:
                with suppress(Exception):
                    target.pool_unlock()
        self._pool_claims.clear()
        self._slot_candidates.clear()
        if self._arbiter is not None and self._owner is not None:
            self._arbiter.release_owner(self._owner)

    def release_pool_claim(self, host: str) -> None:
        """Release one host's in-process arbiter claim.

        Per-host analogue of :meth:`release_pool_claims`, called from
        ``remove_host`` so a disconnected refhost does not stay claimed in the
        process-global :class:`HostArbiter` for the rest of the server's
        lifetime (there is no ``unload`` over MCP, so the template stays
        loaded). ``Target.close()`` already drops the remote operation/pool
        lock files; this clears the in-process ownership that those locks and
        the ``--free`` probe never see. Idempotent and safe when pool
        selection was never used (``_arbiter``/``_owner`` are then ``None``).
        """
        self._pool_claims.discard(host)
        # Drop only this host from each slot's candidate list -- siblings stay
        # available as backup-refhost fallbacks (RFC 5.7). Prune a slot only
        # once it has no candidates left. (``release_pool_claims`` clears the
        # whole map because it tears the entire report down.)
        for slot, candidates in list(self._slot_candidates.items()):
            if host in candidates:
                candidates.remove(host)
                if not candidates:
                    del self._slot_candidates[slot]
        if self._arbiter is not None and self._owner is not None:
            self._arbiter.release(host, self._owner)

    def autoconnect(self) -> None:
        """Connect refhosts for a freshly loaded (manual-fallback) template.

        Deferred from :meth:`UpdateID.make_testreport` so it runs *after*
        :meth:`TemplateRegistry.add` has wired the host arbiter. With the
        arbiter in place, :meth:`refhosts_from_tp` takes the pool-selection
        path and draws one host per test-target slot (instead of the legacy
        ``search()`` path that connected every candidate per arch).

        No-op unless ``make_testreport`` flagged this report for autoconnect.
        """
        if not self._autoconnect_pending:
            return
        self._autoconnect_pending = False

        logger.info("Connect refhosts from testreport")
        self.connect_targets()

        for tp in self.testplatforms:
            logger.debug("Testplatform: %s", tp)
            self.refhosts_from_tp(tp)

        logger.info("Connect refhosts from TestPlatform")
        self.connect_targets()

    def list_bugs(self, sink, arg):
        """Lists the bugs for the test report.

        Args:
            sink: The function to use for listing the bugs.
            arg: An additional argument to pass to the sink function.

        """
        return sink(self.bugs, self.jira, arg)

    def _show_yourself_data(self) -> list[tuple[str, str]]:
        """Returns a list of data to be displayed by `list_metadata`."""
        return (
            [
                ("Category", self.category),
                ("Hosts", " ".join(sorted(self.systems.keys()))),
                ("Reviewer", self.reviewer),
                ("Packager", self.packager),
                ("Bugs", ", ".join(sorted(self.bugs.keys()))),
                ("Jira", ", ".join(sorted(self.jira.keys()))),
                ("Packages", " ".join(sorted(self.get_package_list()))),
                ("Build checks", self._testreport_url()[:-3] + "build_checks"),
                ("Testreport", self._testreport_url()),
                ("Repository", self.repository),
                (
                    "Slack Review",
                    "/".join(self.slack_review) if self.slack_review else "",
                ),
            ]
            + [("Testplatform", x) for x in self.testplatforms]
            + [("Products", x) for x in self.products]
        )

    def show_yourself(self, writer) -> None:
        """Displays the metadata for the test report.

        Args:
            writer: The writer to write the metadata to.

        """
        self._aligned_write(writer, self._show_yourself_data())

    @staticmethod
    def _aligned_write(writer, data: Iterable[tuple[str, str]]) -> None:
        """Writes aligned data to a writer.

        Args:
            writer: The writer to write the data to.
            data: A list of key-value pairs to write.

        """
        for x in sorted(data):
            name, value = x
            if value:
                writer.write(f"{name:15}: {value}\n")

    def _testreport_url(self) -> str:
        """Returns the URL for the test report."""
        return "/".join([self.config.reports_url, str(self.id), "log"])

    def fancy_report_url(self) -> str:
        """Returns the URL for the fancy test report."""
        return "/".join([self.config.fancy_reports_url, str(self.id), "log"])

    def report_wd(self, *paths, **kw) -> Path:
        """Returns the working directory relative to the test report checkout.

        Args:
            *paths: The path components to join to the working directory.
            **kw: Additional keyword arguments.

        Returns:
            The path to the working directory.

        """
        assert self.path, "empty path"

        return self._wd(self.path.parent, *paths, **kw)

    @staticmethod
    def _wd(*paths, **kwargs) -> Path:
        """A helper method for getting a working directory.

        Args:
            *paths: The path components to join to the working directory.
            **kwargs: Additional keyword arguments.

        Returns:
            The path to the working directory.

        """
        return ensure_dir_exists(*paths, **kwargs)

    def target_wd(self, *paths) -> Path:
        """Returns the remote working directory on the SUT.

        Args:
            *paths: The path components to join to the working directory.

        Returns:
            The path to the remote working directory.

        """
        return self.config.target_tempdir.joinpath(str(self.id), *paths)

    def __repr__(self):
        """Returns a string representation of the `TestReport` object."""
        return f"<{self.__module__}.{self.__class__.__name__} {self.id}>"

    def list_versions(self, sink, targets: HostsGroup, packages):
        """Lists the available versions of packages.

        Args:
            sink: The function to use for listing the versions.
            targets: The targets to list the versions for.
            packages: The packages to list the versions for.

        """
        query = r"""
            for p in {!s}; do \
                zypper -n search -s --match-exact -t package $p; \
            done \
            | grep -e ^[iv] \
            | awk -F '|' '{{ print $2 $4 }}' \
            | sort -u
        """

        packages = packages or self.get_package_list()

        targets.run(query.format(" ".join(packages)))

        # this is a bit convoluted because the data is aggregated
        # on display (see the example in CommandPrompt#do_list_versions)
        # but acquired piecemeal in random order.
        #
        # input for a single target:
        #
        #   line = PKKGNAME +SP PKGVER
        #   input = *(line EOL)

        # by_host_pkg[hostname][package] = [version, ...]  # noqa: ERA001
        by_host_pkg: dict[str, Any] = {}
        for hn, t in targets.items():
            by_host_pkg[hn] = {}
            for line in t.lastout().split("\n"):
                if match := re.search(re_ver, line):
                    pkg, ver = match.group(1), match.group(2)
                    by_host_pkg[hn].setdefault(pkg, []).append(ver)
                else:
                    continue

        # by_pkg_vers[package][(version, ...)] = [hostname, ...]  # noqa: ERA001
        by_pkg_vers: dict[str, Any] = {}
        for hn, pvs in by_host_pkg.items():
            for pkg, vs in pvs.items():
                by_pkg_vers.setdefault(pkg, {}).setdefault(tuple(vs), []).append(hn)

        # by_hosts_pkg[(hostname, ...)] = [(package, (version, ...)), ...]  # noqa: ERA001
        by_hosts_pkg: dict[tuple[str, ...], Any] = {}
        for pkg, vshs in by_pkg_vers.items():
            for vs, hs in vshs.items():
                by_hosts_pkg.setdefault(tuple(hs), []).append((pkg, vs))

        return sink(targets, by_hosts_pkg)

    def report_results(
        self, targetHosts: list[Target] | None = None
    ) -> list[TargetMeta]:
        """Reports the results of the test report.

        Args:
            targetHosts: A list of target hosts to report results for.
                If None, results are reported for all targets.

        Returns:
            A list of `TargetMeta` objects.

        """
        results: list[TargetMeta] = []

        targets = targetHosts or self.targets.values()

        results.extend(
            TargetMeta(t.hostname, str(t.system), t.packages, t.out) for t in targets
        )

        return results

    @abstractmethod
    def _update_repos_parser(self) -> dict[Product, str]:
        """An abstract method for parsing update repositories."""

    def _update_repos_parse(self) -> None:
        """Parses the update repositories."""
        self.update_repos = self._update_repos_parser()
