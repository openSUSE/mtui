"""Headless session for the ``mtui-mcp`` MCP server.

:class:`McpSession` is a stripped-down replacement for
:class:`mtui.cli.repl.CommandPrompt` that exposes exactly the attribute
surface :class:`mtui.commands._command.Command` instances read from
``self.prompt``. There is no :mod:`prompt_toolkit`, no history file, no
bottom toolbar, no ``do_<name>`` setattr loop.

Session lifetime depends on transport. Under **stdio** one process
serves one client, so a single :class:`McpSession` lives per process.
Under **http** :class:`mtui.mcp.registry.SessionRegistry` mints one
isolated :class:`McpSession` **per client** — keyed on
``id(ctx.session)`` (the request's ``ServerSession``, 1:1 with the MCP
session) — so concurrent clients never share ``metadata`` / ``targets``
and each has its own lock. Idle http sessions are swept after a
configurable TTL (``[mcp] session_idle_timeout``) and their hosts
disconnected via :meth:`McpSession.close`; the number of concurrent
sessions is bounded by ``[mcp] session_cap``.

Within a single session, locking is **per template**: a
:meth:`McpSession.run_command` call scoped to one loaded template holds
only that template's lock, so two tool calls on *different* templates run
concurrently while two on the *same* template serialise (they cannot
interleave that template's host phase). Registry-mutating commands
(``load_template`` / ``unload`` / ``close``) and unscoped fan-out take a
registry-wide *exclusive* gate that drains in-flight per-template work, so
the loaded set is never observed mid-mutation. Calls from *different*
clients run concurrently against their own sessions. Blocking command
bodies (SSH, subprocess) run inside :func:`asyncio.to_thread` so they
do not stall the event loop.

Stdout / stderr produced by a command are captured per-call via a fresh
:class:`io.StringIO` bound to per-call :mod:`contextvars` (so concurrent
commands never clobber each other's sinks); stdout is returned to the
caller, stderr either surfaces in :class:`McpCommandError` on failure or
is logged at WARNING on a clean return.
"""

from __future__ import annotations

import asyncio
import concurrent.futures
import contextlib
import contextvars
import io
import logging
import shlex
import time
from logging import Handler, Logger, LogRecord, getLogger
from types import SimpleNamespace
from typing import TYPE_CHECKING, Any

from ..cli.argparse import ArgsParseFailureError
from ..cli.display import CommandPromptDisplay
from ..commands import Command
from ..support.concurrency import ContextExecutor
from ..template_registry import TemplateRegistry
from ..test_reports.null_report import NullTestReport
from ._slim import cap_output

if TYPE_CHECKING:
    from ..support.config import Config

logger = getLogger("mtui.mcp.session")

#: Per-call capture token. ``_run_sync`` sets this to a unique object for
#: the duration of one command (and, via
#: :class:`mtui.support.concurrency.ContextExecutor`, the value propagates
#: into any worker threads the command fans out to). Each
#: :class:`_LogCaptureHandler` admits only records whose ambient token
#: matches its own, so concurrent sessions never capture each other's log
#: lines. Default ``None`` means "no command in flight" -> nothing
#: captured.
_capture_token: contextvars.ContextVar[object | None] = contextvars.ContextVar(
    "mtui_mcp_capture_token", default=None
)

#: Per-call output sinks. ``_run_sync`` sets these to the running command's
#: :class:`CommandPromptDisplay` and stdout :class:`io.StringIO` for the
#: duration of one command, and (via
#: :class:`mtui.support.concurrency.ContextExecutor`) the values propagate
#: into any worker threads the command fans out to. Holding them in
#: :mod:`contextvars` rather than on the session instance is what lets two
#: commands run concurrently (per-RRID locking) without clobbering each
#: other's captured output. ``None`` outside a call -> ``println`` falls
#: back to the log.
_current_display: contextvars.ContextVar[CommandPromptDisplay | None] = (
    contextvars.ContextVar("mtui_mcp_current_display", default=None)
)
_current_stdout: contextvars.ContextVar[io.StringIO | None] = contextvars.ContextVar(
    "mtui_mcp_current_stdout", default=None
)


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

#: Upper bound (seconds) that :meth:`McpSession._disconnect_targets` waits
#: for the parallel per-host ``Target.close()`` calls to finish before it
#: gives up and returns. A wedged paramiko close (dead peer, no RST) can
#: block its worker thread forever; the bound guarantees session teardown
#: still completes, abandoning the stuck close rather than blocking the
#: whole disconnect (and, under http, the registry idle-sweep behind it).
DISCONNECT_TIMEOUT_SECONDS: float = 45.0


class _LogCaptureHandler(Handler):
    """Tee ``mtui`` log records emitted *during a command* into its stdout.

    The MCP layer hands the client only what a command wrote to its
    per-call :class:`io.StringIO` (see :meth:`McpSession._run_sync`).
    Records logged through the standard library — e.g. the product-drift
    ``logger.warning`` lines from
    :meth:`mtui.test_reports.testreport.TestReport._verify_target_products`
    — never touch that buffer, so without this handler MCP clients would
    miss them.

    Two scoping rules keep the capture tight:

    * **Level.** Only ``INFO`` and above is teed in; ``DEBUG`` stays out.
    * **Capture token.** :meth:`filter` admits only records whose ambient
      :data:`_capture_token` matches this handler's token. The token is
      set per call by :meth:`McpSession._run_sync` and propagates into
      worker threads via :class:`mtui.support.concurrency.ContextExecutor`,
      so a command's own fan-out (e.g. the connect thread pool that emits
      product-drift warnings) is captured while concurrent sessions —
      each with a different token — never bleed into one another's reply.

    Installed on the ``mtui`` logger (not ``mtui-mcp``) for the duration
    of a single ``_run_sync`` call and removed in its ``finally`` — so a
    session's own bookkeeping (``mtui-mcp``: the per-call "wrote to
    stderr" notice, ``notify:`` lines) is *not* captured.
    """

    def __init__(self, stream: io.StringIO, token: object) -> None:
        """Bind the handler to one call's stdout buffer and capture token.

        Args:
            stream: The per-call :class:`io.StringIO` to tee records into.
            token: The unique per-call object stored in
                :data:`_capture_token`; only records emitted while that
                same token is the ambient value are captured.

        """
        super().__init__(level=logging.INFO)
        self._stream = stream
        self._token = token

    def filter(self, record: LogRecord) -> bool:
        """Admit only records whose ambient capture token is this call's."""
        return _capture_token.get() is self._token

    def emit(self, record: LogRecord) -> None:
        """Write the record as ``LEVEL: message`` into the bound stream.

        The level label is derived from ``record.levelno`` rather than
        ``record.levelname`` because mtui's :class:`ColorFormatter`
        mutates ``levelname`` in place (lowercasing/colourising it) when
        another handler on the same logger formats the shared record
        first; reading ``levelno`` keeps this output stable and
        uncoloured regardless of handler ordering.
        """
        try:
            level = logging.getLevelName(record.levelno)
            self._stream.write(f"{level}: {record.getMessage()}\n")
        except Exception:  # noqa: BLE001 - logging must never raise into callers
            self.handleError(record)


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


class _RWLock:
    """A minimal async readers-writer lock used as the registry gate.

    Many *shared* holders (per-RRID commands, which mutate only their own
    template) may run at once, but a *shared* holder excludes every
    *exclusive* holder and vice-versa. Exclusive holders (registry mutators:
    ``load_template`` / ``unload`` / ``close`` / unscoped fan-out) run one at a
    time with no shared holder present.

    Writer-preference is intentional: while an exclusive waiter is pending, new
    shared acquisitions block, so a steady stream of per-RRID commands cannot
    starve a ``load_template``. There is no fairness queue beyond that; the
    single-session workload (a handful of concurrent subagents) does not need
    one.
    """

    def __init__(self) -> None:
        self._readers = 0
        self._writer_waiting = 0
        # ``_cond`` guards ``_readers`` / ``_writer_waiting`` and is the
        # rendezvous both modes wait on.
        self._cond = asyncio.Condition()

    @contextlib.asynccontextmanager
    async def shared(self):
        """Acquire in shared (reader) mode for the duration of the body."""
        async with self._cond:
            # Wait out any active or pending exclusive holder (writer pref).
            await self._cond.wait_for(lambda: self._writer_waiting == 0)
            self._readers += 1
        try:
            yield
        finally:
            async with self._cond:
                self._readers -= 1
                if self._readers == 0:
                    self._cond.notify_all()

    @contextlib.asynccontextmanager
    async def exclusive(self):
        """Acquire in exclusive (writer) mode for the duration of the body."""
        async with self._cond:
            self._writer_waiting += 1
            try:
                await self._cond.wait_for(lambda: self._readers == 0)
            finally:
                self._writer_waiting -= 1
            # Hold ``_cond`` across the body so no reader/writer can enter:
            # both modes must take ``_cond`` to mutate their counters, and a
            # would-be reader also blocks on ``_writer_waiting``/the readers
            # check. Keeping the condition's underlying lock held is the
            # exclusion.
            try:
                yield
            finally:
                self._cond.notify_all()


class McpSession:
    """Headless mtui session backing one ``mtui-mcp`` client.

    Holds the same mutable state as :class:`CommandPrompt` — ``config``,
    ``metadata``, ``targets`` — so that the existing
    ``Command`` ABI works unchanged. Stateless per-call concerns
    (``display``, ``sys``) are constructed inside :meth:`run_command`
    and torn down after the call returns.

    Under stdio one instance serves the single client; under http
    :class:`mtui.mcp.registry.SessionRegistry` owns one instance per
    client and reaps it (via :meth:`close`) on idle-TTL or eviction.
    It also doubles as the degenerate single-entry session provider —
    see :meth:`get_or_create`.
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

        self.templates = TemplateRegistry(
            config,
            null_factory=lambda: NullTestReport(config, prompter=self.prompter),
        )
        # Mirror ``self.interactive`` onto the HostsGroup so long-running
        # parallel actions (run, set_repo, sftp_*) skip the TTY spinner;
        # MCP uses ``notifications/progress`` as its progress channel.
        self.targets.interactive = False

        # Snapshot of the registry so commands that introspect
        # ``self.prompt.commands`` (e.g. denied ``help``) still see a
        # stable mapping if they are ever re-enabled.
        self.commands: dict[str, type[Command]] = dict(Command.registry)

        # _history is read only by the denied ``quit`` command; expose
        # it so an accidental re-enable fails loudly rather than
        # silently misbehaving.
        self._history = None

        # Per-RRID serialiser layered over a registry shared/exclusive gate.
        #
        # A command scoped to one template takes that template's per-RRID lock
        # (``_rrid_locks``) so same-RRID calls serialise while different-RRID
        # calls run concurrently — but it *also* enters the registry gate in
        # *shared* mode (``_registry`` as a readers-writer lock) so it cannot
        # overlap a registry mutation. Registry mutators (``load_template`` /
        # ``unload`` / ``close``) and unscoped fan-out take the gate in
        # *exclusive* mode, which waits for every in-flight per-RRID command to
        # drain and blocks new ones, giving them a consistent view of the loaded
        # set. ``_locks_guard`` protects lazy creation of the per-RRID lock map.
        # Locks are held across the ``to_thread`` worker so a command's
        # re-entrant body (touching ``self.prompt.load_update``) executes on the
        # same thread without re-acquiring.
        self._registry = _RWLock()
        self._rrid_locks: dict[str, asyncio.Lock] = {}
        self._locks_guard = asyncio.Lock()

        # Background-job table for the async ("don't block on a slow host
        # op") path. A backgrounded command still acquires this session's
        # ``_lock`` for its whole duration — so it serialises against the
        # session's other mutating calls exactly like a foreground call —
        # but ``start_job`` returns a handle immediately instead of
        # holding the request open. Polled via ``job_status`` /
        # ``job_result`` (which read this table and need no lock), so the
        # client can meanwhile issue other (read-only) calls. Keyed by job
        # id. Records persist for the session's lifetime (finished jobs are
        # never evicted); under http the registry's idle sweep drops the
        # whole session and its table with it.
        self._jobs: dict[str, dict[str, Any]] = {}
        self._job_counter = 0

    @property
    def metadata(self):
        """The active template's :class:`TestReport` (or NullTestReport)."""
        return self.templates.active

    @property
    def targets(self):
        """The active template's :class:`HostsGroup`."""
        return self.templates.active.targets

    @property
    def display(self) -> CommandPromptDisplay | None:
        """The running command's per-call display, or ``None`` outside a call.

        Backed by the :data:`_current_display` :class:`~contextvars.ContextVar`
        (set by :meth:`_run_sync`) so concurrent commands each see their own
        :class:`CommandPromptDisplay` rather than a shared session attribute.
        """
        return _current_display.get()

    @property
    def _current_stdout(self) -> io.StringIO | None:
        """The running command's per-call stdout buffer, or ``None``.

        Backed by the :data:`_current_stdout` :class:`~contextvars.ContextVar`
        so :meth:`println` lands in the originating call's buffer even when
        several commands run concurrently.
        """
        return _current_stdout.get()

    # ------------------------------------------------------------------
    # CommandPrompt-compatible surface
    # ------------------------------------------------------------------

    def set_prompt(self) -> None:
        """No-op stub for :class:`CommandPrompt.set_prompt` parity.

        :class:`CommandPrompt.set_prompt` rewrites the REPL prompt
        string; there is no prompt under MCP, so commands can call this
        without effect.
        """

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
        nowhere to put the text, so it goes to the log at WARNING (it is
        addressed at a human, not routine status).

        Args:
            msg: The string to print.
            eol: The end-of-line character.

        """
        if self._current_stdout is not None:
            self._current_stdout.write(msg + eol)
        else:
            self.log.warning(msg)

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
        # A failed load returns a NullTestReport sentinel (empty RRID).
        # ``add`` ignores it; nothing else to do for a failed load.
        self.templates.add(tr)
        # Deliberately do NOT move the active pointer here. The active template
        # is REPL-only navigation state (moved by the ``switch`` command, which
        # is denied over MCP); setting it as a side effect of every load made it
        # hidden, unaddressable state and let unscoped tools silently act on the
        # last-loaded template. MCP clients address a template explicitly per
        # call (``template=``/``-T``); an unscoped action fans out across all
        # loaded templates (see ``Command._resolve_templates``). The registry's
        # ``active`` property still falls back to the first-loaded report so a
        # single-template session behaves exactly as before.
        #
        # Re-apply the non-interactive flag on the *freshly loaded* report's
        # host group (not ``self.targets``, which tracks the active pointer we
        # no longer move) so it inherits the session's headless mode.
        tr.targets.interactive = False
        # Run the deferred autoconnect now that ``add`` has wired the host
        # arbiter, so refhosts are drawn one-per-slot (with backup) instead of
        # connecting every candidate. No-op unless autoconnect was requested.
        tr.autoconnect()
        self.set_prompt()

    # ------------------------------------------------------------------
    # Command dispatch
    # ------------------------------------------------------------------

    async def get_or_create(self, key: str) -> McpSession:
        """Return ``self`` regardless of ``key`` (single-entry provider).

        :class:`McpSession` doubles as the degenerate one-session
        "provider" used by the stdio transport (one process == one
        session) and by direct-call tests. It exposes the same
        ``async get_or_create(key) -> McpSession`` shape as
        :class:`mtui.mcp.registry.SessionRegistry` so
        :mod:`mtui.mcp.tools` and :mod:`mtui.mcp.testreport_tools` can
        resolve a session per call without caring which transport they
        run under. The ``key`` is accepted and ignored.

        Args:
            key: The per-client session key (ignored here).

        Returns:
            This session instance.

        """
        return self

    async def close(self) -> None:
        """Disconnect every connected host; safe to call more than once.

        Owned by the http :class:`mtui.mcp.registry.SessionRegistry`,
        which calls this when it evicts a session (idle-TTL sweep or
        explicit eviction). Mirrors the REPL ``quit`` disconnect path —
        ``Target.close()`` per host, in parallel — but **without** the
        ``sys.exit`` / history-flush tail, since the process keeps
        serving other clients.

        Every loaded template's hosts are disconnected, not just the
        active one's: a session may hold several templates at once (each
        owning its own host group), and evicting the session must reap
        all of them — matching the REPL ``quit`` command, which closes
        every template's host group rather than only ``self.targets``.

        The blocking paramiko closes run in a worker thread (via
        :func:`asyncio.to_thread`) so the event loop is never stalled,
        matching :meth:`run_command`'s threading discipline. The whole
        teardown is best-effort: a per-host close failure is logged and
        swallowed so one wedged connection cannot block reaping the
        rest, and each template's ``targets`` is emptied either way so a
        second call is a cheap no-op.
        """
        # Release any host-arbitration pool claims (in-process ownership +
        # remote pool locks) for every loaded template before disconnect.
        # No-op when pool selection was never used.
        for report in self.templates.all():
            with contextlib.suppress(Exception):
                report.release_pool_claims()
        if not any(report.targets for report in self.templates.all()):
            return
        await asyncio.to_thread(self._disconnect_targets)

    def _disconnect_targets(self, timeout: float = DISCONNECT_TIMEOUT_SECONDS) -> None:
        """Synchronous parallel host-disconnect core for :meth:`close`.

        Runs in a worker thread. Closes every loaded template's
        :class:`Target` on its own pool thread (paramiko teardown is
        blocking) with a genuinely bounded wait, then clears each
        template's host group regardless of individual outcomes.
        Per-host errors are logged at WARNING, never raised.

        The bound is enforced by shutting the pool down with
        ``wait=False`` after the timed :func:`concurrent.futures.wait`,
        *not* by the ``with`` block's exit — ``Executor.__exit__`` calls
        ``shutdown(wait=True)``, which would re-block on a wedged close
        (a dead peer with no RST keeps its worker thread alive forever)
        and defeat the whole point of the timeout. A close that overruns
        ``timeout`` is logged and abandoned: its worker thread leaks, but
        ``close()`` — and, under http, the registry idle-sweep awaiting
        it — always returns. (That worker is a non-daemon pool thread, so
        a close that stays wedged forever can still delay a *clean*
        interpreter exit via ``concurrent.futures``' atexit join; that is
        strictly better than the old behaviour, which blocked ``close()``
        itself during steady-state operation, and is bounded in practice
        by the OS TCP timeout.)
        """
        # (HostsGroup, hostname) for every host across every loaded template.
        work = [
            (report.targets, name)
            for report in self.templates.all()
            for name in list(report.targets)
        ]

        def _close_one(targets, name: str) -> None:
            try:
                targets[name].close()
            except Exception as exc:  # noqa: BLE001 - best-effort teardown
                self.log.warning("error disconnecting host %s: %s", name, exc)

        executor = ContextExecutor()
        try:
            futures = {executor.submit(_close_one, t, name): name for t, name in work}
            _done, not_done = concurrent.futures.wait(futures, timeout=timeout)
            for future in not_done:
                self.log.warning(
                    "host %s did not disconnect within %ss; abandoning its close",
                    futures[future],
                    timeout,
                )
        finally:
            # Never join a wedged close: wait=False keeps the bound above real,
            # cancel_futures drops any per-host close still queued behind it.
            executor.shutdown(wait=False, cancel_futures=True)

        for report in self.templates.all():
            report.targets.clear()

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
        async with self._command_lock(cmd_cls, argv):
            if ctx is None:
                return await asyncio.to_thread(self._run_sync, cmd_cls, argv)
            return await self._run_with_heartbeat(
                ctx, cmd_cls, argv, interval=progress_interval
            )

    async def _lock_for(self, rrid: str) -> asyncio.Lock:
        """Return (creating on first use) the per-template lock for ``rrid``.

        Lazily populates :attr:`_rrid_locks` under :attr:`_locks_guard` so two
        coroutines racing to lock the same fresh RRID share one lock object.
        """
        async with self._locks_guard:
            lock = self._rrid_locks.get(rrid)
            if lock is None:
                lock = asyncio.Lock()
                self._rrid_locks[rrid] = lock
            return lock

    @contextlib.asynccontextmanager
    async def _command_lock(self, cmd_cls: type[Command], argv: list[str]):
        """Hold the right lock(s) for ``cmd_cls`` for the duration of the body.

        Resolution mirrors the foreground dispatch:

        * a command resolving to **exactly one** template → the registry gate
          in *shared* mode **plus** that template's per-RRID lock, so
          different-RRID commands run concurrently while same-RRID commands
          serialise and no command overlaps a registry mutation;
        * fan-out / unscoped-multi commands, registry mutators
          (``load_template`` / ``unload``), or anything that resolves to no
          real template → the registry gate in *exclusive* mode, which drains
          in-flight per-RRID commands and blocks new ones for the duration.

        A single command never holds two per-RRID locks and the exclusive path
        holds only the gate, so the lock order (gate-shared → one rrid lock) is
        total and cannot deadlock.
        """
        rrids = self._resolve_job_rrids(cmd_cls, argv)
        if rrids is not None and len(rrids) == 1:
            async with self._registry.shared():
                lock = await self._lock_for(rrids[0])
                async with lock:
                    yield
        else:
            async with self._registry.exclusive():
                yield

    @contextlib.asynccontextmanager
    async def scoped_lock(self, rrid: str | None):
        """Hold the registry-shared gate plus one template's per-RRID lock.

        For the hand-written testreport tools (which act on a single template's
        files): entering the registry gate in *shared* mode keeps the loaded set
        stable for the body (no concurrent ``load_template`` / ``unload``) while
        still letting tools on *other* templates run in parallel, and the
        per-RRID lock serialises against the foreground dispatch for the *same*
        template (e.g. a concurrent ``commit``).

        ``rrid`` is the resolved target template id, or ``None`` to fall back to
        the active template (single-/zero-loaded case). Callers should resolve
        and validate the target report *inside* the body, where the shared gate
        guarantees the registry cannot change underfoot.
        """
        async with self._registry.shared():
            key = rrid if rrid is not None else str(self.templates.active.id)
            lock = await self._lock_for(key)
            async with lock:
                yield

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

        # Bind the per-call output sinks via context vars (not session
        # attributes) so concurrent commands — now that different-RRID calls
        # hold different locks — each see their own display / stdout and never
        # clobber one another. ``ContextExecutor`` propagates these into the
        # worker threads a command fans out to, exactly like ``_capture_token``.
        display_reset = _current_display.set(display)
        stdout_reset = _current_stdout.set(fake_sys.stdout)

        # Tee the command's own ``mtui.*`` log records (INFO+) into the
        # captured stdout so MCP clients see warnings/errors the command
        # logs rather than prints — e.g. product-drift warnings emitted
        # by ``TestReport._verify_target_products``. Scoped by a per-call
        # capture token (set in this context and propagated into worker
        # threads via ``ContextExecutor``) so a command's own fan-out is
        # captured while concurrent http sessions never cross-pollute, and
        # bound to the ``mtui`` logger (not ``self.log``/``mtui-mcp``) so
        # the session's own bookkeeping below is not echoed back.
        #
        # The ``mtui`` logger defaults to an effective level of WARNING
        # (inherited from root), which would drop INFO records before any
        # handler sees them, so temporarily lower it to INFO for the call
        # when it is currently stricter. The raw level is saved and
        # restored in ``finally`` so a user's ``set_log_level`` choice is
        # left untouched (under MCP that command targets the separate
        # ``mtui-mcp`` logger, but restoring the exact prior value keeps
        # this safe regardless).
        cap_logger = getLogger("mtui")
        cap_token = object()
        cap_handler = _LogCaptureHandler(fake_sys.stdout, cap_token)
        token_reset = _capture_token.set(cap_token)
        prev_cap_level = cap_logger.level
        lowered_cap_level = cap_logger.getEffectiveLevel() > logging.INFO
        if lowered_cap_level:
            cap_logger.setLevel(logging.INFO)
        cap_logger.addHandler(cap_handler)
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
                cmd_cls(args_ns, self.config, fake_sys, self).run()
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

            return cap_output(
                fake_sys.stdout.getvalue(), self.config.mcp_max_output_bytes
            )
        finally:
            cap_logger.removeHandler(cap_handler)
            _capture_token.reset(token_reset)
            if lowered_cap_level:
                cap_logger.setLevel(prev_cap_level)
            _current_display.reset(display_reset)
            _current_stdout.reset(stdout_reset)

    # ------------------------------------------------------------------
    # Background jobs (async path for slow host operations)
    # ------------------------------------------------------------------

    def _resolve_job_rrids(
        self, cmd_cls: type[Command], argv: list[str]
    ) -> list[str] | None:
        """Resolve which template RRIDs a backgrounded ``cmd_cls`` targets.

        Builds the command exactly as :meth:`_run_sync` would (parse argv,
        instantiate) and calls its :meth:`Command._resolve_templates`, so the
        background fan-out matches the foreground one byte-for-byte. Returns the
        ordered RRID list, or ``None`` when resolution is not meaningful (argv
        is unparseable here, or only the Null report resolves) — the caller then
        falls back to a single job whose body re-parses and runs as before.

        Args:
            cmd_cls: The :class:`Command` subclass to run.
            argv: Already-split command-line tokens.

        Returns:
            The ordered list of target RRIDs, or ``None`` to keep the
            single-job path.

        """
        fake_sys = _FakeSys()
        try:
            args_ns = cmd_cls.parse_args(shlex.join(argv), fake_sys)
            cmd = cmd_cls(args_ns, self.config, fake_sys, self)
            reports = cmd._resolve_templates()  # noqa: SLF001 - Command-internal API
        except Exception:  # noqa: BLE001 - resolution is best-effort; fall back
            return None
        rrids = [str(r.id) for r in reports if str(r.id)]
        return rrids or None

    def _mint_job(self, cmd_cls: type[Command], argv: list[str], job_id: str) -> str:
        """Create and start one job record running ``argv`` and return its id.

        Shared core of :meth:`start_job` and :meth:`start_jobs`. The worker task
        acquires the same per-RRID / registry gate as :meth:`run_command` (via
        :meth:`_command_lock`) for its whole duration, so a job scoped to one
        template serialises only against same-RRID work and runs concurrently
        with jobs on other templates, and executes the body in
        :func:`asyncio.to_thread`.

        Args:
            cmd_cls: The :class:`Command` subclass to run.
            argv: Already-split command-line tokens for this job.
            job_id: The pre-allocated, session-unique job id.

        Returns:
            ``job_id``.

        """
        job: dict[str, Any] = {
            "id": job_id,
            "command": cmd_cls.command,
            "argv": list(argv),
            "state": "running",
            "started": time.monotonic(),
            "finished": None,
            "result": None,
            "error": None,
            "exit_code": None,
            "task": None,
        }
        self._jobs[job_id] = job

        async def _runner() -> None:
            try:
                async with self._command_lock(cmd_cls, argv):
                    out = await asyncio.to_thread(self._run_sync, cmd_cls, argv)
                job["result"] = out
                job["state"] = "done"
            except McpCommandError as exc:
                job["state"] = "failed"
                job["error"] = str(exc)
                job["result"] = exc.stdout
                job["exit_code"] = exc.exit_code
            except asyncio.CancelledError:
                job["state"] = "cancelled"
                raise
            except Exception as exc:  # noqa: BLE001 - record, never crash the loop
                job["state"] = "failed"
                job["error"] = repr(exc)
            finally:
                job["finished"] = time.monotonic()

        job["task"] = asyncio.create_task(_runner())
        return job_id

    async def start_job(
        self,
        cmd_cls: type[Command],
        argv: list[str],
        *,
        ctx: Any | None = None,
    ) -> str:
        """Start ``cmd_cls`` in the background and return its job id.

        The command runs in an :func:`asyncio.create_task` worker that
        acquires the same per-RRID / registry gate as :meth:`run_command`
        (via :meth:`_command_lock`) for its whole duration, and executes the
        body in :func:`asyncio.to_thread`. Unlike :meth:`run_command` this
        returns **immediately** with a handle, so the client is not held
        on one request for the minutes a ``run`` / ``update`` /
        ``downgrade`` can take and can meanwhile issue other calls.

        Outcome is recorded on the job record and read back via
        :meth:`job_status` / :meth:`job_result`. ``ctx`` is accepted for
        signature parity but no heartbeat is emitted (the call returns at
        once; there is nothing to keep alive).

        This mints exactly **one** job; the tool layer calls
        :meth:`start_jobs` instead so a fanned-out slow command yields one
        job per template. Kept as the single-job primitive for tests and
        non-fan-out callers.

        Args:
            cmd_cls: The :class:`Command` subclass to run.
            argv: Already-split command-line tokens.
            ctx: Unused; accepted for caller parity.

        Returns:
            The new job id (``"<command>-<n>"``).

        """
        self._job_counter += 1
        return self._mint_job(cmd_cls, argv, f"{cmd_cls.command}-{self._job_counter}")

    async def start_jobs(
        self,
        cmd_cls: type[Command],
        argv: list[str],
        *,
        ctx: Any | None = None,
    ) -> list[str]:
        """Start ``cmd_cls`` in the background, fanning out one job per template.

        Resolves the target templates exactly as the foreground ``.run()`` path
        does (via :meth:`_resolve_templates`). When more than one template
        resolves, mints **one job per template** — each running ``argv`` scoped
        to that template with ``-T <rrid>`` appended — so a backgrounded
        fanned-out slow command is independently observable and cancellable per
        template (``job_list`` shows per-template progress; cancelling one
        leaves the others running). When a single template (or none) resolves,
        this is exactly one job with the unchanged ``<command>-<n>`` id.

        Each fanned-out job is scoped to one template (``-T <rrid>``) and so
        takes only that template's per-RRID lock, so the jobs run **concurrently
        across templates** within one session (same-RRID work still serialises);
        on top of that they remain independently observable and cancellable.

        Args:
            cmd_cls: The :class:`Command` subclass to run.
            argv: Already-split command-line tokens.
            ctx: Unused; accepted for caller parity.

        Returns:
            The list of new job ids (one per resolved template).

        """
        rrids = self._resolve_job_rrids(cmd_cls, argv)
        # Single template, none, or a client-supplied ``-T`` already narrowing
        # to one: keep the single-job path (and its stable id shape).
        if not rrids or len(rrids) <= 1:
            return [await self.start_job(cmd_cls, argv, ctx=ctx)]

        job_ids: list[str] = []
        for rrid in rrids:
            self._job_counter += 1
            token = rrid.replace(":", "_")
            job_id = f"{cmd_cls.command}-{token}-{self._job_counter}"
            # Prepend the scope flag: a positional ``nargs=REMAINDER`` command
            # (``run``) or an append-REMAINDER flag at the tail of ``argv``
            # would swallow a trailing ``-T <rrid>`` into its own value,
            # leaving ``template=None`` (wrong fan-out) and leaking ``-T`` into
            # the remote command. Emitting it first keeps it a real flag.
            scoped_argv = ["-T", rrid, *argv]
            job_ids.append(self._mint_job(cmd_cls, scoped_argv, job_id))
        return job_ids

    def _job_view(self, job: dict[str, Any]) -> dict[str, Any]:
        """Public-facing snapshot of ``job`` (no asyncio Task object)."""
        end = job["finished"] if job["finished"] is not None else time.monotonic()
        return {
            "id": job["id"],
            "command": job["command"],
            "state": job["state"],
            "elapsed_s": round(end - job["started"], 1),
        }

    def job_list(self) -> list[dict[str, Any]]:
        """Return a view of every job started in this session."""
        return [self._job_view(j) for j in self._jobs.values()]

    def job_status(self, job_id: str) -> dict[str, Any]:
        """Return ``job_id``'s state view, or raise if unknown."""
        job = self._jobs.get(job_id)
        if job is None:
            raise McpCommandError("", f"no such job: {job_id}", 1)
        return self._job_view(job)

    def job_result(self, job_id: str) -> str:
        """Return a finished job's stdout, or raise the right envelope.

        * unknown id -> :class:`McpCommandError` (exit 1)
        * still running -> :class:`McpCommandError` telling the caller to
          poll ``job_status`` (the job keeps running)
        * failed -> :class:`McpCommandError` carrying the command's
          captured stdout / error / exit code, exactly as a foreground
          failure would have surfaced
        * cancelled -> :class:`McpCommandError` (exit 1)
        * done -> the captured stdout string
        """
        job = self._jobs.get(job_id)
        if job is None:
            raise McpCommandError("", f"no such job: {job_id}", 1)
        state = job["state"]
        if state == "running":
            elapsed = round(time.monotonic() - job["started"], 1)
            raise McpCommandError(
                "",
                f"job {job_id} still running ({elapsed}s); poll job_status",
                1,
            )
        if state == "failed":
            raise McpCommandError(
                job["result"] or "",
                job["error"] or "job failed",
                job["exit_code"] or 1,
            )
        if state == "cancelled":
            raise McpCommandError("", f"job {job_id} was cancelled", 1)
        return job["result"] or ""

    async def job_cancel(self, job_id: str) -> str:
        """Cancel a running job; raise if the id is unknown.

        Cancels the worker task. NOTE: if the job is mid
        :func:`asyncio.to_thread` (an SSH/subprocess body), cancellation
        detaches the awaiter but the underlying host operation may keep
        running to completion — the same caveat as interrupting a
        foreground ``run``. A finished job is a no-op.
        """
        job = self._jobs.get(job_id)
        if job is None:
            raise McpCommandError("", f"no such job: {job_id}", 1)
        task = job.get("task")
        if task is not None and not task.done():
            task.cancel()
            with contextlib.suppress(asyncio.CancelledError, Exception):
                await task
        return f"cancelled job {job_id}"
