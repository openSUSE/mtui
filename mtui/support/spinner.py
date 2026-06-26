"""A tiny TTY spinner for long-running interactive operations.

Repaints a ``|/-\\`` frame on stderr while work is in flight, but only when
stderr is a TTY. Off a TTY (pytest, redirected output, log files, the
``mtui-mcp`` transport) it is a no-op, so test output and log files stay clean
and the MCP layer can surface progress through its own channel instead.
"""

from __future__ import annotations

import sys
import threading
from collections.abc import Callable, Iterator
from contextlib import contextmanager, suppress


class TtySpinner:
    """A ``|/-\\`` spinner driven by one daemon thread; a no-op off a TTY.

    Safe to :meth:`stop` more than once and from any thread.
    """

    _FRAMES = "|/-\\"
    _INTERVAL = 0.1  # seconds

    def __init__(self, desc: str) -> None:
        self._desc = desc
        self._stop = threading.Event()
        self._thread: threading.Thread | None = None
        self._enabled = sys.stderr.isatty()

    def start(self) -> None:
        """Start the spinner thread (no-op when stderr is not a TTY)."""
        if not self._enabled:
            return
        self._thread = threading.Thread(target=self._spin, daemon=True)
        self._thread.start()

    def stop(self) -> None:
        """Stop the spinner thread and erase the spinner line."""
        # Always flag the stop event so the ``is_stopped`` predicate works even
        # off a TTY (where the painting thread never ran).
        self._stop.set()
        if not self._enabled:
            return
        if self._thread is not None:
            self._thread.join(timeout=1.0)
            self._thread = None
        # Erase the spinner line so the next caller writes from column 0.
        with suppress(Exception):
            sys.stderr.write("\r\033[K")
            sys.stderr.flush()

    def is_stopped(self) -> bool:
        """True once :meth:`stop` has been called (set even off a TTY).

        Exposed as a cooperative-cancellation predicate for long-running callees
        wrapped by :func:`spinner`.
        """
        return self._stop.is_set()

    def _spin(self) -> None:
        i = 0
        while not self._stop.is_set():
            with suppress(Exception):
                sys.stderr.write(f"\r[{self._FRAMES[i % 4]}] {self._desc}")
                sys.stderr.flush()
            i += 1
            self._stop.wait(self._INTERVAL)


@contextmanager
def spinner(desc: str) -> Iterator[Callable[[], bool]]:
    """Run the wrapped block with a TTY spinner labelled ``desc``.

    A no-op when stderr is not a TTY, so it is safe in tests and over MCP.

    Yields a ``is_stopped`` predicate: it returns ``True`` once the block is
    being torn down (normal exit *or* an exception such as ``KeyboardInterrupt``
    unwinding the ``with``). A long-running callee can poll it as a cooperative
    cancellation signal, so Ctrl-C inside the block stops the work promptly
    instead of blocking until the next natural checkpoint.
    """
    s = TtySpinner(desc)
    s.start()
    try:
        yield s.is_stopped
    finally:
        s.stop()
