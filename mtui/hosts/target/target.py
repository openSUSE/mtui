"""The `Target` class, which represents a single target host."""

import errno
from collections.abc import Callable
from logging import getLogger
from pathlib import Path
from string import Template
from traceback import format_exc
from typing import TYPE_CHECKING, Any, final

from ...support import messages
from ...support.config import Config
from ...support.fileops import timestamp
from ...types import ExecutionMode, HostLog, Package, System, TargetState
from ...types.rpmver import RPMVersion
from ...update_workflow.actions import (
    downgrader,
    installer,
    preparer,
    uninstaller,
    updater,
)
from ...update_workflow.checks import (
    downgrade_checks,
    install_checks,
    prepare_checks,
    update_checks,
)
from ..connection import CommandTimeoutError, Connection, policy_from_config
from . import TargetLock, TargetLockedError
from .package_querier import PackageQuerier
from .parsers import parse_system
from .repo_manager import RepoManager
from .reporter import Reporter

if TYPE_CHECKING:
    from ...cli.prompter import Prompter

logger = getLogger("mtui.target")


def _no_checks(*args: tuple[Any, ...]) -> None:
    return None


# Dispatch tables for Target.doer/check. ``installer`` etc. are the
# (release, transactional) -> dict-of-templates registries from
# mtui.update_workflow.actions; ``install_checks`` etc. are the parallel
# callable registries from mtui.update_workflow.checks. ``preparer`` is
# dispatched inline in
# Target.doer because its registry yields a callable, not a dict.
# ``uninstaller`` deliberately consults ``install_checks`` (no dedicated
# uninstall_checks table exists) — preserves prior behaviour.
_DOERS: dict[str, dict[tuple[str, bool], dict[str, Template]]] = {
    "installer": installer,
    "uninstaller": uninstaller,
    "downgrader": downgrader,
    "updater": updater,
}
_CHECKS: dict[str, dict[tuple[str, bool], Callable]] = {
    "installer": install_checks,
    "uninstaller": install_checks,
    "downgrader": downgrade_checks,
    "updater": update_checks,
    "preparer": prepare_checks,
}


@final
class Target:
    """Represents a single target host.

    This class provides methods for interacting with the host, such as
    running commands, transferring files, and managing locks.
    """

    def __init__(
        self,
        config: Config,
        hostname: str,
        packages: dict[str, dict[str, str]] | None = None,
        state: TargetState | str = TargetState.ENABLED,
        timeout: int = 300,
        mode: ExecutionMode = ExecutionMode.PARALLEL,
        lock: type[TargetLock] = TargetLock,
        connection: type[Connection] = Connection,
        prompter: "Prompter | None" = None,
    ) -> None:
        """Initializes the `Target` object.

        Args:
            config: The application configuration.
            hostname: The hostname of the target.
            packages: A dictionary of packages for the target.
            state: The initial state of the target.
            timeout: The command timeout for the target.
            mode: Whether the target runs commands in parallel with the
                rest of its group or holds the group in a serial barrier.
            lock: The lock class to use for the target.
            connection: The connection class to use for the target.
            prompter: Optional :class:`mtui.cli.prompter.Prompter` used to
                surface SSH command-timeout questions to the user with
                cross-thread serialisation. ``None`` means "no prompt,
                silently wait on timeout" — see
                :class:`mtui.connection.Connection`.

        """
        self.config = config
        self.host, _, self.port = hostname.partition(":")
        self.hostname = hostname
        self.system: System
        self.packages: dict[str, Package] = {}
        self.out = HostLog()
        self.TargetLock = lock
        self.Connection = connection
        self._prompter = prompter

        self.state: TargetState | str = TargetState(state)
        # default timeout for target, used only on connecting/reconnecting Target
        self._timeout = timeout
        self.mode = mode
        self.connection: Connection
        self.transactional = False
        # helper for packages before system analysis
        self._pkgs = packages

    def _parse_packages(self) -> dict[str, Package]:
        """Parses the packages for the target host.

        Returns:
            A dictionary of `Package` objects.

        """
        ret: dict[str, Package] = {}
        base_version = self.system.get_base().version
        if self._pkgs:
            if "standard" in self._pkgs and len(self._pkgs) == 1:
                packages = self._pkgs["standard"]
            else:
                packages = self._pkgs.get(base_version, {})
            if base_version.startswith("12"):
                packages.update(self._pkgs.get("12", {}))

            for key, value in packages.items():
                package = Package(key)
                package.required = value
                ret[key] = package
        return ret

    def connect(self) -> None:
        """Connects to the target host."""
        try:
            logger.info("connecting to %s", self.hostname)
            policy_name = getattr(
                self.config, "ssh_strict_host_key_checking", "auto_add"
            )
            self.connection = self.Connection(
                self.host,
                self.port,
                self._timeout,
                missing_host_key_policy=policy_from_config(policy_name),
                timeout_prompt=self._prompter.ask if self._prompter else None,
            )
        except Exception as e:
            logger.critical(messages.ConnectingTargetFailedMessage(self.hostname, e))
            raise e

        self._lock = self.TargetLock(self.connection, self.config)
        if self.is_locked() and not self._lock.reap_if_stale():
            logger.warning(self._lock.locked_by_msg())

        # get system
        self.system, self.transactional = parse_system(self.connection)

        # parse packages
        self.packages = self._parse_packages()
        self.query_versions()

    def reboot(self, command: str) -> None:
        """Sends a reboot command without waiting for it to return.

        The command is expected to drop the SSH connection; callers
        should follow up with :meth:`reconnect`.

        Args:
            command: The reboot command to dispatch.

        """
        self.connection.fire_and_forget(command)

    def boot_id(self) -> str:
        """Returns the host's current boot id.

        Reads ``/proc/sys/kernel/random/boot_id``, which changes on every
        boot. Used to confirm a reboot actually happened. Returns an empty
        string if it cannot be read.

        Returns:
            The boot id, or "" if it could not be read.

        """
        try:
            self.connection.run("cat /proc/sys/kernel/random/boot_id")
        except Exception:
            return ""
        return self.connection.stdout.strip()

    def reconnect(self, retry, backoff) -> None:
        """Reconnects to the target host.

        Args:
            retry: The number of times to retry the connection.
            backoff: Whether to use exponential backoff for retries.

        """
        # ``backoff`` must be passed by keyword: ``Connection.reconnect``'s
        # second positional parameter is ``timeout``, not ``backoff``.
        self.connection.reconnect(retry, backoff=backoff)

    def reload_system(self) -> None:
        """Reloads the system information for the target host."""
        self.system, self.transactional = parse_system(self.connection)

    def __eq__(self, other: object) -> bool:
        """Checks if two `Target` objects are equal.

        Equality is keyed on ``hostname`` so the contract matches
        ``__hash__``. Comparing against a non-``Target`` returns
        ``NotImplemented`` per the Python data model.
        """
        if not isinstance(other, Target):
            return NotImplemented
        return self.hostname == other.hostname

    def __hash__(self) -> int:
        """Hashes the target by hostname."""
        return hash(self.hostname)

    def __ne__(self, other: object) -> bool:
        """Checks if two `Target` objects are not equal."""
        result = self.__eq__(other)
        if result is NotImplemented:
            return NotImplemented  # type: ignore[return-value]
        return not result

    def query_versions(self, packages=None) -> None:
        """Queries the package versions for the target host.

        Args:
            packages: A list of packages to query. If None, all packages
                for the target are queried.

        """
        if packages is None:
            packages = list(self.packages.keys())
        if not packages:
            return
        match self.state:
            case TargetState.ENABLED:
                pvs = self.query_package_versions(packages)
                for p, v in pvs.items():
                    self.packages[p].current = v
            case TargetState.DRYRUN:
                logger.info('dryrun: %s running "rpm -q %s"', self.hostname, packages)
                self.out.append([f"rpm -q {packages}", "dryrun\n", "", 0, 0])
            case TargetState.DISABLED:
                self.out.append(["", "", "", 0, 0])

    def query_package_versions(
        self, packages: list[str]
    ) -> dict[str, RPMVersion | None]:
        """Queries the versions of a list of packages.

        Thin delegate to :class:`PackageQuerier`; kept so existing
        callers (``HostsGroup.query_versions``, this class's own
        ``query_versions``) do not need to change.

        Args:
            packages: A list of packages to query.

        Returns:
            A dictionary mapping package names to `RPMVersion` objects.

        """
        return PackageQuerier(self).versions(packages)

    def set_timeout(self, value: int) -> None:
        """Sets the command timeout for the target host.

        Args:
            value: The timeout in seconds.

        """
        logger.debug("%s: setting timeout to %d", self.hostname, value)
        self.connection.timeout = value
        self._timeout = value

    @property
    def repo_manager(self) -> "RepoManager":
        """The :class:`RepoManager` collaborator for this target.

        Fresh-per-access; see :attr:`reporter` for the rationale.
        """
        return RepoManager(self)

    def run(self, command: str, lock=None) -> None:
        """Runs a command on the target host.

        Args:
            command: The command to run.
            lock: An optional lock to use.

        """
        match self.state:
            case TargetState.ENABLED:
                logger.debug('%s: running "%s"', self.hostname, command)
                time_before = timestamp()
                try:
                    exitcode = self.connection.run(command, lock)
                except CommandTimeoutError:
                    logger.critical(
                        '%s: command "%s" timed out', self.hostname, command
                    )
                    exitcode = -1
                except AssertionError:
                    logger.debug("zombie command terminated")
                    logger.debug(format_exc())
                    return
                except Exception:
                    # failed to run command
                    logger.error(
                        '%s: failed to run command "%s"', self.hostname, command
                    )
                    exitcode = -1

                time_after = timestamp()
                runtime = int(time_after) - int(time_before)
                # this is wrong
                self.out.append(
                    [
                        command,
                        self.connection.stdout,
                        self.connection.stderr,
                        exitcode,
                        runtime,
                    ]
                )
            case TargetState.DRYRUN:
                logger.info('dryrun: %s running "%s"', self.hostname, command)
                self.out.append([command, "dryrun\n", "", 0, 0])
            case TargetState.DISABLED:
                self.out.append(["", "", "", 0, 0])

    def shell(self) -> None:
        """Spawns a shell on the target host."""
        logger.debug("%s: spawning shell", self.hostname)

        try:
            self.connection.shell()
        except Exception:
            # failed to spawn shell
            logger.error("%s: failed to spawn shell", self.hostname)

    def sftp_put(self, local: Path, remote: Path) -> None:
        """Uploads a file to the target host.

        Args:
            local: The local path to the file to upload.
            remote: The remote path to upload the file to.

        """
        if self.state == "enabled":
            logger.debug('%s: sending "%s"', self.hostname, local)
            try:
                return self.connection.sftp_put(local, remote)
            except OSError:
                logger.error("%s: failed to send %s", self.hostname, local)
        elif self.state == "dryrun":
            logger.info("dryrun: put %s %s:%s", local, self.hostname, remote)

    def sftp_get(self, remote: Path, local: Path) -> None:
        """Downloads a file from the target host.

        Args:
            remote: The remote path to the file to download.
            local: The local path to save the downloaded file to.

        """
        if str(remote).endswith("/"):
            f = self.connection.sftp_get_folder
            s = "folder"
            local = Path(str(local) + "/")
        else:
            f = self.connection.sftp_get
            s = "file"
            local = Path(f"{local}.{self.hostname}")

        if self.state == "enabled":
            logger.debug(
                '%s: receiving %s "%s" into "%s', self.hostname, s, remote, local
            )
            try:
                f(remote, local)
            except OSError:
                logger.error(
                    "%s: failed to get %s %s",
                    self.hostname,
                    s,
                    remote,
                )
        elif self.state == "dryrun":
            logger.info("dryrun: get %s %s:%s %s", self.hostname, s, remote, local)

    def lastin(self) -> str:
        """Returns the last command that was run.

        Returns:
            The last command that was run.

        """
        try:
            return self.out[-1][0]
        except IndexError:
            return ""

    def lastout(self) -> str:
        """Returns the last stdout from a command.

        Returns:
            The last stdout from a command.

        """
        try:
            return self.out[-1][1]
        except IndexError:
            return ""

    def lasterr(self) -> str:
        """Returns the last stderr from a command.

        Returns:
            The last stderr from a command.

        """
        try:
            return self.out[-1][2]
        except IndexError:
            return ""

    def lastexit(self) -> str:
        """Returns the last exit code from a command.

        Returns:
            The last exit code from a command.

        """
        try:
            return self.out[-1][3]
        except IndexError:
            return ""

    def is_locked(self) -> bool:
        """Checks if the target is locked.

        Returns:
            True if the target is locked, False otherwise.

        """
        return self._lock.is_locked()

    def lock(self, comment: str = "") -> None:
        """Locks the target.

        Args:
            comment: An optional comment for the lock.

        """
        self._lock.lock(comment)

    def unlock(self, force: bool = False) -> None:
        """Unlocks the target.

        Args:
            force: If True, unlocks the target even if it is locked
                by another user.

        """
        try:
            self._lock.unlock(force)
        except TargetLockedError as e:
            logger.warning(e)
            raise

    def add_history(self, comment: str) -> None:
        """Adds a history entry to the target.

        Args:
            comment: The history entry to add.

        """
        if self.state == "enabled":
            logger.debug("%s: adding history entry", self.hostname)
            try:
                filename = Path("/var/log/mtui.log")
                historyfile = self.connection.sftp_open(filename, "a+")
            except Exception:
                logger.error("failed to open history file")
                return

            now = timestamp()
            user: str = self.config.session_user
            try:
                historyfile.write("{}:{}:{}\n".format(now, user, ":".join(comment)))
                historyfile.close()
            except Exception:
                pass

    def sftp_listdir(self, path: Path) -> list[str]:
        """Lists the contents of a directory on the target.

        Args:
            path: The path to the directory to list.

        Returns:
            A list of filenames in the directory.

        """
        try:
            return self.connection.sftp_listdir(path)
        except OSError as error:
            if error.errno == errno.ENOENT:
                logger.debug("%s: directory %s does not exist", self.hostname, path)
            return []

    def sftp_remove(self, path: Path) -> None:
        """Deletes a file or directory on the target.

        Args:
            path: The path to the file or directory to delete.

        """
        try:
            self.connection.sftp_remove(path)
        except OSError as error:
            if error.errno == errno.ENOENT:
                logger.debug("%s: path %s does not exist", self.hostname, path)
            else:
                try:
                    # might be a directory
                    self.connection.sftp_rmdir(path)
                except OSError:
                    logger.warning("unable to remove %s on %s", path, self.hostname)

    def close(self, action=None) -> None:
        """Closes the connection to the target.

        Args:
            action: An optional action to perform before closing the
                connection ("reboot" or "poweroff").

        """
        try:
            if self.connection and self.connection.is_active():
                self.connection.timeout = 15
                self.unlock()
        except Exception:
            # ignore if the connection seems to be lost
            pass
        else:
            if action == "reboot":
                logger.info("rebooting %s", self.hostname)
                self.run("reboot")
            elif action == "poweroff":
                logger.info("powering off %s", self.hostname)
                self.run("halt")
            else:
                logger.info("closing connection to %s", self.hostname)

        if self.connection:
            self.connection.close()

    @property
    def reporter(self) -> "Reporter":
        """The :class:`Reporter` collaborator for this target.

        Returns a fresh ``Reporter`` per access — the type is stateless,
        so allocation cost is negligible compared to the SSH call that
        is usually about to happen anyway. Avoids keeping a strong
        cached reference, which would otherwise tie ``Reporter`` (and
        anything captured by sink callbacks) into ``Target``'s lifetime.
        """
        return Reporter(self)

    def doer(
        self, role: str, force: bool = False, testing: bool = False
    ) -> dict[str, Template]:
        """Returns the action-template dict for ``role`` on this target.

        ``role`` is one of ``"installer"``, ``"uninstaller"``,
        ``"downgrader"``, ``"updater"``, ``"preparer"``. The lookup is
        keyed by ``(release, transactional)``; missing entries surface as
        the corresponding ``MissingXerError`` raised by the underlying
        registry. ``force`` and ``testing`` are only honoured for
        ``"preparer"`` (the only role whose registry value is a callable
        rather than a dict).
        """
        key = (self.system.get_release(), self.transactional)
        if role == "preparer":
            return preparer[key](force, testing)
        return _DOERS[role][key]

    def check(self, role: str) -> Callable:
        """Returns the post-run check callable for ``role`` on this target.

        Falls back to a no-op when the registry has no entry for the
        current ``(release, transactional)`` tuple. Mirrors
        :meth:`doer`'s ``role`` vocabulary; note that the historical
        ``get_uninstaller_check`` consulted ``install_checks`` (no
        dedicated uninstall-check table exists), and that behaviour is
        preserved here.
        """
        key = (self.system.get_release(), self.transactional)
        return _CHECKS[role].get(key, _no_checks)

    def __repr__(self) -> str:
        """Returns a string representation of the `Target` object."""
        return f"<Target - {self.hostname}>"

    def __str__(self) -> str:
        """Returns the hostname of the target."""
        return self.hostname
