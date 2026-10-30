//! Commands the MCP server must not expose as tools.
//!
//! The permanent-deny criterion lives once, beside the list it governs
//! ([`mtui_core::MCP_DENYLIST`]); it is not restated here. The deny surface
//! itself is **not** re-declared here either: this module is the thin MCP-side
//! accessor, plus the per-tool argument deny ([`MCP_DENIED_ARGS`]) below.
//! [`crate::tools`] filters denied names out when synthesising tools from the
//! command [`mtui_core::Registry`]. (`lrun` and `terms` need no denying: both
//! commands were removed from mtui entirely.)
//!
//! - `quit`, `exit`, `EOF`: exit the process, tearing the server down with it.
//! - `edit`: spawns `$EDITOR` on the controlling TTY; the `report_*` tools
//!   operate on the loaded report document instead.
//! - `shell`: an interactive root PTY needs a TTY the MCP transports lack.
//! - `help`: the MCP protocol already advertises tool descriptions.
//! - `switch`: REPL-only active-template pointer; tools select a template per
//!   call via the `template` parameter.
//! - `get`, `put`: their synthesized forms exchange **server-local paths** a
//!   remote `--transport http` client cannot reach. The hand-written tools in
//!   [`crate::transfer_tools`] carry the content in-band under the same names
//!   instead (#434) — the `edit` → report-document tools precedent, with name reuse
//!   made collision-free by this very deny.
//! - `config_set`: writes operator config — endpoints, credentials, identity,
//!   timeouts — retargeting the infrastructure the session trusts, with no
//!   operator-visible artifact (#410). `config_show` stays exposed, so a client
//!   can still verify the posture it operates under.
//!
//! `unload` is deliberately **not** denied: it names an explicit RRID, mutates
//! only the loaded set, needs no TTY, and does not exit — the addressable
//! counterpart to `load_template`.

pub use mtui_core::MCP_DENYLIST;

/// Whether `name` must not become an MCP tool.
#[must_use]
pub(crate) fn is_denied(name: &str) -> bool {
    MCP_DENYLIST.contains(&name)
}

/// Per-tool argument deny (#410): `clap` args an MCP tool must not advertise
/// in its schema. At dispatch a sent one is accepted and ignored while its
/// `DEPRECATED_KWARGS` shim lives (refused as unknown from 26.5); the strip
/// before argv reconstruction is what keeps it from reaching the engine.
///
/// The `url_*` overrides retarget trust — endpoints, not the operator's
/// configured ones — with no operator-visible artifact, so a headless client
/// uses the operator's endpoints, never its own URLs. `test_pattern` is denied
/// with them: a custom extraction regex silently reshapes which results count
/// as evidence. `no_fetch` is a deferred no-op today (it warns and fetches
/// anyway) and is denied ahead of the overview cache landing, so the MCP
/// surface never blesses a flag whose meaning is still to change. The REPL
/// keeps every flag — this is enforced here, never with `clap .hide(true)`,
/// which would also drop them from REPL `--help`.
///
/// Each tool lists exactly the overrides its own command declares, so a
/// renamed flag fails the `denied_args_name_live_tool_args` pin instead of
/// silently re-exposing.
pub const MCP_DENIED_ARGS: &[(&str, &[&str])] = &[
    (
        "openqa_overview",
        &[
            "url_openqa",
            "url_dashboard_qam",
            "url_qam",
            "test_pattern",
            "no_fetch",
        ],
    ),
    ("openqa_jobs", &["url_openqa", "url_dashboard_qam"]),
];

/// The denied args of `tool` (empty for every other tool).
#[must_use]
pub(crate) fn mcp_denied_args(tool: &str) -> &'static [&'static str] {
    match MCP_DENIED_ARGS.iter().find(|(name, _)| *name == tool) {
        Some((_, args)) => args,
        None => &[],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocked_commands_are_denied() {
        for name in [
            "quit",
            "exit",
            "EOF",
            "edit",
            "shell",
            "help",
            "switch",
            "get",
            "put",
            "config_set",
        ] {
            assert!(is_denied(name), "{name} must be denied");
        }
    }

    #[test]
    fn exposed_commands_are_not_denied() {
        // `config` (the parent) and `config_show` (the kept fan-out) stay
        // exposed: denying the parent would drop the verification half too.
        for name in [
            "run",
            "update",
            "whoami",
            "unload",
            "config",
            "config_show",
            "list_hosts",
        ] {
            assert!(!is_denied(name), "{name} must not be denied");
        }
    }

    #[test]
    fn denied_args_default_to_empty() {
        assert!(mcp_denied_args("run").is_empty());
        assert!(mcp_denied_args("config_show").is_empty());
        assert!(mcp_denied_args("no-such-tool").is_empty());
    }

    /// Every per-arg deny row names a live synthesised tool, and every denied
    /// arg names a live `clap` arg of that tool's command: a renamed tool or
    /// flag fails here instead of silently re-exposing the override. Every
    /// denied-args tool today is a plain (non-fanned-out) one, so the tool
    /// name is the command name.
    #[test]
    fn denied_args_name_live_tool_args() {
        use mtui_core::{command_parser, register_all};

        use crate::tools::build_tools;

        let registry = register_all();
        let tools = build_tools(&registry);
        for (tool, args) in MCP_DENIED_ARGS {
            assert!(
                tools.iter().any(|t| t.name == *tool),
                "{tool}: per-arg deny names no synthesised tool"
            );
            let command = registry
                .get(tool)
                .unwrap_or_else(|| panic!("{tool}: no registry command"));
            let parser = command_parser(command.as_ref());
            let live: Vec<&str> = parser
                .get_arguments()
                .filter(|a| !crate::schema::is_clap_builtin(a))
                .map(|a| a.get_id().as_str())
                .collect();
            assert!(!args.is_empty(), "{tool}: an empty row denies nothing");
            for arg in *args {
                assert!(
                    live.contains(arg),
                    "{tool}.{arg} names no live clap arg (have: {live:?})"
                );
            }
        }
    }
}
