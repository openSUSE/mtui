//! Post-update check.
//!
//! The most elaborate check. Its two non-fatal stdout sections ("additional rpm
//! output", "not supported by its vendor") are returned as [`Diagnostic`]s on
//! the `Ok` path and rendered by the command layer through `session.display` —
//! stdout parity without a crate cycle — with a `tracing` breadcrumb alongside.
//! Lock / dependency / RPM failures still raise [`UpdateError`].

use crate::update_workflow::UpdateError;
use crate::update_workflow::checks::{
    CheckArgs, CheckFn, Diagnostic, EXIT_NOT_RUN, ExitClass, ZYPPER_EXIT_INF_CAP_NOT_FOUND,
    ZYPPER_EXIT_INF_REPO_SKIPPED, classify_exit, log_failed,
};

/// The zypper update check.
///
/// The exit code reaching here **is the patch's own** — the template captures
/// `$?` before the post-state `grep` and the repo-cleanup loop can clobber it
/// (see [`crate::update_workflow::actions::update`]) — so [`classify_exit`]
/// judges it, with the same three sets the install check uses. The one status
/// that is *not* the patch's is a probe failure, gated ahead of the
/// classification by [`probe_failure`]: the script exits with the failed
/// probe's status, which shares the patch's numeric space, so only the marker
/// line tells the two apart.
///
/// The informational carve-out is the load-bearing part, not the failure rule:
/// `102` ("reboot needed" after a kernel patch) and `107` (`%post` failed but
/// the package is installed and registered) are routine outcomes, and failing
/// either would fire the **group-wide** rollback downgrade over every healthy
/// host in the group. Every failing status except `104` and the `-1` sentinel
/// gives the stdout/stderr [`markers`] first refusal, because they name the
/// failure where a class label only admits there was one. All of it is gated on
/// the command text containing `zypper` — inert in production, but it keeps
/// these codes from being read as zypper's on a transcript that is not.
///
/// # Errors
///
/// Returns [`UpdateError`] with a reason of "update command timed out or
/// failed to run" (exit `-1`), "could not determine what to patch" (the
/// [`PROBE_FAILURE_MARKER`] line), "package not found" (`104` always; `4`, `5`,
/// `8` on a clean transcript), "update stack locked", "Dependency Error", "RPM
/// Error", or "Unknown Error". The informational exits (`100`-`103`, `106`,
/// `107`) and the two recognised output sections do not fail the check; `106`
/// additionally logs a warning, and the sections are returned as
/// [`Diagnostic`]s.
fn zypper(args: CheckArgs<'_>) -> Result<Vec<Diagnostic>, UpdateError> {
    not_run(args)?;
    probe_failure(args)?;
    if !args.stdin.contains("zypper") {
        return markers(args);
    }
    if args.exitcode == ZYPPER_EXIT_INF_REPO_SKIPPED {
        tracing::warn!(
            host = args.hostname,
            stderr = args.stderr,
            "zypper returns exitcode 106"
        );
    }
    classified(args)
}

/// The transactional-update (`slmicro`) update check.
///
/// Judged on the exit code as well as the markers: the slmicro template
/// captures the `transactional-update -n pkg in … -t patch` status and exits
/// with it, so the verdict is the patch's, not the trailing repo-cleanup loop's
/// `0`.
///
/// It shares zypper's [`classify_exit`], which here reduces to "`0` passes,
/// anything else fails", because **`transactional-update` does not propagate
/// zypper's exit code** — verified against `openSUSE/transactional-update`
/// `sbin/transactional-update.in` @ `aee1e1b5` (v6.1.3, identical back to
/// v2.28.3): zypper's status lands in `RETVAL` (`:1197`), is tested against a
/// hardcoded tolerance list of `0 | 102 | 103 | (106 unless dup)` (`:1214`),
/// otherwise flattened to `EXITCODE=1` (`:1232`), and never reaches the single
/// `exit $EXITCODE` (`:1505`); `man/transactional-update.8.xml` documents only
/// `0`, `1` and `2`, and `2` comes solely from `apply`, which this template
/// does not use. The informational carve-out and the `PackageNotFound` arm are
/// therefore inert here; the classifier is shared anyway so this key cannot
/// drift from the zypper one, and a future version that *did* propagate would
/// be classified correctly from day one.
///
/// Two consequences. `0` is also what `pkg in` returns when its dry run finds
/// nothing to do (`quit 0` at `:1192`), so a clean exit cannot distinguish
/// "patched" from "had nothing to patch". And with `1` classified `Unknown`,
/// this key fails on any non-zero status — firing the **group-wide** rollback,
/// which downgrades and reboots every host in the group — including on failures
/// that are not the patch's (an already-open transaction, a snapshot that could
/// not be created or deleted), which `transactional-update` flattens into that
/// same `1`. Accepted with the blast radius understood: the alternative reports
/// a failed patch as a successful update.
///
/// Unlike [`zypper`] there is no command-text gate — this template carries both
/// tokens, so a guard would only add a second way for the rule to stop firing
/// silently. One accepted residual: the markers' `Error:` rule reads stderr
/// from the whole `exec`, and the sibling commands (`zypper -n lr -puU`, the
/// `zypper -n rr` cleanup loop) can emit it too. Tolerated because every
/// command here is zypper, whose vocabulary [`markers`] was written against,
/// whereas on [`yum`] the same rule would judge a transcript it was not.
///
/// # Errors
///
/// Returns [`UpdateError`] with a reason of "update command timed out or
/// failed to run", "could not determine what to patch", "package not found",
/// "update stack locked", "Dependency Error", "RPM Error", or "Unknown Error".
fn transactional_update(args: CheckArgs<'_>) -> Result<Vec<Diagnostic>, UpdateError> {
    not_run(args)?;
    probe_failure(args)?;
    classified(args)
}

/// Applies [`classify_exit`] to a patch status, interleaved with the
/// stdout/stderr [`markers`].
///
/// Also the install/uninstall check's `("slmicro", true)` verdict
/// ([`super::install`]), which reuses it so the two `transactional-update` keys
/// cannot drift apart; that caller gates `-1` itself, with its own role-neutral
/// reason, so [`ExitClass::NotRun`]'s `update` wording never surfaces on it.
///
/// The interleaving is the whole content: `104` is a named verdict and returns
/// straight away, while every other failing status lets the markers speak first
/// — "update stack locked" is a diagnosis, "Unknown Error" an admission. A
/// success status runs the markers too: a dependency prompt (`(c): c`) leaves
/// the transaction unfinished with a perfectly clean exit status.
///
/// **Only `104` skips them.** Of the `104 | 4 | 5 | 8` class only `104`
/// (`ZYPPER_EXIT_INF_CAP_NOT_FOUND`) means "capability not found"; `4`/`5`/`8`
/// (`ERR_ZYPP`, `ERR_PRIVILEGES`, `ERR_COMMIT`) mean the package *was* found
/// and the transaction failed — `8` with `Error:` on stderr is the likeliest
/// genuinely failed patch, `5` with `System management is locked` a busy update
/// stack — so letting the not-found label outrank the markers made both
/// headline diagnoses wrong, not merely vaguer. The class stays the install
/// check's, unsplit, so no exit code lands in two classes; the *message* is
/// this check's to choose, and nothing branches on these strings.
///
/// `103` (`ZYPPER_EXIT_INF_RESTART_NEEDED`) is a success even though the
/// *remaining* patches are not installed — inherited from the install check's
/// set and left identical so the two keys cannot drift; the template's
/// post-state `zypper -n patches | grep $repa` surfaces it.
///
/// # Errors
///
/// Returns [`UpdateError`] with a reason of "package not found", "update stack
/// locked", "Dependency Error", "RPM Error", "Unknown Error", or — only if a
/// caller reaches here without its [`not_run`] gate — "update command timed out
/// or failed to run".
pub(super) fn classified(args: CheckArgs<'_>) -> Result<Vec<Diagnostic>, UpdateError> {
    match classify_exit(args.exitcode) {
        ExitClass::Success => markers(args),
        ExitClass::PackageNotFound if args.exitcode == ZYPPER_EXIT_INF_CAP_NOT_FOUND => {
            log_failed(args);
            Err(UpdateError::new("package not found", args.hostname))
        }
        ExitClass::PackageNotFound => {
            markers(args)?;
            log_failed(args);
            Err(UpdateError::new("package not found", args.hostname))
        }
        ExitClass::Unknown => {
            markers(args)?;
            log_failed(args);
            Err(UpdateError::new("Unknown Error", args.hostname))
        }
        // Unreachable — both checks gate on `not_run` first — but a class
        // rather than a fallthrough, so a caller that forgets cannot report a
        // host mtui never reached as a failed patch. Deliberately the same
        // error the gate raises; the gate stays because it is the cheaper
        // check and reads at the top of each key.
        ExitClass::NotRun => Err(not_run_error(args)),
    }
}

/// The yum update check.
///
/// Deliberately gates **only** on the [`not_run`] sentinel — narrow is the
/// point on a key whose failures route the **group-wide** rollback downgrade,
/// and neither other signal survives scrutiny. The **exit code**: `("YUM",
/// false)` is not one package manager (`System::get_release` maps *every*
/// `rhel` version to it, and on RHEL 8/9 `yum` is `dnf`, which differs from yum
/// 3 on a spec matching nothing installed) and mtui hands the updater the whole
/// package list while a refhost routinely carries a subset (why `prepare
/// --installed-only` exists), so a bare `!= 0` risks failing a host that
/// upgraded all it had. The **markers**: three of [`markers`]' four strings are
/// zypper-only, and this template runs three commands in one remote `exec`, so
/// the fourth (`Error:` in stderr) would let a GPG complaint from `yum
/// repolist` fail an update whose patch succeeded. Settling either needs
/// observed `yum`/`dnf` output from a real RHEL refhost, not inference.
///
/// # Errors
///
/// Returns [`UpdateError`] with a reason of "update command timed out or
/// failed to run".
fn yum(args: CheckArgs<'_>) -> Result<Vec<Diagnostic>, UpdateError> {
    not_run(args)?;
    Ok(Vec::new())
}

/// Raises when the command never produced a real exit status.
///
/// `Target::run` records `-1` on a timeout, a mid-command connection failure
/// and a target that is not connected at all, so it means "never ran to
/// completion" on every key. Without this gate a timed-out patch reports
/// success: stdout and stderr are both empty, so no marker matches. Mirrors the
/// identical sentinel in the downgrade check.
///
/// # Errors
///
/// Returns [`UpdateError`] with a reason of "update command timed out or
/// failed to run".
fn not_run(args: CheckArgs<'_>) -> Result<(), UpdateError> {
    if args.exitcode == EXIT_NOT_RUN {
        return Err(not_run_error(args));
    }
    Ok(())
}

/// The `-1` verdict, in one place.
///
/// Built here so [`not_run`] and [`classified`]'s [`ExitClass::NotRun`] arm
/// cannot drift into two spellings of the same sentinel. The flow's veto of the
/// group-wide rollback keys on `lastexit()` being `-1`, not on this string; the
/// string only decides what the operator is told, which for a host mtui never
/// reached must not read as a package problem.
fn not_run_error(args: CheckArgs<'_>) -> UpdateError {
    log_failed(args);
    UpdateError::new("update command timed out or failed to run", args.hostname)
}

/// The line the update templates print when a probe failed, and the whole of
/// the contract between them and [`probe_failure`].
///
/// The templates append `": <probe> exited <status>"`, so this is a prefix
/// match on a line. The text is mtui's own — nothing in zypper's or
/// `transactional-update`'s vocabulary resembles it — and reads as a sentence
/// because an operator meets it in a `show_log` transcript.
///
/// The templates assemble it from a `printf` format string *and* its argument
/// so that **this string does not occur in the script's own text**: a
/// transcript that put the command into stdout or stderr (a `set -x` in the
/// host's profile, a verbose transport) would otherwise make every update on
/// that host report a probe failure. Pinned by
/// `the_rendered_update_templates_do_not_trip_the_gate` from this side and
/// `assert_status_comes_from_the_patch` from the template's.
pub(crate) const PROBE_FAILURE_MARKER: &str = "mtui: could not determine what to patch";

/// Raises when the update script could not work out what to patch.
///
/// The templates decide whether to patch from `zypper -n patches`, and a
/// failure there yields an *empty* patch list indistinguishable from a host
/// carrying none of the update's products — so the script names it with
/// [`PROBE_FAILURE_MARKER`] and exits with the probe's own status. Without this
/// gate the script exited `0` and mtui reported an update it never installed
/// (#447).
///
/// Gated on the **marker**, not the exit code, because no exit code could carry
/// it: a POSIX `exit N` truncates to `0..=255`, so the script's status sits
/// inside the package manager's own space. It runs before [`classified`] for
/// that reason, and before [`zypper`]'s `zypper`-token gate for [`not_run`]'s
/// — this is mtui's verdict about its own script, not a reading of a package
/// manager's transcript. Both streams are read: the templates print to stdout,
/// but a transport merging the two would otherwise disarm the gate.
///
/// # Errors
///
/// Returns [`UpdateError`] with a reason of "could not determine what to
/// patch", flagged as a probe failure so the flow routes it away from the
/// group-wide rollback downgrade: the host never ran a patch, so there is
/// nothing half-applied to repair and the rollback would revert every healthy
/// peer.
fn probe_failure(args: CheckArgs<'_>) -> Result<(), UpdateError> {
    let marked = |stream: &str| {
        stream
            .lines()
            .any(|line| line.starts_with(PROBE_FAILURE_MARKER))
    };
    if marked(args.stdout) || marked(args.stderr) {
        log_failed(args);
        return Err(UpdateError::probe_failure(
            "could not determine what to patch",
            args.hostname,
        ));
    }
    Ok(())
}

/// The stdout/stderr classification shared by every update check.
///
/// Exit-code handling stays in the per-key wrappers because it differs by key
/// (see [`transactional_update`]); everything here reads only the command's
/// output, so it is valid on all of them.
///
/// Also the `("slmicro", true)` *prepare* check's whole verdict beyond its `-1`
/// gate ([`super::prepare`]), where it closes the exit-`0`-with-a-lock-message
/// hole the prepare reboot gate cannot see. The `Error:`-from-a-sibling-command
/// exposure documented on [`transactional_update`] does not arise there: the
/// prepare and install templates are a *single* command.
///
/// # Errors
///
/// Returns [`UpdateError`] with a reason of "update stack locked",
/// "Dependency Error", or "RPM Error".
pub(super) fn markers(args: CheckArgs<'_>) -> Result<Vec<Diagnostic>, UpdateError> {
    let mut diagnostics = Vec::new();
    if let Some(section) = extract_between(args.stdout, "Additional rpm output:", "Retrieving") {
        tracing::warn!(host = args.hostname, "There was additional rpm output");
        diagnostics.push(Diagnostic::highlighted(section));
    }
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
        // Carries `command` and `stderr` too, matching the sibling branches'
        // `log_failed` record: this branch is reachable on `4`/`5`/`8`, the
        // codes an operator is most likely to be investigating.
        tracing::error!(
            host = args.hostname,
            command = args.stdin,
            stdout = args.stdout,
            stderr = args.stderr,
            "unresolved dependency problem. please resolve manually"
        );
        return Err(UpdateError::new("Dependency Error", args.hostname));
    }
    if args.stderr.contains("Error:") {
        log_failed(args);
        return Err(UpdateError::new("RPM Error", args.hostname));
    }
    if let Some(section) = extract_between(
        args.stdout,
        "The following package is not supported by its vendor:\n",
        "\n\n",
    ) {
        // Re-prepends the marker line, which `extract_between` slices off.
        tracing::warn!(host = args.hostname, "package support is uncertain");
        diagnostics.push(Diagnostic::plain(format!(
            "The following package is not supported by its vendor:\n{section}"
        )));
    }
    Ok(diagnostics)
}

/// Returns the substring of `s` from just after `marker` up to the next
/// occurrence of `end` (searched from the marker); `None` when `marker` is
/// absent.
///
/// If `end` is not found this falls back to `len - 1`, matching a slice indexed
/// by a `-1` "not found" sentinel.
fn extract_between<'a>(s: &'a str, marker: &str, end: &str) -> Option<&'a str> {
    let m = s.find(marker)?;
    let start = m + marker.len();
    let rest = &s[start..];
    let stop = match rest.find(end) {
        Some(rel) => start + rel,
        // `end` not found: fall back to slice `[start:-1]`.
        None => s.len().saturating_sub(1).max(start),
    };
    Some(&s[start..stop])
}

/// The update check for `(release, transactional)`, or `None` for an unknown
/// key.
#[must_use]
pub(crate) fn update_check(release: &str, transactional: bool) -> Option<CheckFn> {
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
        ZYPPER_EXIT_INF_REBOOT_NEEDED, ZYPPER_EXIT_INF_RESTART_NEEDED,
        ZYPPER_EXIT_INF_RPM_SCRIPT_FAILED, ZYPPER_EXIT_INF_SEC_UPDATE_NEEDED,
        ZYPPER_EXIT_INF_UPDATE_NEEDED,
    };

    fn args<'a>(stdin: &'a str, stdout: &'a str, stderr: &'a str, exitcode: i32) -> CheckArgs<'a> {
        CheckArgs {
            hostname: "h1",
            stdout,
            stdin,
            stderr,
            exitcode,
        }
    }

    #[test]
    fn zypper_104_is_package_not_found() {
        // Matches the install check's verdict for the same code.
        let err = zypper(args(
            "zypper -n patch",
            "",
            "",
            ZYPPER_EXIT_INF_CAP_NOT_FOUND,
        ))
        .unwrap_err();
        assert_eq!(err.reason, "package not found");
    }

    #[test]
    fn non_zypper_104_is_not_package_not_found() {
        // 104 is only read as zypper's when the command text is zypper's.
        assert!(zypper(args("yum update", "", "", ZYPPER_EXIT_INF_CAP_NOT_FOUND)).is_ok());
    }

    #[test]
    fn zypp_in_progress_is_stack_locked() {
        let err = zypper(args(
            "zypper",
            "",
            "A ZYpp transaction is already in progress.",
            1,
        ))
        .unwrap_err();
        assert_eq!(err.reason, "update stack locked");
    }

    #[test]
    fn system_management_locked_is_stack_locked() {
        let err = zypper(args("zypper", "", "System management is locked", 1)).unwrap_err();
        assert_eq!(err.reason, "update stack locked");
    }

    #[test]
    fn dependency_error_from_stdout() {
        let err = zypper(args("zypper", "(c): c", "", 1)).unwrap_err();
        assert_eq!(err.reason, "Dependency Error");
    }

    #[test]
    fn rpm_error_from_stderr() {
        let err = zypper(args("zypper", "", "Error: boom", 1)).unwrap_err();
        assert_eq!(err.reason, "RPM Error");
    }

    #[test]
    fn clean_output_passes() {
        assert!(zypper(args("zypper", "all good", "", 0)).is_ok());
    }

    #[test]
    fn warnings_do_not_fail_the_check() {
        let stdout = "before Additional rpm output:\nwarning: stuff\nRetrieving repo\nafter";
        // exit 106 warn + additional rpm output warn, still Ok.
        assert!(zypper(args("zypper", stdout, "", ZYPPER_EXIT_INF_REPO_SKIPPED)).is_ok());
    }

    #[test]
    fn additional_rpm_output_returned_as_highlighted_diagnostic() {
        let stdout = "before Additional rpm output:\nwarning: stuff\nRetrieving repo\nafter";
        let diags = zypper(args("zypper", stdout, "", ZYPPER_EXIT_INF_REPO_SKIPPED)).unwrap();
        assert_eq!(diags.len(), 1);
        assert!(diags[0].highlight_warning);
        // Check output, not a degradation: the caller counts degradations to
        // decide whether the run was whole (#534 review).
        assert!(!diags[0].degradation);
        assert_eq!(diags[0].text, "\nwarning: stuff\n");
    }

    #[test]
    fn vendor_section_returned_as_plain_diagnostic_with_marker() {
        let stdout =
            "x\nThe following package is not supported by its vendor:\nfoo bar\n\ntrailing";
        let diags = zypper(args("zypper", stdout, "", 0)).unwrap();
        assert_eq!(diags.len(), 1);
        assert!(!diags[0].highlight_warning);
        // Routine on a healthy update — mtui patches from a test update repo
        // whose vendor differs from the official one — so flagging it as a
        // degradation would qualify almost every confirmation (#534 review).
        assert!(!diags[0].degradation);
        assert_eq!(
            diags[0].text,
            "The following package is not supported by its vendor:\nfoo bar"
        );
    }

    #[test]
    fn clean_output_returns_no_diagnostics() {
        assert!(
            zypper(args("zypper", "all good", "", 0))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn extract_between_returns_middle_section() {
        let s = "x Additional rpm output:\nHELLO\nRetrieving y";
        let got = extract_between(s, "Additional rpm output:", "Retrieving");
        assert_eq!(got, Some("\nHELLO\n"));
    }

    #[test]
    fn extract_between_absent_marker_is_none() {
        assert_eq!(
            extract_between("nothing here", "Additional rpm output:", "Retrieving"),
            None
        );
    }

    #[test]
    fn table_lookup() {
        assert!(update_check("12", false).is_some());
        // Both keys have an updater; with no check, `update` on them reported
        // success whatever happened.
        assert!(update_check("YUM", false).is_some());
        assert!(update_check("slmicro", true).is_some());
        // The key shape still matters — `slmicro` is only ever transactional,
        // and no zypper release is.
        assert!(update_check("slmicro", false).is_none());
        assert!(update_check("15", true).is_none());
        assert!(update_check("nonesuch", false).is_none());
    }

    #[test]
    fn timed_out_command_fails_on_every_key() {
        // Without the gate a timed-out patch passes on every key: stdout and
        // stderr are both empty, so no marker matches.
        for (name, check) in [
            ("zypper", update_check("15", false).unwrap()),
            ("yum", update_check("YUM", false).unwrap()),
            ("transactional", update_check("slmicro", true).unwrap()),
        ] {
            let Err(err) = check(args("some update command", "", "", -1)) else {
                panic!("{name} must fail on exit -1");
            };
            assert_eq!(
                err.reason, "update command timed out or failed to run",
                "{name}"
            );
        }
    }

    #[test]
    fn transactional_update_fails_on_an_unrecognised_non_zero_exit() {
        // The slmicro template exits with the patch's own status, so a non-zero
        // code with clean output is a failed patch, not the cleanup loop's.
        let err = transactional_update(args("transactional-update -n pkg in", "all good", "", 1))
            .expect_err("a failed patch with clean output must fail the check");
        assert_eq!(err.reason, "Unknown Error");
        assert_eq!(err.host.as_deref(), Some("h1"));
    }

    #[test]
    fn transactional_update_still_catches_the_markers() {
        // The other half of the verdict: exit `0` is the shape the exit-code
        // rule cannot see — a run that reported a problem and still succeeded.
        let err = transactional_update(args(
            "transactional-update -n pkg in",
            "",
            "System management is locked",
            0,
        ))
        .unwrap_err();
        assert_eq!(err.reason, "update stack locked");

        let err = transactional_update(args("transactional-update", "(c): c", "", 0)).unwrap_err();
        assert_eq!(err.reason, "Dependency Error");

        let err =
            transactional_update(args("transactional-update", "", "Error: boom", 0)).unwrap_err();
        assert_eq!(err.reason, "RPM Error");
    }

    #[test]
    fn yum_judges_only_whether_the_command_ran() {
        // Both of these would fail a host that updated fine, and route the
        // group-wide rollback downgrade — see `yum`'s doc for why.
        assert!(yum(args("yum -y update pkg-a pkg-b", "Upgraded: pkg-a", "", 1)).is_ok());
        assert!(
            yum(args(
                "yum -y update pkg-a",
                "",
                "Error: Cannot retrieve repository metadata for repo 'x'",
                0,
            ))
            .is_ok()
        );

        // What it does catch: the command never ran to completion.
        let err = yum(args("yum -y update pkg-a", "", "", -1)).unwrap_err();
        assert_eq!(err.reason, "update command timed out or failed to run");
    }

    #[test]
    fn zypper_fails_on_an_unrecognised_non_zero_exit() {
        // The zypper template exits with the patch's own status, so an
        // unrecognised non-zero code is the patch's verdict, not the cleanup
        // loop's.
        let err = zypper(args("zypper -n in -t patch", "all good", "", 7))
            .expect_err("a failed patch with clean output must fail the check");
        assert_eq!(err.reason, "Unknown Error");
        assert_eq!(err.host.as_deref(), Some("h1"));
    }

    #[test]
    fn informational_exit_codes_pass_on_both_zypper_keys() {
        // The carve-out that makes the exit-code rule safe at all: `102`
        // ("reboot needed"), `100`/`101` ("(security) updates available", left
        // behind whenever the host carries more than this update) and `107`
        // (installed and registered, `%post` failed) are routine, and failing
        // any of them fires the group-wide rollback downgrade. The whole band
        // is listed, not a sample — this and the classifier's own test are the
        // two places a code can be forgotten, and `107` once was.
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
                zypper(args("zypper -n in -t patch", "all good", "", code)).is_ok(),
                "zypper must pass on informational exit {code}"
            );
            assert!(
                transactional_update(args("transactional-update -n pkg in", "all good", "", code))
                    .is_ok(),
                "transactional-update must pass on informational exit {code}"
            );
        }
    }

    #[test]
    fn package_not_found_codes_on_both_zypper_keys() {
        // The `104 | 4 | 5 | 8` grouping is the install check's, reproduced
        // exactly, so no exit code lands in two classes. These transcripts are
        // clean, which is when the class label is also the message; see
        // `only_104_outranks_the_markers` for when it is not.
        for code in [
            ZYPPER_EXIT_INF_CAP_NOT_FOUND,
            ZYPPER_EXIT_ERR_ZYPP,
            ZYPPER_EXIT_ERR_PRIVILEGES,
            ZYPPER_EXIT_ERR_COMMIT,
        ] {
            let err = zypper(args("zypper -n in -t patch", "", "", code)).unwrap_err();
            assert_eq!(err.reason, "package not found", "zypper exit {code}");
            let err = transactional_update(args("transactional-update -n pkg in", "", "", code))
                .unwrap_err();
            assert_eq!(
                err.reason, "package not found",
                "transactional-update exit {code}"
            );
        }
    }

    #[test]
    fn the_rendered_update_templates_reach_the_exit_code_rules() {
        // `zypper`'s classification is gated on the literal token `zypper` in
        // the command text, and the gate and the template are two separate
        // files — so this feeds the check the *real* rendered template. A token
        // assertion inside the template's own test would pin it against itself.
        let vars = [("repa", ":p=42:7"), ("packages", "pkg-a")];
        for (key, transactional, check) in [
            (
                "15",
                false,
                Box::new(zypper) as Box<dyn Fn(CheckArgs<'_>) -> _>,
            ),
            ("slmicro", true, Box::new(transactional_update)),
        ] {
            let rendered = crate::update_workflow::actions::update::updater(key, transactional)
                .expect("both keys have an updater")
                .render_command(&vars.into_iter().collect())
                .expect("safe substitution never fails");

            let err = check(args(&rendered, "", "", ZYPPER_EXIT_INF_CAP_NOT_FOUND)).unwrap_err();
            assert_eq!(err.reason, "package not found", "{key}");
            let err = check(args(&rendered, "", "", 7)).unwrap_err();
            assert_eq!(err.reason, "Unknown Error", "{key}");
            assert!(
                check(args(&rendered, "", "", ZYPPER_EXIT_INF_REBOOT_NEEDED)).is_ok(),
                "{key}: 102 is 'reboot needed'"
            );
        }
    }

    #[test]
    fn only_104_outranks_the_markers() {
        // `104` is the one member of its class whose verdict a marker cannot
        // improve on, so it short-circuits even with a marker present.
        let err = zypper(args(
            "zypper",
            "",
            "System management is locked",
            ZYPPER_EXIT_INF_CAP_NOT_FOUND,
        ))
        .unwrap_err();
        assert_eq!(err.reason, "package not found", "104 outranks the marker");

        // `4`/`5`/`8` mean the package was found and the transaction failed, so
        // the transcript names them rather than the class label. Restoring the
        // blanket short-circuit turns all of these into "package not found".
        let err = zypper(args(
            "zypper",
            "",
            "System management is locked",
            ZYPPER_EXIT_ERR_PRIVILEGES,
        ))
        .unwrap_err();
        assert_eq!(err.reason, "update stack locked", "5 + lock marker");
        let err = zypper(args(
            "zypper",
            "",
            "A ZYpp transaction is already in progress.",
            ZYPPER_EXIT_ERR_ZYPP,
        ))
        .unwrap_err();
        assert_eq!(err.reason, "update stack locked", "4 + ZYpp marker");
        let err = zypper(args("zypper", "(c): c", "", ZYPPER_EXIT_ERR_ZYPP)).unwrap_err();
        assert_eq!(err.reason, "Dependency Error", "4 + dependency prompt");
        let err = transactional_update(args(
            "transactional-update",
            "",
            "Error: boom",
            ZYPPER_EXIT_ERR_COMMIT,
        ))
        .unwrap_err();
        assert_eq!(err.reason, "RPM Error", "8 + rpm marker");

        // The install check shares this classifier, so it must answer
        // identically on every row above, or the two can re-diverge.
        let install_zypper = crate::update_workflow::checks::install::install_check("15", false)
            .expect("the zypper key has an install check");
        for (stdout, stderr, code, expected) in [
            (
                "",
                "System management is locked",
                ZYPPER_EXIT_INF_CAP_NOT_FOUND,
                "package not found",
            ),
            (
                "",
                "System management is locked",
                ZYPPER_EXIT_ERR_PRIVILEGES,
                "update stack locked",
            ),
            (
                "",
                "A ZYpp transaction is already in progress.",
                ZYPPER_EXIT_ERR_ZYPP,
                "update stack locked",
            ),
            ("(c): c", "", ZYPPER_EXIT_ERR_ZYPP, "Dependency Error"),
            ("", "Error: boom", ZYPPER_EXIT_ERR_COMMIT, "RPM Error"),
        ] {
            let err = install_zypper(args("zypper", stdout, stderr, code)).unwrap_err();
            assert_eq!(err.reason, expected, "install exit {code}");
        }

        // The clean-transcript verdicts are already pinned by
        // `package_not_found_codes_on_both_zypper_keys`.
    }

    #[test]
    fn the_probe_marker_outranks_the_exit_code_rules() {
        // #447. The script exits with the *probe's* status, which shares its
        // numeric space with the patch's, so the marker has to be read before
        // `classify_exit` gets the code. Each code below is one the classifier
        // has a confident and here wrong answer for; `0` matters most, being a
        // probe failure whose status the transport lost.
        let marker = format!("{PROBE_FAILURE_MARKER}: zypper -n patches exited 6");
        for code in [
            0,
            7,
            ZYPPER_EXIT_INF_CAP_NOT_FOUND,
            ZYPPER_EXIT_INF_REPO_SKIPPED,
        ] {
            for (name, check) in [
                ("zypper", update_check("15", false).unwrap()),
                ("transactional", update_check("slmicro", true).unwrap()),
            ] {
                let err = check(args("zypper -n in -t patch", &marker, "", code))
                    .expect_err("a probe failure must fail the check");
                assert_eq!(
                    err.reason, "could not determine what to patch",
                    "{name}: exit {code}"
                );
                assert_eq!(err.host.as_deref(), Some("h1"), "{name}: exit {code}");
            }
        }
    }

    #[test]
    fn the_probe_marker_is_read_from_either_stream() {
        // The templates print it to stdout; a transport that merged the two
        // streams would otherwise silently disarm the gate.
        let marker = format!("{PROBE_FAILURE_MARKER}: zypper -n refresh exited 4");
        let err = zypper(args("zypper", "", &marker, 4)).unwrap_err();
        assert_eq!(err.reason, "could not determine what to patch");
    }

    #[test]
    fn the_probe_marker_does_not_fire_on_an_ordinary_transcript() {
        // "Nothing to do" is not a failure: a host carrying none of the
        // update's products runs the same script, skips the patch and exits `0`
        // with no marker. Nowhere else does an empty result read as a failure.
        assert!(
            zypper(args("zypper -n in -t patch", "", "", 0))
                .unwrap()
                .is_empty()
        );
        // Nor on a transcript that merely talks about patching.
        assert!(
            zypper(args(
                "zypper -n in -t patch",
                "The following 1 patch is going to be installed:\n  patch-alpha\n",
                "",
                0,
            ))
            .is_ok()
        );
    }

    #[test]
    fn the_probe_marker_is_flagged_for_the_flow_to_route_on() {
        // The reason string is for the operator; the *flag* is what
        // `reports::update_flow` routes on, and routing it wrong fires the
        // group-wide rollback over a host that never ran a patch. Asserted here
        // because in the flow a `false` still produces a plausible error.
        let marker = format!("{PROBE_FAILURE_MARKER}: zypper -n patches exited 6");
        let err = zypper(args("zypper", &marker, "", 6)).unwrap_err();
        assert!(err.probe_failed, "the probe-failure flag must be set");
        // And nowhere else: a patch that genuinely failed is the rollback's.
        let err = zypper(args("zypper", "", "", 7)).unwrap_err();
        assert!(
            !err.probe_failed,
            "a failed patch is not a probe failure: {err:?}"
        );
        let err = zypper(args("zypper", "", "", -1)).unwrap_err();
        assert!(
            !err.probe_failed,
            "a command that never ran is not a probe failure: {err:?}"
        );
    }

    #[test]
    fn the_rendered_update_templates_do_not_trip_the_gate() {
        // The script *text* must not contain the marker, only its output on a
        // real failure: a host whose profile carries a `set -x` puts the script
        // into stdout/stderr too, which would turn every healthy update into
        // "could not determine what to patch". Fed on all three fields, because
        // the gate reads two and the third is where the script lives.
        let vars = [("repa", ":p=42:7"), ("packages", "pkg-a")];
        for (key, transactional, check) in [
            (
                "15",
                false,
                Box::new(zypper) as Box<dyn Fn(CheckArgs<'_>) -> _>,
            ),
            ("slmicro", true, Box::new(transactional_update)),
        ] {
            let rendered = crate::update_workflow::actions::update::updater(key, transactional)
                .expect("both keys have an updater")
                .render_command(&vars.into_iter().collect())
                .expect("safe substitution never fails");
            assert!(
                !rendered.contains(PROBE_FAILURE_MARKER),
                "{key}: the script text must not contain the marker: {rendered}"
            );
            assert!(
                check(args(&rendered, &rendered, &rendered, 0)).is_ok(),
                "{key}: a transcript echoing the script must not read as a probe failure"
            );
        }
        // That the template *prints* this exact string on a probe failure is
        // pinned by `actions::update`'s `rendered_script` cases, which run the
        // real script under a real `/bin/sh`.
    }

    #[test]
    fn the_probe_marker_is_matched_in_full() {
        // Weakened to any prefix (say `"mtui:"`) the gate still passes every
        // other test here, and fires on any mtui-namespaced line.
        for stdout in [
            "mtui: could not remove the test update repo\n",
            "mtui: nothing to do\n",
            "mtui_probe_ok: not found\n",
        ] {
            assert!(
                zypper(args("zypper -n in -t patch", stdout, "", 0)).is_ok(),
                "an unrelated mtui line must not read as a probe failure: {stdout}"
            );
        }

        // The other direction: matched at the *start of a line*. The
        // split-`printf` trick keeps the script's own text from carrying the
        // sentence, but nothing stops a host echoing it mid-line, so only a
        // line that begins with it is mtui's own verdict.
        for stdout in [
            format!("+ echo '{PROBE_FAILURE_MARKER}: zypper -n patches exited 6'\n"),
            format!("2026-08-13 log: saw \"{PROBE_FAILURE_MARKER}\" on the last run\n"),
            format!("   {PROBE_FAILURE_MARKER}: indented, so not ours\n"),
        ] {
            assert!(
                zypper(args("zypper -n in -t patch", &stdout, "", 0)).is_ok(),
                "the marker mid-line must not read as a probe failure: {stdout}"
            );
        }

        // A line that really begins with it still fires, on either stream — or
        // the guard above would have disarmed the gate entirely.
        let marker = format!("{PROBE_FAILURE_MARKER}: zypper -n patches exited 6\n");
        for (out, err) in [(marker.as_str(), ""), ("", marker.as_str())] {
            let err = zypper(args("zypper -n in -t patch", out, err, 0)).unwrap_err();
            assert_eq!(err.reason, "could not determine what to patch");
        }
    }

    #[test]
    fn the_probe_marker_outranks_the_zypper_token_gate() {
        // `probe_failure` runs *before* the `zypper`-token gate: like
        // `not_run`, it is mtui's verdict about its own script. Every other
        // fixture here carries `zypper` in `stdin`, so only this one notices
        // the gate being moved below the token check.
        let marker = format!("{PROBE_FAILURE_MARKER}: zypper -n patches exited 6");
        let err = zypper(args("some update command", &marker, "", 6))
            .expect_err("the marker must be read whatever the command text says");
        assert_eq!(err.reason, "could not determine what to patch");
        assert!(err.probe_failed);
    }

    #[test]
    fn the_never_ran_class_cannot_be_read_as_a_package_failure() {
        // Unreachable through `zypper`/`transactional_update`, which gate on
        // `not_run` first, so this calls `classified` directly. Falling through
        // to `Unknown` reports a host mtui never contacted as a failed patch —
        // the wrong diagnosis, though not a wrong rollback: `never_ran` in
        // `reports::update_flow` reads the target's `lastexit()`.
        let err = classified(args("zypper", "", "", -1)).unwrap_err();
        assert_eq!(err.reason, "update command timed out or failed to run");

        // Both routes give the same string, the point of `not_run_error`. It
        // cannot detect the gate's removal (the arm is identical by design);
        // what it pins is that the two never drift apart.
        for reached in [
            zypper(args("zypper", "", "", -1)),
            transactional_update(args("transactional-update", "", "", -1)),
        ] {
            assert_eq!(reached.unwrap_err().reason, err.reason);
        }
    }

    #[test]
    fn markers_outrank_the_unknown_error_fallback() {
        // "Unknown Error" only says a run failed; the markers say why. A
        // fallback placed before them silently replaces every named reason.
        let err = zypper(args("zypper", "", "System management is locked", 1)).unwrap_err();
        assert_eq!(err.reason, "update stack locked");
        let err = zypper(args("zypper", "(c): c", "", 1)).unwrap_err();
        assert_eq!(err.reason, "Dependency Error");
        let err = zypper(args("zypper", "", "Error: boom", 1)).unwrap_err();
        assert_eq!(err.reason, "RPM Error");

        let err =
            transactional_update(args("transactional-update", "", "Error: boom", 1)).unwrap_err();
        assert_eq!(err.reason, "RPM Error");
    }
}
