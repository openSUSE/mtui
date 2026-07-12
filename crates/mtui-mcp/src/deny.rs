//! REPL-only commands the MCP server must not expose as tools.
//!
//! Port of upstream `mtui/mcp/deny.py`. Each entry cannot meaningfully run
//! outside an interactive terminal session and is filtered out when
//! [`crate::tools`] synthesises tools from the command [`Registry`].
//!
//! The deny surface is **not** re-declared here: it is the single
//! [`mtui_core::MCP_DENYLIST`], which sits beside `register_all` so the list and
//! the command surface it filters live in one place (and `mtui-core` already
//! consistency-checks it against the registry at test time). This module is the
//! thin MCP-side accessor.
//!
//! Per-entry rationale (mirrors upstream):
//!
//! - `quit`, `exit`, `EOF`: exit the process and would tear down the MCP server
//!   along with the client connection.
//! - `edit`: spawns `$EDITOR` on the controlling TTY; the testreport tools
//!   (P7.8) operate on the loaded report file directly instead.
//! - `shell`: opens an interactive root PTY on a refhost and needs a TTY the MCP
//!   transports do not provide.
//! - `help`: prints argparser help to stdout; the MCP protocol already
//!   advertises tool descriptions.
//! - `terms`: launches local terminal-emulator scripts on the operator's
//!   `$DISPLAY`.
//! - `switch`: moves the session's active-template pointer — REPL-only state with
//!   no client-addressable equivalent (tools select a template per call via the
//!   `template` parameter).
//!
//! `unload` is deliberately **not** denied: it names an explicit RRID, mutates
//! only the loaded set, needs no TTY, and does not exit — the addressable
//! counterpart to `load_template`.

pub use mtui_core::MCP_DENYLIST;

/// Whether `name` is a REPL-only command that must not become an MCP tool.
#[must_use]
pub fn is_denied(name: &str) -> bool {
    MCP_DENYLIST.contains(&name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repl_only_commands_are_denied() {
        for name in [
            "quit", "exit", "EOF", "edit", "shell", "help", "terms", "switch",
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
