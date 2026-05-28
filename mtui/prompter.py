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


class Prompter:
    """Serialise interactive :func:`input` calls across worker threads.

    The instance owns one lock. :meth:`ask` acquires it, calls the
    injected reader (default :func:`input`), releases the lock in a
    ``finally``. Callers from any thread are safe to call concurrently;
    they observe strictly sequential prompts in lock-acquisition order.

    The reader is injectable to keep the class unit-testable without
    going through real ``stdin``.
    """

    __slots__ = ("_lock", "_reader")

    def __init__(self, reader: Callable[[str], str] = input) -> None:
        """Initialise the prompter.

        Args:
            reader: Callable invoked with the prompt text; must return
                the user's response. Defaults to :func:`input`.

        """
        self._lock = threading.Lock()
        self._reader = reader

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
