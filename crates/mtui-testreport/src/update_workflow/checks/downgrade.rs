//! Post-downgrade check.

use crate::update_workflow::checks::{
    CheckArgs, CheckFn, Diagnostic, ZYPPER_EXIT_INF_CAP_NOT_FOUND, ZYPPER_EXIT_INF_REPO_SKIPPED,
    lock_or_dependency, not_run,
};
use crate::update_workflow::{Family, UpdateError, family};

/// The zypper downgrade check.
///
/// # Errors
///
/// Returns [`UpdateError`] with a reason of "downgrade command timed out or
/// failed to run" (exit `-1`), "update stack locked", "Dependency Error", or
/// "Unspecified Error" depending on the exit code and stderr/stdout markers.
/// Exit code `106` is a warning (not an error); any other unrecognised
/// result passes. This check surfaces no [`Diagnostic`]s (only `update`
/// does).
fn zypper(args: CheckArgs<'_>) -> Result<Vec<Diagnostic>, UpdateError> {
    // Continuing past `-1` would turn an interrupted rollback into a silent
    // half-rollback.
    not_run(args, NOT_RUN_REASON)?;
    lock_or_dependency(args)?;
    if args.exitcode == ZYPPER_EXIT_INF_CAP_NOT_FOUND {
        tracing::error!(
            host = args.hostname,
            stderr = args.stderr,
            "zypper returned with errorcode 104"
        );
        return Err(UpdateError::new("Unspecified Error", args.hostname));
    }
    if args.exitcode == ZYPPER_EXIT_INF_REPO_SKIPPED {
        tracing::warn!(
            host = args.hostname,
            stderr = args.stderr,
            "zypper returned with errorcode 106"
        );
    }
    Ok(Vec::new())
}

/// The transactional-update and yum downgrade check.
///
/// Only the timed-out/unrunnable gate: the zypper-specific branches (104, lock
/// strings) are zypper vocabulary and must not be reused for
/// `transactional-update` or `yum -y downgrade`. Without this check a dead
/// `transactional-update` sails on to the reboot with no snapshot staged. A
/// non-zero `yum` status is routine on a refhost carrying only part of the
/// package list; a rollback that never ran to completion is not.
///
/// # Errors
///
/// Returns [`UpdateError`] with a reason of "downgrade command timed out or
/// failed to run" when the command recorded exit `-1`.
fn not_run_only(args: CheckArgs<'_>) -> Result<Vec<Diagnostic>, UpdateError> {
    not_run(args, NOT_RUN_REASON)?;
    Ok(Vec::new())
}

const NOT_RUN_REASON: &str = "downgrade command timed out or failed to run";

/// The downgrade check for `(release, transactional)`, or `None` for an
/// unknown key.
#[must_use]
pub(crate) fn downgrade_check(release: &str, transactional: bool) -> Option<CheckFn> {
    family(release, transactional).map(|f| -> CheckFn {
        match f {
            Family::Zypper => Box::new(zypper),
            Family::Yum | Family::Transactional => Box::new(not_run_only),
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::update_workflow::checks::EXIT_NOT_RUN;

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
        // the flow continue past an interrupted rollback.
        let err = zypper(args("", "", EXIT_NOT_RUN)).unwrap_err();
        assert_eq!(err.reason, "downgrade command timed out or failed to run");
        assert_eq!(err.host.as_deref(), Some("h1"));
    }

    #[test]
    fn transactional_update_exitcode_minus_one_raises() {
        // Without this the registry falls back to a no-op and a dead
        // transactional-update sails on to the reboot with no snapshot staged.
        let err = not_run_only(args("", "", EXIT_NOT_RUN)).unwrap_err();
        assert_eq!(err.reason, "downgrade command timed out or failed to run");
        assert_eq!(err.host.as_deref(), Some("h1"));
    }

    #[test]
    fn transactional_update_clean_run_passes() {
        assert!(not_run_only(args("", "", 0)).is_ok());
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
            zypper(args("", "boom", ZYPPER_EXIT_INF_CAP_NOT_FOUND))
                .unwrap_err()
                .reason,
            "Unspecified Error"
        );
    }

    #[test]
    fn exit_106_warns_but_passes() {
        assert!(zypper(args("", "warn", ZYPPER_EXIT_INF_REPO_SKIPPED)).is_ok());
    }

    #[test]
    fn yum_downgrade_judges_only_whether_the_command_ran() {
        // The last key with a downgrader and no check (#406). The verdict is
        // as narrow as the transactional one directly above, and for the same
        // reason: `yum -y downgrade` on a host that carries only part of the
        // update's package list is a routine outcome, and the zypper markers
        // are not this transcript's vocabulary — but a rollback command that
        // never ran leaves the host on the update version while the flow ends
        // looking done.
        let err = not_run_only(args("", "", EXIT_NOT_RUN)).unwrap_err();
        assert_eq!(err.reason, "downgrade command timed out or failed to run");
        assert_eq!(err.host.as_deref(), Some("h1"));

        // Everything the check cannot justify judging still passes.
        assert!(not_run_only(args("", "", 1)).is_ok());
        assert!(not_run_only(args("", "Error: nothing to downgrade", 0)).is_ok());
        assert!(not_run_only(args("", "", ZYPPER_EXIT_INF_CAP_NOT_FOUND)).is_ok());
    }

    #[test]
    fn table_lookup() {
        assert!(downgrade_check("11", false).is_some());
        // The YUM key has had a downgrader since the port; it now has a check
        // too, so a `yum` rollback that never ran is not read as a completed
        // one.
        let check = downgrade_check("YUM", false).expect("YUM check registered");
        let err = check(CheckArgs {
            hostname: "h1",
            stdout: "",
            stdin: "yum -y downgrade pkg-a",
            stderr: "",
            exitcode: EXIT_NOT_RUN,
        })
        .unwrap_err();
        assert_eq!(err.reason, "downgrade command timed out or failed to run");
        assert!(downgrade_check("YUM", true).is_none());
        assert!(downgrade_check("12", false).is_some());
        assert!(downgrade_check("15", false).is_some());
        assert!(downgrade_check("16", false).is_some());
        // The transactional slmicro key now dispatches to its own check (NOT
        // the no-op fallback): a dead run must raise. Verify via behavior
        // since CheckFn is not comparable by identity.
        let check = downgrade_check("slmicro", true).expect("slmicro check registered");
        let err = check(CheckArgs {
            hostname: "h1",
            stdout: "",
            stdin: "tu pkg in",
            stderr: "",
            exitcode: EXIT_NOT_RUN,
        })
        .unwrap_err();
        assert_eq!(err.reason, "downgrade command timed out or failed to run");
        assert!(downgrade_check("99", false).is_none());
    }
}
