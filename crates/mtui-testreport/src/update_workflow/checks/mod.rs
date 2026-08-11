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

/// What a package manager's exit status says about the run.
///
/// zypper's status is not a boolean. Quoting `man zypper` (verified against
/// zypper 1.14.98): *"Codes below 100 denote an error, codes above 100 provide
/// a specific information, 0 represents a normal successful run."* The
/// informational band is therefore **100-107**, not 100-106 — and *"specific
/// information"* is not the same as *"the transaction committed"*. Two members
/// of the band are not successes:
///
/// * `104` (`ZYPPER_EXIT_INF_CAP_NOT_FOUND`) — the requested capability was
///   not found, so nothing was installed. Classified [`PackageNotFound`].
/// * `105` (`ZYPPER_EXIT_ON_SIGNAL`) — *"Returned upon exiting after receiving
///   a SIGINT or SIGTERM"*, i.e. aborted mid-flight. Classified [`Unknown`].
///
/// Getting the band's *extent* wrong is not academic: `107`
/// (`ZYPPER_EXIT_INF_RPM_SCRIPT_FAILED`) sat outside it in the first version of
/// this enum, which made a routine kernel/dracut `%posttrans` hiccup a failed
/// update — and an `update` check failure fires the **group-wide** rollback
/// downgrade, reverting every host in the group, not just the one that
/// reported. `102` ("reboot needed") is the same shape: the routine result of
/// patching a kernel.
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
    /// failed, `-1` is a host mtui never reached. As a member of [`Unknown`] it
    /// told the operator "Unknown Error" about a host that was never contacted,
    /// which is not a vaguer diagnosis but a wrong one.
    ///
    /// What it does **not** change is the rollback. The group-wide downgrade is
    /// vetoed by `never_ran` in `reports::update_flow`, which keys on the
    /// target's recorded `lastexit()` being `-1` and never on the class or the
    /// reason a check produced — so a `-1` host skipped the rollback under the
    /// old fallthrough too. This variant fixes what the operator is told, not
    /// where the flow goes.
    ///
    /// The variant also makes the distinction structural: the one `match` on
    /// this enum ([`classify_exit`]'s caller) is exhaustive, so removing this
    /// arm is a compile error rather than a silent fallthrough. That holds only
    /// for exhaustive matchers — a caller writing `_ =>` still compiles.
    ///
    /// [`Unknown`]: ExitClass::Unknown
    NotRun,
    /// Any other non-zero status except `-1`, including `105` (aborted on a
    /// signal).
    Unknown,
}

/// Classifies a zypper-family exit status.
///
/// This is the one place the classification lives for the `update` check; the
/// install check ([`install`]) still spells the same three sets out inline,
/// interleaved with its stderr markers, and is left alone rather than being
/// bent into this shape (see the note there).
///
/// The `104 | 4 | 5 | 8` grouping is the install check's, reproduced exactly:
/// only `104` (`ZYPPER_EXIT_INF_CAP_NOT_FOUND`) literally means "capability not
/// found", while `4`/`5`/`8` are `ERR_ZYPP`, `ERR_PRIVILEGES` and `ERR_COMMIT`.
/// The grouping is preserved rather than re-derived, so that one exit code
/// cannot be sorted into two different classes by two checks. What a check
/// then *says* about the three inaccurate members is its own decision: the
/// `update` check lets the transcript name them where it can (see
/// `classified` there).
///
/// `-1` classifies as [`NotRun`], not [`Unknown`]. It is mtui's own "never ran
/// to completion" sentinel rather than a status a package manager returned, so
/// it must keep its own reason: an operator reading "Unknown Error" about a
/// host mtui never contacted has been told the wrong thing. The gate that
/// normally catches it first is `not_run` in [`update`].
///
/// The reason string is **not** what vetoes the group-wide rollback for such a
/// host — `reports::update_flow` routes on `Target::lastexit()` and on
/// `UpdateError`'s typed `probe_failed` flag, never on a reason's text. Nothing
/// in the tree makes control flow depend on that text, and nothing should.
///
/// [`NotRun`]: ExitClass::NotRun
/// [`Unknown`]: ExitClass::Unknown
pub(crate) fn classify_exit(exitcode: i32) -> ExitClass {
    match exitcode {
        0 | 100 | 101 | 102 | 103 | 106 | 107 => ExitClass::Success,
        104 | 4 | 5 | 8 => ExitClass::PackageNotFound,
        -1 => ExitClass::NotRun,
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
        // The whole reason the update check can read an exit code at all.
        //
        // `102` is "reboot needed" after a kernel patch — routine, not a
        // failure. `107` is `ZYPPER_EXIT_INF_RPM_SCRIPT_FAILED`, which
        // `man zypper` describes as "Installation basically succeeded, but
        // some of the packages %post install scripts returned an error. These
        // packages were successfully unpacked to disk and are registered in
        // the rpm database" — a kernel/dracut/systemd `%posttrans` returning
        // non-zero is routine, and it is the *last* code in the informational
        // band, which is where it got missed the first time.
        //
        // Each of these failing would fire the group-wide rollback downgrade.
        for code in [0, 100, 101, 102, 103, 106, 107] {
            assert_eq!(
                classify_exit(code),
                ExitClass::Success,
                "exit {code} must be a success"
            );
        }
    }

    #[test]
    fn package_not_found_codes() {
        for code in [104, 4, 5, 8] {
            assert_eq!(
                classify_exit(code),
                ExitClass::PackageNotFound,
                "exit {code} must be package-not-found"
            );
        }
    }

    #[test]
    fn other_non_zero_codes_are_unknown() {
        // `99` sits just below the informational band and `108` just above it.
        // `105` (`ZYPPER_EXIT_ON_SIGNAL`) sits *inside* the band's numeric
        // range without being a success: "Returned upon exiting after
        // receiving a SIGINT or SIGTERM", i.e. the transaction was aborted
        // part-way. Being in the band is not the same as having committed.
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
        // `-1` is mtui's own "timed out / connection lost / not connected"
        // sentinel. It reaches the classifier only if a caller forgets its
        // `not_run` gate, and it must not be mistaken for a status a package
        // manager returned — the flow vetoes the group-wide rollback for a
        // host it never reached, and only the distinct reason string carries
        // that.
        //
        // It used to fall through to `Unknown`, which reported a host mtui
        // never contacted as a failed patch — the wrong diagnosis, though not
        // (as it would be easy to assume) a wrong rollback: `never_ran` in
        // `reports::update_flow` vetoes the group-wide downgrade from the
        // target's `lastexit()`, not from anything a check returns.
        assert_eq!(classify_exit(-1), ExitClass::NotRun);
    }
}
