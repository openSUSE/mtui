"""Typed container for openQA / QEM Dashboard results stored on a TestReport.

This replaces the prior ``dict[str, Any]`` of the form
``{"auto": ..., "kernel": [...]}`` with a small dataclass so accessors are
statically typed and string-key typos are surfaced at definition time.

The :class:`OpenQAResult` Protocol describes the shared structural surface
of :class:`mtui.connector.openqa.standard.AutoOpenQA`,
:class:`mtui.connector.openqa.kernel.KernelOpenQA` and
:class:`mtui.connector.qem_dashboard.DashboardAutoOpenQA`. The latter does
not inherit from the ``OpenQA`` ABC -- a Protocol matches that duck-typed
reality without forcing a class hierarchy change.
"""

from dataclasses import dataclass, field
from typing import Any, Protocol, runtime_checkable


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
class OpenQAResults:
    """Typed container for openQA results attached to a ``TestReport``.

    Attributes:
        auto: The "auto" workflow result, sourced either from openQA
            directly (``AutoOpenQA``) or via the QEM Dashboard
            (``DashboardAutoOpenQA``). ``None`` until populated.
        kernel: The list of "kernel" workflow results. For kernel updates
            this typically contains two entries: a regular openQA instance
            result and a baremetal openQA instance result.

    """

    auto: OpenQAResult | None = None
    kernel: list[OpenQAResult] = field(default_factory=list)

    def __bool__(self) -> bool:
        """True if any result is present and truthy."""
        return bool(self.auto) or any(bool(k) for k in self.kernel)
