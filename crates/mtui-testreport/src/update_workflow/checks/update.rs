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
use crate::update_workflow::checks::{
    CheckArgs, CheckFn, Diagnostic, ExitClass, classify_exit, log_failed,
};

/// The zypper update check.
///
/// The exit code reaching here **is the patch's own**: the update template
/// captures `$?` on the line after the patch, before the post-state `grep` and
/// the repo-cleanup loop can clobber it, and ends with `exit $mtui_status`
/// (see [`crate::update_workflow::actions::update`]). So the status is
/// classified, by [`classify_exit`] — the same three sets the install check
/// uses, in one place.
///
/// The one status that is *not* the patch's is a probe failure, and it is
/// gated ahead of the classification by [`probe_failure`]: the script exits
/// with the failed probe's status, which sits in the same numeric space as the
/// patch's, so only the marker line tells the two apart.
///
/// The informational carve-out is the load-bearing part, not the failure rule:
/// zypper exits `102` ("reboot needed") after patching a kernel and `107` when
/// a package's `%post` script failed but the package is installed and
/// registered — both routine outcomes of the thing mtui exists to do. Failing
/// either would fire the **group-wide** rollback downgrade and revert every
/// healthy host in the group.
///
/// Every failing status except `104` and the `-1` sentinel gives the
/// stdout/stderr [`markers`] first refusal — "update stack locked",
/// "Dependency Error" and "RPM Error" name the failure, where "Unknown Error"
/// and "package not found" only say there was one — so a class label is
/// reported solely on a clean transcript.
///
/// The classification is gated on the command text containing `zypper`, which
/// is inert in production (this check is only registered for keys whose
/// updater is the zypper template, and that template's `zypper` token is
/// pinned by a test) but preserves the long-standing rule that these codes are
/// only read as zypper's when the transcript is zypper's.
///
/// # Errors
///
/// Returns [`UpdateError`] with a reason of "update command timed out or
/// failed to run" (exit `-1`), "could not determine what to patch" (the
/// [`PROBE_FAILURE_MARKER`] line), "package not found" (exit `104` always;
/// `4`, `5`, `8` on a clean transcript), "update stack locked", "Dependency
/// Error", "RPM Error", or "Unknown Error" (any other non-zero exit with a
/// clean transcript). The informational
/// exits (`100`-`103`, `106`, `107`) and the two recognised output sections
/// ("Additional rpm output", "not supported by its vendor") do not fail the
/// check; `106` additionally logs a warning, and the two sections are returned
/// as [`Diagnostic`]s for the caller to render.
fn zypper(args: CheckArgs<'_>) -> Result<Vec<Diagnostic>, UpdateError> {
    not_run(args)?;
    probe_failure(args)?;
    if !args.stdin.contains("zypper") {
        return markers(args);
    }
    if args.exitcode == 106 {
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
/// Judged on the exit code as well as the markers. The slmicro update template
/// captures the `transactional-update -n pkg in … -t patch` status into a
/// variable and exits with it, so `lastexit()` is the patch's status and not
/// the trailing repo-cleanup loop's — which was `0` whenever the loop body
/// never ran, and let a failed patch report success.
///
/// It shares zypper's classifier ([`classify_exit`]), and in practice that
/// reduces to "`0` passes, anything else fails", because
/// **`transactional-update` does not propagate zypper's exit code** — it
/// absorbs it. Verified against upstream `openSUSE/transactional-update`,
/// `sbin/transactional-update.in` @ `aee1e1b5` (v6.1.3): the zypper status is
/// captured into `RETVAL` (`:1197`), tested against a hardcoded tolerance list
/// of `0 | 102 | 103 | (106 unless dup)` (`:1214`), and otherwise turned into a
/// flat `EXITCODE=1` (`:1232`). `RETVAL` is never assigned to `EXITCODE` and
/// never reaches the single `exit $EXITCODE` (`:1505`). The documented exit
/// status (`man/transactional-update.8.xml`) is only `0`, `1` and `2`, and `2`
/// comes solely from the `apply` command, which this template does not use.
/// The behaviour is identical across every tag from v2.28.3 to v6.1.3.
///
/// So on this key the informational carve-out and the `PackageNotFound` arm
/// are both **inert**: `102`/`103`/`106` are already mapped to `0` upstream,
/// and `104`/`4`/`5`/`8` all collapse to `1`. Only `0` and `1` can arrive.
/// The shared classifier is used anyway rather than a hand-written `!= 0`, so
/// this key cannot drift away from the zypper one, and so a future
/// `transactional-update` that *did* propagate would be classified correctly
/// from the day it started to.
///
/// One consequence worth knowing when reading a transcript: `0` is also what
/// `pkg in` returns when its dry run finds nothing to do (`quit 0` at `:1192`,
/// after logging `zypper: nothing to update`), so a clean exit does not
/// distinguish "patched" from "had nothing to patch".
///
/// **The cost of that, stated deliberately rather than left implicit.** Because
/// only `0` and `1` can arrive and `1` is `Unknown`, this key went from "never
/// fails on an exit code" to "fails on any non-zero one" — and a failed update
/// check fires the **group-wide** rollback, downgrading and rebooting every
/// host in the group, not only this one. `transactional-update` flattens
/// failures that are not the patch's into that same `1`: an already-open
/// transaction, a snapshot that could not be created or deleted. Those are
/// genuine failures of the operation mtui asked for, and the alternative —
/// keeping the old silence — reports a failed patch as a successful update,
/// which is the bug this check exists to close. Judged the worse of the two,
/// with the blast radius understood.
///
/// Unlike [`zypper`] this is not gated on a token in the command text: the
/// slmicro template contains both `zypper` and `transactional-update`, so a
/// token guard here would only add a second way for the rule to silently stop
/// firing.
///
/// The markers carry one residual exposure, accepted as a decision rather
/// than an oversight: the `Error:` rule reads stderr from the whole `exec`,
/// and the template's sibling commands (`zypper -n lr -puU`, the `zypper -n
/// rr` cleanup loop) can emit `Error:` too — the same objection that rules
/// the markers out for [`yum`]. The difference is the transcript: every
/// command in this template is zypper, whose `Error:` lines are the
/// vocabulary [`markers`] was written against, and the exposure is exactly
/// the one the plain zypper keys have always carried. On [`yum`] the same
/// rule would be a new one, judging a transcript it was never written for.
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
/// The interleaving is the whole content of this function: `104` is a named
/// verdict and returns straight away, while every other failing status lets the
/// markers speak first, because "update stack locked" is a diagnosis and
/// "Unknown Error" — or "package not found" on a code that means neither — is
/// an admission. A success status still runs the markers too: a dependency
/// prompt (`(c): c`) leaves the transaction unfinished with a perfectly clean
/// exit status.
///
/// Two consequences of that ordering, both deliberate and neither obvious:
///
/// * **Only `104` skips the markers.** The class is `104 | 4 | 5 | 8`, and only
///   `104` (`ZYPPER_EXIT_INF_CAP_NOT_FOUND`) actually means "capability not
///   found" — `4`/`5`/`8` are `ERR_ZYPP`, `ERR_PRIVILEGES` and `ERR_COMMIT`,
///   i.e. the package *was* found and the transaction failed. `8` with `Error:`
///   on stderr is the likeliest status for a genuinely failed patch, and `5`
///   with `System management is locked` for a busy update stack; letting the
///   not-found label outrank the markers replaced both headline diagnoses with
///   a message that is not merely vaguer but wrong. So the three defer to the
///   markers, and keep "package not found" only when the transcript is clean.
///
///   The class itself is still the install check's, unsplit — the sorting is
///   shared so an exit code cannot land in two classes, while the *message* is
///   this check's to choose. Nothing branches on these strings (they are
///   rendered, logged and asserted on, never matched), and on `update` the
///   three used to reach no verdict at all, falling through to the markers, so
///   there is no earlier behaviour for the short-circuit to have preserved.
/// * `103` (`ZYPPER_EXIT_INF_RESTART_NEEDED`) is a success even though zypper
///   means "the patch that updates the package manager itself was installed,
///   run the command again to install the *remaining* patches". Those remaining
///   patches are not installed. Inherited from the install check's set and left
///   identical so the two keys cannot drift; the post-state
///   `zypper -n patches | grep $repa` in the template is what surfaces it in
///   the transcript.
///
/// # Errors
///
/// Returns [`UpdateError`] with a reason of "package not found", "update stack
/// locked", "Dependency Error", "RPM Error", "Unknown Error", or — only if a
/// caller reaches here without its [`not_run`] gate — "update command timed out
/// or failed to run".
fn classified(args: CheckArgs<'_>) -> Result<Vec<Diagnostic>, UpdateError> {
    match classify_exit(args.exitcode) {
        ExitClass::Success => markers(args),
        ExitClass::PackageNotFound if args.exitcode == 104 => {
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
        // Unreachable through either check — both gate on `not_run` first — but
        // a class rather than a fallthrough, so a caller that forgets cannot
        // report a host mtui never reached as a failed patch. Deliberately the
        // same error the gate raises, which makes the gate's removal
        // behaviour-preserving on this path; the gate stays because it is the
        // cheaper check and reads at the top of each key.
        ExitClass::NotRun => Err(not_run_error(args)),
    }
}

/// The yum update check.
///
/// Deliberately gates **only** on the [`not_run`] sentinel. That is a narrow
/// verdict, and narrow is the point: it is the part that can be justified
/// without guessing, on a key whose failures route the **group-wide** rollback
/// downgrade.
///
/// Neither of the other two signals survives scrutiny here:
///
/// * **The exit code.** `("YUM", false)` is not one package manager —
///   `System::get_release` maps *every* `rhel` version to this key, and on
///   RHEL 8/9 `yum` is `dnf`, whose handling of a package spec matching
///   nothing installed differs from yum 3's. mtui hands the updater the
///   update's whole package list while a refhost routinely carries only a
///   subset (the reason `prepare --installed-only` exists), so a bare `!= 0`
///   rule risks failing a host that upgraded everything it had.
/// * **The stdout/stderr markers.** [`markers`] is written for the zypper
///   transcript; three of its four strings are zypper-only and cannot appear
///   here. The fourth, `Error:` in stderr, *can* — but the YUM update template
///   is three commands in one remote `exec`, so `lasterr()` also carries
///   `yum repolist`'s output. An unreachable repository or a GPG complaint
///   there would fail an update whose patch step succeeded.
///
/// Settling either would take observed `yum`/`dnf` output from a real RHEL
/// refhost rather than inference, so the check claims only what it can prove:
/// a command that never ran is not a successful update.
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
        return Err(not_run_error(args));
    }
    Ok(())
}

/// The `-1` verdict, in one place.
///
/// Built here rather than at each site so [`not_run`] and [`classified`]'s
/// [`ExitClass::NotRun`] arm cannot drift into two spellings of the same
/// sentinel. The flow's veto of the group-wide rollback keys on `lastexit()`
/// being `-1`, not on this string; what the string decides is what the operator
/// is told the host's failure *was*, which for a host mtui never reached must
/// not read as a package problem.
fn not_run_error(args: CheckArgs<'_>) -> UpdateError {
    log_failed(args);
    UpdateError::new("update command timed out or failed to run", args.hostname)
}

/// The line the update templates print when a probe failed, and the whole of
/// the contract between them and [`probe_failure`].
///
/// The templates append `": <probe> exited <status>"`, so this is a prefix
/// match on a line, not the whole line. The text is mtui's own — nothing in
/// zypper's or `transactional-update`'s vocabulary resembles it — and it reads
/// as a sentence, which matters because a `show_log` transcript is where an
/// operator meets it.
///
/// The templates assemble it from a `printf` format string *and* its argument
/// specifically so that **this string does not occur in the script's own
/// text**. `show_log` echoes the whole script on every update, healthy or not,
/// and `CheckArgs::stdin` carries it too; a transcript that put the command
/// into stdout or stderr (a `set -x` in the host's profile, a verbose
/// transport) would otherwise make every update on that host report a probe
/// failure. `the_rendered_update_templates_do_not_trip_the_gate` pins it from
/// this side, `assert_status_comes_from_the_patch` from the template's.
pub(crate) const PROBE_FAILURE_MARKER: &str = "mtui: could not determine what to patch";

/// Raises when the update script could not work out what to patch.
///
/// The update templates decide whether to patch at all from `zypper -n
/// patches` (and, on the zypper key, from `zypper -n refresh` before it). A
/// failure there yields an *empty* patch list, which is indistinguishable from
/// the legitimate case — a host carrying none of the update's products — so
/// the script names it explicitly with [`PROBE_FAILURE_MARKER`] and exits with
/// the probe's own status. Without this gate the script exited `0` and mtui
/// reported an update it never installed (issue #447).
///
/// Gated on the **marker**, not on the exit code, because no exit code could
/// carry it: a POSIX `exit N` truncates to `0..=255`, so the script's status
/// necessarily sits inside the package manager's own space — a probe that
/// failed with `6` is indistinguishable from a patch that failed with `6`.
/// The marker is a signal only mtui emits.
///
/// It runs **before** [`classified`] for that same reason, and before
/// [`zypper`]'s `zypper`-token gate for [`not_run`]'s: like the `-1` sentinel
/// this is mtui's own verdict about its own script, not a reading of a package
/// manager's transcript, so the question "is this a zypper transcript?" does
/// not arise.
///
/// Both streams are read. The templates print the marker to stdout, but a
/// transport that merged the two would otherwise silently disarm the gate —
/// the failure mode being closed here in the first place.
///
/// # Errors
///
/// Returns [`UpdateError`] with a reason of "could not determine what to
/// patch", flagged as a probe failure so the flow can route it away from the
/// group-wide rollback downgrade: the host never ran a patch, so there is
/// nothing half-applied for a downgrade to repair, and the rollback would
/// revert every healthy peer.
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
        // Carries `command` and `stderr` as well as `stdout`, matching the
        // sibling branches' `log_failed` record. Without them this branch was
        // the one marker that left no forensic trace of the command — invisible
        // while it could only follow a success or an unrecognised status, but
        // reachable now on `4`/`5`/`8`, which are the codes an operator is most
        // likely to be investigating.
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
    fn non_zypper_104_is_not_package_not_found() {
        // 104 only means "package not found" when the command was a zypper
        // invocation. (The old name said "lock branch"; there is no such
        // branch on 104 — the lock verdict comes from the stderr markers.)
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
    fn transactional_update_fails_on_an_unrecognised_non_zero_exit() {
        // The inverse of the rule this check used to carry. The slmicro
        // template now captures the patch's own status and exits with it, so a
        // non-zero code with clean output is a failed patch, not the trailing
        // repo-cleanup loop's status.
        let err = transactional_update(args("transactional-update -n pkg in", "all good", "", 1))
            .expect_err("a failed patch with clean output must fail the check");
        assert_eq!(err.reason, "Unknown Error");
        assert_eq!(err.host.as_deref(), Some("h1"));
    }

    #[test]
    fn transactional_update_still_catches_the_markers() {
        // The other half of the verdict. Each of these carries exit `0`, which
        // is exactly the shape the exit-code rule cannot see: a
        // `transactional-update` that ran, reported a problem in its output,
        // and still returned success.
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
        // The narrow verdict is deliberate — see `yum`'s doc. Both of these
        // would fail a host that updated fine:
        //
        // - a bare `!= 0`: on RHEL 8/9 `yum` is `dnf`, and mtui passes the
        //   update's whole package list to a host that carries only a subset;
        // - the `Error:` marker: the YUM template runs `yum repolist` in the
        //   same `exec`, so an unreachable repo lands in the same stderr.
        //
        // Both would then route the group-wide rollback downgrade.
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
        // The inverse of the rule this check used to carry. The zypper
        // template captures the patch's status and exits with it, so an
        // unrecognised non-zero code is the patch's verdict and not the
        // trailing repo-cleanup loop's.
        let err = zypper(args("zypper -n in -t patch", "all good", "", 7))
            .expect_err("a failed patch with clean output must fail the check");
        assert_eq!(err.reason, "Unknown Error");
        assert_eq!(err.host.as_deref(), Some("h1"));
    }

    #[test]
    fn informational_exit_codes_pass_on_both_zypper_keys() {
        // The carve-out that makes the exit-code rule safe to switch on at
        // all. `102` is "reboot needed" — the routine result of patching a
        // kernel — `100`/`101` are "(security) updates available", which a
        // patch run leaves behind whenever the host carries more than this
        // update, and `107` is "the package is installed and registered but
        // its %post script failed". Failing any of them would fire the
        // group-wide rollback downgrade and revert every healthy host in the
        // group.
        //
        // The whole informational band is listed here, not a sample: the
        // classifier's own test and this one are the two places a code can be
        // forgotten, and `107` was in fact missing from the band on the first
        // pass.
        for code in [0, 100, 101, 102, 103, 106, 107] {
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
        // exactly, so an exit code cannot be sorted into two different classes
        // by two checks. The transcripts here are clean, which is when the
        // class label is also the message; see `only_104_outranks_the_markers`
        // for what the three inaccurate members say when it is not.
        for code in [104, 4, 5, 8] {
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
        // `zypper`'s classification is gated on the literal token `zypper`
        // appearing in the command text. That gate and the template are two
        // separate files, so this feeds the check the *real* rendered template
        // — the only way the coupling can be pinned. Asserting the token
        // inside the template's own test would pin it against itself.
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

            let err = check(args(&rendered, "", "", 104)).unwrap_err();
            assert_eq!(err.reason, "package not found", "{key}");
            let err = check(args(&rendered, "", "", 7)).unwrap_err();
            assert_eq!(err.reason, "Unknown Error", "{key}");
            assert!(
                check(args(&rendered, "", "", 102)).is_ok(),
                "{key}: 102 is 'reboot needed'"
            );
        }
    }

    #[test]
    fn only_104_outranks_the_markers() {
        // `104` is the one member of its class that literally means "capability
        // not found", and the one whose verdict a marker cannot improve on: it
        // short-circuits even with a marker present.
        let err = zypper(args("zypper", "", "System management is locked", 104)).unwrap_err();
        assert_eq!(err.reason, "package not found", "104 outranks the marker");

        // `4`/`5`/`8` are `ERR_ZYPP`, `ERR_PRIVILEGES` and `ERR_COMMIT` — the
        // package was found and the transaction failed. These three are the
        // headline real-world patch failures, so the transcript names them
        // rather than the class label. Restoring the blanket short-circuit
        // turns every one of these into "package not found".
        let err = zypper(args("zypper", "", "System management is locked", 5)).unwrap_err();
        assert_eq!(err.reason, "update stack locked", "5 + lock marker");
        let err = zypper(args(
            "zypper",
            "",
            "A ZYpp transaction is already in progress.",
            4,
        ))
        .unwrap_err();
        assert_eq!(err.reason, "update stack locked", "4 + ZYpp marker");
        let err = zypper(args("zypper", "(c): c", "", 4)).unwrap_err();
        assert_eq!(err.reason, "Dependency Error", "4 + dependency prompt");
        let err =
            transactional_update(args("transactional-update", "", "Error: boom", 8)).unwrap_err();
        assert_eq!(err.reason, "RPM Error", "8 + rpm marker");

        // What the three say on a *clean* transcript is already pinned by
        // `package_not_found_codes_on_both_zypper_keys`, which passes empty
        // stdout and stderr for all four codes. Repeating it here would
        // constrain nothing the marker cases above do not.
    }

    #[test]
    fn the_probe_marker_outranks_the_exit_code_rules() {
        // Issue #447. The script exits with the *probe's* status, which shares
        // its numeric space with the patch's — `6` could be either — so only
        // the marker tells them apart, and it has to be read before
        // `classify_exit` gets the code. Each pairing below is a status the
        // classifier has a confident answer for, and each answer would be
        // wrong here: `0` would pass the host, `104` would say "package not
        // found", `7` would say "Unknown Error".
        //
        // `0` is in the list for the case that matters most: a probe failure
        // whose status the transport lost would otherwise be reported as a
        // clean update.
        let marker = format!("{PROBE_FAILURE_MARKER}: zypper -n patches exited 6");
        for code in [0, 7, 104, 106] {
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
        // streams would otherwise silently disarm the gate, which is the
        // failure mode being closed here in the first place.
        let marker = format!("{PROBE_FAILURE_MARKER}: zypper -n refresh exited 4");
        let err = zypper(args("zypper", "", &marker, 4)).unwrap_err();
        assert_eq!(err.reason, "could not determine what to patch");
    }

    #[test]
    fn the_probe_marker_does_not_fire_on_an_ordinary_transcript() {
        // The trap this whole issue is built around: "nothing to do" is not a
        // failure. A host carrying none of the update's products runs the same
        // script, skips the patch, and exits `0` with no marker — and there is
        // no other site in the workspace where an empty result reads as a
        // failure, so this must not become the first.
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
        // group-wide rollback downgrade over a host that never ran a patch.
        // Nothing else in the tree makes control flow depend on a reason's
        // text, so the flag is the contract — asserted here rather than only
        // in the flow, where a `false` would still produce a plausible-looking
        // error.
        let marker = format!("{PROBE_FAILURE_MARKER}: zypper -n patches exited 6");
        let err = zypper(args("zypper", &marker, "", 6)).unwrap_err();
        assert!(err.probe_failed, "the probe-failure flag must be set");
        // And it is set nowhere else: a patch that genuinely failed is a
        // rollback's business.
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
        // The script *text* must not contain the marker, only the script's
        // *output* on a real failure. `CheckArgs::stdin` is the whole rendered
        // script on every update, and a host whose profile carries a `set -x`
        // (or a transport that merged the command into the transcript) would
        // put that text into stdout/stderr too — turning every healthy update
        // on that host into "could not determine what to patch". The templates
        // split the marker across `printf`'s format string and its argument
        // exactly so this cannot happen.
        //
        // Fed on all three fields, because the gate reads two of them and the
        // third is where the script legitimately lives.
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
        // The coupling the assertion above does not carry — that the template
        // *prints* this exact string when a probe fails — is pinned where it
        // can be: `actions::update`'s `rendered_script` cases run the real
        // script under a real `/bin/sh` and match its stdout against this
        // constant.
    }

    #[test]
    fn the_probe_marker_is_matched_in_full() {
        // `contains(PROBE_FAILURE_MARKER)` weakened to any prefix of it (say
        // `"mtui:"`) still passes every other test here, and would then fire on
        // any mtui-namespaced line the templates or a future action print.
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

        // The other direction: the marker is matched at the *start of a line*,
        // not anywhere in the stream. The split-`printf` trick keeps the
        // script's own text from carrying the sentence, but it cannot stop a
        // host echoing or quoting it mid-line — a `set -x` trace, a log line
        // that embeds the previous run's output, a package's own message.
        // Only a line that begins with it is mtui's own verdict.
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

        // And a line that really does begin with it still fires, on either
        // stream — or the guard above would have disarmed the gate entirely.
        let marker = format!("{PROBE_FAILURE_MARKER}: zypper -n patches exited 6\n");
        for (out, err) in [(marker.as_str(), ""), ("", marker.as_str())] {
            let err = zypper(args("zypper -n in -t patch", out, err, 0)).unwrap_err();
            assert_eq!(err.reason, "could not determine what to patch");
        }
    }

    #[test]
    fn the_probe_marker_outranks_the_zypper_token_gate() {
        // `zypper`'s exit-code rules are gated on the command text containing
        // the literal `zypper`, and `probe_failure` deliberately runs *before*
        // that gate — like `not_run`, it is mtui's own verdict about its own
        // script, not a reading of a package manager's transcript.
        //
        // Every other fixture here carries `zypper` in `stdin`, so moving the
        // gate below the token check survives them all. The template losing
        // that token is a live hazard the module docs call out; if it ever
        // happens, this is what keeps a host that printed the marker from
        // returning `Ok` through the marker-only path.
        let marker = format!("{PROBE_FAILURE_MARKER}: zypper -n patches exited 6");
        let err = zypper(args("some update command", &marker, "", 6))
            .expect_err("the marker must be read whatever the command text says");
        assert_eq!(err.reason, "could not determine what to patch");
        assert!(err.probe_failed);
    }

    #[test]
    fn the_never_ran_class_cannot_be_read_as_a_package_failure() {
        // Unreachable through `zypper`/`transactional_update`, which gate on
        // `not_run` first — so this calls `classified` directly, which is the
        // only way to prove the arm exists and says the right thing. Before
        // `ExitClass::NotRun`, `-1` fell through to `Unknown`, and a host mtui
        // never contacted was reported as a failed patch. (Not as a *rolled
        // back* one: `never_ran` in `reports::update_flow` reads the target's
        // `lastexit()`, so the group-wide downgrade was skipped either way.)
        let err = classified(args("zypper", "", "", -1)).unwrap_err();
        assert_eq!(err.reason, "update command timed out or failed to run");

        // Both routes to the sentinel give the operator the same string — the
        // point of `not_run_error`. This cannot detect the gate's removal
        // (the arm produces the identical error, by design); what it pins is
        // that the two never drift apart.
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
        // failing patch that also reports a locked stack must keep the
        // diagnosis, on both keys — this is what the ordering inside
        // `classified` buys, and a fallback placed before the markers would
        // silently replace every named reason with "Unknown Error".
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
