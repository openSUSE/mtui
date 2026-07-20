"""The ``updates`` interactive command.

Lists the update queue, fetched live from the TeReGen API
(``GET /api/v1/updates``, fed from SMELT behind the scenes) and sorted by
priority. Each row shows priority, status, kind (SLFO / Maintenance / ...),
deadline and the RRID. The queue merges gitea-sourced updates (SLFO/SL-Micro)
with the classic Maintenance updates in QAM testing.

By default the command shows the actionable pickup queue: **unassigned**
updates that are **in testing** (``--status testing``). Pick another
assignment view (``--assignee``/``--mine``/``--all-assignees``) to drop the
unassigned default, or pass ``--status all`` to see the whole queue (every
status, every assignee), including released updates.
"""

from __future__ import annotations

from logging import getLogger

from ..cli.argparse import ArgumentParser
from ..cli.completion import complete_choices
from ..data_sources import TeReGen
from . import Command

logger = getLogger("mtui.commands.updates")


class Updates(Command):
    """List the unassigned, in-testing update queue (via the TeReGen API)."""

    command = "updates"

    #: ``--status`` value that widens the queue to every status (the escape
    #: hatch translated to ``status=None`` server-side).
    STATUS_ALL = "all"

    @classmethod
    def _add_arguments(cls, parser: ArgumentParser) -> None:
        """Register the command's arguments."""
        parser.add_argument(
            "--review-group",
            default=None,
            help="filter by review group as the bare group name, e.g. qam-sle "
            "(not the '<group>-review' login form, which classic rows lack)",
        )
        parser.add_argument(
            "--status",
            default="testing",
            help="filter by status (default: testing); use 'all' for every status",
        )
        parser.add_argument(
            "--limit",
            type=int,
            default=0,
            help="cap the number of rows (0 = all)",
        )
        # --assignee/--mine/--all-assignees select at most one assignment view;
        # argparse rejects any combination of the three for us. The default
        # (no flag) is the unassigned queue.
        assignment = parser.add_mutually_exclusive_group()
        assignment.add_argument(
            "--assignee",
            default=None,
            help="filter to updates assigned to this user (any qam group)",
        )
        assignment.add_argument(
            "--mine",
            action="store_true",
            help="filter to updates assigned to the current session user",
        )
        assignment.add_argument(
            "--all-assignees",
            action="store_true",
            help="show every update regardless of assignee (assigned and "
            "unassigned), overriding the unassigned default",
        )

    def __call__(self) -> None:
        """Fetch and print the update queue from TeReGen."""
        assignee = self.args.assignee
        if self.args.mine:
            assignee = self.config.session_user

        # '--status all' is the escape hatch: widen to every status by sending
        # no status filter at all (the server returns released updates too).
        status_all = self.args.status == self.STATUS_ALL
        status = None if status_all else self.args.status

        # Default view is the unassigned pickup queue: with no assignment view
        # chosen, filter to unassigned. --assignee/--mine pick a specific user;
        # --all-assignees and --status all opt out of the unassigned filter (the
        # latter because 'unassigned' implies status=testing server-side).
        chose_other_view = bool(assignee or self.args.all_assignees)
        unassigned = not chose_other_view and not status_all

        # Show the assignee column whenever assignment is part of the view.
        want_assignment = bool(assignee or unassigned or self.args.all_assignees)

        updates = TeReGen(self.config).updates(
            review_group=self.args.review_group,
            status=status,
            assignee=assignee,
            unassigned=unassigned,
            with_assignment=want_assignment,
        )
        if not updates:
            self.println("No updates in the queue")
            return

        if self.args.limit:
            updates = updates[: self.args.limit]

        self.println(f"Update queue ({len(updates)}):")
        for u in updates:
            if not isinstance(u, dict):
                self.println(f"  {u}")
                continue
            # deadline is an ISO timestamp; the date alone is enough for a row.
            # str() guards against shape drift (a non-string would crash the
            # whole listing over one row).
            deadline = str(u.get("deadline") or "")[:10] or "-"
            row = (
                f"  prio={u.get('priority', '?')!s:<5} "
                f"{u.get('status', '?')!s:<10} "
                f"{u.get('kind', '?')!s:<12} "
                f"{deadline:<11} {u.get('id', '?')}"
            )
            if want_assignment:
                row += f" assignee={u.get('assignee') or 'unassigned'}"
            self.println(row)

    @staticmethod
    def complete(state, text, line, begidx, endidx) -> list[str]:
        """Provides tab completion for the command."""
        return complete_choices(
            [
                ("--review-group",),
                ("--status",),
                ("--limit",),
                ("--assignee",),
                ("--mine",),
                ("--all-assignees",),
            ],
            line,
            text,
        )
