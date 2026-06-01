"""Tests for the helpers in :mod:`mtui.term`."""

from unittest.mock import patch

import pytest

from mtui.cli import colors as colorctl
from mtui.cli.colors import green
from mtui.cli.term import filter_ansi, prompt_user


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


@pytest.mark.parametrize(
    ("response", "default", "expected"),
    [
        ("", True, True),  # empty input takes the default
        ("", False, False),
        ("y", False, True),  # explicit yes overrides default
        ("n", True, False),  # explicit no overrides default
    ],
)
def test_prompt_user_default_on_empty_input(response, default, expected):
    """An empty interactive response falls back to ``default``."""
    with patch("builtins.input", return_value=response):
        assert prompt_user("? ", ["yes", "y"], True, default=default) is expected


def test_prompt_user_non_interactive_ignores_default():
    """Non-interactive mode never auto-confirms, even with default=True."""
    assert prompt_user("? ", ["yes", "y"], False, default=True) is False
