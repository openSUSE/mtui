//! Post-run check tables: functions that inspect a command's output and raise
//! [`UpdateError`] when they recognise a failure.
//!
//! ## Reference
//!
//! Each check function inspects `(hostname, stdout, stdin, stderr, exitcode)`
//! and raises an [`UpdateError`] with a stable reason string on a recognised
//! failure; the checks are keyed by `(release, transactional)` in a
//! `*_checks` table per role. The [`UpdateError`] reason strings are a stable
//! contract callers match on.
//!
//! A diagnostic breadcrumb is logged via `tracing` before each raised error.
//! `update`'s check additionally *prints* two recognised-but-non-fatal
//! diagnostic sections to stdout (one with the word `warning` highlighted
//! yellow). To reproduce that stdout parity without a crate cycle, a check
//! returns those sections as [`Diagnostic`]s on the `Ok` path; the command
//! layer (`mtui-core::commands::perform`) drains and renders them through
//! `session.display`, where the color mode lives.

pub(crate) mod downgrade;
pub(crate) mod install;
pub(crate) mod prepare;
pub(crate) mod update;

use crate::update_workflow::UpdateError;

/// A recognised-but-non-fatal diagnostic section a check wants surfaced to
/// the operator's terminal.
///
/// Carried out of the check on the `Ok` path and rendered by the command layer
/// through `session.display`, so the check itself stays free of any display or
/// color dependency. `highlight_warning` marks the "Additional rpm output"
/// section, which is printed with the word `warning` recolored yellow, while
/// the "not supported by its vendor" section is printed plain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    /// The section text to print, verbatim as sliced from stdout.
    pub text: String,
    /// When `true`, the renderer recolors occurrences of `warning` yellow.
    pub highlight_warning: bool,
}

impl Diagnostic {
    /// A diagnostic whose `warning` occurrences are recolored yellow (the
    /// "Additional rpm output" section).
    #[must_use]
    pub fn highlighted(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            highlight_warning: true,
        }
    }

    /// A diagnostic printed verbatim, no recoloring (the "not supported by
    /// its vendor" section).
    #[must_use]
    pub fn plain(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            highlight_warning: false,
        }
    }
}

/// The positional arguments passed to a check:
/// `(hostname, stdout, stdin, stderr, exitcode)`.
#[derive(Debug, Clone, Copy)]
pub struct CheckArgs<'a> {
    /// The host the command ran on.
    pub(crate) hostname: &'a str,
    /// The command's stdout.
    pub(crate) stdout: &'a str,
    /// The command that was run.
    pub(crate) stdin: &'a str,
    /// The command's stderr.
    pub(crate) stderr: &'a str,
    /// The command's exit code.
    pub(crate) exitcode: i32,
}

/// A boxed post-run check.
///
/// Returns the recognised-but-non-fatal [`Diagnostic`] sections (empty for
/// most checks) when no failure is recognised, or `Err(UpdateError)` with a
/// stable reason string otherwise.
pub type CheckFn = Box<dyn Fn(CheckArgs<'_>) -> Result<Vec<Diagnostic>, UpdateError> + Send + Sync>;

/// Shared diagnostic-log helper: logs a "command failed" event before each
/// raised `UpdateError`.
fn log_failed(args: CheckArgs<'_>) {
    tracing::error!(
        host = args.hostname,
        command = args.stdin,
        stdout = args.stdout,
        stderr = args.stderr,
        "command failed"
    );
}
