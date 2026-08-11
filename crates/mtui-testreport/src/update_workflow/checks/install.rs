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
/// with its stderr markers in a different order (the markers sit *between* the
/// package-not-found set and the "Unknown Error" fallback, where `update` gives
/// them first refusal on the fallback). Folding this onto the shared helper is
/// a behaviour-preserving refactor only if that ordering is preserved
/// exactly — worth doing on its own, not as a rider.
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

/// The install check for `(release, transactional)`, or `None` for an unknown
/// key.
#[must_use]
pub(crate) fn install_check(release: &str, transactional: bool) -> Option<CheckFn> {
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
