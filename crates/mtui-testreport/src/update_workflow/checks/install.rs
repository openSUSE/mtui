//! Post-install check (upstream `checks/install.py`).
//!
//! Classifies a zypper install result by exit code and stderr/stdout markers.
//! Unlike the other checks, install treats *any* unrecognised non-success as an
//! `UpdateError("Unknown Error")` (upstream's final fall-through raise).

use crate::update_workflow::UpdateError;
use crate::update_workflow::checks::{CheckArgs, CheckFn, Diagnostic, log_failed};

/// The zypper install check (upstream `checks.install.zypper`).
///
/// # Errors
///
/// Returns [`UpdateError`] with a reason of "package not found", "update stack
/// locked", "RPM Error", "Dependency Error", or "Unknown Error" per upstream's
/// branch logic. Exit codes `0, 100, 101, 102, 103, 106` are success. This
/// check surfaces no [`Diagnostic`]s (only `update` does).
pub fn zypper(args: CheckArgs<'_>) -> Result<Vec<Diagnostic>, UpdateError> {
    if matches!(args.exitcode, 0 | 100 | 101 | 102 | 103 | 106) {
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

/// The install check for `(release, transactional)`, or `None` for an unknown
/// key (upstream `install_checks.get(...)`).
#[must_use]
pub fn install_check(release: &str, transactional: bool) -> Option<CheckFn> {
    match (release, transactional) {
        ("11", false) | ("12", false) | ("15", false) | ("16", false) => Some(Box::new(zypper)),
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
        for code in [0, 100, 101, 102, 103, 106] {
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

    #[test]
    fn table_lookup() {
        assert!(install_check("15", false).is_some());
        assert!(install_check("slmicro", true).is_none());
    }
}
