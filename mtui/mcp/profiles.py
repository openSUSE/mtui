"""Selectable tool *profiles* for the ``mtui-mcp`` server.

The server synthesises ~66 command tools plus the testreport and job tools. The
full set is sent to the model on every request, which is the dominant fixed token
cost of an MCP session. Many of those tools (``set_log_level``, ``lrun``,
``reload_*``, ``config_*``, host-bookkeeping verbs) are rarely needed in a normal
maintenance-test workflow.

A *profile* is a named allow-set of tool names. The ``full`` profile is a no-op
(every registered tool stays). The ``core`` profile keeps only the curated
everyday subset below, removing the rest from the live SDK tool table so they
never reach the wire. An operator selects a profile with ``[mcp] tool_profile``
and can fine-tune with ``[mcp] tools_allow`` / ``[mcp] tools_deny`` (see
:func:`apply_profile`).

The default is ``full`` so existing deployments are unchanged; slimming the tool
surface is strictly opt-in.
"""

from __future__ import annotations

from logging import getLogger
from typing import TYPE_CHECKING

if TYPE_CHECKING:
    from mcp.server.fastmcp import FastMCP

logger = getLogger("mtui.mcp.profiles")

#: The curated everyday tool set exposed under ``tool_profile = core``. Chosen to
#: cover load → inspect → run/install → fill report → approve/reject without the
#: long tail of host-bookkeeping and server-tuning verbs. The hand-written
#: ``testreport_*`` and ``job_*`` tools are always part of core because the slow
#: background-command flow and report editing depend on them.
CORE: frozenset[str] = frozenset(
    {
        # load / inspect
        "load_template",
        "unload",
        "list_templates",
        "list_metadata",
        "list_bugs",
        "list_packages",
        "list_products",
        "list_versions",
        "list_hosts",
        "updates",
        "show_diff",
        "show_log",
        "analyze_diff",
        # act
        "assign",
        "run",
        "update",
        "install",
        "uninstall",
        "prepare",
        "set_repo",
        # report lifecycle
        "export",
        "commit",
        "comment",
        "request_review",
        "approve",
        "reject",
        # openQA
        "openqa_overview",
        "openqa_jobs",
        # hand-written tools (always kept)
        "testreport_read",
        "testreport_logs",
        "testreport_patch",
        "testreport_write",
        "testreport_fill",
        "job_list",
        "job_status",
        "job_result",
        "job_cancel",
    }
)

#: Registered profiles. ``full`` maps to ``None`` (sentinel: keep everything).
_PROFILES: dict[str, frozenset[str] | None] = {
    "full": None,
    "core": CORE,
}


def resolve_keep_set(
    registered: set[str],
    profile: str,
    allow: tuple[str, ...] = (),
    deny: tuple[str, ...] = (),
) -> set[str]:
    """Compute the set of tool names to keep, given a profile and overrides.

    Resolution order: start from the profile's allow-set (``full`` → everything),
    add back any ``allow`` names that are actually registered, then subtract
    ``deny`` last (deny always wins). Unknown profile names fall back to ``full``
    with a warning, so a typo never silently hides the whole tool surface.

    Args:
        registered: All currently-registered tool names.
        profile: Profile name (``full`` / ``core``).
        allow: Extra tool names to keep on top of the profile.
        deny: Tool names to remove regardless of profile/allow.

    Returns:
        The subset of ``registered`` to keep.

    """
    base = _PROFILES.get(profile, "MISSING")
    if base == "MISSING":
        logger.warning("unknown [mcp] tool_profile %r; falling back to 'full'", profile)
        keep = set(registered)
    elif base is None:  # full
        keep = set(registered)
    else:
        keep = set(registered) & set(base)

    keep |= set(allow) & registered
    keep -= set(deny)
    return keep


def apply_profile(
    mcp: FastMCP,
    profile: str = "full",
    allow: tuple[str, ...] = (),
    deny: tuple[str, ...] = (),
) -> list[str]:
    """Remove every registered tool not in the resolved keep-set.

    Walks the SDK tool table, computes the keep-set via :func:`resolve_keep_set`,
    and calls ``ToolManager.remove_tool`` for each tool outside it. ``full`` with
    no overrides is a fast no-op. Returns the sorted list of tools that remain.

    Like :mod:`mtui.mcp._slim`, the private-attribute access is isolated here; if
    the SDK shape drifts the filtering is skipped with a warning and the full set
    is left intact (a larger-than-asked tool surface is safe; a broken server is
    not).
    """
    try:
        tools = mcp._tool_manager._tools  # noqa: SLF001
    except AttributeError:  # pragma: no cover - SDK shape drift guard
        logger.warning(
            "MCP SDK tool table not found; skipping profile filtering "
            "(all tools remain exposed)"
        )
        return []

    registered = set(tools)
    if profile == "full" and not allow and not deny:
        return sorted(registered)

    keep = resolve_keep_set(registered, profile, allow, deny)
    removed: list[str] = []
    for name in sorted(registered - keep):
        try:
            mcp._tool_manager.remove_tool(name)  # noqa: SLF001
        except Exception as exc:  # noqa: BLE001 - best-effort; never break boot
            logger.warning("could not remove tool %r: %s", name, exc)
            continue
        removed.append(name)

    remaining = sorted(set(tools))
    logger.info(
        "tool profile %r: kept %d, removed %d tool(s)",
        profile,
        len(remaining),
        len(removed),
    )
    return remaining
