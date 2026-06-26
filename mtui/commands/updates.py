"""The ``updates`` interactive command.

Lists the update queue, fetched live from the TeReGen API
(``GET /api/v1/updates``, fed from SMELT behind the scenes) and sorted by
priority. Each row shows priority, status, kind (SLFO / Maintenance / ...),
deadline and the RRID. The queue merges gitea-sourced updates (SLFO/SL-Micro)
with the classic Maintenance updates in QAM testing.
"""

from __future__ import annotations

from logging import getLogger

from ..cli.argparse import ArgumentParser
from ..cli.completion import complete_choices
from ..data_sources import TeReGen
from . import Command

logger = getLogger("mtui.commands.updates")


class Updates(Command):
    """List the unreleased update queue (via the TeReGen API)."""

    command = "updates"

    @classmethod
    def _add_arguments(cls, parser: ArgumentParser) -> None:
        """Register the command's arguments."""
        parser.add_argument(
            "--review-group",
            default=None,
            help="filter by review group, e.g. qam-sle",
        )
        parser.add_argument(
            "--status",
            default=None,
            help="filter by status, e.g. testing",
        )
        parser.add_argument(
            "--limit",
            type=int,
            default=0,
            help="cap the number of rows (0 = all)",
        )
        # --assignee/--mine/--unassigned select at most one assignment view;
        # argparse rejects any combination of the three for us.
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
            "--unassigned",
            action="store_true",
            help="filter to updates with no assignee",
        )
        parser.add_argument(
            "--show-assignment",
            action="store_true",
            help="show the assignee on each row without filtering",
        )

    def __call__(self) -> None:
        """Fetch and print the update queue from TeReGen."""
        assignee = self.args.assignee
        if self.args.mine:
            assignee = self.config.session_user

        # Any assignment-related flag means rows should carry assignment info.
        want_assignment = bool(
            assignee or self.args.unassigned or self.args.show_assignment
        )

        updates = TeReGen(self.config).updates(
            review_group=self.args.review_group,
            status=self.args.status,
            assignee=assignee,
            unassigned=self.args.unassigned,
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
            deadline = (u.get("deadline") or "")[:10] or "-"
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
                ("--unassigned",),
                ("--show-assignment",),
            ],
            line,
            text,
        )
