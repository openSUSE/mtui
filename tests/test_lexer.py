"""Tests for :class:`mtui.cli._lexer.MtuiCommandLexer`.

The lexer is wired into :class:`~mtui.cli.repl.CommandPrompt`'s
:class:`PromptSession` to colour input as the user types. These tests
exercise the styling rules directly against a synthesised
:class:`~prompt_toolkit.document.Document` so a regression in the
classifier is caught without spinning up the full session.

Coverage matrix:

* known vs unknown first token (green vs red),
* short flag (``-x``) and long flag (``--foo``) get the flag style,
* whitespace is preserved verbatim,
* positional arguments use the default style,
* empty buffer / out-of-range lineno yield ``[]``,
* dynamic registration on the bound prompt is reflected on the next
  call (no stale snapshot), and
* leading whitespace before the command does not break first-token
  detection.
"""

from __future__ import annotations

from unittest.mock import MagicMock

from prompt_toolkit.document import Document
from prompt_toolkit.formatted_text.base import StyleAndTextTuples

from mtui.cli._lexer import (
    _STYLE_DEFAULT,
    _STYLE_FLAG,
    _STYLE_KNOWN,
    _STYLE_UNKNOWN,
    MtuiCommandLexer,
    _tokenize,
)


def _make_prompt(commands: list[str]) -> MagicMock:
    """Build a stand-in :class:`CommandPrompt` with a ``commands`` dict."""
    p = MagicMock()
    # Use a real dict so ``in`` works without configuring a mock spec.
    p.commands = {name: MagicMock() for name in commands}
    return p


def _style(lexer: MtuiCommandLexer, line: str) -> StyleAndTextTuples:
    """Render ``line`` through ``lexer`` and return the styled tuples.

    The return type mirrors the ``Lexer`` protocol's per-line shape
    (``StyleAndTextTuples`` — a list whose elements are either 2-tuples
    of ``(style, text)`` or 3-tuples that bind a mouse handler). The
    lexer never emits the 3-tuple variant, but matching the protocol
    keeps ``ty`` honest without forcing a narrowing assertion in every
    test.
    """
    doc = Document(line)
    get_line = lexer.lex_document(doc)
    return list(get_line(0))


# --------------------------------------------------------------------------- #
# _tokenize                                                                   #
# --------------------------------------------------------------------------- #


def test_tokenize_empty_returns_empty_list():
    assert _tokenize("") == []


def test_tokenize_single_token():
    assert _tokenize("quit") == [("tok", "quit")]


def test_tokenize_preserves_whitespace_runs():
    assert _tokenize("  run    -t  host  ") == [
        ("ws", "  "),
        ("tok", "run"),
        ("ws", "    "),
        ("tok", "-t"),
        ("ws", "  "),
        ("tok", "host"),
        ("ws", "  "),
    ]


def test_tokenize_tab_is_whitespace():
    assert _tokenize("a\tb") == [("tok", "a"), ("ws", "\t"), ("tok", "b")]


# --------------------------------------------------------------------------- #
# First-token classification                                                  #
# --------------------------------------------------------------------------- #


def test_known_command_is_green():
    lexer = MtuiCommandLexer(_make_prompt(["quit"]))
    assert _style(lexer, "quit") == [(_STYLE_KNOWN, "quit")]


def test_unknown_command_is_red():
    lexer = MtuiCommandLexer(_make_prompt(["quit"]))
    assert _style(lexer, "nope") == [(_STYLE_UNKNOWN, "nope")]


def test_leading_whitespace_does_not_misclassify_first_token():
    """Whitespace before the command still leaves ``run`` as the first token."""
    lexer = MtuiCommandLexer(_make_prompt(["run"]))
    assert _style(lexer, "  run") == [
        (_STYLE_DEFAULT, "  "),
        (_STYLE_KNOWN, "run"),
    ]


def test_dash_in_command_name_round_trips():
    """Names containing ``-`` (e.g. ``dash-cmd``) still match the command set."""
    lexer = MtuiCommandLexer(_make_prompt(["dash-cmd"]))
    assert _style(lexer, "dash-cmd") == [(_STYLE_KNOWN, "dash-cmd")]


# --------------------------------------------------------------------------- #
# Flag highlighting                                                           #
# --------------------------------------------------------------------------- #


def test_short_flag_gets_flag_style():
    lexer = MtuiCommandLexer(_make_prompt(["run"]))
    assert _style(lexer, "run -t host") == [
        (_STYLE_KNOWN, "run"),
        (_STYLE_DEFAULT, " "),
        (_STYLE_FLAG, "-t"),
        (_STYLE_DEFAULT, " "),
        (_STYLE_DEFAULT, "host"),
    ]


def test_long_flag_gets_flag_style():
    lexer = MtuiCommandLexer(_make_prompt(["edit"]))
    assert _style(lexer, "edit --foo bar") == [
        (_STYLE_KNOWN, "edit"),
        (_STYLE_DEFAULT, " "),
        (_STYLE_FLAG, "--foo"),
        (_STYLE_DEFAULT, " "),
        (_STYLE_DEFAULT, "bar"),
    ]


def test_command_name_starting_with_dash_in_argument_position_is_flag():
    """A non-first token starting with ``-`` is always a flag, even if it shadows a command name."""
    lexer = MtuiCommandLexer(_make_prompt(["run", "-weird"]))
    out = _style(lexer, "run -weird")
    assert out == [
        (_STYLE_KNOWN, "run"),
        (_STYLE_DEFAULT, " "),
        (_STYLE_FLAG, "-weird"),
    ]


def test_positional_argument_uses_default_style():
    lexer = MtuiCommandLexer(_make_prompt(["run"]))
    out = _style(lexer, "run host1 host2")
    assert out == [
        (_STYLE_KNOWN, "run"),
        (_STYLE_DEFAULT, " "),
        (_STYLE_DEFAULT, "host1"),
        (_STYLE_DEFAULT, " "),
        (_STYLE_DEFAULT, "host2"),
    ]


# --------------------------------------------------------------------------- #
# Edge cases                                                                  #
# --------------------------------------------------------------------------- #


def test_empty_line_returns_empty_list():
    lexer = MtuiCommandLexer(_make_prompt(["quit"]))
    assert _style(lexer, "") == []


def test_out_of_range_lineno_returns_empty_list():
    lexer = MtuiCommandLexer(_make_prompt(["quit"]))
    doc = Document("quit")
    get_line = lexer.lex_document(doc)
    assert get_line(5) == []
    assert get_line(-1) == []


def test_whitespace_only_line_yields_one_default_chunk():
    lexer = MtuiCommandLexer(_make_prompt(["quit"]))
    assert _style(lexer, "   ") == [(_STYLE_DEFAULT, "   ")]


def test_dynamic_command_registration_picked_up_on_next_call():
    """The lexer reads ``self._prompt.commands`` lazily — late registrations count."""
    p = _make_prompt(["quit"])
    lexer = MtuiCommandLexer(p)
    # First pass: ``run`` is unknown.
    assert _style(lexer, "run") == [(_STYLE_UNKNOWN, "run")]
    # Register ``run`` after the lexer was constructed.
    p.commands["run"] = MagicMock()
    # Second pass: ``run`` is now known without rebuilding the lexer.
    assert _style(lexer, "run") == [(_STYLE_KNOWN, "run")]


def test_trailing_whitespace_preserved():
    lexer = MtuiCommandLexer(_make_prompt(["quit"]))
    assert _style(lexer, "quit   ") == [
        (_STYLE_KNOWN, "quit"),
        (_STYLE_DEFAULT, "   "),
    ]
