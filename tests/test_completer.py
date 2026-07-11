"""Tests for the prompt_toolkit completer adapter.

Locks in the translation contract between
:class:`prompt_toolkit.document.Document` and the legacy
``cmd.Cmd``-style ``complete(state, text, line, begidx, endidx)`` API
that every :class:`mtui.commands._command.Command` still implements.

The boundary cases (cursor mid-word, after space, after ``-``, after
``--``, empty first token) are explicit per PLAN.md Risk #1 — these are
the inputs most likely to produce a different completion set than the
historical ``cmd.Cmd`` loop did.
"""

from __future__ import annotations

from unittest.mock import MagicMock

from prompt_toolkit.completion import CompleteEvent
from prompt_toolkit.document import Document

from mtui.cli._completer import MtuiCompleter, _split_text_word
from mtui.cli.completion import (
    complete_choices,
    complete_choices_filelist,
    template_completion,
)
from mtui.cli.repl import CommandPrompt


def _make_prompt() -> CommandPrompt:
    """Build a stock ``CommandPrompt`` for completer tests.

    Mirrors ``tests/test_prompt.py::_make_prompt`` but kept local to
    avoid cross-module coupling between the two test files.
    """
    config = MagicMock()
    config.auto = False
    config.kernel = False
    return CommandPrompt(config, MagicMock(), MagicMock(), MagicMock())


def _completions(p: CommandPrompt, text: str) -> list[str]:
    """Return the ``.text`` of every Completion produced for ``text``.

    The cursor is implicitly at the end of ``text`` (``Document`` default).
    Returns a list (not a set) so callers can assert order when it
    matters; most assertions use ``set(...)`` because the underlying
    ``complete_choices`` builds its candidate set from a ``set``.
    """
    completer = MtuiCompleter(p)
    doc = Document(text=text, cursor_position=len(text))
    return [c.text for c in completer.get_completions(doc, CompleteEvent())]


# --------------------------------------------------------------------------- #
# _split_text_word: low-level translation helper                              #
# --------------------------------------------------------------------------- #


def test_split_empty_line():
    """Empty input yields empty word at column 0."""
    assert _split_text_word("") == ("", 0)


def test_split_no_whitespace():
    """A single contiguous token: whole line is the word, begidx=0."""
    assert _split_text_word("quit") == ("quit", 0)


def test_split_trailing_space_yields_empty_word():
    """Cursor right after a space: text is empty, begidx == len(line).

    Matches ``cmd.Cmd.complete``'s behaviour and keeps the partial-word
    contract documented in :func:`MtuiCompleter.get_completions`.
    """
    assert _split_text_word("run -t ") == ("", 7)


def test_split_mid_argument():
    """Partial argument after a space: word is the tail."""
    assert _split_text_word("run -t host") == ("host", 7)


def test_split_partial_long_option():
    """``--`` and short ``-`` flags are part of the word."""
    assert _split_text_word("run --") == ("--", 4)
    assert _split_text_word("run -") == ("-", 4)


# --------------------------------------------------------------------------- #
# First-token (command name) completion                                       #
# --------------------------------------------------------------------------- #


def test_first_token_prefix_matches_command_names():
    """Typing ``qu`` proposes every registered command starting with ``qu``."""
    p = _make_prompt()
    out = set(_completions(p, "qu"))
    assert "quit" in out
    # No false positives outside the prefix.
    assert all(name.startswith("qu") for name in out)


def test_first_token_empty_lists_all_commands():
    """Empty buffer proposes every registered command name."""
    p = _make_prompt()
    out = set(_completions(p, ""))
    assert out == set(p.commands.keys())


def test_first_token_no_match_yields_nothing():
    """A prefix that matches no command yields no completions."""
    p = _make_prompt()
    assert _completions(p, "definitely-not-a-cmd") == []


# --------------------------------------------------------------------------- #
# Per-command delegation                                                      #
# --------------------------------------------------------------------------- #


def test_quit_completer_offers_boot_args():
    """``quit `` (trailing space) offers the ``reboot``/``poweroff`` choices."""
    p = _make_prompt()
    out = set(_completions(p, "quit "))
    assert out == {"reboot", "poweroff"}


def test_quit_completer_prefix_filters():
    """``quit r`` narrows the choices to those starting with ``r``."""
    p = _make_prompt()
    assert _completions(p, "quit r") == ["reboot"]


def test_run_short_option_completion():
    """``run -`` yields the short and long target flags."""
    p = _make_prompt()
    out = set(_completions(p, "run -"))
    assert {"-t", "--target"}.issubset(out)


def test_run_long_option_completion():
    """``run --`` narrows to the long form only."""
    p = _make_prompt()
    out = set(_completions(p, "run --"))
    assert "--target" in out
    assert "-t" not in out


def test_run_trailing_space_offers_host_names():
    """``run -t `` (post-space) yields the live host list from ``state``.

    The bound ``complete_run`` closure injects
    ``state['hosts'] = self.targets.select()``; we stub ``select()`` to
    a fake object whose ``.names()`` returns a known list, then assert
    those names round-trip through the adapter.
    """
    p = _make_prompt()
    hosts = MagicMock()
    hosts.names.return_value = ["alpha.example", "beta.example"]
    p.targets.select = MagicMock(return_value=hosts)

    out = set(_completions(p, "run -t "))
    assert {"alpha.example", "beta.example"}.issubset(out)


def test_edit_offers_filelist(tmp_path, monkeypatch):
    """``edit `` delegates to ``complete_choices_filelist``.

    Switch CWD to an empty tmp dir so the assertion is deterministic
    across machines (the helper lists files in the current directory).
    """
    monkeypatch.chdir(tmp_path)
    (tmp_path / "alpha.txt").write_text("")
    (tmp_path / "beta.txt").write_text("")

    p = _make_prompt()
    out = set(_completions(p, "edit "))
    assert {"./alpha.txt", "./beta.txt"}.issubset(out)


def test_unknown_command_second_token_yields_nothing():
    """Past the first space, an unknown command name yields no completions."""
    p = _make_prompt()
    assert _completions(p, "definitely-not-a-cmd ") == []
    assert _completions(p, "definitely-not-a-cmd foo") == []


# --------------------------------------------------------------------------- #
# Adapter equivalence vs the legacy complete_choices output                   #
# --------------------------------------------------------------------------- #


def test_adapter_matches_complete_choices_for_quit():
    """Completer output equals ``complete_choices`` direct output for ``quit``."""
    p = _make_prompt()
    # Replicate the closure call the adapter makes under the hood by
    # invoking ``complete_choices`` directly with the same arguments
    # the bound ``complete_quit`` would forward.
    expected = set(complete_choices([("reboot", "poweroff")], "quit r", "r"))
    actual = set(_completions(p, "quit r"))
    assert actual == expected


def test_adapter_matches_complete_choices_for_run():
    """Completer output equals ``complete_choices`` direct output for ``run -``."""
    p = _make_prompt()
    hosts = MagicMock()
    hosts.names.return_value = []
    p.targets.select = MagicMock(return_value=hosts)
    # ``run`` also offers the fan-out template flags; with no templates loaded
    # ``template_completion`` contributes only the flag tokens.
    state = {"templates": p.templates}
    expected = set(
        complete_choices(
            [("-t", "--target"), *template_completion(state)], "run -", "-", []
        )
    )
    actual = set(_completions(p, "run -"))
    assert actual == expected


# --------------------------------------------------------------------------- #
# Tilde (home directory) expansion in file-path completion                    #
# --------------------------------------------------------------------------- #


def test_filelist_tilde_partial_path_completes(tmp_path, monkeypatch):
    """``~/Doc`` offers the home entries matching the partial basename.

    Regression: the helper used to append ``/`` to the tilde-expanded
    text, so ``~/Doc`` was treated as the (non-existent) directory
    ``$HOME/Doc/`` — ``os.listdir`` of it yielded nothing and no
    completions were ever offered for a partial path under ``~``.
    ``os.path.expanduser`` resolves ``~`` from ``$HOME`` on POSIX, so
    pointing ``HOME`` at ``tmp_path`` keeps the test hermetic without
    changing the CWD. The single match is a directory, so it carries a
    trailing ``/`` (shell convention, lets a following TAB descend).
    """
    (tmp_path / "Documents").mkdir()
    (tmp_path / "Downloads").mkdir()
    monkeypatch.setenv("HOME", str(tmp_path))
    assert complete_choices_filelist([], "put ~/Doc", "~/Doc") == [
        f"{tmp_path}/Documents/"
    ]


def test_filelist_tilde_exact_directory_lists_contents(tmp_path, monkeypatch):
    """``~/`` (an exact existing directory) offers every entry in it.

    Also locks in the candidate shape: single ``/`` separators (the old
    unconditional ``text += '/'`` produced ``$HOME//<entry>``), each
    suffixed with its own trailing ``/`` since both entries are
    directories.
    """
    (tmp_path / "Documents").mkdir()
    (tmp_path / "Downloads").mkdir()
    monkeypatch.setenv("HOME", str(tmp_path))
    assert set(complete_choices_filelist([], "put ~/", "~/")) == {
        f"{tmp_path}/Documents/",
        f"{tmp_path}/Downloads/",
    }


def test_filelist_bare_tilde_lists_home_contents(tmp_path, monkeypatch):
    """A bare ``~`` (no trailing slash, no further path) lists $HOME itself.

    Regression: ``os.path.expanduser("~")`` yields ``$HOME`` with *no*
    trailing slash, so naively taking "everything up to the last '/'"
    as the directory to list resolves to $HOME's own *parent* —
    listing sibling home directories instead of $HOME's contents, and
    (via ``complete_choices``'s prefix match) offering only $HOME
    itself as a single dead-end candidate. $HOME must be listed
    directly, exactly like the old (pre-tilde-completion-fix) code did.
    """
    (tmp_path / "Documents").mkdir()
    (tmp_path / "Downloads").mkdir()
    monkeypatch.setenv("HOME", str(tmp_path))
    assert set(complete_choices_filelist([], "put ~", "~")) == {
        f"{tmp_path}/Documents/",
        f"{tmp_path}/Downloads/",
    }


def test_filelist_tilde_exact_directory_no_trailing_slash_descends(
    tmp_path, monkeypatch
):
    """``~/Documents`` (full name, no trailing slash) lists its contents.

    Regression: typing the exact, unambiguous directory name used to
    dead-end on re-offering ``$HOME/Documents`` itself (an exact match
    against its own listing in its parent) instead of descending into
    it, unlike the old code which forced a trailing slash onto any
    tilde path and so always listed the named directory's contents.
    """
    docs = tmp_path / "Documents"
    docs.mkdir()
    (docs / "alpha.txt").write_text("")
    (docs / "beta.txt").write_text("")
    monkeypatch.setenv("HOME", str(tmp_path))
    assert set(complete_choices_filelist([], "put ~/Documents", "~/Documents")) == {
        f"{docs}/alpha.txt",
        f"{docs}/beta.txt",
    }


def test_filelist_tilde_subdirectory_partial_filters_by_basename(tmp_path, monkeypatch):
    """``~/Documents/al`` lists ``~/Documents`` and narrows by basename."""
    docs = tmp_path / "Documents"
    docs.mkdir()
    (docs / "alpha.txt").write_text("")
    (docs / "beta.txt").write_text("")
    monkeypatch.setenv("HOME", str(tmp_path))
    assert complete_choices_filelist([], "put ~/Documents/al", "~/Documents/al") == [
        f"{docs}/alpha.txt"
    ]


def test_filelist_tilde_directory_sibling_is_not_hidden(tmp_path, monkeypatch):
    """A directory whose name is a prefix of a sibling directory's name.

    Regression: forcing the descent whenever the typed (expanded) text
    names an existing directory would, for ``~/Doc`` here, silently
    swallow the sibling ``Documents`` — the parent's entries must be
    listed instead whenever another entry shares the same prefix.
    """
    (tmp_path / "Doc").mkdir()
    (tmp_path / "Documents").mkdir()
    monkeypatch.setenv("HOME", str(tmp_path))
    assert set(complete_choices_filelist([], "put ~/Doc", "~/Doc")) == {
        f"{tmp_path}/Doc/",
        f"{tmp_path}/Documents/",
    }


def test_filelist_tilde_file_does_not_hide_prefixed_directory_sibling(
    tmp_path, monkeypatch
):
    """A *file* whose name exactly matches the typed text must not hide
    a directory sharing that prefix.

    Regression: ``complete_choices`` used to short-circuit the moment
    it found a candidate equal to ``text``, discarding any other
    prefix match already collected (or not yet visited) depending on
    set-iteration order — here that would drop ``Documents`` because
    the file ``Doc`` matches ``text`` exactly.
    """
    (tmp_path / "Doc").write_text("")
    (tmp_path / "Documents").mkdir()
    monkeypatch.setenv("HOME", str(tmp_path))
    assert set(complete_choices_filelist([], "put ~/Doc", "~/Doc")) == {
        f"{tmp_path}/Doc",
        f"{tmp_path}/Documents/",
    }


# --------------------------------------------------------------------------- #
# Error robustness                                                            #
# --------------------------------------------------------------------------- #


def test_filelist_completion_survives_missing_directory(tmp_path, monkeypatch):
    """A typo'd path prefix (e.g. ``,/``) must not raise from completion.

    Tab-completion fires on every keystroke. If the user types ``put ,/``
    (missed shift on ``./``), ``os.listdir(',/')`` raises FileNotFoundError.
    The helper must absorb that and just return no file suggestions —
    otherwise the traceback is logged on every subsequent keypress
    (see repl.CommandPrompt.complete which logger.exception's the raise).
    """
    monkeypatch.chdir(tmp_path)
    # Bare typo with trailing slash hits os.listdir(',/').
    assert complete_choices_filelist([], "put ,/", ",/") == []
    # Same shape, with a partial filename after the bad dir.
    assert complete_choices_filelist([], "put ,/foo", ",/foo") == []


def test_filelist_completion_survives_non_directory_in_path(tmp_path, monkeypatch):
    """A regular file used as a directory prefix must not raise."""
    monkeypatch.chdir(tmp_path)
    (tmp_path / "file.txt").write_text("")
    # ``file.txt/`` is syntactically a directory prefix but file.txt is a file.
    assert complete_choices_filelist([], "put file.txt/", "file.txt/") == []


def test_completer_swallows_underlying_exception(caplog):
    """A raising ``complete_<name>`` must not propagate into prompt_toolkit.

    Locks in PLAN.md Risk #1's safety net: the bound closure already
    logs the exception via ``logger.exception`` before re-raising, so
    the adapter only needs to absorb the re-raise. A leak would tear
    down the entire input loop on every keystroke.
    """
    p = _make_prompt()
    # Override the bound complete_quit with one that always raises.
    p.complete_quit = MagicMock(side_effect=RuntimeError("boom"))  # ty: ignore[unresolved-attribute]
    with caplog.at_level("DEBUG", logger="mtui.completer"):
        out = _completions(p, "quit r")
    assert out == []
    assert any("quit" in r.message for r in caplog.records)
