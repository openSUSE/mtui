//! Selectable tool *profiles* for the `mtui-mcp` server.
//!
//! The whole synthesised set is sent to the model on every request, the dominant
//! fixed token cost of an MCP session, and many of those tools (`set_log_level`,
//! `reload_*`, `config_*`, host bookkeeping) are rarely needed in a maintenance
//! test.
//!
//! A *profile* is a named allow-set of tool names, selected with `[mcp] profile`
//! and fine-tuned with `[mcp] tools_allow` / `[mcp] tools_deny` (see
//! `apply_profile`): `full` (the default, so existing deployments are unchanged)
//! keeps every synthesised tool, `core` only the curated everyday subset in
//! [`CORE`]. Filtering applies to the surface remaining *after* the permanent MCP
//! deny-list, so `tools_allow` cannot restore a never-synthesised command such as
//! `shell`.

use std::collections::BTreeSet;

use crate::tools::ToolDescriptor;

/// The curated everyday tool set exposed under `profile = core`: load → inspect
/// → run/install → fill report → approve/reject, without the long tail of
/// host-bookkeeping and server-tuning verbs. The `testreport_*` and `job_*` tools
/// are always core, since report editing and the background-command flow depend
/// on them; the `get`/`put` transfer tools (#434) are deliberately full-profile
/// only, like the synthesised commands they replaced, and `tools_allow` restores
/// them under `core`.
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

/// Resolve the profile's base allow-set: `None` (keep everything) for `full` and
/// any unknown name, which [`resolve_keep_set`] is responsible for warning about.
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
/// warning, so a typo never silently hides the whole tool surface.
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
        None => registered.clone(),
        Some(base) => registered.intersection(&base).cloned().collect(),
    };

    // Never invent a tool that was not registered.
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

/// Filter `descriptors` in place to the resolved keep-set, returning the sorted
/// names that remain. `full` with no overrides is a fast no-op. The registered
/// set is taken from `descriptors`, so the result is always a subset of what was
/// synthesised.
pub(crate) fn apply_profile(
    descriptors: &mut Vec<ToolDescriptor>,
    profile: &str,
    allow: &[String],
    deny: &[String],
) -> Vec<String> {
    let registered: BTreeSet<String> = descriptors.iter().map(|d| d.name.clone()).collect();

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
