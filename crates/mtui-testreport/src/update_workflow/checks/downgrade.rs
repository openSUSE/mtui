//! Post-downgrade check (upstream `checks/downgrade.py`).

use crate::update_workflow::UpdateError;
use crate::update_workflow::checks::{CheckArgs, CheckFn, Diagnostic, log_failed};

/// The zypper downgrade check (upstream `checks.downgrade.zypper`).
///
/// # Errors
///
/// Returns [`UpdateError`] with a reason of "downgrade command timed out or
/// failed to run" (exit `-1`), "update stack locked", "Dependency Error", or
/// "Unspecified Error" per upstream's branch logic. Exit code `106` is a warning
/// (not an error); any other unrecognised result passes. This check surfaces no
/// [`Diagnostic`]s (only `update` does).
fn zypper(args: CheckArgs<'_>) -> Result<Vec<Diagnostic>, UpdateError> {
    // Exit -1 is what a timed-out (SSH no-output window exceeded) or unrunnable
    // command records. Continuing past it turns an interrupted rollback into a
    // silent half-rollback: the remaining packages stay at the update version
    // while the flow ends looking done (upstream PR #336).
    if args.exitcode == -1 {
        log_failed(args);
        return Err(UpdateError::new(
            "downgrade command timed out or failed to run",
            args.hostname,
        ));
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
    Ok(Vec::new())
}

/// The transactional (`slmicro`) downgrade check (upstream PR #336's
/// `checks.downgrade.transactional_update`).
///
/// Only the timed-out/unrunnable gate: transactional-update's own exit codes and
/// messages differ from zypper's, so the zypper-specific branches (104, lock
/// strings) must not be reused here. Without this check the registry falls back
/// to a no-op and a dead `transactional-update` sails on to the reboot with no
/// snapshot staged.
///
/// # Errors
///
/// Returns [`UpdateError`] with a reason of "downgrade command timed out or
/// failed to run" when the command recorded exit `-1`.
fn transactional_update(args: CheckArgs<'_>) -> Result<Vec<Diagnostic>, UpdateError> {
    if args.exitcode == -1 {
        log_failed(args);
        return Err(UpdateError::new(
            "downgrade command timed out or failed to run",
            args.hostname,
        ));
    }
    Ok(Vec::new())
}

/// The downgrade check for `(release, transactional)`, or `None` for an unknown
/// key (upstream `downgrade_checks.get(...)`).
#[must_use]
pub(crate) fn downgrade_check(release: &str, transactional: bool) -> Option<CheckFn> {
    match (release, transactional) {
        ("11", false) | ("12", false) | ("15", false) | ("16", false) => Some(Box::new(zypper)),
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
    fn zypper_exitcode_minus_one_raises() {
        // Exit -1 (a timed-out or unrunnable command) raises instead of letting
        // the flow continue past an interrupted rollback (upstream PR #336).
        let err = zypper(args("", "", -1)).unwrap_err();
        assert_eq!(err.reason, "downgrade command timed out or failed to run");
        assert_eq!(err.host.as_deref(), Some("h1"));
    }

    #[test]
    fn transactional_update_exitcode_minus_one_raises() {
        // Without this the registry falls back to a no-op and a dead
        // transactional-update sails on to the reboot with no snapshot staged.
        let err = transactional_update(args("", "", -1)).unwrap_err();
        assert_eq!(err.reason, "downgrade command timed out or failed to run");
        assert_eq!(err.host.as_deref(), Some("h1"));
    }

    #[test]
    fn transactional_update_clean_run_passes() {
        assert!(transactional_update(args("", "", 0)).is_ok());
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
        assert!(downgrade_check("11", false).is_some());
        assert!(downgrade_check("12", false).is_some());
        assert!(downgrade_check("15", false).is_some());
        assert!(downgrade_check("16", false).is_some());
        // The transactional slmicro key now dispatches to its own check (NOT
        // the no-op fallback): a dead run must raise (upstream PR #336). Verify
        // via behavior since CheckFn is not comparable by identity.
        let check = downgrade_check("slmicro", true).expect("slmicro check registered");
        let err = check(CheckArgs {
            hostname: "h1",
            stdout: "",
            stdin: "tu pkg in",
            stderr: "",
            exitcode: -1,
        })
        .unwrap_err();
        assert_eq!(err.reason, "downgrade command timed out or failed to run");
        assert!(downgrade_check("99", false).is_none());
    }
}
