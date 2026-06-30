"""The base class for all commands in mtui."""

import shlex
from abc import ABC, abstractmethod
from argparse import Namespace, RawDescriptionHelpFormatter
from logging import getLogger
from typing import ClassVar

from ..cli.argparse import ArgumentParser
from ..hosts.target.hostgroup import HostsGroup
from ..support.messages import (
    FanOutError,
    HostIsNotConnectedError,
    TemplateNotLoadedError,
)

logger = getLogger("mtui.commands.command")


class CommandAlreadyBoundError(RuntimeError):
    """Raised when two ``Command`` subclasses claim the same ``command`` string.

    Lives in ``mtui.commands._command`` so the ``Command.__init_subclass__``
    hook can raise it at class-creation time. ``mtui.prompt`` re-exports the
    name for backwards compatibility with code that imported it from there.
    """


class Command(ABC):
    """An abstract base class for all commands in mtui.

    Concrete subclasses are auto-registered into ``Command.registry`` keyed by
    their ``command`` attribute. Abstract intermediate classes (those that do
    not assign ``command`` in their own body) are skipped.
    """

    command: str

    #: Fan-out scope policy. ``"active"`` (the safe default) runs the command
    #: once against the active template; ``"fanout"`` runs it once per loaded
    #: template. A per-invocation ``-T/--template`` always wins over this, and
    #: ``--all-templates`` forces fan-out regardless of the class default. Only
    #: action commands that are safe to repeat per template opt into
    #: ``"fanout"``; inherently single-target commands (``load_template``,
    #: ``edit``, ``switch``, ``quit``, …) keep ``"active"``.
    #:
    #: ``"single"`` is a stricter variant of ``"active"`` for commands that
    #: name their own target template and must run **exactly once** regardless
    #: of how many templates are loaded — e.g. ``unload <rrid>``, which would
    #: otherwise fan out under MCP (where ``"active"`` defaults to fan-out with
    #: several loaded) and try to remove the same RRID once per template.
    scope: ClassVar[str] = "active"

    #: Auto-populated registry of every concrete ``Command`` subclass that
    #: assigns ``command`` in its own class body. Mutated by
    #: ``__init_subclass__`` at class-creation time; consumers (notably
    #: ``mtui.cli.repl.CommandPrompt``) iterate this dict to discover commands.
    registry: ClassVar[dict[str, type["Command"]]] = {}

    __slots__ = [
        "args",
        "config",
        "display",
        "metadata",
        "prompt",
        "sys",
        "targets",
        "templates",
    ]
    _check_subparser: str = ""
    """
    :type _check_subparser: str
    :param _check_subparser: Name of the subparser attribute if the
        derived class uses subparsers.

        On python 3 L{Command.parse_args} then checks if the attribute
        is set in parsed L{argparse.Namespace} and if not, prints an
        error message.
    """

    def __init_subclass__(cls, **kwargs: object) -> None:
        """Auto-register concrete subclasses into ``Command.registry``.

        Subclasses that do not assign ``command`` in their own body (i.e.
        abstract intermediates such as ``BaseApiCall``) are skipped; only
        explicit ``command = "..."`` declarations register. Duplicate
        declarations of the same ``command`` string raise
        ``CommandAlreadyBoundError`` at class-creation time.
        """
        super().__init_subclass__(**kwargs)
        if "command" not in cls.__dict__:
            return
        name = cls.command
        existing = Command.registry.get(name)
        if existing is not None and existing is not cls:
            raise CommandAlreadyBoundError(name)
        Command.registry[name] = cls

    def __init__(self, args, config, sys, prompt) -> None:
        """Initializes the command object.

        Args:
            args: The command-line arguments for the command.
            config: The application configuration.
            sys: The sys module.
            prompt: The command prompt object.

        """
        self.args = args
        self.sys = sys
        self.config = config
        self.prompt = prompt
        self.templates = prompt.templates
        self.metadata = prompt.metadata
        self.display = prompt.display
        self.targets: HostsGroup = prompt.targets

    @classmethod
    def parse_args(cls, args: str, sys) -> Namespace:
        """Parses the command-line arguments for the command.

        Args:
            args: The command-line arguments to parse.
            sys: The sys module.

        Returns:
            A namespace containing the parsed arguments.

        """
        p = cls.argparser(sys)
        try:
            arg = shlex.split(args)
        except ValueError as e:
            p.error(f"invalid syntax: {e}")
        pa = p.parse_args(arg)

        if cls._check_subparser and not hasattr(pa, cls._check_subparser):
            p.error("too few arguments")

        return pa

    @classmethod  # noqa: B027 - intentional optional hook
    def _add_arguments(cls, parser: ArgumentParser) -> None:
        """A hook for adding arguments to the command's argument parser.

        Args:
            parser: The argument parser.

        """

    @classmethod
    def argparser(cls, sys) -> ArgumentParser:
        """Returns the argument parser for the command.

        Args:
            sys: The sys module.

        Returns:
            The argument parser for the command.

        """
        p = ArgumentParser(
            sys_=sys,
            prog=cls.command,
            description=cls.__doc__,
            formatter_class=RawDescriptionHelpFormatter,
        )
        cls._add_arguments(p)

        return p

    @staticmethod
    def complete(state, text, line, begidx, endidx) -> list[str]:
        """Provides tab completion for the command.

        Args:
            state: A dictionary of prompt instance states.
            text: The text to complete.
            line: The current input line.
            begidx: The beginning index of the text to complete.
            endidx: The ending index of the text to complete.

        Returns:
            A list of possible completions.

        """
        return []

    @abstractmethod
    def __call__(self) -> None:
        """An abstract method that is called when the command is executed."""
        ...

    def println(self, xs: str = "") -> None:
        """A replacement for the `print` function that can be easily tested.

        Args:
            xs: The string to print.

        """
        self.sys.stdout.write(xs + "\n")
        self.sys.stdout.flush()

    @classmethod
    def _add_hosts_arg(cls, parser: ArgumentParser) -> None:
        """Adds the `-t`/`--target` argument to the argument parser.

        Args:
            parser: The argument parser.

        """
        parser.add_argument(
            "-t",
            "--target",
            dest="hosts",
            action="append",
            type=str,
            help="Host to act on. Can be used multiple times. "
            + "If is ommited all hosts are used",
        )

    @classmethod
    def _add_template_arg(cls, parser: ArgumentParser) -> None:
        """Adds the per-command template selection flags.

        ``-T/--template RRID`` scopes the command to a single loaded template;
        ``--all-templates`` forces fan-out across every loaded template. The two
        are mutually exclusive. Action commands that set ``scope = "fanout"``
        call this so a user can still target one template explicitly.

        Args:
            parser: The argument parser.

        """
        group = parser.add_mutually_exclusive_group()
        group.add_argument(
            "-T",
            "--template",
            dest="template",
            action="store",
            type=str,
            default=None,
            help="RRID of a single loaded template to act on "
            "(default: all loaded templates)",
        )
        group.add_argument(
            "--all-templates",
            dest="all_templates",
            action="store_true",
            help="Act on every loaded template (the default for this command)",
        )

    def _resolve_templates(self):
        """Return the ordered list of reports this invocation should act on.

        Resolution order:

        1. ``-T/--template RRID`` → exactly that template (raises
           :class:`TemplateNotLoadedError` if it is not loaded).
        2. ``--all-templates`` or ``scope == "fanout"`` → every loaded template.
        3. otherwise → the active template only, **except** under MCP (a
           non-interactive prompt) with more than one template loaded, where
           there is no client-addressable "active" pointer (``switch`` is a
           REPL-only command), so the call fans out across every loaded
           template instead.

        The fan-out branches fall back to the active template when the registry
        is empty so an unloaded session behaves exactly like the historical
        single-call dispatch.
        """
        rrid = getattr(self.args, "template", None)
        if rrid:
            try:
                return [self.templates.get(rrid)]
            except KeyError:
                raise TemplateNotLoadedError(rrid) from None

        # Inherently single-target commands (``unload <rrid>``) name their own
        # template and must run exactly once; never auto-fan-out them, not even
        # under MCP with several loaded.
        if self.scope == "single":
            return [self.templates.active]

        if getattr(self.args, "all_templates", False) or self.scope == "fanout":
            return self.templates.all() or [self.templates.active]

        # Under MCP there is no interactive ``switch``, so the active pointer is
        # hidden, unaddressable state. With several templates loaded, default an
        # otherwise-unscoped call to fanout instead of silently picking the
        # last-loaded one. The REPL keeps its active-template behaviour.
        if not getattr(self.prompt, "interactive", True) and len(self.templates) > 1:
            return self.templates.all()

        return [self.templates.active]

    def run(self) -> None:
        """Drive the command across the resolved templates.

        Single-template resolution calls :meth:`__call__` directly so the error
        contract is byte-for-byte unchanged (errors propagate as today). When
        more than one template is resolved, each gets an RRID banner and its own
        ``try/except``: a per-template failure is logged and collected, the loop
        continues, and a :class:`FanOutError` aggregate is raised afterwards if
        any template failed.
        """
        resolved = self._resolve_templates()

        if len(resolved) <= 1:
            report = resolved[0]
            self.metadata = report
            self.targets = report.targets
            self.__call__()
            return

        failures: list[tuple[str, BaseException]] = []
        for report in resolved:
            rrid = str(report.id)
            self.metadata = report
            self.targets = report.targets
            self.display.template_banner(rrid)
            try:
                self.__call__()
            except BaseException as exc:  # noqa: BLE001 - collect & continue
                logger.error("%s failed on %s: %s", self.command, rrid, exc)
                failures.append((rrid, exc))

        ok = [str(r.id) for r in resolved if str(r.id) not in {f[0] for f in failures}]
        if ok:
            logger.info("%s succeeded on: %s", self.command, ", ".join(ok))
        if failures:
            raise FanOutError(failures)

    def parse_hosts(self, enabled: bool = True) -> HostsGroup:
        """Parses the `hosts` argument and returns a `HostsGroup` object.

        By default, this method selects only enabled hosts.

        Args:
            enabled: Whether to select only enabled hosts.

        Returns:
            A `HostsGroup` object containing the selected hosts.

        """
        try:
            if self.args.hosts:
                targets = self.targets.select(self.args.hosts)
            else:
                targets = self.targets.select(enabled=enabled)
        except HostIsNotConnectedError as e:
            if e.host == "all":
                logger.error(e)
                logger.info("Using all hosts. Warning: option 'all' is deprecated")

                targets = self.targets.select(enabled=enabled)

            else:
                raise

        return targets
