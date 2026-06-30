"""The `unload` command."""

from typing import ClassVar

from ..cli.argparse import ArgumentParser
from ..cli.completion import complete_choices
from ..support.messages import TemplateNotLoadedError
from . import Command


class Unload(Command):
    """Unloads one loaded template, closing only its host connections.

    Other loaded templates are left untouched. If the unloaded template was
    the active one, the next remaining template becomes active.
    """

    command = "unload"
    #: ``unload`` names its own target RRID and removes exactly that template,
    #: so it must run once regardless of how many templates are loaded. Without
    #: this it would fan out under MCP (where ``"active"`` defaults to fan-out
    #: with several loaded) and try to remove the same RRID once per template,
    #: failing on the second pass with ``TemplateNotLoadedError``.
    scope: ClassVar[str] = "single"

    @classmethod
    def _add_arguments(cls, parser: ArgumentParser) -> None:
        """Adds arguments to the command's argument parser."""
        parser.add_argument(
            "rrid",
            action="store",
            type=str,
            help="RRID of the loaded template to unload",
        )

    def __call__(self) -> None:
        """Executes the `unload` command."""
        rrid: str = self.args.rrid
        try:
            self.templates.remove(rrid)
        except KeyError:
            raise TemplateNotLoadedError(rrid) from None
        self.prompt.set_prompt()

    @staticmethod
    def complete(state, text, line, begidx, endidx) -> list[str]:
        """Provides tab completion for the command over loaded RRIDs."""
        templates = state.get("templates")
        rrids = [(rrid,) for rrid in templates.rrids()] if templates else []
        return complete_choices(rrids, line, text)
