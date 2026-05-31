"""Per-target reporting collaborator.

This module is the home of the seven sink-dispatch methods that used to
live directly on :class:`Target` (``report_self``, ``report_history``,
``report_locks``, ``report_timeout``, ``report_sessions``,
``report_log``, ``report_products``). They are short, related, and have
no business sharing namespace with the SSH-and-lock skeleton on
``Target``; extracting them keeps ``Target`` focused on connection
lifecycle and turns the reporting surface into one easy-to-discover
collaborator.

The ``Reporter`` instance keeps a live reference to its owning
:class:`Target`, so all dispatches read the most up-to-date values
(``out``, ``state``, ``mode``, ``connection.timeout``, ``_lock``, etc.)
at call time.

Method names drop the ``report_`` prefix — inside this class it is
redundant — and use ``self_`` (with a trailing underscore) for the
``report_self`` equivalent to avoid clashing with the ``self`` keyword.
"""

from collections.abc import Callable
from logging import getLogger
from typing import TYPE_CHECKING, final

from ...types import ExecutionMode, System, TargetState

if TYPE_CHECKING:
    from . import TargetLock
    from .target import Target

logger = getLogger("mtui.target.reporter")


@final
class Reporter:
    """Adapter that drives the seven status sinks for one :class:`Target`."""

    def __init__(self, target: "Target") -> None:
        """Bind the reporter to ``target``.

        The reference is intentionally a strong reference: ``Reporter``
        is created lazily by :attr:`Target.reporter` and discarded with
        the target.
        """
        self.target = target

    def self_(
        self,
        sink: Callable[[str, System, bool, TargetState | str, ExecutionMode], None],
    ) -> None:
        """Report ``(hostname, system, transactional, state, mode)`` to ``sink``."""
        t = self.target
        sink(t.hostname, t.system, t.transactional, t.state, t.mode)

    def history(self, sink: Callable[[str, System, list[str]], None]) -> None:
        """Report the parsed history lines (last stdout split on ``\\n``)."""
        t = self.target
        sink(t.hostname, t.system, t.lastout().split("\n"))

    def locks(self, sink: Callable[[str, System, "TargetLock"], None]) -> None:
        """Report the lock object to ``sink``."""
        t = self.target
        sink(t.hostname, t.system, t._lock)  # noqa: SLF001

    def timeout(self, sink: Callable[[str, System, int], None]) -> None:
        """Report the current connection timeout to ``sink``."""
        t = self.target
        sink(t.hostname, t.system, t.connection.timeout)

    def sessions(self, sink: Callable[[str, System, str], None]) -> None:
        """Report the last stdout (used for `who`-style session listings)."""
        t = self.target
        sink(t.hostname, t.system, t.lastout())

    def log(self, sink: Callable, arg) -> None:
        """Report the full host log to ``sink``.

        ``arg`` is a caller-provided extra (typically an output
        accumulator); forwarded verbatim as the third positional arg.
        """
        t = self.target
        sink(t.hostname, t.out, arg)

    def products(self, sink: Callable[[str, System], None]) -> None:
        """Report ``(hostname, system)`` to ``sink``."""
        t = self.target
        sink(t.hostname, t.system)
