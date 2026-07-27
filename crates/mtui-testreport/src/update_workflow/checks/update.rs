//! Post-update check.
//!
//! The most elaborate check: it surfaces diagnostic sections for "additional
//! rpm output" and "not supported by its vendor" (printed to the terminal,
//! one with the word `warning` highlighted yellow). To reproduce that stdout
//! parity without a crate cycle, the sections are returned as
//! [`Diagnostic`]s on the `Ok` path and rendered by the command layer through
//! `session.display`; a diagnostic breadcrumb is logged via `tracing`
//! alongside. Lock / dependency / RPM failures still raise [`UpdateError`].

use crate::update_workflow::UpdateError;
use crate::update_workflow::checks::{CheckArgs, CheckFn, Diagnostic, log_failed};

/// The zypper update check.
///
/// # Errors
///
/// Returns [`UpdateError`] with a reason of "update command timed out or
/// failed to run" (exit `-1`), "package not found" (zypper exit `104`),
/// "update stack locked", "Dependency Error", or "RPM Error"
/// depending on the exit code and stderr/stdout markers. Warnings
/// (exit `106`, "Additional rpm output", "not supported by its vendor") do not
/// fail the check; the two output sections are returned as [`Diagnostic`]s for
/// the caller to render.
///
/// Note this check does **not** treat an unrecognised non-zero exit as a
/// failure: the zypper update template ends with the same repo-cleanup loop
/// that masks the patch status on `slmicro` (see [`transactional_update`]), so
/// the code reaching here is not the patch's either. It is judged on the
/// markers plus the two exit codes zypper is known to surface.
fn zypper(args: CheckArgs<'_>) -> Result<Vec<Diagnostic>, UpdateError> {
    not_run(args)?;
    if args.stdin.contains("zypper") && args.exitcode == 104 {
        log_failed(args);
        return Err(UpdateError::new("package not found", args.hostname));
    }
    if args.stdin.contains("zypper") && args.exitcode == 106 {
        tracing::warn!(
            host = args.hostname,
            stderr = args.stderr,
            "zypper returns exitcode 106"
        );
    }
    markers(args)
}

/// The transactional-update (`slmicro`) update check.
///
/// Deliberately has **no exit-code rule beyond the
/// [`not_run`] sentinel**, because `lastexit()` does not carry the patch
/// command's status on this key: the `slmicro` update template is a multi-line
/// script executed as a single remote `exec`, and its last line is the
/// `zypper -n lr | … | while read r; do zypper -n rr $r; done` repo-cleanup
/// loop. The shell therefore reports *that loop's* status — which is `0`
/// whenever the loop body never runs — and discards the
/// `transactional-update -n pkg in … -t patch` status entirely.
///
/// Judging this key on the exit code would be wrong in both directions: a
/// failed patch still exits `0`, and a hiccup in the cosmetic repo cleanup
/// would be reported as a failed update — which, because an `update` check
/// failure routes the **group-wide** rollback downgrade, would roll back the
/// whole fleet over a no-op. The stdout/stderr markers below are unaffected by
/// the masking and are what this key is judged on.
///
/// # Errors
///
/// Returns [`UpdateError`] with a reason of "update command timed out or
/// failed to run", "update stack locked", "Dependency Error", or "RPM Error".
fn transactional_update(args: CheckArgs<'_>) -> Result<Vec<Diagnostic>, UpdateError> {
    not_run(args)?;
    markers(args)
}

/// The yum update check.
///
/// Unlike [`transactional_update`], this key *may* be judged on its exit code:
/// the `YUM` update template's last line **is** `yum -y update $packages`, so
/// the recorded status is genuinely the updater's. `yum` has no informational
/// non-zero success code for `update` (`100` is `check-update`-only), so any
/// non-zero status is a real failure.
///
/// A recognised marker still wins over the generic verdict, so a locked stack
/// or an RPM error keeps its specific reason.
///
/// # Errors
///
/// Returns [`UpdateError`] with a reason of "update command timed out or
/// failed to run", one of [`markers`]' reasons, or "update command failed" for
/// an unrecognised non-zero exit.
fn yum(args: CheckArgs<'_>) -> Result<Vec<Diagnostic>, UpdateError> {
    not_run(args)?;
    let diagnostics = markers(args)?;
    if args.exitcode != 0 {
        log_failed(args);
        return Err(UpdateError::new("update command failed", args.hostname));
    }
    Ok(diagnostics)
}

/// Raises when the command never produced a real exit status.
///
/// `Target::run` records `-1` when a command times out, when the connection
/// fails mid-command, and when the target is not connected at all, so `-1`
/// means "this never ran to completion" on every key. Without this gate a
/// timed-out patch reports success: the recorded stdout and stderr are both
/// empty, so no marker matches and the check returns `Ok`.
///
/// Mirrors the identical sentinel in the downgrade check.
///
/// # Errors
///
/// Returns [`UpdateError`] with a reason of "update command timed out or
/// failed to run".
fn not_run(args: CheckArgs<'_>) -> Result<(), UpdateError> {
    if args.exitcode == -1 {
        log_failed(args);
        return Err(UpdateError::new(
            "update command timed out or failed to run",
            args.hostname,
        ));
    }
    Ok(())
}

/// The stdout/stderr classification shared by every update check.
///
/// Exit-code handling stays in the per-key wrappers because it differs by key
/// (see [`transactional_update`]); everything here reads only the command's
/// output and so is valid on all of them.
///
/// # Errors
///
/// Returns [`UpdateError`] with a reason of "update stack locked",
/// "Dependency Error", or "RPM Error".
fn markers(args: CheckArgs<'_>) -> Result<Vec<Diagnostic>, UpdateError> {
    let mut diagnostics = Vec::new();
    if let Some(section) = extract_between(args.stdout, "Additional rpm output:", "Retrieving") {
        // Logs a breadcrumb, then prints the section with "warning"
        // highlighted yellow.
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
    if let Some(section) = extract_between(
        args.stdout,
        "The following package is not supported by its vendor:\n",
        "\n\n",
    ) {
        // Logs `package support is uncertain`, then prints the section plain
        // (no recoloring). Reconstruct the marker line kept by the
        // `stdout[start:end]` slice (`start` sits *at* the marker).
        tracing::warn!(host = args.hostname, "package support is uncertain");
        diagnostics.push(Diagnostic::plain(format!(
            "The following package is not supported by its vendor:\n{section}"
        )));
    }
    Ok(diagnostics)
}

/// Returns the substring of `s` starting just after `marker` up to the next
/// occurrence of `end` (searched from the marker), as a `stdout[start:end]`
/// slice. `None` when `marker` is absent.
///
/// The section retained *includes* everything from just past `marker` to the
/// first `end`; if `end` is not found, this falls back to `len - 1` (all but
/// the last character), matching a slice indexed by a `-1` "not found"
/// sentinel.
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
        // zypper + exit 104 is "package not found"
        // (ZYPPER_EXIT_INF_CAP_NOT_FOUND), matching the install check.
        let err = zypper(args("zypper -n patch", "", "", 104)).unwrap_err();
        assert_eq!(err.reason, "package not found");
    }

    #[test]
    fn non_zypper_104_does_not_trip_lock_branch() {
        // 104 only means "locked" when the command was a zypper invocation.
        assert!(zypper(args("yum update", "", "", 104)).is_ok());
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
        assert!(zypper(args("zypper", stdout, "", 106)).is_ok());
    }

    #[test]
    fn additional_rpm_output_returned_as_highlighted_diagnostic() {
        let stdout = "before Additional rpm output:\nwarning: stuff\nRetrieving repo\nafter";
        let diags = zypper(args("zypper", stdout, "", 106)).unwrap();
        assert_eq!(diags.len(), 1);
        assert!(diags[0].highlight_warning);
        assert_eq!(diags[0].text, "\nwarning: stuff\n");
    }

    #[test]
    fn vendor_section_returned_as_plain_diagnostic_with_marker() {
        let stdout =
            "x\nThe following package is not supported by its vendor:\nfoo bar\n\ntrailing";
        let diags = zypper(args("zypper", stdout, "", 0)).unwrap();
        assert_eq!(diags.len(), 1);
        assert!(!diags[0].highlight_warning);
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
        // Both keys that have an updater but used to have no check at all:
        // an `update` on them reported success whatever happened.
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
        // `-1` is `Target::run`'s catch-all for "timed out", "connection
        // failed mid-command" and "not connected". Before this gate existed a
        // timed-out patch reported success on every key: stdout and stderr are
        // both empty, so no marker matched and the check returned `Ok`.
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
    fn transactional_update_ignores_the_masked_exit_code() {
        // The slmicro template's last line is the repo-cleanup loop, so
        // `lastexit()` is that loop's status, not the patch's. Judging this
        // key on the exit code would roll the whole group back over a cosmetic
        // cleanup hiccup, so a non-zero code with clean output must pass.
        let diags = transactional_update(args("transactional-update -n pkg in", "all good", "", 1))
            .expect("a masked non-zero exit must not fail the transactional check");
        assert!(diags.is_empty());
    }

    #[test]
    fn transactional_update_still_catches_the_markers() {
        // What it *is* judged on: the markers survive the exit-code masking.
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
    fn yum_is_judged_on_its_exit_code() {
        // The YUM template's last line *is* `yum -y update`, so unlike
        // slmicro the recorded status is genuinely the updater's.
        let err = yum(args("yum -y update pkg-a", "", "", 1)).unwrap_err();
        assert_eq!(err.reason, "update command failed");
        assert!(yum(args("yum -y update pkg-a", "all good", "", 0)).is_ok());
    }

    #[test]
    fn yum_prefers_a_recognised_marker_over_the_generic_verdict() {
        // A non-zero exit *and* a known marker: the specific reason wins, so
        // the operator is told the stack is locked rather than just "failed".
        let err = yum(args(
            "yum -y update pkg-a",
            "",
            "System management is locked",
            1,
        ))
        .unwrap_err();
        assert_eq!(err.reason, "update stack locked");
    }

    #[test]
    fn zypper_does_not_fail_on_an_unrecognised_non_zero_exit() {
        // The zypper template ends with the same masking cleanup loop, so a
        // bare `!= 0` rule here would be a false failure — and would fire the
        // group-wide rollback. Only -1, 104 and the markers judge this key.
        assert!(zypper(args("zypper -n in -t patch", "all good", "", 7)).is_ok());
    }
}
