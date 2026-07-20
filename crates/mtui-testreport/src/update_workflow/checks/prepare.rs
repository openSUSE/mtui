//! Post-prepare check (upstream `checks/prepare.py`).

use crate::update_workflow::UpdateError;
use crate::update_workflow::checks::{CheckArgs, CheckFn, Diagnostic, log_failed};

/// The zypper prepare check (upstream `checks.prepare.zypper`).
///
/// # Errors
///
/// Returns [`UpdateError`] with a reason of "update stack locked",
/// "Dependency Error", or "RPM Error" per upstream's branch logic. This check
/// surfaces no [`Diagnostic`]s (only `update` does).
pub fn zypper(args: CheckArgs<'_>) -> Result<Vec<Diagnostic>, UpdateError> {
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

/// The prepare check for `(release, transactional)`, or `None` for an unknown
/// key (upstream `prepare_checks.get(...)`).
#[must_use]
pub fn prepare_check(release: &str, transactional: bool) -> Option<CheckFn> {
    match (release, transactional) {
        ("11", false) | ("12", false) | ("15", false) | ("16", false) => Some(Box::new(zypper)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args<'a>(stdout: &'a str, stderr: &'a str) -> CheckArgs<'a> {
        CheckArgs {
            hostname: "h1",
            stdout,
            stdin: "zypper -n in -y -l pkg",
            stderr,
            exitcode: 0,
        }
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
    fn table_lookup() {
        assert!(prepare_check("11", false).is_some());
        assert!(prepare_check("YUM", false).is_none());
    }
}
