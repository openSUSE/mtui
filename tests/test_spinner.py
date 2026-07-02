"""Tests for the TTY spinner's active (is-a-TTY) path.

The no-op (non-TTY) behaviour is covered in ``test_actions.py``; here stderr is
faked as a TTY so the thread-lifecycle and frame-painting branches run.
"""

from __future__ import annotations

from unittest.mock import MagicMock

from mtui.support import spinner as _spinner
from mtui.support.spinner import TtySpinner, spinner


def _fake_tty(monkeypatch) -> MagicMock:
    fake = MagicMock()
    fake.isatty.return_value = True
    monkeypatch.setattr(_spinner.sys, "stderr", fake)
    return fake


def test_spinner_enabled_starts_and_stops_thread(monkeypatch):
    fake = _fake_tty(monkeypatch)
    s = TtySpinner("working")
    s.start()
    assert s._thread is not None  # noqa: SLF001
    s.stop()
    # Thread is joined and cleared; the line is erased on stop.
    assert s._thread is None  # noqa: SLF001
    fake.write.assert_any_call("\r\033[K")


def test_stop_is_idempotent(monkeypatch):
    _fake_tty(monkeypatch)
    s = TtySpinner("working")
    s.start()
    s.stop()
    s.stop()  # must not raise even though the thread is already gone


def test_spin_paints_a_frame(monkeypatch):
    """Drive one iteration of ``_spin`` deterministically and check the frame."""
    fake = _fake_tty(monkeypatch)
    s = TtySpinner("regen")

    # ``_spin`` checks the stop event twice per iteration: once in the loop
    # condition and once under the paint lock (so a frame is never repainted
    # after ``stop`` erased it). One painted frame, then exit.
    calls = iter([False, False, True])
    s._stop = MagicMock()  # noqa: SLF001
    s._stop.is_set.side_effect = lambda: next(calls)  # noqa: SLF001
    s._stop.wait.return_value = None  # noqa: SLF001

    s._spin()  # noqa: SLF001

    fake.write.assert_called_once_with("\r[|] regen")


def test_spinner_contextmanager_enabled(monkeypatch):
    fake = _fake_tty(monkeypatch)
    with spinner("ctx") as is_stopped:
        # Inside the block the spinner is live, so the cancel predicate is False.
        assert is_stopped() is False
    # Entering started a thread and exiting erased the line.
    fake.write.assert_any_call("\r\033[K")


def test_spinner_handle_flips_to_stopped_on_exit(monkeypatch):
    _fake_tty(monkeypatch)
    with spinner("ctx") as is_stopped:
        captured = is_stopped
    # After teardown the predicate reports stopped (the cancellation signal).
    assert captured() is True


def test_is_stopped_set_even_off_a_tty(monkeypatch):
    # Off a TTY the painting thread never runs, but the stop predicate must
    # still flip so a callee polling it as a cancel hook works in tests / MCP.
    fake = MagicMock()
    fake.isatty.return_value = False
    monkeypatch.setattr(_spinner.sys, "stderr", fake)
    s = TtySpinner("x")
    s.start()
    assert s.is_stopped() is False
    s.stop()
    assert s.is_stopped() is True


def test_spin_never_repaints_after_stop_wins_the_lock(monkeypatch):
    """stop() landing between the while-check and the paint lock wins.

    A long ``spinner_suspended`` hold can outlive ``stop()``; the frame
    painter must re-check the stop flag under the lock and bail without
    painting, or it would repaint a frame stop() already erased.
    """
    fake = _fake_tty(monkeypatch)
    s = TtySpinner("desc")
    # First is_set(): the outer while condition (not stopped yet). Second:
    # the re-check under the paint lock (stop() got there first).
    s._stop.is_set = MagicMock(side_effect=[False, True])

    s._spin()

    fake.write.assert_not_called()
