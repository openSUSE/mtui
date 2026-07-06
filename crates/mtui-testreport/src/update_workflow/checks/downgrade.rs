//! Post-downgrade check (upstream `checks/downgrade.py`).

use crate::update_workflow::UpdateError;
use crate::update_workflow::checks::{CheckArgs, CheckFn, log_failed};

/// The zypper downgrade check (upstream `checks.downgrade.zypper`).
///
/// # Errors
///
/// Returns [`UpdateError`] with a reason of "update stack locked",
/// "Dependency Error", or "Unspecified Error" per upstream's branch logic.
/// Exit code `106` is a warning (not an error); any other unrecognised result
/// passes.
pub fn zypper(args: CheckArgs<'_>) -> Result<(), UpdateError> {
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
    if args.exitcode == 104 {
        tracing::error!(
            host = args.hostname,
            stderr = args.stderr,
            "zypper returned with errorcode 104"
        );
        return Err(UpdateError::new("Unspecified Error", args.hostname));
    }
    if args.exitcode == 106 {
        tracing::warn!(
            host = args.hostname,
            stderr = args.stderr,
            "zypper returned with errorcode 106"
        );
    }
    Ok(())
}

/// The downgrade check for `(release, transactional)`, or `None` for an unknown
/// key (upstream `downgrade_checks.get(...)`).
#[must_use]
pub fn downgrade_check(release: &str, transactional: bool) -> Option<CheckFn> {
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
            stdin: "zypper -n in -C --oldpackage",
            stderr,
            exitcode,
        }
    }

    #[test]
    fn clean_output_passes() {
        assert!(zypper(args("ok", "", 0)).is_ok());
    }

    #[test]
    fn stack_locked_variants() {
        assert_eq!(
            zypper(args("", "A ZYpp transaction is already in progress.", 1))
                .unwrap_err()
                .reason,
            "update stack locked"
        );
        assert_eq!(
            zypper(args("", "System management is locked", 1))
                .unwrap_err()
                .reason,
            "update stack locked"
        );
    }

    #[test]
    fn dependency_error_from_stdout() {
        assert_eq!(
            zypper(args("(c): c", "", 1)).unwrap_err().reason,
            "Dependency Error"
        );
    }

    #[test]
    fn exit_104_is_unspecified_error() {
        assert_eq!(
            zypper(args("", "boom", 104)).unwrap_err().reason,
            "Unspecified Error"
        );
    }

    #[test]
    fn exit_106_warns_but_passes() {
        assert!(zypper(args("", "warn", 106)).is_ok());
    }

    #[test]
    fn table_lookup() {
        assert!(downgrade_check("16", false).is_some());
        assert!(downgrade_check("slmicro", true).is_none());
    }
}
