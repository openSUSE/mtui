"""Process-global, workspace-aware refhost ownership for ``mtui-mcp``.

Within ONE ``mtui-mcp`` process every named workspace shares the OS pid, so the
remote ``/var/lock/mtui.lock`` (keyed on user+pid) cannot tell two workspaces
apart — both see a lock they took as "mine". The :class:`HostArbiter` is the
in-process counterpart that makes a refhost **pool** usable from a single
client: it records which workspace currently owns which refhost so two
workspaces never drive the same pool host, and lets a workspace **queue** for a
busy pool host (block until another workspace releases it). The remote lock
still arbitrates against *other* processes / users.

One instance is created by :class:`mtui.mcp.registry.SessionRegistry` and shared
by every session it mints; each session is the ``owner`` identified by its
registry key. The arbiter is thread-safe: refhost connect/claim runs in
``asyncio.to_thread`` worker threads, so a :class:`threading.Condition` guards
the ownership map and powers the queue.
"""

from __future__ import annotations

import threading
import time
from collections.abc import Iterable
from logging import getLogger

logger = getLogger("mtui.mcp.host_arbiter")


class HostArbiter:
    """In-process map of refhost -> owning workspace, with a wait queue."""

    def __init__(self) -> None:
        """Initialise an empty ownership map and its condition variable."""
        self._owner: dict[str, str] = {}
        self._cv = threading.Condition()

    def owner_of(self, host: str) -> str | None:
        """Return the workspace owning ``host`` in this process, or ``None``."""
        with self._cv:
            return self._owner.get(host)

    def claim(self, host: str, owner: str) -> bool:
        """Claim ``host`` for ``owner`` if free (or already ours).

        Returns ``True`` when ``owner`` now holds it, ``False`` when another
        workspace in this process already does (caller should pick another).
        """
        with self._cv:
            cur = self._owner.get(host)
            if cur is not None and cur != owner:
                return False
            self._owner[host] = owner
            return True

    def release(self, host: str, owner: str) -> None:
        """Release ``host`` if owned by ``owner`` and wake any waiters."""
        with self._cv:
            if self._owner.get(host) == owner:
                del self._owner[host]
                self._cv.notify_all()

    def release_owner(self, owner: str) -> None:
        """Release every host held by ``owner`` (workspace close) and wake waiters."""
        with self._cv:
            freed = [h for h, o in self._owner.items() if o == owner]
            for h in freed:
                del self._owner[h]
            if freed:
                logger.debug("released %d host(s) held by %s", len(freed), owner)
                self._cv.notify_all()

    def acquire_any(
        self,
        candidates: Iterable[str],
        owner: str,
        *,
        timeout: float = 0.0,
        poll: float = 5.0,
    ) -> str | None:
        """Reserve and return one candidate that is free in this process.

        Scans ``candidates`` in order; a host that is unowned, or already owned
        by ``owner``, is reserved for ``owner`` and returned. If every candidate
        is currently held by *other* workspaces, waits (queues) until one is
        released or ``timeout`` seconds elapse. ``timeout <= 0`` means do not
        wait — return ``None`` immediately when nothing is free.

        Args:
            candidates: Hostnames to choose from, in preference order.
            owner: The requesting workspace key.
            timeout: Max seconds to queue for a free host.
            poll: Re-check cadence while waiting (a wake also re-checks).

        Returns:
            A hostname now owned by ``owner``, or ``None`` on timeout/empty.

        """
        cands = list(candidates)
        deadline = time.monotonic() + max(0.0, timeout)
        with self._cv:
            while True:
                for h in cands:
                    cur = self._owner.get(h)
                    if cur is None or cur == owner:
                        self._owner[h] = owner
                        return h
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    return None
                self._cv.wait(min(poll, remaining))
