"""Spinner/logging coordination: records emitted mid-spin render flush-left.

Regression tests for the interleaving artifact where a log record emitted
while :class:`mtui.support.spinner.TtySpinner` was painting started at the
cursor column the frame write left behind (e.g. 19 phantom leading columns
after a ``[|] set_repo remove`` frame), because the old ``ColorFormatter``
erased the line with ``\\x1b[2K`` without homing the cursor first.

The fix routes every record through
:class:`mtui.cli.colors.formatter.SpinnerAwareStreamHandler`, which holds
:func:`mtui.support.spinner.spinner_suspended` around the emit: the frame is
erased with ``\\r\\x1b[K`` (cursor homed to column 0), the record is written
from a clean line, and the spinner repaints on its next tick.

The main test here drives the REAL spinner and the REAL ``create_logger``
handler against a genuine pseudo-terminal and asserts on the raw bytes the
terminal receives, plus on a minimal rendering of those bytes (``\\r``,
``\\n``, ``\\x1b[K``, ``\\x1b[2K``; SGR colour sequences stripped).
"""

from __future__ import annotations

import contextlib
import io
import logging
import os
import pty
import re
import select
import sys
import time
from collections.abc import Callable
from unittest.mock import MagicMock

import pytest

from mtui.cli import colors
from mtui.cli.colors.formatter import ColorFormatter, create_logger
from mtui.support import spinner as _spinner
from mtui.support.spinner import TtySpinner, spinner_suspended

# --------------------------------------------------------------------- pty


def _read_until(
    master_fd: int,
    buf: bytes,
    predicate: Callable[[bytes], bool],
    timeout: float = 5.0,
) -> bytes:
    """Accumulate bytes from the pty master until ``predicate`` or timeout."""
    deadline = time.monotonic() + timeout
    while not predicate(buf) and time.monotonic() < deadline:
        readable, _, _ = select.select([master_fd], [], [], 0.05)
        if readable:
            try:
                data = os.read(master_fd, 4096)
            except OSError:
                break
            if not data:
                break
            buf += data
    return buf


_SGR = re.compile(r"\x1b\[[0-9;]*m")


def _render(stream: bytes) -> list[str]:
    """Minimal terminal emulation of the captured byte stream.

    Handles ``\\r`` (cursor to column 0), ``\\n`` (new line; the pty's ONLCR
    already inserts the ``\\r``), ``\\x1b[K`` (erase to end of line, cursor
    unmoved) and ``\\x1b[2K`` (erase entire line, cursor unmoved). SGR colour
    sequences are stripped. Returns the visible lines.
    """
    text = _SGR.sub("", stream.decode("utf-8", "replace"))
    lines: list[list[str]] = [[]]
    col = 0
    i = 0
    while i < len(text):
        if text.startswith("\x1b[2K", i):
            lines[-1] = [" "] * len(lines[-1])
            i += 4
            continue
        if text.startswith("\x1b[K", i):
            del lines[-1][col:]
            i += 3
            continue
        ch = text[i]
        if ch == "\r":
            col = 0
        elif ch == "\n":
            lines.append([])
            col = 0
        else:
            row = lines[-1]
            while len(row) < col:
                row.append(" ")
            if col < len(row):
                row[col] = ch
            else:
                row.append(ch)
            col += 1
        i += 1
    return ["".join(row) for row in lines]


@pytest.mark.parametrize("mode", ["always", "never"])
def test_log_emitted_mid_spin_is_erased_and_flush_left(monkeypatch, mode):
    """A record landing while a frame is on screen renders from column 0.

    ``mode="always"`` covers the coloured path (the field symptom: 19 phantom
    leading columns); ``mode="never"`` covers the NO_COLOR-on-a-TTY path where
    the old code appended the record straight after the frame text.
    """
    master_fd, slave_fd = pty.openpty()
    slave = io.TextIOWrapper(
        os.fdopen(slave_fd, "wb", buffering=0),
        encoding="utf-8",
        write_through=True,
    )
    # Both the spinner and StreamHandler() (constructed inside create_logger)
    # must see the pty slave as stderr, exactly like mtui.main on a real tty.
    monkeypatch.setattr(sys, "stderr", slave)
    assert sys.stderr.isatty()

    saved_mode = colors.get_mode()
    colors.set_mode(mode)
    logger = create_logger(f"test-spinlog-{mode}")
    spin = TtySpinner("set_repo remove")  # '[|] set_repo remove' = 19 cols
    buf = b""
    try:
        spin.start()
        # Wait until at least one frame is painted, so the record provably
        # lands mid-spin with the cursor parked at the end of the frame.
        buf = _read_until(master_fd, buf, lambda b: b"] set_repo remove" in b)
        assert b"] set_repo remove" in buf, f"no frame painted: {buf!r}"

        logger.info("Removing repo repo-oscrc on shrikebill")
        buf = _read_until(master_fd, buf, lambda b: b"Removing repo" in b)
    finally:
        spin.stop()
        colors.set_mode(saved_mode)
        for h in list(logger.handlers):
            logger.removeHandler(h)
        with contextlib.suppress(OSError):
            slave.close()
        with contextlib.suppress(OSError):
            os.close(master_fd)

    msg_at = buf.index(b"Removing repo")
    frame_at = buf.rindex(b"set_repo remove", 0, msg_at)
    # The record must be preceded by a clean line-erase that homes the cursor
    # (CR + erase-to-end), not by the old cursor-in-place \x1b[2K.
    assert b"\r\x1b[K" in buf[frame_at:msg_at], buf[frame_at:msg_at]
    assert b"\x1b[2K" not in buf

    rendered = _render(buf)
    line = next(ln for ln in rendered if "Removing repo" in ln)
    # Flush-left: no phantom leading columns, no frame residue on the line.
    assert line.startswith("info: Removing repo"), rendered
    assert not any(
        "set_repo remove" in ln and "Removing repo" in ln for ln in rendered
    ), rendered


# ------------------------------------------------------- spinner_suspended


def _fake_stderr(monkeypatch, isatty: bool) -> MagicMock:
    fake = MagicMock()
    fake.isatty.return_value = isatty
    monkeypatch.setattr(_spinner.sys, "stderr", fake)
    return fake


def test_suspended_erases_frame_while_spinner_active(monkeypatch):
    fake = _fake_stderr(monkeypatch, isatty=True)
    s = TtySpinner("working")
    s.start()
    try:
        fake.write.reset_mock()
        with spinner_suspended():
            fake.write.assert_any_call("\r\033[K")
    finally:
        s.stop()


def test_suspended_noop_without_active_spinner(monkeypatch):
    fake = _fake_stderr(monkeypatch, isatty=True)
    with spinner_suspended():
        pass
    fake.write.assert_not_called()


def test_suspended_noop_off_tty_even_with_started_spinner(monkeypatch):
    """Off a TTY the spinner never registers, so nothing is ever written."""
    fake = _fake_stderr(monkeypatch, isatty=False)
    s = TtySpinner("working")
    s.start()
    with spinner_suspended():
        pass
    s.stop()
    fake.write.assert_not_called()


# ----------------------------------------------------- non-TTY logger path


def test_handler_off_tty_emits_plain_record_byte_exact(monkeypatch):
    """Off a TTY (pytest, redirects, mtui-mcp) the handler adds nothing."""
    stream = io.StringIO()  # isatty() is False
    monkeypatch.setattr(sys, "stderr", stream)
    saved_mode = colors.get_mode()
    colors.set_mode("never")
    logger = create_logger("test-spinlog-notty")
    try:
        s = TtySpinner("set_repo remove")
        s.start()  # no-op off a TTY
        logger.info("hello")
        s.stop()
    finally:
        colors.set_mode(saved_mode)
        for h in list(logger.handlers):
            logger.removeHandler(h)

    assert stream.getvalue() == "info: hello\n"


def test_colorformatter_no_longer_injects_line_erase():
    """Screen management moved to the handler; the formatter emits only SGR."""
    saved_mode = colors.get_mode()
    colors.set_mode("always")
    try:
        formatter = ColorFormatter("%(levelname)s: %(message)s")
        record = logging.LogRecord(
            "test-spinlog", logging.INFO, __file__, 1, "msg", None, None
        )
        out = formatter.format(record)
    finally:
        colors.set_mode(saved_mode)
    assert "\x1b[2K" not in out
    assert out.startswith("\x1b[1;32minfo\x1b[0m: msg")
