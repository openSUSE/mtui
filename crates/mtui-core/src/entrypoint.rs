//! The process exit-code contract shared by mtui's two process entrypoints
//! (`mtui`, `mtui-mcp`).
//!
//! Three distinct argparse layers exist; do not conflate them:
//!
//! 1. **App invocation** — [`Args`](crate::args::Args); `Args::parse` exits the
//!    process on `--help`/`--version`/error, the clap convention.
//! 2. **REPL commands** — the per-command parsers the [`engine`](crate::engine)
//!    synthesises from the [`Registry`](crate::registry::Registry) and `mtui-mcp`
//!    reuses as tools. These never exit the process; they return a typed
//!    [`EngineError`](crate::engine::EngineError).
//! 3. **MCP tool schema** — `mtui-mcp` translating each command's parser into
//!    JSON parameters. Not touched here.
//!
//! Neither entrypoint has a headless single-command CLI mode: the only surfaces
//! are the interactive REPL (`mtui-cli::seed_session` + `Repl`) and `mtui-mcp`,
//! and neither takes a positional command.
//!
//! ## Exit-code contract
//!
//! `0` success or help/version, `1` runtime failure, `2` clap/argparse's
//! usage-error convention, `130` (128 + `SIGINT`) a REPL force-quit by a double
//! Ctrl-C — the status the process had when the signal still killed it outright.
//! That is the whole vocabulary; downstream packaging, wrapper scripts and CI
//! can rely on it and nothing else. See [`ExitStatus`] and `mtui_cli::ReplExit`.

/// A process exit status of either entrypoint.
///
/// * [`Ok`](ExitStatus::Ok) → `0` — the command ran, or clap printed
///   `--help`/`--version` (a success in argparse terms).
/// * [`Failure`](ExitStatus::Failure) → `1` — a runtime failure: unknown
///   command, unbalanced quotes, or the command body erroring.
/// * [`Usage`](ExitStatus::Usage) → `2` — a genuine argument *usage* error.
/// * [`Interrupted`](ExitStatus::Interrupted) → `130` — the REPL was
///   force-quit by a double Ctrl-C; see `mtui_cli::ReplExit`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitStatus {
    /// Success (or help/version output). Process exit code `0`.
    Ok,
    /// Runtime failure. Process exit code `1`.
    Failure,
    /// Argument usage error. Process exit code `2`.
    Usage,
    /// Force-quit by a double Ctrl-C. Process exit code `130`.
    ///
    /// Not an argparse outcome: 128 + `SIGINT`, so a caller already reading
    /// "killed by signal N" as 128 + N sees what it saw before mtui handled the
    /// signal at all.
    Interrupted,
}

impl ExitStatus {
    /// The numeric process exit code (`0`, `1`, `2`, or `130`).
    #[must_use]
    fn code(self) -> i32 {
        match self {
            ExitStatus::Ok => 0,
            ExitStatus::Failure => 1,
            ExitStatus::Usage => 2,
            ExitStatus::Interrupted => 130,
        }
    }
}

impl From<ExitStatus> for i32 {
    fn from(status: ExitStatus) -> Self {
        status.code()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_status_maps_to_i32() {
        assert_eq!(i32::from(ExitStatus::Ok), 0);
        assert_eq!(i32::from(ExitStatus::Failure), 1);
        assert_eq!(i32::from(ExitStatus::Usage), 2);
        // 128 + SIGINT: what wrapper scripts already read as "interrupted".
        assert_eq!(i32::from(ExitStatus::Interrupted), 130);
    }
}
