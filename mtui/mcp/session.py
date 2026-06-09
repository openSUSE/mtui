"""Headless session for the ``mtui-mcp`` MCP server.

:class:`McpSession` is a stripped-down replacement for
:class:`mtui.cli.repl.CommandPrompt` that exposes exactly the attribute
surface :class:`mtui.commands._command.Command` instances read from
``self.prompt``. There is no :mod:`prompt_toolkit`, no history file, no
bottom toolbar, no ``do_<name>`` setattr loop.

One :class:`McpSession` lives per ``mtui-mcp`` server process; every
:meth:`McpSession.run_command` call is serialised through a single
:class:`asyncio.Lock` so HTTP-transport concurrency cannot interleave
mutations of ``metadata`` / ``targets``. Blocking command bodies (SSH,
subprocess) run inside :func:`asyncio.to_thread` so they do not stall
the event loop.

Stdout / stderr produced by a command are captured per-call via a
fresh :class:`io.StringIO`; stdout is returned to the caller, stderr
either surfaces in :class:`McpCommandError` on failure or is logged at
WARNING on a clean return.
"""

from __future__ import annotations

import asyncio
import io
import shlex
import time
from logging import Logger, getLogger
from types import SimpleNamespace
from typing import TYPE_CHECKING, Any

from ..cli.argparse import ArgsParseFailureError
from ..cli.display import CommandPromptDisplay
from ..commands import Command
from ..test_reports.null_report import NullTestReport

if TYPE_CHECKING:
    from ..support.config import Config

logger = getLogger("mtui.mcp.session")

#: Default heartbeat interval, in seconds, between
#: ``notifications/progress`` frames emitted while a long-running tool
#: call is in flight. The MCP SDK's client-side httpx default is 30 s
#: (``mcp.shared._httpx_utils.MCP_DEFAULT_TIMEOUT``) and most LLM
#: clients (Claude Desktop, opencode, Inspector) sit in the same
#: ballpark; 10 s sits comfortably under that floor while keeping wire
#: traffic negligible for short calls (one frame every 10 s costs
#: nothing on the wire and stops well-behaved clients from timing out
#: on ``run`` / ``update`` / ``set_repo`` / ``commit`` etc.).
DEFAULT_PROGRESS_INTERVAL_SECONDS: float = 10.0


class McpCommandError(RuntimeError):
    """Raised by :meth:`McpSession.run_command` when a command fails.

    Carries the streams captured during the failed run so the MCP
    server layer can surface them to the client:

    * ``stdout`` — everything the command printed before failing.
    * ``stderr`` — argparse complaints, exception messages, etc.
    * ``exit_code`` — non-zero status from ``sys.exit`` or argparse, or
      ``1`` for an unhandled exception.

    ``__str__`` returns a single-line summary plus the captured stderr
    so the default MCP error envelope is human-readable.
    """

    def __init__(self, stdout: str, stderr: str, exit_code: int) -> None:
        """Stores the captured streams and exit code.

        Args:
            stdout: Captured stdout up to the point of failure.
            stderr: Captured stderr (argparse output, exception repr).
            exit_code: Non-zero exit code reported by the command.

        """
        self.stdout = stdout
        self.stderr = stderr
        self.exit_code = exit_code
        super().__init__(self._render())

    def _render(self) -> str:
        """Builds the one-line + stderr message exposed via ``str()``."""
        head = f"command failed (exit_code={self.exit_code})"
        tail = self.stderr.strip()
        return f"{head}: {tail}" if tail else head


class _FakeSys(SimpleNamespace):
    """Per-call stand-in for the :mod:`sys` module passed to a Command.

    Exposes ``stdout`` / ``stderr`` as fresh :class:`io.StringIO`
    buffers, ``argv`` as a defensive ``["mtui-mcp"]`` (some commands
    introspect it), and an ``exit`` callable that raises
    :class:`SystemExit` exactly like the real :func:`sys.exit`. The
    surrounding :meth:`McpSession._run_sync` catches ``SystemExit`` and
    converts non-zero codes into :class:`McpCommandError`.
    """

    def __init__(self) -> None:
        """Allocate fresh StringIO buffers and the standard surface."""
        super().__init__(
            stdout=io.StringIO(),
            stderr=io.StringIO(),
            argv=["mtui-mcp"],
            exit=self._exit,
        )

    @staticmethod
    def _exit(code: int = 0) -> None:
        """Raises :class:`SystemExit` like the real :func:`sys.exit`."""
        raise SystemExit(code)


class McpSession:
    """Headless mtui session shared by every ``mtui-mcp`` tool call.

    Holds the same mutable state as :class:`CommandPrompt` — ``config``,
    ``metadata``, ``targets``, ``session`` — so that the existing
    ``Command`` ABI works unchanged. Stateless per-call concerns
    (``display``, ``sys``) are constructed inside :meth:`run_command`
    and torn down after the call returns.
    """

    def __init__(self, config: Config, log: Logger) -> None:
        """Initialises the session with config-derived defaults.

        Args:
            config: The application configuration (already merged with
                CLI args by the caller, see ``mtui.mcp.main``).
            log: A configured logger; reused by commands that touch
                ``self.prompt.log``.

        """
        self.config = config
        self.log = log
        # MCP transports have no TTY, so every command sees the
        # non-interactive contract (``prompt_user(default, …)`` returns
        # ``default``). Documented in Documentation/mcp.rst.
        self.interactive: bool = False
        # Cross-thread SSH command-timeout prompts have nowhere to go
        # over MCP — leave the prompter unset so TestReport silently
        # waits, matching the non-interactive contract above.
        self.prompter = None

        self.metadata = NullTestReport(config, prompter=self.prompter)
        self.targets = self.metadata.targets
        # Mirror ``self.interactive`` onto the HostsGroup so long-running
        # parallel actions (run, set_repo, sftp_*) skip the TTY spinner;
        # MCP uses ``notifications/progress`` as its progress channel.
        self.targets.interactive = False
        self.session: str | None = None

        # Snapshot of the registry so commands that introspect
        # ``self.prompt.commands`` (e.g. denied ``help``) still see a
        # stable mapping if they are ever re-enabled.
        self.commands: dict[str, type[Command]] = dict(Command.registry)

        # _history is read only by the denied ``quit`` command; expose
        # it so an accidental re-enable fails loudly rather than
        # silently misbehaving.
        self._history = None

        # Single process-wide serialiser. Concurrent HTTP-transport
        # clients queue here; the lock is held across the
        # ``to_thread`` worker so re-entrant calls (a Command's body
        # touching ``self.prompt.load_update``) execute on the same
        # thread without re-acquiring.
        self._lock = asyncio.Lock()

        # Per-call stdout pointer. ``println`` falls back to ``log``
        # when no command is currently running.
        self._current_stdout: io.StringIO | None = None
        # Per-call display, swapped in by ``_run_sync`` so
        # ``Command.__init__`` picks up the right StringIO-bound
        # writer. ``None`` outside a call.
        self.display: CommandPromptDisplay | None = None

    # ------------------------------------------------------------------
    # CommandPrompt-compatible surface
    # ------------------------------------------------------------------

    def set_prompt(self, session: str | None = None) -> None:
        """Records the session label (RRID or ``None``).

        :class:`CommandPrompt.set_prompt` also rewrites the REPL prompt
        string; there is no prompt under MCP, so this is a plain
        attribute assignment.

        Args:
            session: The session label to record, or ``None`` to clear.

        """
        self.session = session

    def notify_user(self, msg: str, class_: str = "") -> None:
        """Logs a notification at INFO.

        The REPL pops a desktop notification; over MCP the analogous
        signal is a log line. ``class_`` is recorded for parity with
        the REPL's signature.

        Args:
            msg: The notification text.
            class_: The notification class (unused; logged for parity).

        """
        if class_:
            self.log.info("notify[%s]: %s", class_, msg)
        else:
            self.log.info("notify: %s", msg)

    def println(self, msg: str = "", eol: str = "\n") -> None:
        """Writes to the per-call stdout, or falls back to the log.

        Some commands (``addhost``, the autoconnect path inside
        ``load_update``) call ``self.prompt.println`` directly. While a
        command is running the per-call StringIO is active and the
        write lands in the captured output; outside a call we have
        nowhere to put the text, so it goes to the log at INFO.

        Args:
            msg: The string to print.
            eol: The end-of-line character.

        """
        if self._current_stdout is not None:
            self._current_stdout.write(msg + eol)
        else:
            self.log.info(msg)

    def load_update(self, update, autoconnect: bool) -> None:
        """Loads an update and swaps in the resulting TestReport.

        Verbatim translation of :meth:`CommandPrompt.load_update` minus
        the prompt-string rewrite (handled by :meth:`set_prompt`).

        Args:
            update: An OBS update id object exposing ``make_testreport``.
            autoconnect: Forwarded to ``make_testreport``.

        """
        tr = update.make_testreport(
            self.config,
            autoconnect,
            self.interactive,
            prompter=self.prompter,
        )
        self.metadata = tr
        self.targets = tr.targets
        # Re-apply the non-interactive flag after the testreport swap so
        # the fresh HostsGroup inherits the session's headless mode.
        self.targets.interactive = False
        self.set_prompt(None)

    # ------------------------------------------------------------------
    # Command dispatch
    # ------------------------------------------------------------------

    async def run_command(
        self,
        cmd_cls: type[Command],
        argv: list[str],
        ctx: Any | None = None,
        *,
        progress_interval: float = DEFAULT_PROGRESS_INTERVAL_SECONDS,
    ) -> str:
        """Runs a registered command and returns its captured stdout.

        The lock makes the call sequentially consistent with every
        other ``run_command`` invocation in the process; the
        :func:`asyncio.to_thread` hop keeps blocking command bodies
        off the event loop.

        When ``ctx`` is supplied (the synthesised tool wrappers in
        :mod:`mtui.mcp.tools` and the testreport tools pass it through
        from FastMCP), a heartbeat coroutine emits
        ``notifications/progress`` every ``progress_interval`` seconds
        while the worker thread runs, so MCP clients that honour the
        protocol's progress contract do not time out on long-running
        commands (``run``, ``update``, ``set_repo``, ``commit``, slow
        ``add_host``, ``load_template``, ...). The notification is
        addressed at the request's ``progressToken``; if the client
        did not supply one the SDK's :meth:`Context.report_progress`
        is a no-op, so the heartbeat costs nothing in that case.

        Args:
            cmd_cls: The :class:`Command` subclass to invoke.
            argv: The command-line tokens (already split, no shell
                quoting required from the caller).
            ctx: Optional FastMCP :class:`Context` for the in-flight
                tool call. ``None`` skips the heartbeat (used by the
                autoconnect path in :mod:`mtui.mcp.main` and by tests).
            progress_interval: Seconds between heartbeat frames.
                Defaults to :data:`DEFAULT_PROGRESS_INTERVAL_SECONDS`.

        Returns:
            The text the command wrote to stdout during the call.

        Raises:
            McpCommandError: If argparse rejects ``argv``, the command
                calls ``sys.exit`` with a non-zero code, or its body
                raises an unhandled exception.

        """
        async with self._lock:
            if ctx is None:
                return await asyncio.to_thread(self._run_sync, cmd_cls, argv)
            return await self._run_with_heartbeat(
                ctx, cmd_cls, argv, interval=progress_interval
            )

    async def _run_with_heartbeat(
        self,
        ctx: Any,
        cmd_cls: type[Command],
        argv: list[str],
        *,
        interval: float,
    ) -> str:
        """Drive ``_run_sync`` in a worker thread while emitting heartbeats.

        The worker task is created with :func:`asyncio.create_task` and
        we wait on it with :func:`asyncio.wait` so the heartbeat loop
        wakes every ``interval`` seconds regardless of how long the
        underlying blocking body takes. ``ctx.report_progress`` is
        ``await``-ed inside the loop; a notification-send failure is
        logged at ``DEBUG`` and swallowed so a flaky transport never
        masks the actual command outcome.
        """
        worker = asyncio.create_task(asyncio.to_thread(self._run_sync, cmd_cls, argv))
        started = time.monotonic()
        try:
            while True:
                done, _pending = await asyncio.wait({worker}, timeout=interval)
                if worker in done:
                    break
                elapsed = time.monotonic() - started
                try:
                    await ctx.report_progress(
                        progress=elapsed,
                        total=None,
                        message=f"{cmd_cls.command} running ({elapsed:.0f}s)…",
                    )
                except Exception as exc:  # noqa: BLE001 - never mask the command result
                    logger.debug(
                        "progress notification failed for %s: %s",
                        cmd_cls.command,
                        exc,
                    )
        except BaseException:
            # The caller (or the surrounding task group) cancelled us.
            # Cancel the worker too so we do not leak a background
            # thread future; then re-raise.
            worker.cancel()
            raise
        return worker.result()

    def _run_sync(self, cmd_cls: type[Command], argv: list[str]) -> str:
        """Synchronous core of :meth:`run_command`; runs in a worker thread."""
        fake_sys = _FakeSys()
        display = CommandPromptDisplay(fake_sys.stdout)

        prev_display = self.display
        prev_stdout = self._current_stdout
        self.display = display
        self._current_stdout = fake_sys.stdout
        try:
            try:
                args_ns = cmd_cls.parse_args(shlex.join(argv), fake_sys)
            except ArgsParseFailureError as e:
                raise McpCommandError(
                    fake_sys.stdout.getvalue(),
                    fake_sys.stderr.getvalue(),
                    e.status or 2,
                ) from e

            try:
                cmd_cls(args_ns, self.config, fake_sys, self)()
            except SystemExit as e:
                code = (
                    e.code if isinstance(e.code, int) else (0 if e.code is None else 1)
                )
                if code != 0:
                    raise McpCommandError(
                        fake_sys.stdout.getvalue(),
                        fake_sys.stderr.getvalue(),
                        code,
                    ) from e
            except McpCommandError:
                raise
            except Exception as exc:
                stderr = fake_sys.stderr.getvalue() or repr(exc)
                raise McpCommandError(
                    fake_sys.stdout.getvalue(),
                    stderr,
                    1,
                ) from exc

            stderr = fake_sys.stderr.getvalue()
            if stderr:
                # Clean return but the command wrote to stderr; surface
                # via the server log so operators can see it, but still
                # hand stdout back to the client.
                self.log.warning(
                    "command %s wrote to stderr: %s",
                    cmd_cls.command,
                    stderr.rstrip(),
                )

            return fake_sys.stdout.getvalue()
        finally:
            self.display = prev_display
            self._current_stdout = prev_stdout
