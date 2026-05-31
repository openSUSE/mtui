"""Tests for the runtime colour-mode toggle."""

from __future__ import annotations

import logging
from unittest.mock import patch

import pytest

from mtui.cli import colors


@pytest.fixture(autouse=True)
def _reset_color_mode():
    """Restore the default colour mode after every test."""
    saved = colors.get_mode()
    yield
    colors.set_mode(saved)


@pytest.mark.parametrize(
    ("mode", "no_color", "color_env", "isatty", "expected"),
    [
        # Explicit always wins over everything.
        ("always", "1", "never", False, True),
        # Explicit never wins over everything.
        ("never", "", "always", True, False),
        # auto + NO_COLOR set → off, regardless of TTY / legacy COLOR.
        ("auto", "1", "always", True, False),
        ("auto", "yes", "", True, False),
        # auto + legacy COLOR=never → off.
        ("auto", "", "never", True, False),
        # auto + legacy COLOR=always → on.
        ("auto", "", "always", False, True),
        # auto, no env hints → follow isatty(stderr).
        ("auto", "", "", True, True),
        ("auto", "", "", False, False),
    ],
)
def test_colors_enabled_decision_matrix(
    monkeypatch, mode, no_color, color_env, isatty, expected
):
    """colors_enabled() must follow the documented precedence order."""
    colors.set_mode(mode)
    if no_color:
        monkeypatch.setenv("NO_COLOR", no_color)
    else:
        monkeypatch.delenv("NO_COLOR", raising=False)
    if color_env:
        monkeypatch.setenv("COLOR", color_env)
    else:
        monkeypatch.delenv("COLOR", raising=False)

    with patch("mtui.cli.colors.mode.sys.stderr.isatty", return_value=isatty):
        assert colors.colors_enabled() is expected


def test_green_returns_plain_when_disabled(monkeypatch):
    """colors.green() must omit ANSI escapes when colour is off."""
    colors.set_mode("never")
    assert colors.green("hello") == "hello"
    assert colors.red("err") == "err"
    assert colors.yellow("warn") == "warn"
    assert colors.blue("info") == "info"


def test_green_emits_ansi_when_enabled():
    """colors.green() must wrap in the expected ANSI sequence."""
    colors.set_mode("always")
    assert colors.green("hi") == "\033[1;32mhi\033[1;m\033[0m"
    assert colors.red("hi") == "\033[1;31mhi\033[1;m\033[0m"
    assert colors.yellow("hi") == "\033[1;33mhi\033[1;m\033[0m"
    assert colors.blue("hi") == "\033[1;34mhi\033[1;m\033[0m"


def test_color_formatter_plain_when_disabled():
    """ColorFormatter.formatColor() emits plain lowercase when off."""
    colors.set_mode("never")
    fmt = colors.ColorFormatter("%(levelname)s: %(message)s")
    assert fmt.formatColor("INFO") == "info"
    assert fmt.formatColor("ERROR") == "error"


def test_color_formatter_includes_module_suffix_for_debug():
    """DEBUG output keeps its '[module:function]' suffix in both modes."""
    colors.set_mode("never")
    fmt = colors.ColorFormatter("%(levelname)s: %(message)s")
    out = fmt.formatColor("DEBUG")
    assert out.startswith("debug ")
    assert "[" in out
    assert ":" in out
    assert out.endswith("]")


def test_color_formatter_full_record_no_color():
    """End-to-end: a LogRecord rendered through the formatter is plain."""
    colors.set_mode("never")
    fmt = colors.ColorFormatter("%(levelname)s: %(message)s")
    record = logging.LogRecord(
        name="mtui.test",
        level=logging.WARNING,
        pathname=__file__,
        lineno=1,
        msg="watch out",
        args=(),
        exc_info=None,
    )
    rendered = fmt.format(record)
    assert "\033[" not in rendered
    assert rendered == "warning: watch out"


def test_set_mode_round_trip():
    """set_mode + get_mode are symmetric."""
    for mode in ("auto", "always", "never"):
        colors.set_mode(mode)
        assert colors.get_mode() == mode
