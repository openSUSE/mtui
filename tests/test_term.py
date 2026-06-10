"""Tests for the helpers in :mod:`mtui.term`."""

from unittest.mock import MagicMock, patch

import pytest

from mtui.cli import colors as colorctl
from mtui.cli._history import default_history_path
from mtui.cli.colors import green
from mtui.cli.term import ask_user, filter_ansi, page, prompt_user


def _patch_prompt(response):
    """Stub the ``PromptSession`` used by :func:`prompt_user` / :func:`ask_user`.

    Returns a context manager that swaps ``mtui.cli.term.PromptSession``
    for a factory yielding a session whose ``prompt`` call returns
    ``response``. Passing an exception class (e.g. ``KeyboardInterrupt``,
    ``EOFError``) raises it instead, so the bail-out branches can be
    exercised the same way.
    """
    session = MagicMock()
    if isinstance(response, type) and issubclass(response, BaseException):
        session.prompt.side_effect = response
    else:
        session.prompt.return_value = response
    factory = MagicMock(return_value=session)
    return patch("mtui.cli.term.PromptSession", factory)


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
    with _patch_prompt(response):
        assert prompt_user("? ", ["yes", "y"], True, default=default) is expected


def test_prompt_user_non_interactive_returns_default():
    """Non-interactive mode returns the ``default`` argument.

    Unlike interactive mode (where an empty response falls back to
    ``default``), non-interactive mode *always* returns ``default``
    regardless of what the user types — there is no stdin read.  This
    lets callers control whether a scripted run auto-confirms or
    always cancels.
    """
    assert prompt_user("? ", ["yes", "y"], False, default=True) is True
    assert prompt_user("? ", ["yes", "y"], False, default=False) is False


def test_prompt_user_pops_history_after_answer():
    """A typed confirmation answer triggers the defensive history scrub.

    Even though the in-line ``InMemoryHistory`` keeps the answer out of
    ``~/.mtui_history``, :func:`pop_last_entry` is still invoked so a
    stale entry written by an older ``readline``-era mtui (or by a
    hand-edit) gets cleaned up. Locking this contract keeps the
    leftover answer out of the up-arrow stack and out of
    acceptance-test history snapshots.
    """
    with (
        _patch_prompt("y"),
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
        _patch_prompt(""),
        patch("mtui.cli.term.pop_last_entry") as pop_mock,
    ):
        prompt_user("Y? ", ("y",), default=False)
    pop_mock.assert_not_called()


@pytest.mark.parametrize("bail", [KeyboardInterrupt, EOFError])
def test_prompt_user_bailout_returns_false(bail):
    """Ctrl-C / Ctrl-D at the confirmation reads as a "no" answer.

    Both interrupts must short-circuit to ``False`` regardless of
    ``default`` so a stray Ctrl-C never auto-confirms a destructive
    action, and so a closed stdin (EOF) does not propagate up and crash
    the surrounding REPL loop.
    """
    with (
        _patch_prompt(bail),
        patch("mtui.cli.term.pop_last_entry") as pop_mock,
    ):
        assert prompt_user("Y? ", ("y",), default=True) is False
    pop_mock.assert_not_called()


def test_ask_user_returns_stripped_response():
    """A free-form answer is returned with surrounding whitespace removed.

    Comment bodies typed at the ``comment`` command are forwarded
    verbatim to OSC / Gitea, so stripping accidental leading/trailing
    whitespace keeps the round-trip identical to what the user sees
    on screen.
    """
    with _patch_prompt("  hello world  "):
        assert ask_user("Comment: ") == "hello world"


def test_ask_user_non_interactive_returns_empty():
    """Non-interactive callers get an empty string, not a stdin read.

    Mirrors :func:`prompt_user`'s non-interactive contract so a
    scripted run never blocks on a closed stdin.
    """
    assert ask_user("Comment: ", interactive=False) == ""


@pytest.mark.parametrize("bail", [KeyboardInterrupt, EOFError])
def test_ask_user_bailout_returns_empty(bail):
    """Ctrl-C / Ctrl-D at a free-form prompt reads as an empty answer.

    Callers (e.g. ``comment``) treat an empty body as "user backed
    out"; raising would tear down the surrounding REPL loop instead.
    """
    with _patch_prompt(bail):
        assert ask_user("Comment: ") == ""


def test_page_non_interactive_no_writer_is_noop():
    """Historical contract: non-interactive + no writer = no output, no error."""
    # Must not call termsize() or print(); a list of "lines" is left
    # untouched. Asserted indirectly via the absence of mutation and the
    # absence of any exception.
    text = ["a", "b", "c"]
    page(text, interactive=False)
    assert text == ["a", "b", "c"]  # not reversed by the pager body


def test_page_non_interactive_writer_receives_each_line():
    """Non-interactive callers (MCP) get each line forwarded to ``writer``.

    Regression guard: the fix for the missing MCP run-command output
    depends on this path delivering all lines, in order, with trailing
    CR/LF stripped.
    """
    captured: list[str] = []
    page(["alpha", "beta\n", "gamma\r\n"], interactive=False, writer=captured.append)
    assert captured == ["alpha", "beta", "gamma"]
