"""prompt_toolkit ``Lexer`` for the mtui REPL input line.

Colors the input buffer to give a quick visual signal about what the
user has typed:

* The **first token** (the command name) is rendered green when it
  matches a registered command and red otherwise. This makes typos and
  out-of-context commands visible before pressing Enter.
* Subsequent tokens starting with ``-`` (short flags) or ``--`` (long
  flags) are rendered cyan, mirroring how shells and most CLIs visually
  separate options from positional arguments.
* Everything else uses the terminal default color.

The lexer holds a live reference to a :class:`~mtui.cli.repl.CommandPrompt`
rather than a snapshot of the command set, so commands registered after
construction (the same dynamic-registration affordance used by
:class:`~mtui.cli._completer.MtuiCompleter`) are picked up on the next
keystroke without re-binding the session.

Style tokens are namespaced under ``command.*`` and ``flag`` and are
resolved by the :class:`~prompt_toolkit.styles.Style` instance assembled
in :mod:`mtui.cli.repl`. No Pygments dependency is introduced.
"""

from __future__ import annotations

from typing import TYPE_CHECKING

from prompt_toolkit.lexers import Lexer

if TYPE_CHECKING:
    from collections.abc import Callable

    from prompt_toolkit.document import Document
    from prompt_toolkit.formatted_text.base import StyleAndTextTuples

    from .repl import CommandPrompt


_STYLE_KNOWN = "class:command.known"
_STYLE_UNKNOWN = "class:command.unknown"
_STYLE_FLAG = "class:flag"
_STYLE_DEFAULT = ""


def _tokenize(line: str) -> list[tuple[str, str]]:
    """Split ``line`` into ``(kind, text)`` chunks preserving whitespace.

    Kinds:

    * ``"ws"``  — a run of whitespace characters (spaces or tabs).
    * ``"tok"`` — a run of non-whitespace characters.

    Whitespace is preserved verbatim so the styled output round-trips
    the user's input exactly; prompt_toolkit re-renders the line from
    the lexer's ``(style, text)`` tuples and any character we drop would
    visually shift the cursor.
    """
    chunks: list[tuple[str, str]] = []
    i = 0
    n = len(line)
    while i < n:
        ch = line[i]
        if ch in (" ", "\t"):
            j = i
            while j < n and line[j] in (" ", "\t"):
                j += 1
            chunks.append(("ws", line[i:j]))
            i = j
        else:
            j = i
            while j < n and line[j] not in (" ", "\t"):
                j += 1
            chunks.append(("tok", line[i:j]))
            i = j
    return chunks


class MtuiCommandLexer(Lexer):
    """Color the first token by command-known-ness and flag tokens cyan.

    Bound to a :class:`~mtui.cli.repl.CommandPrompt` so live changes to
    the registered command set (``_add_subcommand`` at any point) are
    reflected on the next keystroke.
    """

    def __init__(self, prompt: CommandPrompt) -> None:
        """Bind to a live :class:`CommandPrompt`.

        Args:
            prompt: the REPL instance whose ``commands`` dict drives
                first-token coloring.

        """
        self._prompt = prompt

    def lex_document(
        self,
        document: Document,
    ) -> Callable[[int], StyleAndTextTuples]:
        """Return a per-line callable that yields styled chunks.

        prompt_toolkit calls the returned callable once per visible
        line. mtui input is conceptually single-line (the REPL doesn't
        support multi-line buffers), so any ``lineno`` past the buffer
        height returns an empty list.

        Args:
            document: the current buffer the user is editing.

        Returns:
            A callable mapping ``lineno → list[(style, text)]``.

        """
        lines = document.lines

        def get_line(lineno: int) -> StyleAndTextTuples:
            if lineno < 0 or lineno >= len(lines):
                return []
            return self._style_line(lines[lineno])

        return get_line

    def _style_line(self, line: str) -> StyleAndTextTuples:
        """Translate ``line`` into ``(style, text)`` tuples.

        Algorithm:

        1. Tokenize into alternating whitespace / non-whitespace chunks.
        2. The first non-whitespace chunk is the command name: green
           when it's in ``self._prompt.commands``, red otherwise.
        3. Later non-whitespace chunks starting with ``-`` are flags
           (cyan); other tokens (positional arguments, values) use the
           default style.
        4. Whitespace chunks use the default style and are preserved
           verbatim so cursor positioning stays consistent.

        Empty lines yield an empty list, matching prompt_toolkit's
        convention for "nothing to render here".
        """
        if not line:
            return []

        chunks = _tokenize(line)
        styled: StyleAndTextTuples = []
        saw_command = False

        for kind, text in chunks:
            if kind == "ws":
                styled.append((_STYLE_DEFAULT, text))
                continue

            if not saw_command:
                style = (
                    _STYLE_KNOWN if text in self._prompt.commands else _STYLE_UNKNOWN
                )
                styled.append((style, text))
                saw_command = True
            elif text.startswith("-"):
                styled.append((_STYLE_FLAG, text))
            else:
                styled.append((_STYLE_DEFAULT, text))

        return styled
