"""Cooperative cancellation for command bodies running in worker threads.

The MCP session runs blocking command bodies in :func:`asyncio.to_thread`
workers; cancelling the awaiting task (``job_cancel``, a client disconnect)
detaches the awaiter but cannot kill the thread. The session therefore binds a
:class:`threading.Event` here (contextvars propagate into ``to_thread``) and
sets it when the awaiter is cancelled, so long-running command bodies that
poll — e.g. ``request_review``'s Slack watch — can exit promptly instead of
running on unobserved to their own timeout.
"""

from __future__ import annotations

import threading
from contextvars import ContextVar

#: The cancellation event for the current command invocation, or ``None`` when
#: the caller provides no cancellation channel (the interactive REPL, tests).
current_cancel_event: ContextVar[threading.Event | None] = ContextVar(
    "current_cancel_event", default=None
)
