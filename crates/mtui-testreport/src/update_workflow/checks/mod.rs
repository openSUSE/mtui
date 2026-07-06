//! Post-run check tables: functions that inspect a command's output and raise
//! [`UpdateError`] when they recognise a failure.
//!
//! ## Reference
//!
//! Ports upstream `mtui/update_workflow/checks/`. Each upstream module defines a
//! `zypper(hostname, stdout, stdin, stderr, exitcode) -> None` function that
//! raises `UpdateError` on a recognised failure, plus a `*_checks` dict keyed by
//! `(release, transactional)`. This port keeps the branch logic and the exact
//! [`UpdateError`] reason strings, which are a stable contract callers match on.
//!
//! Upstream's checks additionally `logger.critical(...)` / `logger.warning(...)`
//! before raising, and one branch prints colorized diagnostic text (via
//! `cli.colors.yellow`). Logging is reproduced with `tracing`; the colorized
//! terminal print is a display concern owned by `mtui-cli` (Phase 6), so it is
//! emitted here as a plain `tracing::warn!` breadcrumb instead.

pub mod downgrade;
pub mod install;
pub mod prepare;
pub mod update;

use crate::update_workflow::UpdateError;

/// The positional arguments passed to a check, mirroring upstream's
/// `(hostname, stdout, stdin, stderr, exitcode)`.
#[derive(Debug, Clone, Copy)]
pub struct CheckArgs<'a> {
    /// The host the command ran on.
    pub hostname: &'a str,
    /// The command's stdout.
    pub stdout: &'a str,
    /// The command that was run (upstream `stdin`).
    pub stdin: &'a str,
    /// The command's stderr.
    pub stderr: &'a str,
    /// The command's exit code.
    pub exitcode: i32,
}

/// A boxed post-run check.
///
/// The Rust analogue of upstream's `Callable[[str, str, str, str, int], None]`
/// dict value. Returns `Ok(())` when no failure is recognised, or
/// `Err(UpdateError)` with the upstream-matching reason string otherwise.
pub type CheckFn = Box<dyn Fn(CheckArgs<'_>) -> Result<(), UpdateError> + Send + Sync>;

/// Shared diagnostic-log helper mirroring upstream's `logger.critical(...)`
/// "command failed" line emitted before each raised `UpdateError`.
fn log_failed(args: CheckArgs<'_>) {
    tracing::error!(
        host = args.hostname,
        command = args.stdin,
        stdout = args.stdout,
        stderr = args.stderr,
        "command failed"
    );
}
