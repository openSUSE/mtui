"""Typed container for openQA / QEM Dashboard results stored on a TestReport.

This replaces the prior ``dict[str, Any]`` of the form
``{"auto": ..., "kernel": [...]}`` with a small dataclass so accessors are
statically typed and string-key typos are surfaced at definition time.

The :class:`OpenQAResult` Protocol describes the shared structural surface
of :class:`mtui.data_sources.openqa.standard.AutoOpenQA`,
:class:`mtui.data_sources.openqa.kernel.KernelOpenQA` and
:class:`mtui.connector.qem_dashboard.DashboardAutoOpenQA`. The latter does
not inherit from the ``OpenQA`` ABC -- a Protocol matches that duck-typed
reality without forcing a class hierarchy change.
"""

from dataclasses import dataclass, field
from typing import TYPE_CHECKING, Any, Protocol, runtime_checkable

if TYPE_CHECKING:
    from ..connector.oqa_search import (
        BuildCheckResult,
        GroupResult,
        VersionResult,
    )


@runtime_checkable
class OpenQAResult(Protocol):
    """Structural type implemented by all openQA result connectors.

    The concrete connectors (``AutoOpenQA``, ``KernelOpenQA``,
    ``DashboardAutoOpenQA``) all expose ``pp`` and ``results``, but
    with workflow-specific element types -- ``pp`` is a string for the
    auto/dashboard connectors and a list of strings for the kernel
    connector; ``results`` is a list of ``URLs`` for auto connectors
    and a list of ``Test`` for the kernel connector. The Protocol uses
    ``Any`` for these so existing call sites remain valid without a
    larger refactor of the consumer code.
    """

    kind: str
    pp: Any
    results: Any

    def run(self) -> "OpenQAResult": ...

    def __bool__(self) -> bool: ...


@dataclass
class OpenQAOverviewResult:
    """Structured payload produced by the ``openqa_overview`` command.

    Carries the three sections the upstream oqa-search script prints so
    other consumers (e.g. exporters) can render them without re-fetching.
    """

    single_incidents: list["VersionResult"] = field(default_factory=list)
    aggregated_updates: list["GroupResult"] = field(default_factory=list)
    build_checks: list["BuildCheckResult"] = field(default_factory=list)

    def __bool__(self) -> bool:
        """True if any of the three sections has content."""
        return bool(
            self.single_incidents or self.aggregated_updates or self.build_checks
        )


@dataclass
class OpenQAResults:
    """Typed container for openQA results attached to a ``TestReport``.

    Attributes:
        auto: The "auto" workflow result, sourced either from openQA
            directly (``AutoOpenQA``) or via the QEM Dashboard
            (``DashboardAutoOpenQA``). ``None`` until populated.
        kernel: The list of "kernel" workflow results. For kernel updates
            this typically contains two entries: a regular openQA instance
            result and a baremetal openQA instance result.
        overview: Output of the ``openqa_overview`` command (ported from
            oqa-search). ``None`` until the command is run.

    """

    auto: OpenQAResult | None = None
    kernel: list[OpenQAResult] = field(default_factory=list)
    overview: OpenQAOverviewResult | None = None

    def __bool__(self) -> bool:
        """True if any result is present and truthy."""
        return (
            bool(self.auto) or any(bool(k) for k in self.kernel) or bool(self.overview)
        )
