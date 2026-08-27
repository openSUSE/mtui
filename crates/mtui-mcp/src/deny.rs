//! Commands the MCP server must not expose as tools.
//!
//! Each entry either cannot meaningfully run outside an interactive terminal
//! session, or is replaced by a richer hand-written tool; [`crate::tools`]
//! filters them out when synthesising tools from the command
//! [`mtui_core::Registry`]. (`lrun` needs no denying: the command was removed
//! from mtui entirely.)
//!
//! The deny surface is **not** re-declared here: it is the single
//! [`mtui_core::MCP_DENYLIST`], which sits beside `register_all` and is
//! consistency-checked against the registry there. This module is the thin
//! MCP-side accessor.
//!
//! - `quit`, `exit`, `EOF`: exit the process, tearing the server down with it.
//! - `edit`: spawns `$EDITOR` on the controlling TTY; the testreport tools
//!   operate on the loaded report file directly instead.
//! - `shell`: an interactive root PTY needs a TTY the MCP transports lack.
//! - `help`: the MCP protocol already advertises tool descriptions.
//! - `terms`: launches terminal-emulator scripts on the operator's `$DISPLAY`.
//! - `switch`: REPL-only active-template pointer; tools select a template per
//!   call via the `template` parameter.
//! - `get`, `put`: their synthesized forms exchange **server-local paths** a
//!   remote `--transport http` client cannot reach. The hand-written tools in
//!   [`crate::transfer_tools`] carry the content in-band under the same names
//!   instead (#434) — the `edit` → testreport-tools precedent, with name reuse
//!   made collision-free by this very deny.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocked_commands_are_denied() {
        for name in [
            "quit", "exit", "EOF", "edit", "shell", "help", "terms", "switch", "get", "put",
        ] {
            assert!(is_denied(name), "{name} must be denied");
        }
    }

    #[test]
    fn exposed_commands_are_not_denied() {
        for name in ["run", "update", "whoami", "unload", "config", "list_hosts"] {
            assert!(!is_denied(name), "{name} must not be denied");
        }
    }
}
