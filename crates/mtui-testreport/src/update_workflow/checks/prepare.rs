//! Post-prepare check.

use crate::update_workflow::UpdateError;
use crate::update_workflow::checks::{CheckArgs, CheckFn, Diagnostic, EXIT_NOT_RUN, log_failed};

/// The zypper prepare check.
///
/// # Errors
///
/// Returns [`UpdateError`] with a reason of "update stack locked",
/// "Dependency Error", or "RPM Error" depending on the stderr/stdout markers.
/// This check surfaces no [`Diagnostic`]s (only `update` does).
fn zypper(args: CheckArgs<'_>) -> Result<Vec<Diagnostic>, UpdateError> {
    if args
        .stderr
        .contains("A ZYpp transaction is already in progress.")
    {
        log_failed(args);
        return Err(UpdateError::new("update stack locked", args.hostname));
    }
    if args.stderr.contains("System management is locked") {
        log_failed(args);
        return Err(UpdateError::new("update stack locked", args.hostname));
    }
    if args.stdout.contains("(c): c") {
        tracing::error!(
            host = args.hostname,
            stdout = args.stdout,
            "unresolved dependency problem. please resolve manually"
        );
        return Err(UpdateError::new("Dependency Error", args.hostname));
    }
    if args.stderr.contains("Error:") {
        log_failed(args);
        return Err(UpdateError::new("RPM Error", args.hostname));
    }
    Ok(Vec::new())
}

/// The transactional-update (`slmicro`) prepare check.
///
/// Two rules, and the *absence* of a third is the load-bearing part.
///
/// * **`-1`**, mtui's "timed out / connection lost / not connected" sentinel:
///   the transcript is empty, so no marker would catch it. Worth its own
///   verdict even though `host_command_failures` also reports the host —
///   "prepare command failed" says the command ran and returned a failure, `-1`
///   says it did neither, and the operator's next step differs. `prepare_body`
///   drops its coarser entry for a host this check has named, so the sharper
///   reason survives *and* stays attributed.
/// * **The shared stdout/stderr [`markers`](super::update::markers)**, closing
///   the hole this check exists for: `transactional-update` can report a locked
///   update stack, a dependency prompt or an RPM error and still exit `0`, and
///   nothing else in `prepare_body` sees that — the reboot gate reads exit
///   codes and check verdicts, deliberately not stderr — so the host rebooted
///   straight into the failed transaction. The markers'
///   `Error:`-from-a-sibling-command caveat does not arise here: a prepare
///   command is a single `transactional-update` invocation.
///
/// **No bare non-zero-exit arm, on purpose.** `prepare_body` owns exit codes:
/// `note_dispatch` feeds them to the reboot gate and `host_command_failures`
/// reports them accurately, the prepare templates being a single command so a
/// non-zero status *is* the prepare's own. An arm here could only reword that
/// (this key's `transactional-update` returns just `0` or `1`) while changing
/// the reason string
/// `perform_prepare_skips_the_reboot_of_a_host_whose_prepare_failed` pins.
///
/// # Errors
///
/// Returns [`UpdateError`] with a reason of "prepare command timed out or
/// failed to run" (exit `-1`), "update stack locked", "Dependency Error", or
/// "RPM Error".
fn transactional_update(args: CheckArgs<'_>) -> Result<Vec<Diagnostic>, UpdateError> {
    if args.exitcode == EXIT_NOT_RUN {
        log_failed(args);
        return Err(UpdateError::new(
            "prepare command timed out or failed to run",
            args.hostname,
        ));
    }
    super::update::markers(args)
}

/// The yum prepare check.
///
/// Gates **only** on the `-1` sentinel and claims nothing else. Both richer
/// signals are ruled out, by different arguments than on the sibling install
/// check:
///
/// * **The exit code**, which the install check *does* read, is unreadable on
///   this role: `prepare --installed-only` renders `rpm -q $package
///   &>/dev/null && yum … install $package`
///   ([`crate::update_workflow::actions::prepare`]), which exits `1` on the
///   routine "the host does not carry this package" skip that the flag exists
///   to serve, indistinguishably from a failed `yum`. (The zypper and slmicro
///   forms use `if …; then …; fi`, which exits `0` on the same skip.)
/// * **The markers**, for the reason the yum *update* check records: three of
///   the four strings are zypper-only vocabulary and the fourth was written
///   against zypper transcripts.
///
/// # Errors
///
/// Returns [`UpdateError`] with a reason of "prepare command timed out or
/// failed to run" when the command recorded exit `-1`.
fn yum(args: CheckArgs<'_>) -> Result<Vec<Diagnostic>, UpdateError> {
    if args.exitcode == EXIT_NOT_RUN {
        log_failed(args);
        return Err(UpdateError::new(
            "prepare command timed out or failed to run",
            args.hostname,
        ));
    }
    Ok(Vec::new())
}

/// The prepare check for `(release, transactional)`, or `None` for an unknown
/// key.
#[must_use]
pub(crate) fn prepare_check(release: &str, transactional: bool) -> Option<CheckFn> {
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
    use crate::update_workflow::checks::ZYPPER_EXIT_INF_CAP_NOT_FOUND;

    fn args<'a>(stdout: &'a str, stderr: &'a str) -> CheckArgs<'a> {
        CheckArgs {
            hostname: "h1",
            stdout,
            stdin: "zypper -n in -y -l pkg",
            stderr,
            exitcode: 0,
        }
    }

    /// Like [`args`] but with the command text and the exit code spelled out,
    /// for the keys whose verdict depends on either.
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

    /// The rendered `("YUM", false)` prepare templates, so the tests judge the
    /// command the flow really sends rather than a hand-written approximation.
    fn yum_templates() -> (String, String) {
        use std::collections::HashMap;
        let cmds = crate::update_workflow::actions::prepare::preparer("YUM", false, false, false)
            .expect("the YUM key has a preparer");
        let vars: HashMap<&str, &str> = [("package", "pkg-a")].into_iter().collect();
        (
            cmds.render_command(&vars).expect("renders"),
            cmds.render_installed_only(&vars)
                .expect("renders")
                .expect("the YUM key has an installed-only variant"),
        )
    }

    #[test]
    fn clean_output_passes() {
        assert!(zypper(args("ok", "")).is_ok());
    }

    #[test]
    fn zypp_in_progress_is_stack_locked() {
        let err = zypper(args("", "A ZYpp transaction is already in progress.")).unwrap_err();
        assert_eq!(err.reason, "update stack locked");
    }

    #[test]
    fn system_management_locked_is_stack_locked() {
        let err = zypper(args("", "System management is locked")).unwrap_err();
        assert_eq!(err.reason, "update stack locked");
    }

    #[test]
    fn dependency_error_from_stdout() {
        let err = zypper(args("(c): c", "")).unwrap_err();
        assert_eq!(err.reason, "Dependency Error");
    }

    #[test]
    fn rpm_error_from_stderr() {
        let err = zypper(args("", "Error: boom")).unwrap_err();
        assert_eq!(err.reason, "RPM Error");
    }

    #[test]
    fn slmicro_lock_message_on_a_clean_exit_is_stack_locked() {
        // The exact hole #406 names: `transactional-update` exited `0` and
        // still reported a locked stack, which neither `note_dispatch`'s
        // exit-code rule nor `host_command_failures`' stderr rule (ignored by
        // the reboot gate) can see — only the markers.
        let err = transactional_update(args_for(
            "transactional-update -n pkg in -l  pkg-a",
            "",
            "System management is locked",
            0,
        ))
        .expect_err("a locked stack must fail the check whatever the exit code");
        assert_eq!(err.reason, "update stack locked");
        assert_eq!(err.host.as_deref(), Some("h1"));

        // The other two markers reach it on a clean exit too.
        let err = transactional_update(args_for(
            "transactional-update -n pkg in -l  pkg-a",
            "choose (c): c",
            "",
            0,
        ))
        .unwrap_err();
        assert_eq!(err.reason, "Dependency Error");
        let err = transactional_update(args_for(
            "transactional-update -n pkg in -l  pkg-a",
            "",
            "Error: boom",
            0,
        ))
        .unwrap_err();
        assert_eq!(err.reason, "RPM Error");
    }

    #[test]
    fn slmicro_clean_exit_with_progress_on_stderr_passes() {
        // No `!stderr.is_empty()` rule: `transactional-update` writes progress
        // to stderr on a successful run, and failing here would strand the
        // healthy staged snapshot by skipping its reboot. The string is the one
        // `perform_prepare_still_reboots_a_host_that_only_wrote_to_stderr`
        // feeds the flow.
        assert!(
            transactional_update(args_for(
                "transactional-update -n pkg in -l  pkg-a",
                "",
                "1 issue found. see the log",
                0,
            ))
            .is_ok()
        );
    }

    #[test]
    fn slmicro_prepare_does_not_judge_a_bare_non_zero_exit() {
        // Exit codes are `prepare_body`'s to report, and it reports them
        // accurately. A verdict here could only reword "prepare command
        // failed", changing the reason string
        // `perform_prepare_skips_the_reboot_of_a_host_whose_prepare_failed`
        // pins — so a clean transcript with a failing status passes *this*
        // check while the flow still fails the host.
        assert!(
            transactional_update(args_for(
                "transactional-update -n pkg in -l  pkg-a",
                "",
                "",
                1,
            ))
            .is_ok()
        );
    }

    #[test]
    fn timed_out_prepare_raises_on_both_new_keys() {
        // The transcript on `-1` is empty, so without the gate the slmicro
        // key's markers match nothing and the YUM key has no other rule at all
        // — both would report a prepare that never ran as a success.
        for (name, check) in [
            ("slmicro", prepare_check("slmicro", true).unwrap()),
            ("yum", prepare_check("YUM", false).unwrap()),
        ] {
            let Err(err) = check(args_for("some prepare command", "", "", EXIT_NOT_RUN)) else {
                panic!("{name} must fail on exit -1");
            };
            assert_eq!(
                err.reason, "prepare command timed out or failed to run",
                "{name}"
            );
            assert_eq!(err.host.as_deref(), Some("h1"), "{name}");
        }
    }

    #[test]
    fn yum_prepare_judges_only_whether_the_command_ran() {
        let (plain, installed_only) = yum_templates();
        // The installed-only template exits `1` on the routine "not installed
        // here" skip the flag exists for, so a bare non-zero rule would fail
        // every host that legitimately skipped a package.
        assert!(
            installed_only.starts_with("rpm -q pkg-a &>/dev/null &&"),
            "the reason the exit code is unreadable here: {installed_only}"
        );
        assert!(yum(args_for(&installed_only, "", "", 1)).is_ok());
        // No marker copying either: `Error:` is zypper vocabulary, and these
        // strings were never written against a yum transcript.
        assert!(
            yum(args_for(
                &plain,
                "",
                "Error: Cannot retrieve repository metadata for repo 'x'",
                1,
            ))
            .is_ok()
        );
        // Nor are zypper's exit-code classes transferable: `104` means nothing
        // to yum, so the shared classifier would fail this host.
        assert!(yum(args_for(&plain, "", "", ZYPPER_EXIT_INF_CAP_NOT_FOUND)).is_ok());

        // What it does catch: the command never ran to completion.
        let err = yum(args_for(&plain, "", "", EXIT_NOT_RUN)).unwrap_err();
        assert_eq!(err.reason, "prepare command timed out or failed to run");
    }

    #[test]
    fn table_lookup() {
        assert!(prepare_check("11", false).is_some());
        // Both keys have a preparer; with no check `run_checks` skipped them,
        // so a prepare on an SL Micro or RHEL host contributed no verdict — and
        // the transactional reboot gate is fed by check verdicts, so a prepare
        // that failed with exit `0` rebooted into the failed transaction.
        assert!(prepare_check("YUM", false).is_some());
        assert!(prepare_check("slmicro", true).is_some());
        // The key shape still matters — `slmicro` is only ever transactional,
        // and no zypper release is.
        assert!(prepare_check("slmicro", false).is_none());
        assert!(prepare_check("15", true).is_none());
        assert!(prepare_check("nonesuch", false).is_none());
    }

    #[test]
    fn the_new_arms_resolve_to_their_own_check_not_a_sibling() {
        // `is_some()` above pins that the arm exists, not *what* it binds to:
        // every behaviour test here calls the functions directly and both new
        // ones answer identically on `-1`, so merging them onto one function
        // would keep the suite green. A clean exit carrying a zypper lock
        // marker discriminates all three.

        // YUM reads no markers, so this transcript passes; `zypper` and
        // `transactional_update` both fail it, and binding either here would
        // fail a RHEL host whose successful `yum` run printed the string.
        let yum_fn = prepare_check("YUM", false).expect("the YUM key has a prepare check");
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

        // slmicro reads them, so the same transcript fails — ruling out `yum`;
        // `timed_out_prepare_raises_on_both_new_keys` rules out `zypper`, which
        // has no `-1` gate.
        let slmicro_fn =
            prepare_check("slmicro", true).expect("the slmicro key has a prepare check");
        let err = slmicro_fn(args_for(
            "transactional-update -n pkg in -l  pkg-a",
            "",
            "System management is locked",
            0,
        ))
        .expect_err("the slmicro arm must bind the marker-reading check");
        assert_eq!(err.reason, "update stack locked");
    }
}
