"""Cross-thread serialised user prompter.

Worker threads (e.g. SSH command runners spawned by
:func:`mtui.target.actions.run_parallel`) used to call :func:`input`
directly. With multiple targets racing for ``stdin`` the prompt text
got interleaved with sibling ``stdout`` writes and two workers could
attempt to read the same line of input.

:class:`Prompter` serialises those prompts behind a single
:class:`threading.Lock`. Only one worker reads ``stdin`` at a time;
the others queue politely on the lock until the current prompt
returns. The lock alone is enough because :func:`input` is the only
``stdin`` reader — there is no background dispatcher thread, no
``queue.Queue``, no module-level singleton.
"""

from __future__ import annotations

import threading
from collections.abc import Callable


def _default_password_reader(text: str) -> str:
    """Read a password masked, safe inside a prompt_toolkit REPL.

    A bare :func:`getpass.getpass` performs a raw terminal read that does
    not cooperate with the surrounding prompt_toolkit ``PromptSession``
    that drives the mtui REPL: the terminal is left in prompt_toolkit's
    half-configured state, so the prompt text is never rendered and the
    read appears to hang silently (the reported "silent wait" bug). Going
    through a fresh ``PromptSession`` saves and restores the terminal
    state correctly -- the same pattern :func:`mtui.cli.term._read_line`
    uses for visible prompts.

    The prompt text (``user@host's password: ``) is shown while the user
    types and the characters are masked with ``*``. ``erase_when_done``
    wipes the whole prompt line once Enter is pressed, so the terminal is
    not left with a leftover ``user@host's password: *********`` line --
    matching the clean behaviour of :func:`getpass.getpass`.
    """
    # Imported lazily so importing Prompter (e.g. under mtui-mcp, which
    # has no TTY) never drags in prompt_toolkit's terminal machinery.
    from prompt_toolkit import PromptSession
    from prompt_toolkit.history import InMemoryHistory

    session: PromptSession[str] = PromptSession(
        message=text,
        is_password=True,
        erase_when_done=True,
        history=InMemoryHistory(),
    )
    return session.prompt()


class Prompter:
    """Serialise interactive prompts across worker threads.

    The instance owns one lock. :meth:`ask` (visible input) and
    :meth:`ask_password` (masked input) both acquire it for the duration
    of the read and release it in a ``finally``, so callers from any
    thread are safe to call concurrently; they observe strictly
    sequential prompts in lock-acquisition order.

    Both readers are injectable to keep the class unit-testable without
    going through a real ``stdin`` / terminal.
    """

    __slots__ = ("_lock", "_password_reader", "_reader")

    def __init__(
        self,
        reader: Callable[[str], str] = input,
        password_reader: Callable[[str], str] = _default_password_reader,
    ) -> None:
        """Initialise the prompter.

        Args:
            reader: Callable invoked with the prompt text for visible
                input; must return the user's response. Defaults to
                :func:`input`.
            password_reader: Callable invoked with the prompt text for
                masked (non-echoing) input; must return the user's
                response. Defaults to a prompt_toolkit-backed reader that
                masks input and is safe to call between the REPL's own
                ``PromptSession`` reads.

        """
        self._lock = threading.Lock()
        self._reader = reader
        self._password_reader = password_reader

    def ask(self, text: str) -> str:
        """Prompt the user with ``text`` and return the typed response.

        Acquires the prompter's lock for the duration of the read so
        sibling workers cannot race for ``stdin``.

        Args:
            text: The prompt text shown to the user.

        Returns:
            The string the user typed (stripped of nothing; behaviour
            identical to :func:`input`).

        """
        with self._lock:
            return self._reader(text)

    def ask_password(self, text: str) -> str:
        """Prompt for a password with ``text``, returning the typed value.

        Like :meth:`ask`, the read is fenced by the prompter's lock so
        parallel workers cannot race for the terminal, but the typed
        characters are not echoed.

        Args:
            text: The prompt text shown to the user.

        Returns:
            The password the user typed.

        """
        with self._lock:
            return self._password_reader(text)
