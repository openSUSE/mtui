"""Process-global arbitration of reference hosts across loaded templates.

When several templates are loaded in one process (one REPL, or several MCP
sessions sharing an interpreter) and fan-out connects them concurrently, the
remote ``/var/lock/mtui.lock`` cannot keep two templates off the same shared
host: the lock is keyed on ``(user, pid)`` and every same-process template
shares the pid, so :meth:`~mtui.hosts.target.locks.TargetLock.is_mine` is
``True`` for all of them (RFC §5.7).

:class:`HostArbiter` is the in-process authority that closes that gap. It is a
thread-safe map ``hostname -> owner-key`` where the owner-key is the composite
``(registry_id, RRID)`` so that two MCP sessions loading the **same** RRID are
still distinct owners. A claim that finds every candidate held queues on a
:class:`threading.Condition` until one is released, reaped, or the wait budget
(``[lock] wait``) expires.

The arbiter dedups *claims* (which template gets which host), not *connections*:
each template still owns its own :class:`~mtui.hosts.target.target.Target` and
SSH session. A single process-global instance is shared by every
:class:`~mtui.template_registry.TemplateRegistry`; obtain it via
:func:`get_arbiter`.
"""

from __future__ import annotations

import threading
import time
from logging import getLogger

logger = getLogger("mtui.host_arbiter")

#: Owner key: ``(registry_id, rrid)``.
Owner = tuple[str, str]


class HostArbiter:
    """Thread-safe ``hostname -> owner`` map with a wait queue.

    One instance per process, shared across registries. All public methods
    are safe to call from the worker threads ``connect_targets`` fans out
    across.
    """

    def __init__(self) -> None:
        """Initialise an empty arbiter."""
        self._owner: dict[str, Owner] = {}
        self._cond = threading.Condition(threading.Lock())

    def try_acquire(self, host: str, owner: Owner) -> bool:
        """Claim ``host`` for ``owner`` if it is free or already ours.

        Returns:
            ``True`` if ``owner`` now holds ``host``; ``False`` if another
            owner holds it.

        """
        with self._cond:
            held = self._owner.get(host)
            if held is None or held == owner:
                self._owner[host] = owner
                return True
            return False

    def acquire_any(
        self,
        candidates: list[str],
        owner: Owner,
        wait: int = 0,
        poll: int = 15,
    ) -> str | None:
        """Claim one free host from ``candidates`` for ``owner``.

        Tries each candidate in order. If all are held by other owners and
        ``wait > 0``, blocks on the condition until a candidate is released
        (or ``poll`` seconds elapse, whichever is sooner) and retries, up to
        ``wait`` seconds total.

        Args:
            candidates: Ordered hostnames to try (e.g. the hosts in one
                test-target slot).
            owner: The claiming ``(registry_id, rrid)``.
            wait: Total seconds to queue when all candidates are busy;
                ``<= 0`` fails fast.
            poll: Max seconds to block per wake-up while queueing.

        Returns:
            The claimed hostname, or ``None`` if none could be claimed
            within the wait budget.

        """
        if not candidates:
            return None
        deadline = time.monotonic() + wait if wait > 0 else None
        warned = False
        with self._cond:
            while True:
                for host in candidates:
                    held = self._owner.get(host)
                    if held is None or held == owner:
                        self._owner[host] = owner
                        return host
                if deadline is None:
                    return None
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    logger.warning(
                        "host pool exhausted for %s; gave up after %ds",
                        owner,
                        wait,
                    )
                    return None
                if not warned:
                    logger.warning(
                        "host pool busy for %s; waiting up to %ds for a free host",
                        owner,
                        wait,
                    )
                    warned = True
                self._cond.wait(timeout=min(poll, remaining))

    def owner_of(self, host: str) -> Owner | None:
        """Return the owner currently holding ``host``, or ``None``."""
        with self._cond:
            return self._owner.get(host)

    def release(self, host: str, owner: Owner) -> None:
        """Release ``host`` if held by ``owner`` and wake any waiters."""
        with self._cond:
            if self._owner.get(host) == owner:
                del self._owner[host]
                self._cond.notify_all()

    def release_owner(self, owner: Owner) -> None:
        """Release every host held by ``owner`` and wake any waiters."""
        with self._cond:
            freed = [h for h, o in self._owner.items() if o == owner]
            for h in freed:
                del self._owner[h]
            if freed:
                self._cond.notify_all()


_ARBITER: HostArbiter | None = None
_ARBITER_LOCK = threading.Lock()


def get_arbiter() -> HostArbiter:
    """Return the process-global :class:`HostArbiter` (created on first use)."""
    global _ARBITER
    if _ARBITER is None:
        with _ARBITER_LOCK:
            if _ARBITER is None:
                _ARBITER = HostArbiter()
    return _ARBITER
