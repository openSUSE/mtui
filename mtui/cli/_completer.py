"""prompt_toolkit ``Completer`` adapter for the mtui REPL.

Bridges :class:`prompt_toolkit.completion.Completer` to the historical
``cmd.Cmd``-style ``complete(state, text, line, begidx, endidx)`` methods
defined by every :class:`mtui.commands._command.Command` subclass and
already wrapped, per instance, by
:meth:`mtui.cli.repl.CommandPrompt._add_subcommand` as
``complete_<name>(text, line, begidx, endidx)`` closures.

Adapter strategy (see PLAN.md decision 1): translate the prompt_toolkit
:class:`~prompt_toolkit.document.Document` into the legacy
``(text, line, begidx, endidx)`` tuple, dispatch to the bound
``complete_<name>`` closure on the :class:`~mtui.cli.repl.CommandPrompt`
instance (so the closure's injected ``state`` dict — ``hosts``,
``metadata``, ``config`` — is preserved), and re-emit each returned
string as a :class:`~prompt_toolkit.completion.Completion` anchored on
the partial word.

First-token (command name) completion is handled directly in this module
by matching against the registered command name set;
``cmd.Cmd.complete`` does the same thing via ``completenames`` and there
is no per-command hook to delegate to.
"""

from __future__ import annotations

from logging import getLogger
from typing import TYPE_CHECKING

from prompt_toolkit.completion import Completer, Completion

if TYPE_CHECKING:
    from collections.abc import Iterable

    from prompt_toolkit.completion import CompleteEvent
    from prompt_toolkit.document import Document

    from .repl import CommandPrompt

logger = getLogger("mtui.completer")


def _split_text_word(line: str) -> tuple[str, int]:
    """Split ``line`` into ``(word_before_cursor, begidx)``.

    Mirrors :meth:`cmd.Cmd.complete`'s contract: ``text`` is the
    contiguous non-whitespace tail of the input before the cursor,
    ``begidx`` is the offset where that tail starts. When the cursor
    sits right after whitespace (e.g. ``"run -t "``) ``text`` is empty
    and ``begidx`` equals ``len(line)`` — the command's completer is
    still invoked, matching legacy behaviour.

    Args:
        line: the buffer up to (not including) the cursor, already left-
            stripped by the caller so column offsets line up with what
            :meth:`cmd.Cmd.complete` would have computed.

    Returns:
        ``(text, begidx)``.

    """
    # Walk back from the end skipping any trailing whitespace is *not*
    # what cmd.Cmd does — it keeps the cursor where it is and lets the
    # tail simply be empty after a space. So: find the last whitespace
    # in `line`; everything after it is the partial word.
    if not line:
        return "", 0
    # rfind returns -1 when no whitespace is present; +1 then maps to 0.
    last_ws = max(line.rfind(" "), line.rfind("\t"))
    begidx = last_ws + 1
    return line[begidx:], begidx


class MtuiCompleter(Completer):
    """``prompt_toolkit`` completer that defers to per-command callbacks.

    Holds a reference to a :class:`~mtui.cli.repl.CommandPrompt` rather
    than a static command map so that:

    * dynamic registration (``_add_subcommand`` at any later point) is
      picked up automatically — the next keystroke walks the live
      ``prompt.commands`` dict;
    * the instance-bound ``complete_<name>`` closures are reachable via
      :func:`getattr`, which is what carries the injected ``state`` dict
      into the underlying command's static ``complete`` method.
    """

    def __init__(self, prompt: CommandPrompt) -> None:
        """Bind to a live :class:`CommandPrompt`.

        Args:
            prompt: the REPL instance whose ``commands`` dict and
                ``complete_<name>`` closures back this completer.

        """
        self._prompt = prompt

    def get_completions(
        self,
        document: Document,
        complete_event: CompleteEvent,
    ) -> Iterable[Completion]:
        """Yield :class:`Completion` objects for ``document``.

        Translation rules (cmd.Cmd parity):

        * ``origline`` = ``document.text_before_cursor`` (the buffer up
          to the cursor; what ``readline.get_line_buffer()`` returned
          for ``cmd.Cmd``).
        * ``line``     = ``origline.lstrip()`` — cmd.Cmd strips leading
          whitespace before computing offsets.
        * ``endidx``   = ``len(line)`` — cursor column relative to the
          stripped line.
        * ``text, begidx`` = :func:`_split_text_word(line)`.

        Dispatch rules:

        * ``begidx == 0`` → completing the first token: match against
          registered command names (case-sensitive, prefix match) —
          mirrors :meth:`cmd.Cmd.completenames`.
        * otherwise → look up the bound ``complete_<first_token>``
          closure on the prompt instance; if missing, yield nothing.

        Errors from the underlying completer are swallowed (logged at
        debug) so a buggy command cannot crash the input loop. The
        closure already escalates via ``logger.exception`` before
        re-raising, so the trace is captured exactly once.
        """
        origline = document.text_before_cursor
        stripped = len(origline) - len(origline.lstrip())
        line = origline[stripped:]
        endidx = len(line)
        text, begidx = _split_text_word(line)

        if begidx == 0:
            # First-token completion: match command names.
            for name in self._prompt.commands:
                if name.startswith(text):
                    yield Completion(name, start_position=-len(text))
            return

        first_token = line.split(" ", 1)[0]
        complete = getattr(self._prompt, f"complete_{first_token}", None)
        if complete is None:
            return

        try:
            matches = complete(text, line, begidx, endidx)
        except Exception as e:
            # The bound closure has already logged via logger.exception
            # before re-raising; swallow here so completion failures do
            # not propagate into prompt_toolkit and tear down the REPL.
            logger.debug("completer for %r raised: %s", first_token, e)
            return

        if not matches:
            return
        for match in matches:
            yield Completion(match, start_position=-len(text))
