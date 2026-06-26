"""The ``checkers`` interactive command.

Lists the build-check (checker) result runs for the loaded update, fetched live
from the TeReGen report API (``GET /reports/{id}/checkers``).
"""

from __future__ import annotations

from logging import getLogger

from ..cli.argparse import ArgumentParser
from ..cli.colors import green, red
from ..cli.completion import complete_choices, template_completion
from ..data_sources import TeReGen
from ..support.misc import requires_update
from . import Command

logger = getLogger("mtui.commands.checkers")

# Checker result strings that count as success (everything else is shown red).
_PASSING = frozenset({"passed", "success", "ok", "done"})


class Checkers(Command):
    """List the build-check (checker) results for the loaded update (via TeReGen)."""

    command = "checkers"
    scope = "fanout"

    @classmethod
    def _add_arguments(cls, parser: ArgumentParser) -> None:
        """Register the command's arguments."""
        cls._add_template_arg(parser)

    @requires_update
    def __call__(self) -> None:
        """Fetch and print the update's checker results from TeReGen."""
        checkers = TeReGen(self.config).checkers(self.metadata.rrid)
        if not checkers:
            self.println(f"No checker results for {self.metadata.rrid}")
            return

        self.println(f"Checker results for {self.metadata.rrid} ({len(checkers)}):")
        for c in checkers:
            name = c.get("name", "?") if isinstance(c, dict) else str(c)
            status = (
                str(c.get("status") or c.get("state") or "?")
                if isinstance(c, dict)
                else "?"
            )
            colour = green if status.lower() in _PASSING else red
            self.println(f"  {colour(status.ljust(10))} {name}")

    @staticmethod
    def complete(state, text, line, begidx, endidx) -> list[str]:
        """Provides tab completion for the command."""
        return complete_choices([*template_completion(state)], line, text)
