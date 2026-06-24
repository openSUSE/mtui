"""The `switch` command."""

from ..cli.argparse import ArgumentParser
from ..cli.completion import complete_choices
from ..support.messages import TemplateNotLoadedError
from . import Command


class Switch(Command):
    """Switches the active template to another loaded one.

    Plain action commands act on the active template. Use ``list_templates``
    to see the loaded RRIDs and which one is active.
    """

    command = "switch"

    @classmethod
    def _add_arguments(cls, parser: ArgumentParser) -> None:
        """Adds arguments to the command's argument parser."""
        parser.add_argument(
            "rrid",
            action="store",
            type=str,
            help="RRID of the loaded template to make active",
        )

    def __call__(self) -> None:
        """Executes the `switch` command."""
        rrid: str = self.args.rrid
        try:
            self.templates.set_active(rrid)
        except KeyError:
            raise TemplateNotLoadedError(rrid) from None
        self.prompt.set_prompt()

    @staticmethod
    def complete(state, text, line, begidx, endidx) -> list[str]:
        """Provides tab completion for the command over loaded RRIDs."""
        templates = state.get("templates")
        rrids = [(rrid,) for rrid in templates.rrids()] if templates else []
        return complete_choices(rrids, line, text)
