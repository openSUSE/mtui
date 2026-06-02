"""The `help` command — REPL command discovery and per-command help.

Replaces the built-in :meth:`cmd.Cmd.do_help` that the historic REPL
provided for free. With no argument, prints the list of registered
commands (split by whether the command class has a docstring). With a
command name, prints that command's ``--help`` output via its
``argparser``.
"""

from logging import getLogger

from ..cli.argparse import ArgumentParser
from ..cli.completion import complete_choices
from ..support.messages import UserError
from . import Command

logger = getLogger("mtui.commands.help")

# Layout knobs for the no-arg listing. Mirrors the column feel of the old
# ``cmd.Cmd.print_topics`` output without dragging :mod:`cmd` back in.
_COLUMN_WIDTH = 22
_COLUMNS_PER_ROW = 4


class UnknownHelpTopicError(UserError):
    """Raised when ``help <name>`` is asked for an unregistered command."""

    def __init__(self, name: str) -> None:
        self.name = name
        self.message = f"No help available: {name!r} is not a known command"


class Help(Command):
    """Prints a list of available commands, or detailed help for one command.

    With no argument, lists every command registered in the REPL,
    grouping commands whose class lacks a docstring at the bottom under
    an "Undocumented" header. With a command name, prints that command's
    ``--help`` output (same as typing ``<command> --help``).
    """

    command = "help"

    @classmethod
    def _add_arguments(cls, parser: ArgumentParser) -> None:
        """Adds arguments to the command's argument parser."""
        # No ``choices=`` — we want the live registry lookup at call
        # time so commands registered after this parser was built (none
        # today, but cheap to future-proof) still resolve.
        parser.add_argument(
            "command",
            nargs="?",
            help="MTUI command to print help for; omit to list all commands",
        )

    def __call__(self) -> None:
        """Executes the `help` command."""
        registry = self.prompt.commands

        if self.args.command is None:
            self._print_listing(registry)
            return

        target = registry.get(self.args.command)
        if target is None:
            raise UnknownHelpTopicError(self.args.command)

        target.argparser(self.sys).print_help()

    def _print_listing(self, registry: dict[str, type[Command]]) -> None:
        """Prints the no-arg listing: documented + undocumented buckets."""
        documented: list[str] = []
        undocumented: list[str] = []
        for name in sorted(registry):
            doc = registry[name].__doc__
            if doc and doc.strip():
                documented.append(name)
            else:
                undocumented.append(name)

        self.println("Documented commands (type help <topic>):")
        self.println("=" * 40)
        self._print_columns(documented)

        if undocumented:
            self.println()
            self.println("Undocumented commands:")
            self.println("=" * 40)
            self._print_columns(undocumented)

    def _print_columns(self, names: list[str]) -> None:
        """Print ``names`` in a simple fixed-width column layout."""
        if not names:
            return
        for i in range(0, len(names), _COLUMNS_PER_ROW):
            row = names[i : i + _COLUMNS_PER_ROW]
            self.println("".join(name.ljust(_COLUMN_WIDTH) for name in row).rstrip())

    @staticmethod
    def complete(state, text, line, begidx, endidx) -> list[str]:
        """Provides tab completion over registered command names."""
        # ``state["commands"]`` is not part of the legacy state dict
        # (the cmd.Cmd era never needed it), but the live registry on
        # the Command base class is the canonical source of truth and
        # is always populated by the time the REPL is taking input.
        names = tuple(Command.registry)
        return complete_choices([names], line, text)
