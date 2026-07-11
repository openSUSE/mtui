//! Post-update check (upstream `checks/update.py`).
//!
//! The most elaborate check: it surfaces diagnostic sections for "additional rpm
//! output" and "not supported by its vendor" (upstream prints these to the
//! terminal, one with `cli.colors.yellow` highlighting on the word `warning`).
//! To reproduce that stdout parity without a crate cycle, the sections are
//! returned as [`Diagnostic`]s on the `Ok` path and rendered by the command
//! layer through `session.display`; the `logger.warning`/`logger.critical`
//! breadcrumbs upstream emits alongside are reproduced with `tracing`. Lock /
//! dependency / RPM failures still raise [`UpdateError`].

use crate::update_workflow::UpdateError;
use crate::update_workflow::checks::{CheckArgs, CheckFn, Diagnostic, log_failed};

/// The zypper update check (upstream `checks.update.zypper`).
///
/// # Errors
///
/// Returns [`UpdateError`] with a reason of "update stack locked",
/// "Dependency Error", or "RPM Error" per upstream's branch logic. Warnings
/// (exit `106`, "Additional rpm output", "not supported by its vendor") do not
/// fail the check; the two output sections are returned as [`Diagnostic`]s for
/// the caller to render.
pub fn zypper(args: CheckArgs<'_>) -> Result<Vec<Diagnostic>, UpdateError> {
    let mut diagnostics = Vec::new();
    if args.stdin.contains("zypper") && args.exitcode == 104 {
        log_failed(args);
        return Err(UpdateError::new("update stack locked", args.hostname));
    }
    if args.stdin.contains("zypper") && args.exitcode == 106 {
        tracing::warn!(
            host = args.hostname,
            stderr = args.stderr,
            "zypper returns exitcode 106"
        );
    }
    if let Some(section) = extract_between(args.stdout, "Additional rpm output:", "Retrieving") {
        // Upstream: logs a breadcrumb, then prints the section with "warning"
        // highlighted yellow (`replace("warning", yellow("warning"))`).
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
        // Upstream: logs `package support is uncertain`, then prints the section
        // plain (no recoloring). Reconstruct the marker line upstream keeps in
        // its `stdout[start:end]` slice (its `start` sits *at* the marker).
        tracing::warn!(host = args.hostname, "package support is uncertain");
        diagnostics.push(Diagnostic::plain(format!(
            "The following package is not supported by its vendor:\n{section}"
        )));
    }
    Ok(diagnostics)
}

/// Returns the substring of `s` starting just after `marker` up to the next
/// occurrence of `end` (searched from the marker), mirroring upstream's
/// `stdout[start:end]` slice. `None` when `marker` is absent.
///
/// Matches upstream semantics: the section retained *includes* everything from
/// just past `marker` to the first `end`; if `end` is not found, upstream's
/// `str.find` returns `-1`, slicing `stdout[start:-1]` (all but the last char) —
/// reproduced here by falling back to `len - 1`.
fn extract_between<'a>(s: &'a str, marker: &str, end: &str) -> Option<&'a str> {
    let m = s.find(marker)?;
    let start = m + marker.len();
    let rest = &s[start..];
    let stop = match rest.find(end) {
        Some(rel) => start + rel,
        // Upstream `find` returns -1 → slice `[start:-1]`.
        None => s.len().saturating_sub(1).max(start),
    };
    Some(&s[start..stop])
}

/// The update check for `(release, transactional)`, or `None` for an unknown
/// key (upstream `update_checks.get(...)`).
#[must_use]
pub fn update_check(release: &str, transactional: bool) -> Option<CheckFn> {
    match (release, transactional) {
        ("11", false) | ("12", false) | ("15", false) | ("16", false) => Some(Box::new(zypper)),
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
    fn zypper_104_is_stack_locked() {
        let err = zypper(args("zypper -n patch", "", "", 104)).unwrap_err();
        assert_eq!(err.reason, "update stack locked");
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
        assert!(update_check("YUM", false).is_none());
    }
}
