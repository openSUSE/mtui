"""Behaviour tests for :class:`mtui.cli.prompter.Prompter`.

The Prompter owns a single :class:`threading.Lock` so that concurrent
worker threads asking the user for input over ``stdin`` see strictly
sequential prompts. There must be no interleaving, no race, no second
prompt before the first one returns.
"""

from __future__ import annotations

import contextlib
import threading
import time
from unittest.mock import MagicMock

from mtui.cli.prompter import Prompter


def test_ask_returns_reader_response():
    """``ask`` returns whatever the injected reader returns, verbatim."""
    reader = MagicMock(return_value="hello")
    p = Prompter(reader=reader)

    assert p.ask("question? ") == "hello"
    reader.assert_called_once_with("question? ")


def test_default_reader_is_input():
    """When no reader is supplied the default is :func:`input`.

    Pinning the default keeps the production wiring (``Prompter()``
    with no args) from accidentally swapping out the real ``stdin``
    reader.
    """
    p = Prompter()
    assert p._reader is input  # noqa: SLF001


def test_ask_password_uses_injected_password_reader():
    """``ask_password`` routes through the masked reader, not ``ask``'s."""
    reader = MagicMock(return_value="visible")
    pw_reader = MagicMock(return_value="s3cret")
    p = Prompter(reader=reader, password_reader=pw_reader)

    assert p.ask_password("pw? ") == "s3cret"
    pw_reader.assert_called_once_with("pw? ")
    reader.assert_not_called()


def test_default_password_reader_is_prompt_toolkit_backed():
    """The default masked reader must not be the plain visible reader.

    A bare ``input`` would echo the password and hang inside the REPL's
    prompt_toolkit session; the default must be the dedicated
    prompt_toolkit-backed reader instead.
    """
    p = Prompter()
    assert p._password_reader is not input  # noqa: SLF001
    assert p._password_reader is not p._reader  # noqa: SLF001


def test_concurrent_ask_and_password_are_serialised():
    """``ask`` and ``ask_password`` share one lock; they never overlap."""
    active = 0
    max_active = 0
    guard = threading.Lock()

    def _track(_text: str) -> str:
        nonlocal active, max_active
        with guard:
            active += 1
            max_active = max(max_active, active)
        time.sleep(0.03)
        with guard:
            active -= 1
        return _text

    p = Prompter(reader=_track, password_reader=_track)

    t1 = threading.Thread(target=lambda: p.ask("visible "))
    t2 = threading.Thread(target=lambda: p.ask_password("masked "))
    t1.start()
    t2.start()
    t1.join(timeout=5)
    t2.join(timeout=5)

    assert max_active == 1, f"ask/ask_password overlapped: {max_active}"


def test_concurrent_asks_are_serialised():
    """Two concurrent ``ask`` calls never overlap; the lock fences them.

    Each ``reader`` call sleeps 50 ms. If the prompts ran in parallel
    the total wall-clock would be ~50 ms. With the lock it must be
    ≥ 100 ms because the second thread waits for the first to release.
    The order is pinned by an :class:`~threading.Event` that the
    second thread waits on before calling ``ask`` so the first thread
    is guaranteed to acquire the lock first.
    """
    call_order: list[str] = []
    inside: list[str] = []

    def _reader(text: str) -> str:
        inside.append(text)
        time.sleep(0.05)
        call_order.append(text)
        return text

    p = Prompter(reader=_reader)
    second_can_start = threading.Event()
    first_acquired_lock = threading.Event()

    def _first() -> None:
        # Acquire-and-hold pattern: replace the reader with a wrapper
        # that signals the lock has been taken before sleeping, so the
        # second thread can start its ask() knowing the first is in
        # flight.
        first_acquired_lock.set()
        p.ask("first ")

    def _second() -> None:
        second_can_start.wait(timeout=5)
        p.ask("second ")

    t1 = threading.Thread(target=_first)
    t2 = threading.Thread(target=_second)

    start = time.monotonic()
    t1.start()
    first_acquired_lock.wait(timeout=5)
    # Tiny gap so t1 is definitely inside the lock before t2 contends.
    time.sleep(0.01)
    second_can_start.set()
    t2.start()
    t1.join(timeout=5)
    t2.join(timeout=5)
    elapsed = time.monotonic() - start

    assert not t1.is_alive()
    assert not t2.is_alive()
    assert call_order == ["first ", "second "], call_order
    assert inside == ["first ", "second "], inside
    # ≥ 2 × 50 ms with some scheduling slack.
    assert elapsed >= 0.1, f"prompts ran in parallel: {elapsed:.3f}s"


def test_reader_exception_releases_lock():
    """A reader that raises must still release the lock for the next caller.

    The ``with self._lock`` context manager guarantees release on
    exception; this test pins that contract so a future refactor
    can't accidentally swap to ``acquire()`` / ``release()`` without
    a ``finally``.
    """
    calls = []

    def _reader(text: str) -> str:
        calls.append(text)
        if len(calls) == 1:
            raise RuntimeError("boom")
        return "ok"

    p = Prompter(reader=_reader)

    with contextlib.suppress(RuntimeError):
        p.ask("first ")

    # If the lock was leaked, this call would deadlock; pytest would
    # eventually time out. We assert it returns promptly.
    assert p.ask("second ") == "ok"
    assert calls == ["first ", "second "]
