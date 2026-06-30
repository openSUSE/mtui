"""Concurrency helpers shared across MTUI.

Currently a single export: :class:`ContextExecutor`, a drop-in
:class:`concurrent.futures.ThreadPoolExecutor` that propagates the
caller's :mod:`contextvars` context into every worker thread.

A plain ``ThreadPoolExecutor`` does **not** copy context into its
workers (unlike :func:`asyncio.to_thread`), so any
:class:`contextvars.ContextVar` set on the submitting thread reads back
its default inside a pool task. MTUI relies on context propagation so a
per-call marker (set once at the top of a command) stays visible in the
worker threads that command fans out to -- most importantly the
``mtui-mcp`` log-capture token (see
:class:`mtui.mcp.session._LogCaptureHandler`), which lets the MCP layer
attribute a worker thread's log records to the originating tool call.

Routing MTUI's thread pools through this class instead of the bare
``ThreadPoolExecutor`` keeps that behaviour uniform across every command
and every transport (stdio / http / REPL); code that does not set any
context var is unaffected (copying an unmodified context is cheap and
side-effect free).

When constructed without ``max_workers`` (the common case for MTUI's
HTTP fan-out), this inherits ``ThreadPoolExecutor``'s default of
``min(32, cpu + 4)`` workers. :func:`mtui.support.http.default_pool_size`
mirrors that same formula to size the shared ``requests`` connection
pool, so a fan-out of workers hitting one host has one cached connection
per worker and never triggers ``urllib3``'s pool-full connection churn.
"""

from collections.abc import Callable
from concurrent.futures import Future, ThreadPoolExecutor
from contextvars import copy_context
from typing import Any, TypeVar, override

_T = TypeVar("_T")


class ContextExecutor(ThreadPoolExecutor):
    """A ``ThreadPoolExecutor`` that runs tasks in a copy of the caller's context.

    Each :meth:`submit` snapshots the current :mod:`contextvars` context
    at call time and runs the task inside that copy on the worker thread,
    so :class:`~contextvars.ContextVar` values set by the submitter are
    visible to the task (and to anything it logs). The copy is taken per
    submit, matching the semantics of :func:`asyncio.to_thread`.
    """

    @override
    def submit(self, fn: Callable[..., _T], /, *args: Any, **kwargs: Any) -> Future[_T]:
        """Submit ``fn`` to run inside a copy of the caller's context.

        Args:
            fn: The callable to execute on a worker thread.
            *args: Positional arguments forwarded to ``fn``.
            **kwargs: Keyword arguments forwarded to ``fn``.

        Returns:
            A :class:`concurrent.futures.Future` for the call.

        """
        ctx = copy_context()
        return super().submit(lambda: ctx.run(fn, *args, **kwargs))
