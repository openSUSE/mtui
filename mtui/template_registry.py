"""Owner of the collection of loaded templates plus an active pointer.

The :class:`TemplateRegistry` replaces the historical scalar
``prompt.metadata`` / ``prompt.targets`` state with a keyed collection of
:class:`~mtui.test_reports.testreport.TestReport` instances and an "active"
pointer. In this phase the registry always holds at most one entry and is
exposed through read-only ``metadata`` / ``targets`` properties on
``CommandPrompt`` and ``McpSession``, so command bodies and the test suite keep
working unchanged.

A stable per-instance :attr:`TemplateRegistry.id` is established here; it is the
owner-key seed the later host-arbitration work (RFC §5.7) keys on as
``(registry.id, RRID)``.
"""

from __future__ import annotations

import contextlib
from collections.abc import Callable
from typing import TYPE_CHECKING
from uuid import uuid4

if TYPE_CHECKING:
    from .support.config import Config
    from .test_reports.testreport import TestReport


class TemplateRegistry:
    """Holds the loaded templates and tracks the active one.

    The registry keys live :class:`TestReport` instances by their RRID
    (``str(report.id)``). A single :class:`NullTestReport` is held as the
    fallback so :attr:`active` never returns ``None`` and is never inserted
    into the keyed collection (its ``id`` is the empty string).
    """

    def __init__(
        self,
        config: Config,
        *,
        null_factory: Callable[[], TestReport],
    ) -> None:
        """Initialise an empty registry.

        Args:
            config: The application configuration (kept for parity with the
                rest of the runtime and for future arbiter wiring).
            null_factory: Zero-argument callable returning a fresh
                :class:`NullTestReport`; used as the active-pointer fallback
                when no template is loaded.

        """
        self.config = config
        #: Stable per-registry identity; the owner-key seed for host
        #: arbitration (RFC §5.7). One registry per REPL process, one per MCP
        #: session.
        self.id: str = uuid4().hex
        self._null: TestReport = null_factory()
        self._entries: dict[str, TestReport] = {}
        self._active: str | None = None

    def add(self, report: TestReport) -> None:
        """Insert (or replace) ``report`` keyed by its RRID.

        The first template added becomes active. Re-adding an existing RRID
        replaces the stored report but does not change the active pointer.
        """
        rrid = str(report.id)
        self._entries[rrid] = report
        if self._active is None:
            self._active = rrid

    def remove(self, rrid: str) -> None:
        """Drop ``rrid`` from the registry, closing its host connections.

        Mirrors the established per-:class:`Target` teardown pattern. If the
        removed template was active, the next remaining entry (insertion
        order) becomes active, or ``None`` when the registry empties.
        """
        report = self._entries.pop(rrid)
        targets = report.targets
        for name in list(targets):
            # Best-effort teardown: one wedged connection must not block
            # reaping the rest (mirrors McpSession._disconnect_targets).
            with contextlib.suppress(Exception):
                targets[name].close()
        targets.clear()
        if self._active == rrid:
            self._active = next(iter(self._entries), None)

    def get(self, rrid: str) -> TestReport:
        """Return the loaded report for ``rrid`` (raises ``KeyError`` if absent)."""
        return self._entries[rrid]

    @property
    def active(self) -> TestReport:
        """The active report, or the :class:`NullTestReport` fallback."""
        if self._active is None:
            return self._null
        return self._entries[self._active]

    def set_active(self, rrid: str) -> None:
        """Make ``rrid`` the active template (raises ``KeyError`` if absent)."""
        if rrid not in self._entries:
            raise KeyError(rrid)
        self._active = rrid

    def all(self) -> list[TestReport]:
        """Return every loaded report in insertion order (for fan-out)."""
        return list(self._entries.values())

    def rrids(self) -> list[str]:
        """Return every loaded RRID in insertion order (for completion)."""
        return list(self._entries.keys())

    def __bool__(self) -> bool:
        """``True`` when at least one (non-null) template is loaded."""
        return bool(self._entries)

    def __len__(self) -> int:
        """Number of loaded templates."""
        return len(self._entries)

    def __contains__(self, rrid: object) -> bool:
        """Membership test by RRID."""
        return rrid in self._entries
