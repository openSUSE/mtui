//! Post-run check tables: functions that inspect a command's output and raise
//! [`UpdateError`] when they recognise a failure.
//!
//! Each check inspects `(hostname, stdout, stdin, stderr, exitcode)` and raises
//! an [`UpdateError`] with a stable reason string on a recognised failure,
//! keyed by `(release, transactional)` in a table per role, with a `tracing`
//! breadcrumb logged before each raised error.
//!
//! Only `update`'s check has its *diagnostic sections* surfaced to the
//! operator's terminal: it returns two non-fatal stdout sections as
//! [`Diagnostic`]s on the `Ok` path, which the command layer
//! (`mtui-core::commands::perform`) renders through `session.display`, where
//! the color mode lives — stdout parity without a crate cycle. The prepare and
//! install `("slmicro", true)` checks reuse `update`'s marker classification so
//! they can return sections too, but neither caller renders them
//! (`prepare_body` discards its sink, the `PlanProvider` adapter info-logs).

pub(crate) mod downgrade;
pub(crate) mod install;
pub(crate) mod prepare;
pub(crate) mod update;

use crate::update_workflow::UpdateError;

/// A recognised-but-non-fatal diagnostic section a check wants surfaced to
/// the operator's terminal.
///
/// Carried out on the `Ok` path and rendered by the command layer through
/// `session.display`, so the check stays free of any display or color
/// dependency. `highlight_warning` marks the "Additional rpm output" section,
/// printed with the word `warning` recolored yellow; the "not supported by its
/// vendor" section is printed plain.
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

/// zypper's documented exit codes, by their `zypper(8)` names.
pub(crate) const ZYPPER_EXIT_ERR_ZYPP: i32 = 4;
pub(crate) const ZYPPER_EXIT_ERR_PRIVILEGES: i32 = 5;
pub(crate) const ZYPPER_EXIT_ERR_COMMIT: i32 = 8;
pub(crate) const ZYPPER_EXIT_INF_UPDATE_NEEDED: i32 = 100;
pub(crate) const ZYPPER_EXIT_INF_SEC_UPDATE_NEEDED: i32 = 101;
pub(crate) const ZYPPER_EXIT_INF_REBOOT_NEEDED: i32 = 102;
pub(crate) const ZYPPER_EXIT_INF_RESTART_NEEDED: i32 = 103;
pub(crate) const ZYPPER_EXIT_INF_CAP_NOT_FOUND: i32 = 104;
pub(crate) const ZYPPER_EXIT_INF_REPO_SKIPPED: i32 = 106;
pub(crate) const ZYPPER_EXIT_INF_RPM_SCRIPT_FAILED: i32 = 107;
/// mtui's own "the command never produced a status" sentinel — *not* zypper's.
pub(crate) const EXIT_NOT_RUN: i32 = -1;

/// What a package manager's exit status says about the run.
///
/// zypper's status is not a boolean. Per `man zypper` (verified against zypper
/// 1.14.98): *"Codes below 100 denote an error, codes above 100 provide a
/// specific information, 0 represents a normal successful run."* The
/// informational band is therefore **100-107**, not 100-106 — and *"specific
/// information"* is not *"the transaction committed"*: `104`
/// ([`ZYPPER_EXIT_INF_CAP_NOT_FOUND`], nothing was installed) is classified
/// [`PackageNotFound`] and `105` (`ZYPPER_EXIT_ON_SIGNAL`, aborted on SIGINT or
/// SIGTERM) [`Unknown`].
///
/// The band's *extent* is load-bearing: `107`
/// ([`ZYPPER_EXIT_INF_RPM_SCRIPT_FAILED`]) once sat outside it, making a
/// routine kernel/dracut `%posttrans` hiccup a failed update — and an `update`
/// check failure fires the **group-wide** rollback downgrade. `102` ("reboot
/// needed" after a kernel patch) is the same shape.
///
/// [`PackageNotFound`]: ExitClass::PackageNotFound
/// [`Unknown`]: ExitClass::Unknown
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ExitClass {
    /// The transaction committed: `0`, plus the informational `100`, `101`,
    /// `102`, `103`, `106` and `107`.
    Success,
    /// `104`, `4`, `5`, `8` — the verdict "package not found". Only `104`
    /// literally means that; see [`classify_exit`] for why the other three
    /// share the class, and the `update` check for why they no longer share
    /// its message when the transcript can say more.
    PackageNotFound,
    /// `-1`, mtui's own sentinel: the command never produced a status at all.
    ///
    /// Its own variant rather than a member of [`Unknown`], because the two
    /// mean opposite things — an unrecognised status is a patch that ran and
    /// failed, `-1` is a host mtui never reached, and "Unknown Error" about a
    /// host never contacted is a wrong diagnosis, not a vaguer one. It does
    /// **not** change the rollback: `never_ran` in `reports::update_flow` vetoes
    /// the group-wide downgrade from the target's recorded `lastexit()`, never
    /// from a class or reason. Being a variant also makes the distinction
    /// structural — the one `match` on this enum is exhaustive, so removing the
    /// arm is a compile error (for exhaustive matchers; a `_ =>` still
    /// compiles).
    ///
    /// [`Unknown`]: ExitClass::Unknown
    NotRun,
    /// Any other non-zero status except `-1`, including `105` (aborted on a
    /// signal).
    Unknown,
}

/// Classifies a zypper-family exit status.
///
/// The `104 | 4 | 5 | 8` grouping is the install check's, reproduced exactly so
/// one exit code cannot be sorted into two classes by two checks: only `104`
/// ([`ZYPPER_EXIT_INF_CAP_NOT_FOUND`]) literally means "capability not found",
/// while `4`/`5`/`8` are `ERR_ZYPP`, `ERR_PRIVILEGES` and `ERR_COMMIT`. What a
/// check *says* about those three is its own decision — `update`'s `classified`
/// lets the transcript name them where it can.
///
/// `-1` classifies as [`NotRun`], not [`Unknown`]: it is mtui's own "never ran
/// to completion" sentinel, and "Unknown Error" about a host mtui never
/// contacted is the wrong thing to tell an operator. `not_run` in [`update`] is
/// the gate that normally catches it first. The reason string is **not** what
/// vetoes the group-wide rollback for such a host — `reports::update_flow`
/// routes on `Target::lastexit()` and `UpdateError`'s typed `probe_failed`
/// flag. Nothing in the tree makes control flow depend on a reason's text, and
/// nothing should.
///
/// [`NotRun`]: ExitClass::NotRun
/// [`Unknown`]: ExitClass::Unknown
pub(crate) fn classify_exit(exitcode: i32) -> ExitClass {
    match exitcode {
        0
        | ZYPPER_EXIT_INF_UPDATE_NEEDED
        | ZYPPER_EXIT_INF_SEC_UPDATE_NEEDED
        | ZYPPER_EXIT_INF_REBOOT_NEEDED
        | ZYPPER_EXIT_INF_RESTART_NEEDED
        | ZYPPER_EXIT_INF_REPO_SKIPPED
        | ZYPPER_EXIT_INF_RPM_SCRIPT_FAILED => ExitClass::Success,
        ZYPPER_EXIT_INF_CAP_NOT_FOUND
        | ZYPPER_EXIT_ERR_ZYPP
        | ZYPPER_EXIT_ERR_PRIVILEGES
        | ZYPPER_EXIT_ERR_COMMIT => ExitClass::PackageNotFound,
        EXIT_NOT_RUN => ExitClass::NotRun,
        _ => ExitClass::Unknown,
    }
}

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn informational_codes_are_success() {
        // The whole reason the update check can read an exit code at all: `102`
        // ("reboot needed" after a kernel patch) and `107` ("Installation
        // basically succeeded, but some of the packages %post install scripts
        // returned an error … registered in the rpm database") are routine, and
        // each failing would fire the group-wide rollback downgrade. `107` is
        // the *last* code in the band, which is where it got missed.
        for code in [
            0,
            ZYPPER_EXIT_INF_UPDATE_NEEDED,
            ZYPPER_EXIT_INF_SEC_UPDATE_NEEDED,
            ZYPPER_EXIT_INF_REBOOT_NEEDED,
            ZYPPER_EXIT_INF_RESTART_NEEDED,
            ZYPPER_EXIT_INF_REPO_SKIPPED,
            ZYPPER_EXIT_INF_RPM_SCRIPT_FAILED,
        ] {
            assert_eq!(
                classify_exit(code),
                ExitClass::Success,
                "exit {code} must be a success"
            );
        }
    }

    #[test]
    fn package_not_found_codes() {
        for code in [
            ZYPPER_EXIT_INF_CAP_NOT_FOUND,
            ZYPPER_EXIT_ERR_ZYPP,
            ZYPPER_EXIT_ERR_PRIVILEGES,
            ZYPPER_EXIT_ERR_COMMIT,
        ] {
            assert_eq!(
                classify_exit(code),
                ExitClass::PackageNotFound,
                "exit {code} must be package-not-found"
            );
        }
    }

    #[test]
    fn other_non_zero_codes_are_unknown() {
        // `99` and `108` bracket the informational band; `105`
        // (`ZYPPER_EXIT_ON_SIGNAL`) sits *inside* it without being a success —
        // the transaction was aborted part-way, so being in the band is not the
        // same as having committed.
        for code in [1, 2, 3, 6, 7, 99, 105, 108, 255] {
            assert_eq!(
                classify_exit(code),
                ExitClass::Unknown,
                "exit {code} must be unknown"
            );
        }
    }

    #[test]
    fn the_never_ran_sentinel_gets_its_own_class() {
        // Reached only if a caller forgets its `not_run` gate. Falling through
        // to `Unknown` reports a host mtui never contacted as a failed patch —
        // the wrong diagnosis, but not a wrong rollback: `never_ran` in
        // `reports::update_flow` vetoes the group-wide downgrade from the
        // target's `lastexit()`, not from anything a check returns.
        assert_eq!(classify_exit(EXIT_NOT_RUN), ExitClass::NotRun);
    }
}
