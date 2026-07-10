//! The command-layer error hierarchy.
//!
//! Ports the command-relevant subset of upstream `mtui.support.messages`. The
//! `Display` strings are byte-for-byte identical to the Python originals so the
//! REPL and MCP surfaces present the same user-facing messages:
//!
//! * [`CommandError::NoRefhostsDefined`] ← `NoRefhostsDefinedError`
//! * [`CommandError::HostNotConnected`] ← `HostIsNotConnectedError`
//! * [`CommandError::TemplateNotLoaded`] ← `TemplateNotLoadedError`
//! * [`CommandError::MissingPackages`] ← `MissingPackagesError`
//! * [`CommandError::FanOut`] ← `FanOutError`
//!
//! Upstream distinguishes `UserError` (usage mistakes) from `ErrorMessage`
//! (program errors); that split drives only logging tone, not control flow, so
//! it is not modelled as separate Rust types.

use thiserror::Error;

/// The result type every [`Command`](crate::Command) returns.
pub type CommandResult = Result<(), CommandError>;

/// An error raised while resolving or running a command.
#[derive(Debug, Error)]
pub enum CommandError {
    /// A `-T/--template RRID` named a template that is not loaded (upstream
    /// `TemplateNotLoadedError`).
    #[error("Template not loaded: {0}")]
    TemplateNotLoaded(String),

    /// A command resolved to no runnable target — every candidate template was
    /// skipped for lack of a connected host (upstream `NoRefhostsDefinedError`).
    #[error("No refhosts defined")]
    NoRefhostsDefined,

    /// An explicitly named host is not among the connected targets (upstream
    /// `HostIsNotConnectedError`). The `!r` repr renders as single quotes.
    #[error("Host '{0}' is not connected")]
    HostNotConnected(String),

    /// `list_packages` had nothing to list — no template is loaded and no
    /// `-p/--package` was given (upstream `MissingPackagesError`).
    #[error("Missing packages: TestReport not loaded and no -p given.")]
    MissingPackages,

    /// Aggregate raised after a fan-out command failed on one or more templates
    /// (upstream `FanOutError`). Every template still got its turn; the
    /// per-template failures are collected here keyed by RRID.
    #[error("fan-out failed on {} ({})", .0.iter().map(|(r, _)| r.as_str()).collect::<Vec<_>>().join(", "), .0.iter().map(|(r, e)| format!("{r}: {e}")).collect::<Vec<_>>().join("; "))]
    FanOut(Vec<(String, CommandError)>),

    /// A command-specific failure whose message the command supplies directly.
    ///
    /// Catch-all for the many concrete upstream `ErrorMessage` subclasses not
    /// yet ported individually; command bodies (Phase 5 waves) map their own
    /// failure conditions onto this until a dedicated variant is warranted.
    #[error("{0}")]
    Other(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_refhosts_matches_upstream() {
        assert_eq!(
            CommandError::NoRefhostsDefined.to_string(),
            "No refhosts defined"
        );
    }

    #[test]
    fn template_not_loaded_matches_upstream() {
        let e = CommandError::TemplateNotLoaded("SUSE:Maintenance:1:1".into());
        assert_eq!(e.to_string(), "Template not loaded: SUSE:Maintenance:1:1");
    }

    #[test]
    fn host_not_connected_uses_single_quotes() {
        let e = CommandError::HostNotConnected("host1".into());
        assert_eq!(e.to_string(), "Host 'host1' is not connected");
    }

    #[test]
    fn missing_packages_matches_upstream() {
        assert_eq!(
            CommandError::MissingPackages.to_string(),
            "Missing packages: TestReport not loaded and no -p given."
        );
    }

    #[test]
    fn fanout_display_matches_upstream_format() {
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
