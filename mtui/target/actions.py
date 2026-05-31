"""Classes for performing actions on groups of target hosts in parallel."""

from __future__ import annotations

import sys
import threading
from abc import ABC, abstractmethod
from collections.abc import Callable, ValuesView
from concurrent.futures import ThreadPoolExecutor, as_completed
from contextlib import suppress
from pathlib import Path
from threading import Lock
from typing import Any

from ..cli.term import prompt_user
from ..types import ExecutionMode
from . import Target


class _TtySpinner:
    """A tiny ``|/-\\`` spinner that writes to stderr only when it is a TTY.

    Drives a single daemon thread that repaints ``\\r<frame> <desc>`` on
    a fixed cadence, then erases the line on stop. When stderr is not a
    TTY (pytest, redirected output, log files) the spinner is a no-op so
    test output and log files stay clean. Safe to ``stop()`` more than
    once and from any thread.
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
        if not self._enabled:
            return
        self._stop.set()
        if self._thread is not None:
            self._thread.join(timeout=1.0)
            self._thread = None
        # Erase the spinner line so the next caller writes from column 0.
        with suppress(Exception):
            sys.stderr.write("\r\033[K")
            sys.stderr.flush()

    def _spin(self) -> None:
        i = 0
        while not self._stop.is_set():
            with suppress(Exception):
                sys.stderr.write(f"\r[{self._FRAMES[i % 4]}] {self._desc}")
                sys.stderr.flush()
            i += 1
            self._stop.wait(self._INTERVAL)


def run_parallel(
    work: list[tuple[Callable[..., Any], tuple[Any, ...]]],
    desc: str | None = None,
) -> None:
    """Submit ``(callable, args)`` pairs to a pool and re-raise on failure.

    Each callable runs in its own worker thread. The first worker
    exception surfaces to the caller. On any exception (including
    ``KeyboardInterrupt``) the executor is shut down with
    ``cancel_futures=True`` and ``wait=False`` so queued-but-not-yet-
    started callables are dropped and the caller is not blocked on
    in-flight workers. Note: Python cannot interrupt a foreign C-level
    blocking call, so callables already in flight will run to
    completion -- callers that need prompt cancellation must arrange
    for the worker's blocking I/O to be unblocked externally (e.g. by
    closing the underlying socket).

    When ``desc`` is given the helper drives a TTY-only ``|/-\\``
    spinner labelled with ``desc`` while work is in flight. The
    spinner is silent when stderr is not a TTY (log files, redirected
    output, pytest), and the call is behaviourally identical to a
    ``desc=None`` call from the worker callables' point of view.
    """
    if not work:
        return
    spinner = _TtySpinner(desc) if desc else None
    if spinner is not None:
        spinner.start()
    ex = ThreadPoolExecutor(max_workers=len(work))
    try:
        futures = [ex.submit(fn, *args) for fn, args in work]
        for f in as_completed(futures):
            f.result()  # propagate any worker exception
    except BaseException:
        ex.shutdown(wait=False, cancel_futures=True)
        raise
    else:
        ex.shutdown(wait=True)
    finally:
        if spinner is not None:
            spinner.stop()


class ThreadedTargetGroup(ABC):
    """An abstract base class for performing actions on a group of targets."""

    def __init__(self, targets: list[Target] | ValuesView[Target]) -> None:
        """Initializes the threaded target group.

        Args:
            targets: A list of targets to perform actions on.

        """
        self.targets = targets

    def run(self) -> None:
        """Runs the action on every target in parallel."""
        run_parallel(
            [self.mk_cmd(t) for t in self.targets],
            desc=type(self).__name__,
        )

    @abstractmethod
    def mk_cmd(self, t: Target) -> tuple[Callable[..., Any], tuple[Any, ...]]:
        """Build a ``(callable, args)`` pair to dispatch for one target."""


class FileDelete(ThreadedTargetGroup):
    """Deletes a file on a group of targets in parallel."""

    def __init__(self, targets: list[Target] | ValuesView[Target], path: Path) -> None:
        """Initializes the file delete action.

        Args:
            targets: A list of targets to delete the file from.
            path: The path to the file to delete.

        """
        super().__init__(targets)
        self.path = path

    def mk_cmd(self, t: Target) -> tuple[Callable[..., Any], tuple[Any, ...]]:
        """Creates a command to delete a file on a target."""
        return (t.sftp_remove, (self.path,))


class FileUpload(ThreadedTargetGroup):
    """Uploads a file to a group of targets in parallel."""

    def __init__(
        self, targets: list[Target] | ValuesView[Target], local: Path, remote: Path
    ) -> None:
        """Initializes the file upload action.

        Args:
            targets: A list of targets to upload the file to.
            local: The local path to the file to upload.
            remote: The remote path to upload the file to.

        """
        super().__init__(targets)
        self.local = local
        self.remote = remote

    def mk_cmd(self, t: Target) -> tuple[Callable[..., Any], tuple[Any, ...]]:
        """Creates a command to upload a file to a target."""
        return (t.sftp_put, (self.local, self.remote))


class FileDownload(ThreadedTargetGroup):
    """Downloads a file from a group of targets in parallel."""

    def __init__(
        self, targets: list[Target] | ValuesView[Target], remote: Path, local: Path
    ) -> None:
        """Initializes the file download action.

        Args:
            targets: A list of targets to download the file from.
            remote: The remote path to the file to download.
            local: The local path to save the downloaded file to.

        """
        super().__init__(targets)

        self.remote = remote
        self.local = local

    def mk_cmd(self, t: Target) -> tuple[Callable[..., Any], tuple[Any, ...]]:
        """Creates a command to download a file from a target."""
        return (t.sftp_get, (self.remote, self.local))


class RunCommand:
    """Runs a command on a group of targets, parallel or serial."""

    def __init__(
        self, targets: dict[str, Target], command: str | dict[str, Any]
    ) -> None:
        """Initializes the run command action.

        Args:
            targets: A dictionary of targets to run the command on.
            command: The command to run.

        """
        self.targets = targets
        self.command = command

    def _cmd_for(self, hostname: str) -> Any:
        """Resolve the per-host command, accepting a string or dict shape."""
        if isinstance(self.command, dict):
            return self.command[hostname]
        return self.command

    def run(self) -> None:
        """Runs the command: parallel hosts in a pool, serial hosts one at a time."""
        parallel = {
            h: t for h, t in self.targets.items() if t.mode is not ExecutionMode.SERIAL
        }
        serial = {
            h: t for h, t in self.targets.items() if t.mode is ExecutionMode.SERIAL
        }
        lock = Lock()

        try:
            run_parallel(
                [(t.run, (self._cmd_for(h), lock)) for h, t in parallel.items()],
                desc="run",
            )

            for h, t in serial.items():
                prompt_user(
                    f"press Enter key to proceed with {t.hostname!s}",
                    "",
                )
                t.run(self._cmd_for(h), lock)

        except KeyboardInterrupt:
            # ``run_parallel`` already cancelled queued futures and
            # returned without joining in-flight workers. Close every
            # session here so any worker still blocked inside
            # ``connection.run`` unblocks promptly and exits cleanly.
            print("stopping command queue, please wait.")  # noqa: T201
            for t in self.targets.values():
                with suppress(Exception):
                    t.connection.close_session()
            print()  # noqa: T201
            raise
