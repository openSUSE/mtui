//! Selectable tool *profiles* for the `mtui-mcp` server.
//!
//! The server synthesises one tool per command plus the testreport and job
//! tools. The full set is sent to the model on every request, which is the
//! dominant fixed token cost of an MCP session. Many of those tools
//! (`set_log_level`, `reload_*`, `config_*`, host-bookkeeping verbs) are rarely
//! needed in a normal maintenance-test workflow.
//!
//! A *profile* is a named allow-set of tool names. The `full` profile is a no-op
//! (every synthesised tool stays). The `core` profile keeps only the curated
//! everyday subset in [`CORE`], removing the rest so they never reach the wire.
//! An operator selects a profile with `[mcp] profile` and can fine-tune with
//! `[mcp] tools_allow` / `[mcp] tools_deny` (see [`apply_profile`]).
//! Profiles only filter the surface remaining after the permanent MCP deny-list;
//! `tools_allow` cannot restore a command such as `shell` that was never
//! synthesised.
//!
//! The default is `full` so existing deployments are unchanged; slimming the tool
//! surface is strictly opt-in.
//!
//! Unlike upstream `mtui/mcp/profiles.py` — which mutates the live SDK tool table
//! (`FastMCP._tool_manager._tools`) — the Rust surface is built from a plain
//! `Vec<`[`ToolDescriptor`]`>`, so [`apply_profile`] simply filters that vec
//! before it is converted to `rmcp::model::Tool`s.

use std::collections::BTreeSet;

use crate::tools::ToolDescriptor;

/// The curated everyday tool set exposed under `profile = core`. Chosen to cover
/// load → inspect → run/install → fill report → approve/reject without the long
/// tail of host-bookkeeping and server-tuning verbs. The hand-written
/// `testreport_*` and `job_*` tools are always part of core because the slow
/// background-command flow and report editing depend on them. Mirrors upstream
/// `profiles.CORE`.
pub const CORE: &[&str] = &[
    // load / inspect
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
    // act
    "assign",
    "run",
    "update",
    "install",
    "uninstall",
    "prepare",
    "set_repo",
    // report lifecycle
    "export",
    "commit",
    "comment",
    "approve",
    "reject",
    // openQA
    "openqa_overview",
    "openqa_jobs",
    // hand-written tools (always kept)
    "testreport_read",
    "testreport_logs",
    "testreport_patch",
    "testreport_write",
    "testreport_fill",
    "job_list",
    "job_status",
    "job_result",
    "job_cancel",
];

/// The `core` profile as an owned set.
fn core_set() -> BTreeSet<String> {
    CORE.iter().map(|s| (*s).to_owned()).collect()
}

/// Resolve the profile's base allow-set. `full` (and any unknown name) yields
/// `None` (keep everything); `core` yields the curated set. An unknown name is
/// the caller's responsibility to warn about — see [`resolve_keep_set`].
fn profile_base(profile: &str) -> Option<BTreeSet<String>> {
    match profile {
        "core" => Some(core_set()),
        _ => None,
    }
}

/// `true` if `profile` names a registered profile (`full` / `core`).
fn is_known_profile(profile: &str) -> bool {
    matches!(profile, "full" | "core")
}

/// Compute the set of tool names to keep, given a profile and overrides.
///
/// Resolution order: start from the profile's allow-set (`full` → everything),
/// add back any `allow` names that are actually registered, then subtract `deny`
/// last (deny always wins). Unknown profile names fall back to `full` with a
/// warning, so a typo never silently hides the whole tool surface. Mirrors
/// upstream `profiles.resolve_keep_set`.
#[must_use]
pub fn resolve_keep_set(
    registered: &BTreeSet<String>,
    profile: &str,
    allow: &[String],
    deny: &[String],
) -> BTreeSet<String> {
    if !is_known_profile(profile) {
        tracing::warn!(
            profile,
            "unknown [mcp] profile; falling back to 'full' (all tools kept)"
        );
    }

    let mut keep: BTreeSet<String> = match profile_base(profile) {
        // full (or unknown → full): keep everything registered.
        None => registered.clone(),
        // core: intersect the curated set with what is actually registered.
        Some(base) => registered.intersection(&base).cloned().collect(),
    };

    // Add back allow names that are actually registered (never invent a tool).
    for name in allow {
        if registered.contains(name) {
            keep.insert(name.clone());
        }
    }
    // Deny wins last.
    for name in deny {
        keep.remove(name);
    }
    keep
}

/// Filter `descriptors` in place, removing every tool not in the resolved
/// keep-set. `full` with no overrides is a fast no-op. Returns the sorted list of
/// tool names that remain. Mirrors upstream `profiles.apply_profile`.
///
/// The registered set is taken from `descriptors` themselves, so the result is
/// always a subset of what was synthesised.
pub(crate) fn apply_profile(
    descriptors: &mut Vec<ToolDescriptor>,
    profile: &str,
    allow: &[String],
    deny: &[String],
) -> Vec<String> {
    let registered: BTreeSet<String> = descriptors.iter().map(|d| d.name.clone()).collect();

    // full + no overrides: nothing to do.
    if profile == "full" && allow.is_empty() && deny.is_empty() {
        return registered.into_iter().collect();
    }

    let keep = resolve_keep_set(&registered, profile, allow, deny);
    descriptors.retain(|d| keep.contains(&d.name));

    let remaining: Vec<String> = descriptors.iter().map(|d| d.name.clone()).collect();
    tracing::info!(
        profile,
        kept = remaining.len(),
        removed = registered.len() - remaining.len(),
        "applied MCP tool profile"
    );
    remaining
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Map;

    fn set(names: &[&str]) -> BTreeSet<String> {
        names.iter().map(|s| (*s).to_owned()).collect()
    }

    fn owned(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| (*s).to_owned()).collect()
    }

    fn descriptor(name: &str) -> ToolDescriptor {
        ToolDescriptor {
            name: name.to_owned(),
            description: name.to_owned(),
            input_schema: Map::new(),
            read_only: false,
        }
    }

    #[test]
    fn full_keeps_everything() {
        let reg = set(&["run", "update", "whoami"]);
        assert_eq!(resolve_keep_set(&reg, "full", &[], &[]), reg);
    }

    #[test]
    fn core_intersects_with_registered() {
        let reg = set(&["run", "whoami", "set_log_level"]);
        let keep = resolve_keep_set(&reg, "core", &[], &[]);
        assert!(keep.contains("run")); // in CORE
        assert!(!keep.contains("set_log_level")); // not in CORE
        assert!(!keep.contains("whoami")); // not in CORE
    }

    #[test]
    fn allow_adds_back_only_registered() {
        let reg = set(&["run", "whoami"]);
        let keep = resolve_keep_set(&reg, "core", &owned(&["whoami", "ghost"]), &[]);
        assert!(keep.contains("whoami"));
        assert!(!keep.contains("ghost")); // not registered → not invented
    }

    #[test]
    fn deny_wins_last() {
        let reg = set(&["run", "update"]);
        let keep = resolve_keep_set(&reg, "full", &[], &owned(&["run"]));
        assert!(!keep.contains("run"));
        assert!(keep.contains("update"));
    }

    #[test]
    fn allow_then_deny_same_name_denies() {
        let reg = set(&["run"]);
        let keep = resolve_keep_set(&reg, "core", &owned(&["run"]), &owned(&["run"]));
        assert!(!keep.contains("run"));
    }

    #[test]
    fn unknown_profile_falls_back_to_full() {
        let reg = set(&["run", "whoami"]);
        assert_eq!(resolve_keep_set(&reg, "does-not-exist", &[], &[]), reg);
    }

    #[test]
    fn apply_full_is_noop() {
        let mut tools = vec![descriptor("run"), descriptor("set_log_level")];
        let before: Vec<String> = tools.iter().map(|d| d.name.clone()).collect();
        let remaining = apply_profile(&mut tools, "full", &[], &[]);
        assert_eq!(remaining, before);
        assert_eq!(tools.len(), 2);
    }

    #[test]
    fn apply_core_removes_non_core_tools() {
        let mut tools = vec![descriptor("run"), descriptor("set_log_level")];
        let remaining = apply_profile(&mut tools, "core", &[], &[]);
        assert_eq!(remaining, vec!["run".to_owned()]);
        assert!(tools.iter().all(|d| d.name != "set_log_level"));
        assert!(tools.iter().any(|d| d.name == "run"));
    }

    #[test]
    fn apply_core_with_allow_and_deny() {
        let mut tools = vec![descriptor("run"), descriptor("whoami")];
        let remaining = apply_profile(&mut tools, "core", &owned(&["whoami"]), &owned(&["run"]));
        assert!(remaining.contains(&"whoami".to_owned()));
        assert!(!remaining.contains(&"run".to_owned()));
    }
}
