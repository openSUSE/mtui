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
//! before raising. Those breadcrumbs are reproduced with `tracing`. Upstream's
//! `checks/update.py` additionally *prints* two recognised-but-non-fatal
//! diagnostic sections to stdout (one with `cli.colors.yellow` highlighting on
//! the word `warning`). To reproduce that stdout parity without a crate cycle,
//! a check returns those sections as [`Diagnostic`]s on the `Ok` path; the
//! command layer (`mtui-core::commands::perform`) drains and renders them
//! through `session.display`, where the color mode lives.

pub(crate) mod downgrade;
pub(crate) mod install;
pub(crate) mod prepare;
pub(crate) mod update;

use crate::update_workflow::UpdateError;

/// A recognised-but-non-fatal diagnostic section a check wants surfaced to the
/// operator's terminal (upstream `checks/update.py`'s two `print(...)` blocks).
///
/// Carried out of the check on the `Ok` path and rendered by the command layer
/// through `session.display`, so the check itself stays free of any display or
/// color dependency. `highlight_warning` mirrors upstream: the "Additional rpm
/// output" section is printed with the word `warning` recolored yellow
/// (`replace("warning", yellow("warning"))`), while the "not supported by its
/// vendor" section is printed plain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    /// The section text to print (verbatim, as upstream slices it from stdout).
    pub text: String,
    /// When `true`, the renderer recolors occurrences of `warning` yellow.
    pub highlight_warning: bool,
}

impl Diagnostic {
    /// A diagnostic whose `warning` occurrences are recolored yellow (upstream
    /// "Additional rpm output" section).
    #[must_use]
    pub fn highlighted(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            highlight_warning: true,
        }
    }

    /// A diagnostic printed verbatim, no recoloring (upstream "not supported by
    /// its vendor" section).
    #[must_use]
    pub fn plain(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            highlight_warning: false,
        }
    }
}

/// The positional arguments passed to a check, mirroring upstream's
/// `(hostname, stdout, stdin, stderr, exitcode)`.
#[derive(Debug, Clone, Copy)]
pub struct CheckArgs<'a> {
    /// The host the command ran on.
    pub(crate) hostname: &'a str,
    /// The command's stdout.
    pub(crate) stdout: &'a str,
    /// The command that was run (upstream `stdin`).
    pub(crate) stdin: &'a str,
    /// The command's stderr.
    pub(crate) stderr: &'a str,
    /// The command's exit code.
    pub(crate) exitcode: i32,
}

/// A boxed post-run check.
///
/// The Rust analogue of upstream's `Callable[[str, str, str, str, int], None]`
/// dict value. Returns the recognised-but-non-fatal [`Diagnostic`] sections
/// (empty for most checks) when no failure is recognised, or `Err(UpdateError)`
/// with the upstream-matching reason string otherwise.
pub type CheckFn = Box<dyn Fn(CheckArgs<'_>) -> Result<Vec<Diagnostic>, UpdateError> + Send + Sync>;

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
