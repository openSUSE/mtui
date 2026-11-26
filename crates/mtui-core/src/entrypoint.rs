//! The process exit-code contract shared by mtui's two process entrypoints
//! (`mtui`, `mtui-mcp`).
//!
//! Two argparse layers exist; do not conflate them:
//!
//! 1. **App invocation** — [`Args`](crate::args::Args); `Args::parse` exits the
//!    process on `--help`/`--version`/error, the clap convention.
//! 2. **Commands** — the per-command parsers the [`engine`](crate::engine)
//!    synthesises from the [`Registry`](crate::registry::Registry). These never
//!    exit the process; they return a typed
//!    [`EngineError`](crate::engine::EngineError).
//!
//! Neither entrypoint has a headless single-command CLI mode.
//!
//! ## Exit-code contract
//!
//! `0` success or help/version, `1` runtime failure, `2` usage error, `130`
//! (128 + `SIGINT`) a REPL force-quit by a double Ctrl-C, and `129`/`143`
//! (128 + `SIGHUP`/`SIGTERM`) a REPL that tore down on that signal. That is the
//! whole vocabulary callers can rely on. See [`ExitStatus`] and
//! `mtui_cli::ReplExit`.

/// A process exit status of either entrypoint.
///
/// * [`Ok`](ExitStatus::Ok) → `0` — the command ran, or clap printed
///   `--help`/`--version` (a success in argparse terms).
/// * [`Failure`](ExitStatus::Failure) → `1` — a runtime failure: unknown
///   command, unbalanced quotes, or the command body erroring.
/// * [`Usage`](ExitStatus::Usage) → `2` — a genuine argument *usage* error.
/// * [`Interrupted`](ExitStatus::Interrupted) → `130` — the REPL was
///   force-quit by a double Ctrl-C; see `mtui_cli::ReplExit`.
/// * [`Hangup`](ExitStatus::Hangup) → `129`,
///   [`Terminated`](ExitStatus::Terminated) → `143` — the REPL tore down on
///   `SIGHUP`/`SIGTERM`.
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
    /// 128 + `SIGINT`, as a caller reading "killed by signal N" would expect.
    Interrupted,
    /// Torn down on `SIGHUP`. Process exit code `129` (128 + `SIGHUP`).
    Hangup,
    /// Torn down on `SIGTERM`. Process exit code `143` (128 + `SIGTERM`).
    Terminated,
}

impl ExitStatus {
    /// The numeric process exit code (`0`, `1`, `2`, `129`, `130` or `143`).
    #[must_use]
    fn code(self) -> i32 {
        match self {
            ExitStatus::Ok => 0,
            ExitStatus::Failure => 1,
            ExitStatus::Usage => 2,
            ExitStatus::Interrupted => 130,
            ExitStatus::Hangup => 129,
            ExitStatus::Terminated => 143,
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
        assert_eq!(i32::from(ExitStatus::Hangup), 129);
        assert_eq!(i32::from(ExitStatus::Terminated), 143);
    }
}
