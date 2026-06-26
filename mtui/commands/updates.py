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

    def __call__(self) -> None:
        """Fetch and print the update queue from TeReGen."""
        updates = TeReGen(self.config).updates(
            review_group=self.args.review_group, status=self.args.status
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
            self.println(
                f"  prio={u.get('priority', '?')!s:<5} "
                f"{u.get('status', '?')!s:<10} "
                f"{u.get('kind', '?')!s:<12} "
                f"{deadline:<11} {u.get('id', '?')}"
            )

    @staticmethod
    def complete(state, text, line, begidx, endidx) -> list[str]:
        """Provides tab completion for the command."""
        return complete_choices(
            [("--review-group",), ("--status",), ("--limit",)], line, text
        )
