//! Post-install check.
//!
//! Classifies a zypper install result by exit code and stderr/stdout markers.
//! Unlike the other checks, install treats *any* unrecognised non-success as an
//! `UpdateError("Unknown Error")`.

use crate::update_workflow::UpdateError;
use crate::update_workflow::checks::{CheckArgs, CheckFn, Diagnostic, EXIT_NOT_RUN, log_failed};

/// The zypper install check.
///
/// Below its own role-neutral `-1` gate, this is [`super::update::classified`]:
/// install and update share one exit-code/marker classifier, so the two cannot
/// drift into different verdicts for the same transcript.
///
/// # Errors
///
/// Returns [`UpdateError`] with a reason of "command timed out or failed to
/// run" (exit `-1`), "package not found", "update stack locked", "RPM Error",
/// "Dependency Error", or "Unknown Error" depending on the exit code and
/// stderr/stdout markers. Exit codes `0, 100, 101, 102, 103, 106, 107` are
/// success. This check surfaces the same [`Diagnostic`]s `update` does on a
/// clean transcript.
fn zypper(args: CheckArgs<'_>) -> Result<Vec<Diagnostic>, UpdateError> {
    if args.exitcode == EXIT_NOT_RUN {
        log_failed(args);
        return Err(UpdateError::new(
            "command timed out or failed to run",
            args.hostname,
        ));
    }
    super::update::classified(args)
}

/// The transactional-update (`slmicro`) install/uninstall check.
///
/// Reuses the `update` check's [`classified`](super::update::classified)
/// verdict so the two `transactional-update` keys cannot drift apart. What that
/// classifier means on this key is argued once, on `checks::update`'s
/// `transactional_update`: the command returns **only** `0` or `1` (verified
/// against `openSUSE/transactional-update` v6.1.3, unchanged from v2.28.3),
/// because it absorbs zypper's status. So the informational band and the
/// package-not-found set are inert here, while the markers still catch what an
/// exit code cannot — a run that reported a locked update stack, a dependency
/// prompt or an RPM error and still exited `0`.
///
/// The `-1` sentinel is gated here rather than left to the classifier's own
/// [`NotRun`](super::ExitClass::NotRun) arm, which speaks the `update`
/// vocabulary; this table serves `install` *and* `uninstall`, hence the
/// role-neutral wording.
///
/// # Errors
///
/// Returns [`UpdateError`] with a reason of "command timed out or failed to
/// run" (exit `-1`), "update stack locked", "Dependency Error", "RPM Error",
/// "package not found" or "Unknown Error".
fn transactional_update(args: CheckArgs<'_>) -> Result<Vec<Diagnostic>, UpdateError> {
    if args.exitcode == EXIT_NOT_RUN {
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
/// Deliberately narrow: the exit code is the whole verdict, with no markers and
/// no zypper exit-code semantics.
///
/// * **The accepted statuses.** `yum(8)`/`dnf(8)` document `0` as success and
///   anything else as an error for `install`/`remove`; there is no
///   informational band. zypper's `100`-`107` must **not** transfer — `dnf`'s
///   only documented `100` belongs to `check-update`, which no doer here runs,
///   so accepting it would pass a failed transaction. `-1` keeps its own
///   reason, so a host mtui never reached is not reported as a package failure.
/// * **No markers**, for the reason the yum *update* check argues: three of
///   [`markers`](super::update::markers)' four strings are zypper-only and the
///   fourth was written against zypper transcripts.
///
/// "Unknown Error" is therefore an admission, not a diagnosis: naming the
/// failure needs observed `yum`/`dnf` output from a real RHEL refhost. Reading
/// the exit code at all is safe here only because a failed install/uninstall
/// verdict fails just the operation asked for, where the *update* check's would
/// fire the group-wide rollback downgrade.
///
/// # Errors
///
/// Returns [`UpdateError`] with a reason of "command timed out or failed to
/// run" (exit `-1`) or "Unknown Error" (any other non-zero exit).
fn yum(args: CheckArgs<'_>) -> Result<Vec<Diagnostic>, UpdateError> {
    if args.exitcode == EXIT_NOT_RUN {
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
    use crate::update_workflow::checks::{
        ZYPPER_EXIT_ERR_COMMIT, ZYPPER_EXIT_ERR_PRIVILEGES, ZYPPER_EXIT_ERR_ZYPP,
        ZYPPER_EXIT_INF_CAP_NOT_FOUND, ZYPPER_EXIT_INF_REBOOT_NEEDED, ZYPPER_EXIT_INF_REPO_SKIPPED,
        ZYPPER_EXIT_INF_RESTART_NEEDED, ZYPPER_EXIT_INF_RPM_SCRIPT_FAILED,
        ZYPPER_EXIT_INF_SEC_UPDATE_NEEDED, ZYPPER_EXIT_INF_UPDATE_NEEDED,
    };

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
        // `107` (`ZYPPER_EXIT_INF_RPM_SCRIPT_FAILED`) is in the informational
        // band per `man zypper` — the packages are unpacked and registered,
        // only a `%post` script failed — and was once missing, so a routine
        // `%posttrans` hiccup failed the install.
        for code in [
            0,
            ZYPPER_EXIT_INF_UPDATE_NEEDED,
            ZYPPER_EXIT_INF_SEC_UPDATE_NEEDED,
            ZYPPER_EXIT_INF_REBOOT_NEEDED,
            ZYPPER_EXIT_INF_RESTART_NEEDED,
            ZYPPER_EXIT_INF_REPO_SKIPPED,
            ZYPPER_EXIT_INF_RPM_SCRIPT_FAILED,
        ] {
            assert!(
                zypper(args("", "", code)).is_ok(),
                "code {code} should pass"
            );
        }
    }

    #[test]
    fn successful_install_now_returns_the_additional_rpm_output_diagnostic() {
        // Sharing `classified` means a successful install surfaces the same
        // "Additional rpm output" section `update` does.
        let stdout = "before Additional rpm output:\nwarning: stuff\nRetrieving repo\nafter";
        let diags = zypper(args(stdout, "", 0)).unwrap();
        assert_eq!(diags.len(), 1);
        assert!(diags[0].highlight_warning);
        assert_eq!(diags[0].text, "\nwarning: stuff\n");
    }

    #[test]
    fn package_not_found_codes() {
        for code in [
            ZYPPER_EXIT_INF_CAP_NOT_FOUND,
            ZYPPER_EXIT_ERR_ZYPP,
            ZYPPER_EXIT_ERR_PRIVILEGES,
            ZYPPER_EXIT_ERR_COMMIT,
        ] {
            let err = zypper(args("", "", code)).unwrap_err();
            assert_eq!(err.reason, "package not found");
            assert_eq!(err.host.as_deref(), Some("h1"));
        }
    }

    #[test]
    fn only_104_outranks_the_markers_here() {
        // Shares `update`'s ordering: only `104`
        // (`ZYPPER_EXIT_INF_CAP_NOT_FOUND`) short-circuits ahead of the stderr
        // markers, while `4`/`5`/`8` let the transcript name the failure. This
        // and `update`'s `only_104_outranks_the_markers` assert identical
        // verdicts, so neither side can drift.
        let err = zypper(args(
            "",
            "System management is locked",
            ZYPPER_EXIT_INF_CAP_NOT_FOUND,
        ))
        .unwrap_err();
        assert_eq!(err.reason, "package not found", "104 outranks the marker");

        let err = zypper(args(
            "",
            "System management is locked",
            ZYPPER_EXIT_ERR_PRIVILEGES,
        ))
        .unwrap_err();
        assert_eq!(err.reason, "update stack locked", "5 + lock marker");
        let err = zypper(args(
            "",
            "A ZYpp transaction is already in progress.",
            ZYPPER_EXIT_ERR_ZYPP,
        ))
        .unwrap_err();
        assert_eq!(err.reason, "update stack locked", "4 + ZYpp marker");
        let err = zypper(args("choose (c): c", "", ZYPPER_EXIT_ERR_ZYPP)).unwrap_err();
        assert_eq!(err.reason, "Dependency Error", "4 + dependency prompt");
        let err = zypper(args("", "Error: something", ZYPPER_EXIT_ERR_COMMIT)).unwrap_err();
        assert_eq!(err.reason, "RPM Error", "8 + rpm marker");
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
    fn both_markers_present_favors_the_dependency_prompt() {
        // `markers` checks the dependency prompt before stderr's `Error:`, so a
        // transcript carrying both reads "Dependency Error" — matching
        // `update`. The order is otherwise free to flip silently.
        let err = zypper(args("choose (c): c", "Error: boom", 1)).unwrap_err();
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
        // This key's check replaced the adapter's exit-code-only fallback, so
        // it must still fail — a check that cannot fail is worse than none.
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
        // `transactional-update` and `yum` write progress and warnings to
        // stderr on a successful run, so no `!stderr.is_empty()` rule may exist
        // here. Twin of `perform_install_ignores_stderr_on_a_clean_exit`, whose
        // string this is.
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
        // The markers must run on the *success* class too, or a short-circuit
        // `if exitcode == 0 { return Ok(…) }` silently passes the failure an
        // exit-code rule cannot see.
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
        // Exact string: without the local `-1` gate the shared classifier's
        // `NotRun` arm answers in the *update* check's vocabulary, on a table
        // that also serves `uninstall`.
        let err = transactional_update(args_for(
            "transactional-update -n pkg install pkg-a",
            "",
            "",
            EXIT_NOT_RUN,
        ))
        .expect_err("a command that never ran must fail the check");
        assert_eq!(err.reason, "command timed out or failed to run");
        assert_eq!(err.host.as_deref(), Some("h1"));
    }

    #[test]
    fn install_timed_out_reason_is_role_neutral_not_updates() {
        // As above: this table also serves `uninstall`, so its `-1` gate must
        // answer role-neutrally, not in the *update* check's vocabulary.
        let err = zypper(args("", "", EXIT_NOT_RUN))
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
        // (c) zypper's informational band does NOT transfer: `100` is "updates
        // available" to zypper and an error to yum/dnf (whose only documented
        // `100` belongs to `check-update`, which no doer here runs), so the
        // shared classifier would pass a failed transaction.
        for code in [
            ZYPPER_EXIT_INF_UPDATE_NEEDED,
            ZYPPER_EXIT_INF_SEC_UPDATE_NEEDED,
            ZYPPER_EXIT_INF_REBOOT_NEEDED,
            ZYPPER_EXIT_INF_RESTART_NEEDED,
            ZYPPER_EXIT_INF_REPO_SKIPPED,
            ZYPPER_EXIT_INF_RPM_SCRIPT_FAILED,
        ] {
            assert!(
                yum(args_for("yum -y install pkg-a", "", "", code)).is_err(),
                "zypper's informational exit {code} is not yum's"
            );
        }
        // (d) And the never-ran sentinel keeps its own reason.
        let err = yum(args_for("yum -y install pkg-a", "", "", EXIT_NOT_RUN)).unwrap_err();
        assert_eq!(err.reason, "command timed out or failed to run");
    }

    #[test]
    fn table_lookup() {
        assert!(install_check("15", false).is_some());
        // Both keys have an installer and an uninstaller; with no check they
        // fell back to the `PlanProvider` adapter's exit-code-only verdict, so
        // no lock/dependency/RPM attribution and no never-ran verdict.
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
        // `is_some()` above pins that the arm exists, not *what* it binds to:
        // every behaviour test here calls the functions directly and all three
        // answer "Unknown Error" on exit `1`, so merging the arms onto one
        // function would leave the suite green. Two transcripts separate them.
        let yum_fn = install_check("YUM", false).expect("the YUM key has an install check");
        // (a) A clean exit carrying a zypper lock marker — the shape a
        // successful `yum install` on a RHEL host can really produce: `yum`
        // reads no markers and passes it, `transactional_update` fails it.
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
        // (b) Exit `100` is a success to `zypper` and a failed transaction to
        // yum/dnf, so it separates those two arms, which (a) cannot.
        assert_eq!(
            yum_fn(args_for(
                "yum -y install pkg-a",
                "",
                "",
                ZYPPER_EXIT_INF_UPDATE_NEEDED
            ))
            .expect_err("zypper's informational band is not yum's")
            .reason,
            "Unknown Error"
        );

        // The same transcript must FAIL on the slmicro arm, which runs the
        // markers on the success class too — so it cannot have been bound to
        // the marker-blind `yum` check.
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
