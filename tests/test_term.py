"""Tests for the helpers in :mod:`mtui.term`."""

import subprocess
import sys
from unittest.mock import MagicMock, patch

import pytest

from mtui.cli import colors as colorctl
from mtui.cli._history import default_history_path, get_history
from mtui.cli.colors import green
from mtui.cli.term import ask_user, filter_ansi, page, prompt_user


def _patch_prompt(response):
    """Stub the ``PromptSession`` used by :func:`prompt_user` / :func:`ask_user`.

    Returns a context manager that swaps ``prompt_toolkit.PromptSession``
    for a factory yielding a session whose ``prompt`` call returns
    ``response``. Passing an exception class (e.g. ``KeyboardInterrupt``,
    ``EOFError``) raises it instead, so the bail-out branches can be
    exercised the same way.

    ``prompt_toolkit`` is imported lazily inside
    :func:`mtui.cli.term._read_line`, so the patch targets the class on
    its home module (``prompt_toolkit.PromptSession``) rather than a
    re-export on ``mtui.cli.term``; the lazy ``from prompt_toolkit import
    PromptSession`` then binds the stub. This keeps the real ``_read_line``
    body (and its ``InMemoryHistory`` construction) exercised.
    """
    session = MagicMock()
    if isinstance(response, type) and issubclass(response, BaseException):
        session.prompt.side_effect = response
    else:
        session.prompt.return_value = response
    factory = MagicMock(return_value=session)
    return patch("prompt_toolkit.PromptSession", factory)


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


def test_prompt_user_preserves_last_command_in_history(tmp_path, monkeypatch):
    """A confirmation answer must not erase the user's last REPL command.

    Regression guard for the history-scrub bug. ``prompt_user`` used to
    pop the newest entry from the shared on-disk history in a ``finally``
    block. Because the answer itself is read through an ephemeral
    ``InMemoryHistory`` and never reaches that file, the entry actually
    removed was the *real* command the user had just run in the REPL —
    silently deleting it from both the up-arrow deque and the persisted
    file. ``prompt_user`` must now leave history completely untouched.
    """
    monkeypatch.setenv("HOME", str(tmp_path))
    # Point the shared history at a throwaway file under the fake HOME so
    # any stray scrub would hit *this* file, not the developer's real one.
    hist_path = default_history_path()
    assert hist_path == tmp_path / ".mtui_history"

    # Seed a genuine command line the way the REPL's PromptSession would.
    history = get_history(hist_path)
    history.append_string("approve -r joe")

    with _patch_prompt("y"):
        assert prompt_user("Y? ", ("y",)) is True

    # The real command survives in both the cached in-memory strings and
    # the persisted file — prompt_user touched neither.
    assert get_history(hist_path).get_strings()[-1] == "approve -r joe"
    assert "approve -r joe" in hist_path.read_text()


def test_prompt_user_does_not_touch_history_on_empty_input(tmp_path, monkeypatch):
    """An empty submission must leave the history file alone.

    Nothing was typed, so there is nothing to act on; the seeded command
    stays put and the default is returned.
    """
    monkeypatch.setenv("HOME", str(tmp_path))
    hist_path = default_history_path()
    history = get_history(hist_path)
    history.append_string("approve -r joe")

    with _patch_prompt(""):
        assert prompt_user("Y? ", ("y",), default=False) is False

    assert get_history(hist_path).get_strings()[-1] == "approve -r joe"
    assert "approve -r joe" in hist_path.read_text()


@pytest.mark.parametrize("bail", [KeyboardInterrupt, EOFError])
def test_prompt_user_bailout_returns_false(bail, tmp_path, monkeypatch):
    """Ctrl-C / Ctrl-D at the confirmation reads as a "no" answer.

    Both interrupts must short-circuit to ``False`` regardless of
    ``default`` so a stray Ctrl-C never auto-confirms a destructive
    action, and so a closed stdin (EOF) does not propagate up and crash
    the surrounding REPL loop. History must remain untouched either way.
    """
    monkeypatch.setenv("HOME", str(tmp_path))
    hist_path = default_history_path()
    history = get_history(hist_path)
    history.append_string("approve -r joe")

    with _patch_prompt(bail):
        assert prompt_user("Y? ", ("y",), default=True) is False

    assert get_history(hist_path).get_strings()[-1] == "approve -r joe"


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


def test_importing_term_does_not_load_prompt_toolkit():
    """Importing :mod:`mtui.cli.term` must not pull in ``prompt_toolkit``.

    The headless ``mtui-mcp`` server imports this module only for
    :func:`termsize`/:func:`page` and never reads a line, so
    ``prompt_toolkit`` (~115 submodules, ~170ms) is imported lazily
    inside :func:`_read_line` instead of at module scope. Run in a fresh
    interpreter because the test session itself imports prompt_toolkit
    elsewhere, which would poison an in-process ``sys.modules`` check.
    """
    code = (
        "import sys; import mtui.cli.term; "
        "loaded = [m for m in sys.modules if m.startswith('prompt_toolkit')]; "
        "assert not loaded, loaded; print('ok')"
    )
    result = subprocess.run(
        [sys.executable, "-c", code],
        capture_output=True,
        text=True,
        check=False,
    )
    assert result.returncode == 0, result.stderr
    assert result.stdout.strip() == "ok"


def test_read_line_lazy_import_still_prompts():
    """The lazy import inside :func:`_read_line` still reaches prompt_toolkit.

    Patching ``prompt_toolkit.PromptSession`` (its home, not a re-export
    on ``mtui.cli.term``) proves the deferred ``from prompt_toolkit
    import PromptSession`` binds the current attribute at call time.
    """
    from mtui.cli.term import _read_line

    with _patch_prompt("typed answer"):
        assert _read_line("prompt> ") == "typed answer"


def test_termsize_fallback_matches_normal_path_order(monkeypatch):
    """The ACCTEST fallback must return (width, height) like the ioctl path.

    It returned (rows, cols) -- the transposed geometry -- so under the
    acceptance harness page() wrapped lines at the row count and printed
    a column-count of lines per page, and the SSH PTY got swapped
    dimensions.
    """
    from mtui.cli import term as term_mod

    def _no_tty(*_a, **_kw):
        raise OSError("not a tty")

    monkeypatch.setattr(term_mod.fcntl, "ioctl", _no_tty)
    monkeypatch.setenv("ACCTEST_ROWS", "24")
    monkeypatch.setenv("ACCTEST_COLS", "80")

    width, height = term_mod.termsize()

    assert (width, height) == (80, 24)
