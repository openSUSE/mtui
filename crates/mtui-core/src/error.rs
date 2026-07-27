//! The command-layer error hierarchy.
//!
//! The command-relevant error variants. The
//! `Display` strings are **frozen** — the REPL and MCP surfaces both render
//! them, operators grep for them, and the tests below pin each one, so reword a
//! variant only deliberately and update its test with it.
//!
//! Usage mistakes and program errors are not modelled as separate Rust types:
//! the distinction only drives logging tone, not control flow.

use thiserror::Error;

/// The result type every [`Command`](crate::Command) returns.
pub type CommandResult = Result<(), CommandError>;

/// An error raised while resolving or running a command.
#[derive(Debug, Error)]
pub enum CommandError {
    /// A `-T/--template RRID` named a template that is not loaded.
    #[error("Template not loaded: {0}")]
    TemplateNotLoaded(String),

    /// A command resolved to no runnable target — every candidate template was
    /// skipped for lack of a connected host.
    #[error("No refhosts defined")]
    NoRefhostsDefined,

    /// An explicitly named host is not among the connected targets.
    /// The `!r` repr renders as single quotes.
    #[error("Host '{0}' is not connected")]
    HostNotConnected(String),

    /// `list_packages` had nothing to list — no template is loaded and no
    /// `-p/--package` was given.
    #[error("Missing packages: TestReport not loaded and no -p given.")]
    MissingPackages,

    /// Aggregate raised after a fan-out command failed on one or more templates.
    /// Every template still got its turn; the
    /// per-template failures are collected here keyed by RRID.
    #[error("fan-out failed on {} ({})", .0.iter().map(|(r, _)| r.as_str()).collect::<Vec<_>>().join(", "), .0.iter().map(|(r, e)| format!("{r}: {e}")).collect::<Vec<_>>().join("; "))]
    FanOut(Vec<(String, CommandError)>),

    /// The dispatch was cancelled mid-flight (MCP `job_cancel`).
    ///
    /// Raised by [`Session::check_cancelled`](crate::Session::check_cancelled)
    /// at a cancellation checkpoint — the pre-dispatch check and the
    /// between-templates check in the [`Command::run`](crate::Command::run)
    /// fan-out driver — and by a flow that stopped at one of its own
    /// checkpoints. The payload carries what the flow managed to do before
    /// stopping (which packages were applied, how many templates ran), so a
    /// cancel is never a verdict the operator cannot act on. It is empty only
    /// for the pre-dispatch checkpoint, where nothing had run yet. A genuine
    /// failure always outranks a cancellation — a broken host is never buried
    /// behind "cancelled".
    #[error("cancelled{}", if .0.is_empty() { String::new() } else { format!(": {}", .0) })]
    Cancelled(String),

    /// A command-specific failure whose message the command supplies directly.
    ///
    /// Catch-all for command-specific failures not yet given a dedicated
    /// variant; command bodies map their own
    /// failure conditions onto this until a dedicated variant is warranted.
    #[error("{0}")]
    Other(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancelled_display_is_pinned() {
        // The MCP error envelope surfaces this string as stderr; keep it stable.
        assert_eq!(
            CommandError::Cancelled(String::new()).to_string(),
            "cancelled"
        );
        // A flow-level cancel keeps its detail so the operator can act on it.
        assert_eq!(
            CommandError::Cancelled("prepare cancelled after 3/10 packages".to_owned()).to_string(),
            "cancelled: prepare cancelled after 3/10 packages"
        );
    }

    #[test]
    fn no_refhosts_message_is_stable() {
        assert_eq!(
            CommandError::NoRefhostsDefined.to_string(),
            "No refhosts defined"
        );
    }

    #[test]
    fn template_not_loaded_message_is_stable() {
        let e = CommandError::TemplateNotLoaded("SUSE:Maintenance:1:1".into());
        assert_eq!(e.to_string(), "Template not loaded: SUSE:Maintenance:1:1");
    }

    #[test]
    fn host_not_connected_uses_single_quotes() {
        let e = CommandError::HostNotConnected("host1".into());
        assert_eq!(e.to_string(), "Host 'host1' is not connected");
    }

    #[test]
    fn missing_packages_message_is_stable() {
        assert_eq!(
            CommandError::MissingPackages.to_string(),
            "Missing packages: TestReport not loaded and no -p given."
        );
    }

    #[test]
    fn fanout_display_has_stable_format() {
        // Upstream: "fan-out failed on {rrids} ({detail})" where
        // rrids = ", ".join(rrid) and detail = "; ".join(f"{rrid}: {exc}").
        let e = CommandError::FanOut(vec![
            ("a".into(), CommandError::Other("boom".into())),
            ("b".into(), CommandError::NoRefhostsDefined),
        ]);
        assert_eq!(
            e.to_string(),
            "fan-out failed on a, b (a: boom; b: No refhosts defined)"
        );
    }

    #[test]
    fn fanout_with_single_failure() {
        let e = CommandError::FanOut(vec![("x".into(), CommandError::Other("nope".into()))]);
        assert_eq!(e.to_string(), "fan-out failed on x (x: nope)");
    }
}
