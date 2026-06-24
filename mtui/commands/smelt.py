"""SMELT query commands, auto-exposed over MCP as ``mcp__mtui__smelt_*``.

* ``smelt_update``  — SMELT detail (priority, deadline, status, …) for the
  loaded update; dispatches SLFO -> REST v2, Maintenance -> GraphQL.
* ``smelt_checkers`` — checker (build-check) result runs for the loaded SLFO
  update.
* ``smelt_updates`` — enumerate the SLFO (new) update queue with filters, e.g.
  the testing updates still pending ``qam-sle-review``; ``--unassigned`` /
  ``--show-assignment`` annotate each with the current assignee (read from the
  PR's mtui comments via Gitea).
* ``smelt_requests`` — enumerate the classic Maintenance (old) review-request
  queue, e.g. those pending ``qam-sle``.

All are read-only and require ``[smelt] url`` to be configured.
"""

from __future__ import annotations

from logging import getLogger

from ..cli.argparse import ArgumentParser
from ..cli.completion import complete_choices, template_completion
from ..data_sources import Smelt
from ..data_sources.gitea import Gitea, pr_api_url
from ..data_sources.smelt import slfo_update_id
from ..support.exceptions import FailedGiteaCallError, MissingGiteaTokenError
from ..support.misc import requires_update
from ..types import RequestKind
from . import Command

logger = getLogger("mtui.commands.smelt")


def _smelt(cmd: Command) -> Smelt | None:
    """Build the client; print a hint and return ``None`` when unconfigured."""
    smelt = Smelt(cmd.config)
    if not smelt.configured:
        cmd.println("SMELT not configured — set [smelt] url in your mtui config")
        return None
    return smelt


def _loaded_slfo_id(metadata) -> str | None:
    """REST v2 update id for the loaded update, or ``None`` if not SLFO."""
    rrid = metadata.rrid
    if rrid.kind is not RequestKind.SLFO:
        return None
    return slfo_update_id(getattr(metadata, "giteapr", None), rrid.review_id)


class SmeltUpdate(Command):
    """Show the loaded update's SMELT detail (priority, deadline, status, …)."""

    command = "smelt_update"
    scope = "fanout"

    @classmethod
    def _add_arguments(cls, parser: ArgumentParser) -> None:
        """Adds arguments to the command's argument parser."""
        cls._add_template_arg(parser)

    @requires_update
    def __call__(self) -> None:
        """Print SMELT detail for the loaded update."""
        smelt = _smelt(self)
        if smelt is None:
            return
        rrid = self.metadata.rrid
        if rrid.kind is RequestKind.SLFO:
            uid = _loaded_slfo_id(self.metadata)
            data = smelt.update(uid) if uid else None
            if not data:
                self.println("no SMELT data for this update")
                return
            self.println(f"id       : {data.get('human_readable_id')}")
            self.println(f"status   : {data.get('status')}")
            self.println(f"category : {data.get('category')}")
            self.println(f"rating   : {(data.get('rating') or {}).get('name')}")
            self.println(f"priority : {data.get('priority')}")
            self.println(f"deadline : {data.get('deadline')}")
            pkgs = [
                p.get("name")
                for p in (data.get("packages") or [])
                if isinstance(p, dict)
            ]
            self.println(f"packages : {', '.join(pkgs)}")
        elif rrid.kind is RequestKind.MAINTENANCE:
            node = smelt.incident(rrid.maintenance_id)
            if not node:
                self.println("no SMELT incident data for this update")
                return
            self.println(f"incident : {node.get('incidentId')}")
            self.println(f"status   : {(node.get('status') or {}).get('name')}")
            self.println(f"rating   : {(node.get('rating') or {}).get('name')}")
            self.println(f"priority : {node.get('priority')}")
            self.println(f"deadline : {node.get('crd') or node.get('prd')}")
            self.println(f"crd      : {node.get('crd')}")
            self.println(f"prd      : {node.get('prd')}")
        else:
            self.println("SMELT detail is not available for this request kind")

    @staticmethod
    def complete(state, text, line, begidx, endidx) -> list[str]:
        """Provides tab completion for the command."""
        return complete_choices(template_completion(state), line, text)


class SmeltCheckers(Command):
    """Show checker (build-check) result runs for the loaded SLFO update."""

    command = "smelt_checkers"
    scope = "fanout"

    @classmethod
    def _add_arguments(cls, parser: ArgumentParser) -> None:
        """Adds arguments to the command's argument parser."""
        cls._add_template_arg(parser)

    @requires_update
    def __call__(self) -> None:
        """Print the checker-result runs for the loaded update."""
        smelt = _smelt(self)
        if smelt is None:
            return
        uid = _loaded_slfo_id(self.metadata)
        if not uid:
            self.println("checker results are available for SLFO updates only")
            return
        runs = smelt.checker_results(uid)
        if not runs:
            self.println("no checker results for this update")
            return
        for r in runs:
            self.println(
                f"{str(r.get('checker_type', '?')):10} "
                f"pass={r.get('pass_count', 0)} fail={r.get('fail_count', 0)} "
                f"warn={r.get('warn_count', 0)} error={r.get('error_count', 0)} "
                f"running={r.get('running_count', 0)}  "
                f"{r.get('finished') or r.get('started') or ''}"
            )

    @staticmethod
    def complete(state, text, line, begidx, endidx) -> list[str]:
        """Provides tab completion for the command."""
        return complete_choices(template_completion(state), line, text)


class SmeltRequests(Command):
    """Enumerate the classic Maintenance review-request queue (GraphQL).

    The old-SMELT counterpart of ``smelt_updates``: lists requests assigned to a
    review group (default ``qam-sle``), with ``--pending`` for those whose group
    review is still ``new``. Shows the per-request assignee, which the SLFO feed
    does not expose.
    """

    command = "smelt_requests"

    @classmethod
    def _add_arguments(cls, parser: ArgumentParser) -> None:
        """Register filters for the classic request queue."""
        parser.add_argument(
            "--group",
            default="qam-sle",
            help="review assigned-by group (default: qam-sle)",
        )
        parser.add_argument(
            "--pending",
            action="store_true",
            help="only requests whose group review is still 'new'",
        )
        parser.add_argument("--status", help="request status, e.g. review / accepted")
        parser.add_argument(
            "--limit", type=int, default=0, help="cap the number of rows (0 = all)"
        )

    def __call__(self) -> None:
        """Print the filtered classic request queue, highest priority first."""
        smelt = _smelt(self)
        if smelt is None:
            return
        nodes = smelt.review_requests(group=self.args.group, status=self.args.status)
        rows = []
        for n in nodes:
            mine = next(
                (
                    e["node"]
                    for e in n["reviewSet"]["edges"]
                    if (e["node"].get("assignedByGroup") or {}).get("name")
                    == self.args.group
                ),
                None,
            )
            if self.args.pending and (
                not mine or (mine.get("status") or {}).get("name") != "new"
            ):
                continue
            inc = n.get("incident") or {}
            pkgs = [e["node"]["name"] for e in inc.get("packages", {}).get("edges", [])]
            rows.append(
                {
                    "priority": inc.get("priority") or 0,
                    "request": n.get("requestId"),
                    "incident": inc.get("incidentId"),
                    "assignee": (mine.get("assignedTo") or {}).get("username")
                    if mine
                    else None,
                    "packages": ", ".join(pkgs),
                }
            )
        rows.sort(key=lambda r: -(r["priority"] or 0))
        if self.args.limit:
            rows = rows[: self.args.limit]
        for r in rows:
            self.println(
                f"req {str(r['request']):8} inc {str(r['incident']):8} "
                f"prio={str(r['priority']):<5} {str(r['assignee'] or 'unassigned'):14} "
                f"{str(r['packages'])[:40]}"
            )
        self.println(f"\n{len(rows)} request(s)")


class SmeltUpdates(Command):
    """Enumerate the SLFO update queue (with filters).

    ``--unassigned`` keeps only updates nobody has picked up for ``--group``
    (default ``qam-sle``); ``--show-assignment`` adds an assignee column.
    Both derive the assignee from the PR's mtui assign/unassign comments via
    Gitea, so they need a Gitea token. The lookup is one Gitea call per row
    and runs lazily, highest-priority first, so e.g. ``--pending
    qam-sle-review --unassigned --limit 1`` finds the top unassigned update in
    only a few calls.
    """

    command = "smelt_updates"

    @classmethod
    def _add_arguments(cls, parser: ArgumentParser) -> None:
        """Register filters for the queue listing."""
        parser.add_argument(
            "--status", help="only updates with this status, e.g. testing"
        )
        parser.add_argument(
            "--review-group",
            dest="review_group",
            help="narrow to updates assigned to this review group",
        )
        parser.add_argument(
            "--pending",
            help="only updates whose review by this group is not yet APPROVED",
        )
        parser.add_argument(
            "--group",
            default="qam-sle",
            help="review group for assignment lookup (default: qam-sle)",
        )
        parser.add_argument(
            "--unassigned",
            action="store_true",
            help="only updates not currently assigned to anyone for --group "
            "(assignment is read from the PR's mtui comments)",
        )
        parser.add_argument(
            "--show-assignment",
            action="store_true",
            help="show the current assignee column (per-PR Gitea lookup)",
        )
        parser.add_argument(
            "--limit", type=int, default=0, help="cap the number of rows (0 = all)"
        )

    def _assignee(self, item: dict) -> str | None:
        """Current assignee for ``item``'s PR and ``--group``, or ``None``.

        Best-effort: a malformed URL or a Gitea hiccup logs at debug and
        yields ``None`` (treated as unassigned) so one bad row never aborts
        the listing.
        """
        url = item.get("external_url")
        if not url:
            return None
        try:
            return Gitea(self.config, pr_api_url(url), group=self.args.group).assignee()
        except (ValueError, FailedGiteaCallError, MissingGiteaTokenError) as e:
            logger.debug("assignee lookup failed for %s: %s", url, e)
            return None

    def __call__(self) -> None:
        """Print the filtered SLFO update queue, highest priority first."""
        smelt = _smelt(self)
        if smelt is None:
            return
        items = smelt.unreleased(self.args.review_group)
        out = []
        for it in items:
            if self.args.status and it.get("status") != self.args.status:
                continue
            if self.args.pending:
                rev = next(
                    (
                        r
                        for r in (it.get("reviews") or [])
                        if r.get("name") == self.args.pending
                    ),
                    None,
                )
                if not rev or rev.get("state") == "APPROVED":
                    continue
            out.append(it)
        out.sort(key=lambda x: -(x.get("priority") or 0))

        want_assignment = self.args.unassigned or self.args.show_assignment
        if want_assignment and not self.config.gitea_token:
            self.println(
                "assignment lookup needs a Gitea token ([gitea] token); "
                "ignoring --unassigned/--show-assignment"
            )
            want_assignment = False

        # Compute the assignee lazily, highest priority first, and stop as soon
        # as --limit rows are collected — so --unassigned --limit 1 is cheap.
        rows: list[tuple[dict, str | None]] = []
        for it in out:
            assignee = self._assignee(it) if want_assignment else None
            if self.args.unassigned and assignee is not None:
                continue
            rows.append((it, assignee))
            if self.args.limit and len(rows) >= self.args.limit:
                break

        for it, assignee in rows:
            pkgs = [
                p.get("name") for p in (it.get("packages") or []) if isinstance(p, dict)
            ]
            line = (
                f"{str(it.get('human_readable_id', '')):20} "
                f"{str(it.get('status')):16} prio={it.get('priority')}  "
            )
            if want_assignment:
                line += f"{str(assignee or 'unassigned'):16} "
            line += f"{','.join(pkgs)[:50]}"
            self.println(line)
        self.println(f"\n{len(rows)} update(s)")
