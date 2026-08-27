//! The command-layer error hierarchy.
//!
//! The `Display` strings are **frozen** — the REPL and MCP surfaces both render
//! them, operators grep for them, and the tests below pin each one, so reword a
//! variant only deliberately and update its test with it. Usage mistakes and
//! program errors share one type: the distinction drives logging tone, not
//! control flow.

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
    #[error("Host '{0}' is not connected")]
    HostNotConnected(String),

    /// `list_packages` had nothing to list — no template is loaded and no
    /// `-p/--package` was given.
    #[error("Missing packages: TestReport not loaded and no -p given.")]
    MissingPackages,

    /// Aggregate raised after a fan-out command failed on one or more templates.
    /// The per-template failures are collected in `failures`, keyed by RRID.
    ///
    /// Every template got its turn unless `stop` is set: the fan-out's own stop
    /// summary (`stopped after N of M templates`, plus the interrupted flow's
    /// detail), carried when a cancel landed but a real failure outranked it.
    /// The cancel is deliberately not a `failures` entry — it is not a broken
    /// template — but it may not vanish either, or the caller reads the
    /// templates the stop never reached as having run clean.
    #[error(
        "fan-out failed on {} ({}){}",
        .failures.iter().map(|(r, _)| r.as_str()).collect::<Vec<_>>().join(", "),
        .failures.iter().map(|(r, e)| format!("{r}: {e}")).collect::<Vec<_>>().join("; "),
        .stop.as_ref().map(|s| format!("; {s}")).unwrap_or_default()
    )]
    FanOut {
        /// The per-template failures, keyed by RRID, in fan-out order.
        failures: Vec<(String, CommandError)>,
        /// How far the fan-out got when a cancel stopped it early, if one did.
        stop: Option<String>,
    },

    /// The dispatch was cancelled mid-flight (MCP `job_cancel`).
    ///
    /// Raised by [`Session::check_cancelled`](crate::Session::check_cancelled)
    /// at a checkpoint (pre-dispatch, and between templates in the
    /// [`Command::run`](crate::Command::run) driver) and by a flow stopping at
    /// one of its own. The payload carries what the flow managed before
    /// stopping — which packages were applied, how many templates ran — so a
    /// cancel is never a verdict the operator cannot act on; it is empty only
    /// pre-dispatch, where nothing had run. A genuine failure always outranks a
    /// cancellation: a broken host is never buried behind "cancelled".
    ///
    /// **Fan-out contract:** coming *out of a command body* this variant
    /// terminates the fan-out independent of the session token — the driver
    /// breaks at that template boundary, keeping the payload as its own verdict
    /// (or as the [`FanOut`](Self::FanOut) aggregate's `stop` note). Every
    /// producer derives from a genuine session-level cancel, so that is what the
    /// break means; a per-template condition that is *not* a session-level stop
    /// would silently abandon the templates after it and wants
    /// [`Other`](Self::Other) instead, which is collected and continues.
    #[error("cancelled{}", if .0.is_empty() { String::new() } else { format!(": {}", .0) })]
    Cancelled(String),

    /// A command-specific failure whose message the command supplies directly:
    /// the catch-all until a condition warrants its own variant.
    #[error("{0}")]
    Other(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancelled_display_is_pinned() {
        // The MCP error envelope surfaces this string as stderr.
        assert_eq!(
            CommandError::Cancelled(String::new()).to_string(),
            "cancelled"
        );
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
        let e = CommandError::FanOut {
            failures: vec![
                ("a".into(), CommandError::Other("boom".into())),
                ("b".into(), CommandError::NoRefhostsDefined),
            ],
            stop: None,
        };
        assert_eq!(
            e.to_string(),
            "fan-out failed on a, b (a: boom; b: No refhosts defined)"
        );
    }

    #[test]
    fn fanout_with_single_failure() {
        let e = CommandError::FanOut {
            failures: vec![("x".into(), CommandError::Other("nope".into()))],
            stop: None,
        };
        assert_eq!(e.to_string(), "fan-out failed on x (x: nope)");
    }

    #[test]
    fn fanout_stop_note_is_appended_to_the_aggregate() {
        // An outranked cancel: it is not a `failures` entry, but its summary
        // rides on the end so the templates after the break do not read clean.
        let e = CommandError::FanOut {
            failures: vec![("h1".into(), CommandError::Other("boom".into()))],
            stop: Some("stopped after 1 of 4 templates".to_owned()),
        };
        assert_eq!(
            e.to_string(),
            "fan-out failed on h1 (h1: boom); stopped after 1 of 4 templates"
        );
    }
}
