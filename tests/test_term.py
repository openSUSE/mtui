"""Tests for the helpers in :mod:`mtui.term`."""

from unittest.mock import patch

import pytest

from mtui.cli import colors as colorctl
from mtui.cli._history import default_history_path
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


def test_prompt_user_pops_history_after_answer():
    """A typed confirmation answer is scrubbed from the history file.

    The y/n answer is appended to history by the surrounding
    :class:`PromptSession`, so :func:`prompt_user` calls
    :func:`pop_last_entry` to drop it before returning. Locking this
    contract keeps the leftover answer out of the up-arrow stack and
    out of acceptance-test history snapshots.
    """
    with (
        patch("builtins.input", return_value="y"),
        patch("mtui.cli.term.pop_last_entry") as pop_mock,
    ):
        assert prompt_user("Y? ", ("y",)) is True
    pop_mock.assert_called_once_with(default_history_path())


def test_prompt_user_does_not_pop_on_empty_input():
    """An empty submission must leave the history file alone.

    Nothing was typed, so there is nothing to scrub. The previous
    ``readline``-based code shared this property and tests relied on
    it; preserved here so the migration is behaviourally inert.
    """
    with (
        patch("builtins.input", return_value=""),
        patch("mtui.cli.term.pop_last_entry") as pop_mock,
    ):
        prompt_user("Y? ", ("y",), default=False)
    pop_mock.assert_not_called()
