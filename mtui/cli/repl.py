"""prompt_toolkit-backed interactive REPL for the mtui application.

Replaces the historical :mod:`cmd`-based prompt. The previous
implementation inherited from :class:`cmd.Cmd` and relied on GNU
``readline`` for line editing, history, and tab completion. This module
exposes ``CommandPrompt``, ``QuitLoopError``, and
``CommandAlreadyBoundError``; the input loop is driven by
:class:`prompt_toolkit.PromptSession` and history goes through the shared
:mod:`mtui.cli._history` backend.
"""

from __future__ import annotations

import subprocess
from collections.abc import Callable
from logging import DEBUG, getLogger
from pathlib import Path
from typing import TYPE_CHECKING, Any

from prompt_toolkit import PromptSession
from prompt_toolkit.auto_suggest import AutoSuggestFromHistory
from prompt_toolkit.styles import Style

from .. import commands
from ..commands import Command, CommandAlreadyBoundError
from ..support import messages
from ..test_reports.null_report import NullTestReport
from ..types import Workflow
from . import notification
from ._completer import MtuiCompleter
from ._history import default_history_path, get_history
from ._lexer import MtuiCommandLexer
from .argparse import ArgsParseFailureError

if TYPE_CHECKING:
    from prompt_toolkit.input import Input
    from prompt_toolkit.output import Output

    from .prompter import Prompter

logger = getLogger("mtui.prompt")


# Token classes are defined by ``MtuiCommandLexer`` (``command.known``,
# ``command.unknown``, ``flag``) and resolved to terminal colors here so
# the palette is one edit away from a theme swap. The ``bottom-toolbar``
# class uses prompt_toolkit's reverse-video default style to match what
# users expect from other prompt_toolkit applications.
_PROMPT_STYLE = Style.from_dict(
    {
        "command.known": "ansigreen",
        "command.unknown": "ansired",
        "flag": "ansicyan",
        "bottom-toolbar": "reverse",
    }
)


class QuitLoopError(RuntimeError):
    """Exception raised to exit the command loop."""


class _LoopContinueError(Exception):
    """Internal sentinel: input phase requests :meth:`cmdloop` to reprompt.

    Used by the ``KeyboardInterrupt`` handler so the loop skips dispatch
    and goes straight to the next read cycle.
    """


class CommandPrompt:
    """The main interactive REPL for the mtui application.

    Public surface mirrors the previous :class:`cmd.Cmd` subclass so the
    rest of the codebase (``main.py``, tests, command dispatch) does not
    move. The loop, history, and line editing now come from
    :mod:`prompt_toolkit` rather than :mod:`cmd` and ``readline``.
    """

    def __init__(
        self,
        config,
        log,
        sys,
        display_factory,
        prompter: Prompter | None = None,
        *,
        _input: Input | None = None,
        _output: Output | None = None,
    ) -> None:
        """Initializes the command prompt.

        Args:
            config: The application configuration.
            log: The logger instance.
            sys: The sys module.
            display_factory: A factory for creating display objects.
            prompter: Optional :class:`mtui.cli.prompter.Prompter` forwarded
                to every constructed :class:`TestReport` so that SSH
                command-timeout prompts surface to the user with
                cross-thread serialisation. ``None`` (the default)
                disables the prompt: command timeouts silently wait.
            _input: Test-only :class:`prompt_toolkit.input.Input`; when
                provided it is passed to :class:`PromptSession` so unit
                tests can drive the loop through a
                :class:`~prompt_toolkit.input.PipeInput` without touching
                the real terminal. Production callers leave this ``None``
                so prompt_toolkit auto-detects the TTY.
            _output: Test-only :class:`prompt_toolkit.output.Output`;
                paired with ``_input`` (typically
                :class:`~prompt_toolkit.output.DummyOutput`) to silence
                terminal rendering in tests.

        """
        self.sys = sys
        self.prompter = prompter

        self.interactive: bool = True
        self.display = display_factory(self.sys.stdout)
        self.metadata = NullTestReport(config, prompter=prompter)
        self.targets = self.metadata.targets
        """
        alias to ease refactoring
        """

        self.homedir = Path("~").expanduser()
        self.config = config
        self.log = log

        # Shared FileHistory: one writer per process, also used by
        # mtui.cli.term.prompt_user and mtui.commands.quit so the file
        # is not raced.
        self._history = get_history(default_history_path())

        self.commands: dict[str, type[Command]] = {}

        # Single PromptSession owns input, output, history, completion,
        # lexing, and the bottom toolbar. ``MtuiCompleter`` and
        # ``MtuiCommandLexer`` are constructed with a back-reference to
        # ``self`` and look up ``self.commands`` lazily on each
        # keystroke, so the registration loop below (which still
        # mutates ``self.commands``) is reflected immediately — no
        # post-init wiring needed.
        #
        # ``complete_while_typing=False`` keeps the legacy tab-only
        # completion behaviour. ``enable_history_search=True`` gives
        # Ctrl-R reverse history search (and Ctrl-S forward, where
        # supported). ``AutoSuggestFromHistory`` displays a greyed-out
        # suggestion based on the most recent matching history entry;
        # right-arrow accepts it. Mouse support is intentionally
        # omitted: enabling it would capture click events and break
        # terminal text selection / copy-paste.
        #
        # ``_input``/``_output`` are the test seam: tests construct a
        # PipeInput + DummyOutput; production passes None so
        # prompt_toolkit binds to the controlling TTY.
        self._session: PromptSession[str] = PromptSession(
            history=self._history,
            completer=MtuiCompleter(self),
            auto_suggest=AutoSuggestFromHistory(),
            complete_while_typing=False,
            enable_history_search=True,
            lexer=MtuiCommandLexer(self),
            style=_PROMPT_STYLE,
            bottom_toolbar=self._bottom_toolbar,
            input=_input,
            output=_output,
        )

        # register commands
        for cls in commands.registry.values():
            self._add_subcommand(cls)

        # Default prompt; load_update / set_prompt override once a test
        # report is loaded.
        self.stdout = self.sys.stdout
        self.prompt: str = "mtui-empty>"

    def _bottom_toolbar(self) -> str:
        """Build the bottom status line shown beneath the prompt.

        Rendered fields:

        * ``mode`` — the active report's :attr:`workflow`
          (:class:`~mtui.types.Workflow`): ``kernel``, ``auto``, or
          ``manual``.
        * ``session`` — resolved in precedence order: an explicit name
          set via ``set_session_name`` (:attr:`self.session`) wins; else
          the loaded test report's :attr:`id` (its RRID); else the
          literal ``"empty"`` when no test report is loaded.
          :class:`NullTestReport` returns ``""`` from ``id``, so the
          fallback to ``"empty"`` covers the pre-load state.
        * ``hosts`` — count of connected refhosts. Falls back to ``"?"``
          if ``self.targets`` does not expose ``__len__`` (defensive
          guard for the brief construction window between ``__init__``
          and the first ``load_update``).

        The toolbar is invoked by prompt_toolkit on every redraw, so
        this stays cheap — three attribute reads plus a ``len()``.
        prompt_toolkit auto-detects non-TTY stdout and suppresses the
        toolbar's ANSI output, so acceptance tests that scrape stdout
        are not affected.

        Returns:
            The single-line status string, including its surrounding
            spaces so it reads naturally inside the reverse-video bar.

        """
        mode = self.metadata.workflow.value

        sess = self.session if getattr(self, "session", None) else ""
        if not sess:
            metadata = getattr(self, "metadata", None)
            sess = getattr(metadata, "id", "") or "empty"

        try:
            n_hosts: int | str = len(self.targets)
        except TypeError:
            n_hosts = "?"

        return f" mode: {mode}  session: {sess}  hosts: {n_hosts} "

    def notify_user(self, msg: str, class_: str = "") -> None:
        """Displays a desktop notification.

        Args:
            msg: The message to display.
            class_: The notification class. ``"stock_dialog-error"`` (the
                historical error class) maps to the freedesktop ``dialog-error``
                icon; any other value uses the default icon.

        """
        icon = "dialog-error" if class_ == "stock_dialog-error" else None
        notification.display("MTUI", msg, icon)

    def println(self, msg: str = "", eol: str = "\n") -> None:
        """Prints a message to the output stream.

        Args:
            msg: The message to print.
            eol: The end-of-line character.

        """
        self.stdout.write(msg + eol)

    def _add_subcommand(self, cmd: type[Command]) -> None:
        """Adds a subcommand to the prompt.

        Binds ``do_<name>``, ``help_<name>``, and ``complete_<name>`` as
        instance attributes so that normal Python attribute lookup
        resolves them directly. ``setattr`` accepts attribute names that
        aren't valid Python identifiers, so command names containing
        ``-`` (e.g. ``dash-cmd``) work the same way as the
        underscore-only ones.

        Args:
            cmd: The command class to add.

        """
        if cmd.command in self.commands:
            raise CommandAlreadyBoundError(cmd.command)
        self.commands[cmd.command] = cmd

        name = cmd.command
        c = cmd  # bind once for each closure

        def do(arg) -> None:
            try:
                args = c.parse_args(arg, self.sys)
            except ArgsParseFailureError:
                return
            c(args, self.config, self.sys, self)()

        def help() -> None:
            c.argparser(self.sys).print_help()

        def complete(*args, **kw):
            try:
                return c.complete(
                    {
                        "hosts": self.targets.select(),
                        "metadata": self.metadata,
                        "config": self.config,
                    },
                    *args,
                    **kw,
                )
            except Exception as e:
                logger.exception(e)
                raise e

        setattr(self, f"do_{name}", do)
        setattr(self, f"help_{name}", help)
        setattr(self, f"complete_{name}", complete)

    def _dispatch(self, line: str) -> None:
        """Parse ``line`` and invoke the matching ``do_<name>`` closure.

        Splits on the first run of whitespace. Command names may contain
        ``-`` (the historical :attr:`cmd.Cmd.identchars` widening); we
        accept any non-whitespace first token. Unknown commands log a
        warning and return so the loop keeps going — matching the
        previous ``cmd.Cmd.default`` behaviour.

        ``postcmd`` is invoked here (rather than in the loop) so every
        dispatch path — interactive prompt and the
        ``EOFError → "EOF"`` shortcut — gets the same post-command hook
        treatment.
        """
        line = line.strip()
        if not line:
            return
        name, _, rest = line.partition(" ")
        rest = rest.lstrip()
        do = getattr(self, f"do_{name}", None)
        if do is None:
            logger.warning("unknown command: %s", name)
            return
        do(rest)
        self.postcmd(False, line)

    def cmdloop(self, intro: str | None = None) -> None:
        """Run the main command loop.

        Asks :class:`PromptSession` for a new line each iteration and
        dispatches it.

        Args:
            intro: Optional banner string printed before the first
                prompt. Preserved for signature parity with
                :meth:`cmd.Cmd.cmdloop`; emitted via :meth:`println`.

        """
        if intro is not None:
            self.println(str(intro))

        while True:
            try:
                line = self._read_next_line()
            except _LoopContinueError:
                continue

            try:
                self._dispatch(line)
            except QuitLoopError:
                return
            except KeyboardInterrupt:
                # Ctrl-C during a command should abort the command and
                # drop back to the prompt -- not tear down the REPL.
                # Individual commands are expected to clean up their own
                # resources in their own KeyboardInterrupt handlers; this
                # is the last-resort safety net so an uncaught one never
                # kills the session.
                self.println()
                logger.warning("command interrupted by user")
            except (messages.UserMessage, subprocess.CalledProcessError) as e:
                if logger.isEnabledFor(DEBUG):
                    logger.exception(e)
                else:
                    logger.error(e)
            except Exception as e:
                if logger.isEnabledFor(DEBUG):
                    logger.exception("Unexpected error")
                else:
                    logger.error("Unexpected error: %s", e)

    def _read_next_line(self) -> str:
        """Acquire the next line for dispatch.

        Centralises PromptSession → control-key handling so
        :meth:`cmdloop` only deals with dispatch-phase exceptions.

        Returns:
            The raw input line (still un-stripped; :meth:`_dispatch`
            normalises it).

        Raises:
            _LoopContinueError: when the loop must skip dispatch and
                reprompt (Ctrl-C / ``KeyboardInterrupt``).

        """
        try:
            return self._session.prompt(self.prompt)
        except KeyboardInterrupt:
            # Ctrl-C on a partial input line clears it and reprompts.
            self.println()
            raise _LoopContinueError from None
        except EOFError:
            # Ctrl-D on an empty buffer dispatches the registered EOF
            # command (alias of Quit). Returning the literal "EOF" lets
            # the normal dispatch try/except in :meth:`cmdloop` handle
            # any exception that ``do_EOF`` raises (notably
            # ``QuitLoopError``) uniformly with typed-command dispatch.
            return "EOF"

    def postcmd(self, stop: bool, line: str) -> bool:
        """A hook that is called after a command is executed.

        Args:
            stop: Whether to stop the command loop.
            line: The command that was executed.

        Returns:
            Whether to stop the command loop.

        """
        if isinstance(self.metadata, NullTestReport):
            return stop
        self.set_prompt(session=self.__dict__.get("session", None))
        return stop

    def get_names(self) -> list[str]:
        """Returns a list of all command names.

        Surfaces ``do_<name>`` and ``help_<name>`` for every registered
        command. Used by external introspection (e.g. help auto-listing)
        and locked in by the test suite.
        """
        names: list[str] = []
        names += [f"do_{x}" for x in self.commands]
        names += [f"help_{x}" for x in self.commands]
        return names

    def emptyline(self) -> bool:
        """Called when an empty line is entered."""
        return False

    def set_prompt(self, session: str | None = None) -> None:
        """Sets the command prompt string.

        Args:
            session: The current session name.

        """
        self.session = session
        session = ":" + str(session) if session else ""
        mode = "mtui"
        if self.metadata.workflow is not Workflow.MANUAL:
            mode += f"-{self.metadata.workflow.value}"
        self.prompt = f"{mode}{session}> "

    if TYPE_CHECKING:
        # Typing-only escape hatch. The ``do_<name>``, ``help_<name>``,
        # and ``complete_<name>`` attributes are bound at runtime by
        # ``_add_subcommand`` via ``setattr``; the type checker can't
        # see through that. This stub tells ``ty`` (and IDEs) that any
        # attribute access on a ``CommandPrompt`` is callable, mirroring
        # the previous runtime ``__getattr__`` typing contract without
        # the lazy synthesis. Runtime lookups never reach this method.
        def __getattr__(self, name: str) -> Callable[..., Any]: ...

    def load_update(self, update, autoconnect: bool) -> None:
        """Loads an update and sets the test report.

        Args:
            update: The update to load.
            autoconnect: Whether to automatically connect to hosts.

        """
        tr = update.make_testreport(
            self.config,
            autoconnect,
            self.interactive,
            prompter=self.prompter,
        )
        self.metadata = tr
        self.targets = tr.targets
        self.set_prompt(None)
