//! Post-install check.
//!
//! Classifies a zypper install result by exit code and stderr/stdout markers.
//! Unlike the other checks, install treats *any* unrecognised non-success as an
//! `UpdateError("Unknown Error")`.

use crate::update_workflow::UpdateError;
use crate::update_workflow::checks::{CheckArgs, CheckFn, Diagnostic, log_failed};

/// The zypper install check.
///
/// The three exit-code sets are the same ones
/// [`classify_exit`](super::classify_exit) expresses for the `update` check,
/// but they are spelled out inline here because this check interleaves them
/// with its stderr markers in a different order: the markers sit *between* the
/// package-not-found set and the "Unknown Error" fallback, where `update` now
/// gives them first refusal on everything except `104`. So the same transcript
/// can read differently across the two checks — an exit `5` carrying
/// `System management is locked` is "package not found" here and "update stack
/// locked" there, pinned on both sides so the divergence stays deliberate.
/// Folding this onto the shared helper is a behaviour-preserving refactor only
/// if that ordering is preserved exactly — worth doing on its own, not as a
/// rider.
///
/// # Errors
///
/// Returns [`UpdateError`] with a reason of "package not found", "update stack
/// locked", "RPM Error", "Dependency Error", or "Unknown Error" depending on
/// the exit code and stderr/stdout markers. Exit codes `0, 100, 101, 102,
/// 103, 106, 107` are success. This check surfaces no [`Diagnostic`]s (only
/// `update` does).
fn zypper(args: CheckArgs<'_>) -> Result<Vec<Diagnostic>, UpdateError> {
    if matches!(args.exitcode, 0 | 100 | 101 | 102 | 103 | 106 | 107) {
        return Ok(Vec::new());
    }
    if matches!(args.exitcode, 104 | 4 | 5 | 8) {
        log_failed(args);
        return Err(UpdateError::new("package not found", args.hostname));
    }
    if args
        .stderr
        .contains("A ZYpp transaction is already in progress.")
        || args.stderr.contains("System management is locked")
    {
        log_failed(args);
        return Err(UpdateError::new("update stack locked", args.hostname));
    }
    if args.stderr.contains("Error:") {
        log_failed(args);
        return Err(UpdateError::new("RPM Error", args.hostname));
    }
    if args.stdout.contains("(c): c") {
        tracing::error!(
            host = args.hostname,
            stdout = args.stdout,
            "unresolved dependency problem. please resolve manually"
        );
        return Err(UpdateError::new("Dependency Error", args.hostname));
    }
    log_failed(args);
    Err(UpdateError::new("Unknown Error", args.hostname))
}

/// The transactional-update (`slmicro`) install/uninstall check.
///
/// Reuses the `update` check's [`classified`](super::update::classified)
/// verdict — `classify_exit` interleaved with the shared stdout/stderr markers
/// — rather than restating it, so the two `transactional-update` keys cannot
/// drift apart. The reasoning for what that classifier means on this key is
/// recorded once, on `checks::update`'s `transactional_update`: the command
/// returns **only** `0` or `1` (verified against upstream
/// `openSUSE/transactional-update` v6.1.3, unchanged from v2.28.3), because it
/// absorbs zypper's status instead of propagating it. So the informational band
/// and the package-not-found set are inert here by construction, and the
/// classifier reduces to "`0` passes, anything else is an Unknown Error" —
/// while the markers still catch the shape an exit code cannot: a run that
/// reported a locked update stack, a dependency prompt or an RPM error and
/// still exited `0`.
///
/// The `-1` sentinel is gated here rather than deferred to the classifier's own
/// [`NotRun`](super::ExitClass::NotRun) arm because that arm speaks the
/// `update` vocabulary ("update command timed out…"), and this table serves
/// `install` *and* `uninstall` — hence the role-neutral wording.
///
/// # Errors
///
/// Returns [`UpdateError`] with a reason of "command timed out or failed to
/// run" (exit `-1`), "update stack locked", "Dependency Error", "RPM Error",
/// "package not found" or "Unknown Error".
fn transactional_update(args: CheckArgs<'_>) -> Result<Vec<Diagnostic>, UpdateError> {
    if args.exitcode == -1 {
        log_failed(args);
        return Err(UpdateError::new(
            "command timed out or failed to run",
            args.hostname,
        ));
    }
    super::update::classified(args)
}

/// The yum install/uninstall check.
///
/// Deliberately narrow: the exit code is the whole verdict, with no marker
/// reading and no zypper exit-code semantics.
///
/// * **The accepted statuses.** `yum(8)` and `dnf(8)` document `0` as success
///   and any other status as an error for `install`/`remove`; unlike zypper
///   there is no informational band to carve out. zypper's `100`-`107` must
///   **not** transfer — `dnf`'s only documented `100` belongs to
///   `check-update`, which no install or uninstall doer runs, so accepting it
///   here would pass a genuinely failed transaction. `-1` is mtui's own "never
///   produced a status" sentinel and gets its own reason, so a host mtui never
///   reached is not reported as a package failure.
/// * **No markers.** Three of [`markers`](super::update::markers)' four
///   strings are zypper-only vocabulary, and the fourth (`Error:` on stderr)
///   was written against zypper transcripts; the yum *update* check's doc
///   records the same reasoning at length. Adding it here would be a new rule
///   judging a transcript it was never written for.
///
/// "Unknown Error" is therefore an admission rather than a diagnosis: naming
/// the failure would take observed `yum`/`dnf` output from a real RHEL refhost
/// rather than inference. Unlike the *update* check — where a false failure
/// fires the group-wide rollback downgrade — a failed install/uninstall verdict
/// only fails the operation that was asked for, which is why judging the exit
/// code at all is safe here.
///
/// # Errors
///
/// Returns [`UpdateError`] with a reason of "command timed out or failed to
/// run" (exit `-1`) or "Unknown Error" (any other non-zero exit).
fn yum(args: CheckArgs<'_>) -> Result<Vec<Diagnostic>, UpdateError> {
    if args.exitcode == -1 {
        log_failed(args);
        return Err(UpdateError::new(
            "command timed out or failed to run",
            args.hostname,
        ));
    }
    if args.exitcode != 0 {
        log_failed(args);
        return Err(UpdateError::new("Unknown Error", args.hostname));
    }
    Ok(Vec::new())
}

/// The install check for `(release, transactional)`, or `None` for an unknown
/// key.
///
/// Shared with `uninstall` (see [`CheckProvider`](crate::update_workflow::CheckProvider)),
/// so every reason here is worded role-neutrally.
#[must_use]
pub(crate) fn install_check(release: &str, transactional: bool) -> Option<CheckFn> {
    match (release, transactional) {
        ("11", false) | ("12", false) | ("15", false) | ("16", false) => Some(Box::new(zypper)),
        ("YUM", false) => Some(Box::new(yum)),
        ("slmicro", true) => Some(Box::new(transactional_update)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args<'a>(stdout: &'a str, stderr: &'a str, exitcode: i32) -> CheckArgs<'a> {
        CheckArgs {
            hostname: "h1",
            stdout,
            stdin: "zypper -n in -y -l pkg",
            stderr,
            exitcode,
        }
    }

    #[test]
    fn success_exit_codes_pass() {
        // `107` (`ZYPPER_EXIT_INF_RPM_SCRIPT_FAILED`) is in the list because
        // `man zypper` puts it in the informational band: "Installation
        // basically succeeded, but some of the packages %post install scripts
        // returned an error. These packages were successfully unpacked to disk
        // and are registered in the rpm database". It was missing, so a
        // routine `%posttrans` hiccup failed the install.
        for code in [0, 100, 101, 102, 103, 106, 107] {
            assert!(
                zypper(args("", "", code)).is_ok(),
                "code {code} should pass"
            );
        }
    }

    #[test]
    fn package_not_found_codes() {
        for code in [104, 4, 5, 8] {
            let err = zypper(args("", "", code)).unwrap_err();
            assert_eq!(err.reason, "package not found");
            assert_eq!(err.host.as_deref(), Some("h1"));
        }
    }

    #[test]
    fn the_not_found_set_still_outranks_the_markers_here() {
        // The install check keeps its own ordering: the whole `104 | 4 | 5 | 8`
        // set short-circuits ahead of the stderr markers, where the `update`
        // check now lets them speak first on everything but `104`. So the same
        // transcript reads differently across the two, deliberately — pinned
        // here and in `update`'s `only_104_outranks_the_markers` so neither
        // side can drift without a test saying so.
        //
        // Every other fixture in this module passes an empty transcript, which
        // is why hoisting the marker block above the set used to break nothing.
        for (stdout, stderr, code) in [
            ("", "System management is locked", 5),
            ("", "A ZYpp transaction is already in progress.", 4),
            ("", "Error: something", 8),
            ("choose (c): c", "", 4),
        ] {
            let err = zypper(args(stdout, stderr, code)).unwrap_err();
            assert_eq!(
                err.reason, "package not found",
                "install exit {code} must outrank its markers"
            );
        }
    }

    #[test]
    fn zypp_lock_is_stack_locked() {
        let err = zypper(args("", "A ZYpp transaction is already in progress.", 1)).unwrap_err();
        assert_eq!(err.reason, "update stack locked");
        let err2 = zypper(args("", "System management is locked", 1)).unwrap_err();
        assert_eq!(err2.reason, "update stack locked");
    }

    #[test]
    fn rpm_error_from_stderr() {
        let err = zypper(args("", "Error: something", 1)).unwrap_err();
        assert_eq!(err.reason, "RPM Error");
    }

    #[test]
    fn dependency_error_from_stdout_marker() {
        let err = zypper(args("choose (c): c", "", 1)).unwrap_err();
        assert_eq!(err.reason, "Dependency Error");
    }

    #[test]
    fn unrecognised_failure_is_unknown_error() {
        let err = zypper(args("", "", 1)).unwrap_err();
        assert_eq!(err.reason, "Unknown Error");
    }

    /// Like [`args`] but with the command text spelled out, for the keys whose
    /// transcript is not zypper's.
    fn args_for<'a>(
        stdin: &'a str,
        stdout: &'a str,
        stderr: &'a str,
        exitcode: i32,
    ) -> CheckArgs<'a> {
        CheckArgs {
            hostname: "h1",
            stdout,
            stdin,
            stderr,
            exitcode,
        }
    }

    #[test]
    fn slmicro_failed_exit_is_unknown_error() {
        // Before this key had a check the adapter's fallback answered
        // "install command failed" for every failure. The verdict must survive
        // the move — a check that could not fail would be worse than the
        // fallback it replaced.
        let err = transactional_update(args_for(
            "transactional-update -n pkg install pkg-a",
            "",
            "",
            1,
        ))
        .expect_err("a failed transactional install must fail the check");
        assert_eq!(err.reason, "Unknown Error");
        assert_eq!(err.host.as_deref(), Some("h1"));
    }

    #[test]
    fn slmicro_clean_exit_with_chatty_stderr_passes() {
        // The unit-level twin of `perform_install_ignores_stderr_on_a_clean_
        // exit`, which now routes through this check: `transactional-update`
        // and `yum` write progress and warnings to stderr on a successful run,
        // so no `!stderr.is_empty()` rule may exist here. This exact string is
        // the one that flow test feeds.
        assert!(
            transactional_update(args_for(
                "transactional-update -n pkg install pkg-a",
                "",
                "warning: chatty",
                0,
            ))
            .is_ok()
        );
    }

    #[test]
    fn slmicro_lock_message_on_a_clean_exit_is_stack_locked() {
        // The markers must run on the *success* class too: a
        // `transactional-update` that reported a locked stack and still exited
        // `0` is the failure an exit-code rule cannot see. A short-circuit
        // `if exitcode == 0 { return Ok(...) }` ahead of the classifier would
        // silently pass it.
        let err = transactional_update(args_for(
            "transactional-update -n pkg install pkg-a",
            "",
            "System management is locked",
            0,
        ))
        .expect_err("a locked stack fails the install whatever the exit code");
        assert_eq!(err.reason, "update stack locked");
    }

    #[test]
    fn slmicro_timed_out_reason_is_its_own_not_updates() {
        // Exact string, because this is what the local `-1` gate buys: without
        // it the shared classifier's `NotRun` arm answers, in the *update*
        // check's vocabulary ("update command timed out or failed to run"),
        // on a table that also serves `uninstall`.
        let err = transactional_update(args_for(
            "transactional-update -n pkg install pkg-a",
            "",
            "",
            -1,
        ))
        .expect_err("a command that never ran must fail the check");
        assert_eq!(err.reason, "command timed out or failed to run");
        assert_eq!(err.host.as_deref(), Some("h1"));
    }

    #[test]
    fn yum_exit_code_is_the_whole_verdict() {
        // (a) No markers: `Error:` is zypper vocabulary, and this key's
        // transcript is not zypper's.
        assert!(
            yum(args_for(
                "yum -y install pkg-a",
                "",
                "Error: Cannot retrieve repository metadata for repo 'x'",
                0,
            ))
            .is_ok()
        );
        // (b) A failed transaction is still a failure.
        let err = yum(args_for("yum -y install pkg-a", "", "", 1)).unwrap_err();
        assert_eq!(err.reason, "Unknown Error");
        assert_eq!(err.host.as_deref(), Some("h1"));
        // (c) zypper's informational band does NOT transfer. `100` is
        // "updates available" to zypper and an error to yum/dnf (whose only
        // documented `100` belongs to `check-update`, which no install or
        // uninstall doer runs), so routing this key through the shared
        // classifier would pass a failed transaction.
        for code in [100, 101, 102, 103, 106, 107] {
            assert!(
                yum(args_for("yum -y install pkg-a", "", "", code)).is_err(),
                "zypper's informational exit {code} is not yum's"
            );
        }
        // (d) And the never-ran sentinel keeps its own reason.
        let err = yum(args_for("yum -y install pkg-a", "", "", -1)).unwrap_err();
        assert_eq!(err.reason, "command timed out or failed to run");
    }

    #[test]
    fn table_lookup() {
        assert!(install_check("15", false).is_some());
        // Both keys that have an *installer* and an *uninstaller* but used to
        // have no check: they fell back to the `PlanProvider` adapter's
        // exit-code-only verdict, with no lock/dependency/RPM attribution and
        // no distinct verdict for a command that never ran.
        assert!(install_check("slmicro", true).is_some());
        assert!(install_check("YUM", false).is_some());
        // The key shape still matters — `slmicro` is only ever transactional,
        // and no zypper release is.
        assert!(install_check("slmicro", false).is_none());
        assert!(install_check("15", true).is_none());
        assert!(install_check("nonesuch", false).is_none());
    }

    #[test]
    fn the_new_arms_resolve_to_their_own_check_not_a_sibling() {
        // `is_some()` above pins that the arm exists, not *what* it binds to.
        // Every behaviour test in this module calls the functions directly, and
        // on exit `1` all three answer "Unknown Error" — so merging the arms
        // onto one function would leave the suite green. Two transcripts
        // separate all three here.
        let yum_fn = install_check("YUM", false).expect("the YUM key has an install check");
        // (a) A clean exit carrying a zypper lock marker: `yum` reads no
        // markers and passes it; `transactional_update` fails it. This is the
        // shape a successful `yum install` on a RHEL host can really produce.
        assert!(
            yum_fn(args_for(
                "yum -y install pkg-a",
                "",
                "System management is locked",
                0
            ))
            .is_ok(),
            "the YUM arm must bind the marker-blind yum check"
        );
        // (b) Exit `100`: zypper's informational band is success to `zypper`
        // and a failed transaction to yum/dnf, so this separates the YUM arm
        // from the zypper one, which (a) cannot.
        assert_eq!(
            yum_fn(args_for("yum -y install pkg-a", "", "", 100))
                .expect_err("zypper's informational band is not yum's")
                .reason,
            "Unknown Error"
        );

        // The same transcript on the slmicro arm must FAIL: `transactional_
        // update` runs the markers on the success class too, while both `yum`
        // (no markers) and `zypper` (exit `0` short-circuits ahead of them)
        // pass it. One assertion separates that arm from both siblings.
        let slmicro_fn =
            install_check("slmicro", true).expect("the slmicro key has an install check");
        let err = slmicro_fn(args_for(
            "transactional-update -n pkg install pkg-a",
            "",
            "System management is locked",
            0,
        ))
        .expect_err("the slmicro arm must bind the marker-reading check");
        assert_eq!(err.reason, "update stack locked");
    }
}
