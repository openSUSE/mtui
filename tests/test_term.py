"""Tests for the helpers in :mod:`mtui.term`."""

from mtui.cli import colors as colorctl
from mtui.cli.colors import green
from mtui.cli.term import filter_ansi


def test_filter_ansi():
    """ANSI escape sequences are stripped from the text."""
    saved = colorctl.get_mode()
    colorctl.set_mode("always")
    try:
        text = "some text"
        ansi_text = green(text)
        assert filter_ansi(ansi_text) == text
    finally:
        colorctl.set_mode(saved)
