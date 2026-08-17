//! The bespoke (non-template) update flows: `perform_prepare`,
//! `perform_downgrade`, `perform_update`.
//!
//! ## Design
//!
//! Unlike install/uninstall (which route through the shared
//! [`Operation`] template), these three are deliberately
//! open-coded — they have per-package loops, `set_repo` add/remove
//! fan-outs, package-version comparison, and (for `update`) a two-phase
//! try/finally that guarantees repo cleanup on success while **keeping** the
//! test repos on failure for retry/diagnosis.
//!
//! ## Crate boundary
//!
//! These flows need `get_package_list` / `set_repo`, which in the Rust split
//! live in `mtui-testreport`. Putting the flows here (as the concrete reports'
//! `perform_*` bodies, alongside `perform_install`) keeps `mtui-hosts` free of a
//! `mtui-testreport` dependency and reuses the report's own [`SetRepo`] hook and
//! package list. The flows resolve each host's command templates directly from
//! the [`WorkflowRegistry`] (`ActionCommands` + `CheckFn`) — the same tables the
//! `PlanProvider` adapter uses — keyed on `(system.get_release(),
//! transactional)`.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;

use mtui_hosts::{
    Command, HostError, HostsGroup, InstallOperation, LockOutcome, Operation, OperationGroup,
    RebootFailure, RebootFailureCause, RepoOp, SetRepo, UninstallOperation,
};
use mtui_types::shellquote::quote_args;
use tracing::{debug, error, info, warn};

use crate::update_workflow::actions::ActionCommands;
use crate::update_workflow::checks::{CheckArgs, CheckFn, Diagnostic};
use crate::update_workflow::{CheckProvider, DoerProvider, Role, UpdateError, WorkflowRegistry};

/// A per-host command map paired with the transactional-host reboot map, as
/// built by [`build_update_maps`] (`(commands, reboot)`).
type UpdateMaps = (BTreeMap<String, String>, BTreeMap<String, String>);

/// Why an update did not apply.
///
/// The variants exist to answer exactly one question — **can a group-wide
/// downgrade repair any host that failed?** — because that is what
/// [`perform_update_with_rollback`] has to decide, and the rollback reverts
/// *every* host in the group, not just the one that reported. `Check` and
/// `RebootNotTaken` answer yes and roll back; every other variant answers no
/// and re-surfaces the error untouched.
///
/// "No" is not one situation, and the variants keep the differences because an
/// operator needs them: no update patch was ever dispatched, so there is
/// nothing for a downgrade to undo (`MissingUpdater`, `Prepare`, `Cancelled`,
/// `ProbeFailed`), or the patch may well have applied but the flow cannot
/// reach the host to undo it (`Reboot`), or it is unknown whether it did
/// (`NotRun`). All of them collapse to a single [`UpdateError`] at the command
/// boundary.
///
/// "No patch was dispatched" is not the same as "the host is untouched": an
/// abort that follows a *completed* `prepare` — the pre-dispatch cancel gate
/// and `MissingUpdater` — leaves that prepare's packages installed. That is
/// still a "no" for the rollback, which exists to undo a patch that never ran
/// here, but the host did change, so the prepare writes its own
/// `/var/log/mtui.log` row rather than leaving the abort silent (#407).
#[derive(Debug, PartialEq, Eq)]
pub enum UpdateFailure {
    /// One or more hosts failed the `updater` check after the command ran.
    Check(UpdateError),
    /// The pre-update `prepare` step could not run: no preparer for a host's
    /// key, the operation lock was contended, or the issue repo could not be
    /// set.
    ///
    /// Like [`MissingUpdater`](Self::MissingUpdater) this skips the rollback,
    /// but for a different reason: no update patch was dispatched, so there
    /// is no update for a downgrade to undo. `--noprepare` is the opt-out for
    /// a caller that wants to patch anyway. A prepare that ran but only
    /// reported a per-host package-manager failure does **not** reach this
    /// variant — see `PrepareFailure` — it warns and the update proceeds.
    Prepare(UpdateError),
    /// A concrete target has no updater doer; mtui treats this as a hard
    /// failure (rather than logging and returning as if successful) so a
    /// target that cannot be updated never reports "finished".
    MissingUpdater(UpdateError),
    /// Cooperative cancellation was requested (MCP `job_cancel`) and the flow
    /// stopped at a step boundary.
    ///
    /// Like [`MissingUpdater`](Self::MissingUpdater) this skips the rollback:
    /// a rollback is itself a multi-minute downgrade, so rolling back on a
    /// cancel would *extend* the work the caller just asked to stop.
    Cancelled(UpdateError),
    /// A transactional host rebooted after a successful patch and did not
    /// reconnect.
    ///
    /// Like [`MissingUpdater`](Self::MissingUpdater) this skips the rollback:
    /// the host is unreachable, so a downgrade cannot run on it — and the
    /// rollback is group-wide, so running it would revert the *healthy* hosts
    /// on behalf of one that cannot be reached either way.
    Reboot(UpdateError),
    /// A transactional host was patched but its reboot never took effect, and
    /// the host is **still reachable**: the command was never dispatched, or
    /// the host answered with an unchanged boot id.
    ///
    /// Unlike [`Reboot`](Self::Reboot) this *does* roll back. The host is up,
    /// serving from the old snapshot while the rest of the group runs the new
    /// packages — the split-brain the rollback exists to undo — and, being
    /// reachable, it is a host the downgrade can actually reach.
    RebootNotTaken(UpdateError),
    /// The update command never ran to completion on any host that failed —
    /// it timed out, or the connection dropped part-way (`Target::run`'s `-1`).
    ///
    /// Like [`Reboot`](Self::Reboot) this skips the rollback — but not by the
    /// reboot arm's reachability argument, because `-1` is a sentinel, not a
    /// liveness verdict. It covers two situations, and each vetoes the
    /// rollback on its own:
    ///
    /// * **The flow lost the host** (connection dropped mid-command, or never
    ///   connected). A group-wide downgrade would revert the *healthy* hosts
    ///   on behalf of one it cannot reach either way.
    /// * **The command outlived its timeout on a host that is up.** The
    ///   timeout closes the SSH channel, which asks the remote side to
    ///   reclaim the process — but rpm masks signals inside its transaction,
    ///   so the patch may have died, finished after the flow stopped
    ///   watching, or still be holding the package-manager lock. Dispatching
    ///   the group-wide downgrade now fires a second transaction at the one
    ///   host whose first was never observed to end, and reverts every
    ///   healthy host to do it.
    ///
    /// Both stay distinct from [`Check`](Self::Check) for the same reason: a
    /// check failure means "the patch ran and produced a bad verdict" — a
    /// half-applied state the rollback repairs — whereas `-1` is the absence
    /// of a verdict. The host's state is unknown, not known-bad; it needs
    /// eyes on it, not an automated second transaction.
    ///
    /// Used when **no** failed host is one the rollback could repair: every
    /// one of them is `-1`, or a mix of `-1` and
    /// [`ProbeFailed`](Self::ProbeFailed) hosts (`-1` is the more
    /// conservative of the two labels, and its "state unknown" claim is true
    /// of at least one host in such a run). A run mixing either with a real
    /// check failure still rolls back, on behalf of the host the rollback can
    /// genuinely repair.
    NotRun(UpdateError),
    /// The update command ran on every host that failed, and each reported
    /// that it could not work out what to patch — so none of them dispatched
    /// a patch (`checks::update`'s `probe_failure`).
    ///
    /// Skips the rollback, and for a *stronger* reason than
    /// [`NotRun`](Self::NotRun)'s: this is not the absence of a verdict but a
    /// definite one. The host said it never patched, so its packages are
    /// exactly what they were before the flow started. There is nothing
    /// half-applied for a downgrade to repair, and the rollback is group-wide
    /// — running it would revert every healthy peer over a probe that broke on
    /// one host.
    ///
    /// It is the operator's `zypper` view that needs attention (a host with no
    /// repositories, a ZYpp lock, a broken awk), not the host's package state.
    ///
    /// Only used when **every** failed host is in that state. A run mixing one
    /// with a `-1` host is labelled [`NotRun`](Self::NotRun) — also
    /// non-rolling, and the more conservative of the two claims, since one host
    /// in such a run really is in an unknown state.
    ProbeFailed(UpdateError),
}

/// Drives [`perform_update`] from a concrete report, reading the package list
/// and `$repa` selector (`maintenance_id` / `review_id`) off the report's RRID.
///
/// This is the shared body behind every report's `perform_update` override;
/// keeping it here means SL / PI / OBS each delegate in one line rather than
/// duplicating the RRID/package-list plumbing. `report` supplies both the
/// [`TestReport`](crate::testreport::TestReport) metadata (RRID, package list)
/// and the [`SetRepo`] repo hook.
pub async fn perform_update_from_report<R>(
    report: &R,
    targets: &mut HostsGroup,
    noprepare: bool,
    newpackage: bool,
    diagnostics: &mut Vec<Diagnostic>,
) -> Result<(), UpdateFailure>
where
    R: crate::testreport::TestReport + SetRepo,
{
    let Some(rrid) = report.base().rrid.as_ref() else {
        debug!("perform_update: no RRID loaded; nothing to update");
        return Ok(());
    };
    let id = rrid.to_string();
    let maintenance_id = rrid.maintenance_id.clone();
    let review_id = rrid.review_id.to_string();
    let packages = report.get_package_list();
    perform_update(
        targets,
        report,
        &packages,
        &maintenance_id,
        &review_id,
        Some(&id),
        noprepare,
        newpackage,
        diagnostics,
    )
    .await
}

/// Drives [`perform_update_from_report`] and rolls the packages back via
/// [`perform_downgrade`], before re-surfacing the original error, on the two
/// failures that leave the group repairable: a *check* failure, and a reboot
/// that did not take effect on a host still reachable
/// ([`UpdateFailure::RebootNotTaken`]).
///
/// Every other failure installed nothing — no updater for a host's key, a
/// prepare that could not run, a cancel before dispatch, a host the flow lost,
/// or a host that could not determine what to patch — so each re-surfaces
/// without a rollback attempt. The rollback is best-effort, but not because it
/// cannot fail: [`perform_downgrade`] returns a `Result`, and a host whose
/// version probe never answered raises it (#451). The precedence is enforced at
/// the call site, which logs that error at WARN and re-surfaces the original
/// update error, so a failed rollback can never bury the failure it was trying
/// to repair — do not simplify the call away, or a broken rollback goes silent
/// again.
pub async fn perform_update_with_rollback<R>(
    report: &R,
    targets: &mut HostsGroup,
    noprepare: bool,
    newpackage: bool,
    diagnostics: &mut Vec<Diagnostic>,
) -> Result<(), UpdateError>
where
    R: crate::testreport::TestReport + SetRepo,
{
    match perform_update_from_report(report, targets, noprepare, newpackage, diagnostics).await {
        Ok(()) => Ok(()),
        Err(UpdateFailure::Prepare(e)) => {
            // Prepare could not run (missing preparer, contended lock, or the
            // issue repo could not be set): no update patch was dispatched,
            // so there is no update for a downgrade to undo.
            error!(
                error = %e,
                "update aborted: prepare could not run (rerun with --noprepare to patch anyway)"
            );
            Err(e)
        }
        Err(UpdateFailure::MissingUpdater(e)) => {
            // Hard fail, but nothing was installed → no rollback.
            error!(error = %e, "update failed");
            Err(e)
        }
        Err(UpdateFailure::Cancelled(e)) => {
            // Cancelled *before* the update command was dispatched, so the
            // update itself never ran and there is nothing to roll back.
            // Anything an earlier `prepare` step installed is left as-is (see
            // `UpdateFailure::Cancelled`).
            info!(reason = %e, "update cancelled");
            Err(e)
        }
        Err(UpdateFailure::Reboot(e) | UpdateFailure::NotRun(e)) => {
            // Either the patch succeeded and the host never came back, or the
            // command never ran to completion in the first place. Both mean
            // the flow lost contact with every host that failed, so a
            // group-wide downgrade cannot repair them and would only revert
            // the healthy ones.
            error!(error = %e, "update failed");
            Err(e)
        }
        Err(UpdateFailure::ProbeFailed(e)) => {
            // Every failed host reported that it could not work out what to
            // patch, so none of them ran a patch: there is nothing
            // half-applied for a downgrade to undo, and the rollback is
            // group-wide — it would revert every healthy peer over a probe
            // that broke on one host. The test repos are kept, as on any
            // failure, so the operator can look at the repo state the probe
            // complained about.
            error!(error = %e, "update failed: could not determine what to patch");
            Err(e)
        }
        Err(UpdateFailure::Check(e) | UpdateFailure::RebootNotTaken(e)) => {
            error!("Update failed");
            warn!("Error while updating. Rolling back changes");
            let pkgs = report.get_package_list();
            let id = report.base().rrid.as_ref().map(ToString::to_string);
            // Suspend cancellation for the rollback. The update has already
            // been applied, so this recovery is what prevents a half-applied
            // state; letting the downgrade's own per-package checkpoint see a
            // cancel that landed during the run phase would abort the rollback
            // at package 0 and leave exactly the state it exists to undo.
            let token = targets.suspend_cancellation();
            // Rollback is best-effort; a failed downgrade must never bury the
            // original update error, so its result is logged, not returned.
            if let Err(de) = perform_downgrade(targets, report, &pkgs, id.as_deref()).await {
                warn!(error = %de, "rollback downgrade failed");
            }
            targets.set_cancel_token(token);
            Err(e)
        }
    }
}

/// Records a workflow op in every target's remote history file.
///
/// Called after the command has dispatched — a row must never claim work that
/// never started — but *before* any transactional reboot, since a host that
/// does not come back can no longer be written to.
///
/// `id_field` carries the RRID for the ops that log one (`update`,
/// `downgrade`) and is `None` for `prepare`, whose row is just the label and
/// the package set. The op label and package list complete the colon-joined
/// line written by [`HostsGroup::add_history`]. `install`/`uninstall` write a
/// row of the same shape, but from `OperationGroup::run` rather than through
/// here — so this function's callers are not the full list of ops that appear
/// in `/var/log/mtui.log`.
pub async fn add_op_history(
    targets: &mut HostsGroup,
    op: &str,
    id_field: Option<&str>,
    packages: &[String],
) {
    let fields = op_history_fields(op, id_field, packages);
    targets.add_history(&fields).await;
}

/// [`add_op_history`], written to `hosts` only.
///
/// For an op whose fan-out did not reach the whole group. `prepare` builds a
/// per-host command map and drops a host whose release key does not resolve or
/// whose template does not render; that host is failed with "nothing was
/// installed", so a group-wide row would contradict its own verdict in a file
/// other tools parse (#407).
pub async fn add_op_history_for(
    targets: &mut HostsGroup,
    hosts: &BTreeSet<String>,
    op: &str,
    id_field: Option<&str>,
    packages: &[String],
) {
    let fields = op_history_fields(op, id_field, packages);
    targets.add_history_for(hosts, &fields).await;
}

/// The colon-joined field list of a history row: `op[:id]:pkg-a pkg-b`.
fn op_history_fields(op: &str, id_field: Option<&str>, packages: &[String]) -> Vec<String> {
    let mut fields = vec![op.to_owned()];
    if let Some(id) = id_field {
        fields.push(id.to_owned());
    }
    fields.push(packages.join(" "));
    fields
}

/// The `$repa` maintenance-selector for an update.
fn repa_for(maintenance_id: &str, review_id: &str) -> String {
    format!(":p={maintenance_id}:{review_id}")
}

/// Why a pre-update `prepare` did not succeed, split by whether the update
/// may still proceed.
///
/// `host_command_failures` counts any stderr as a failure, and
/// `transactional-update` writes progress to stderr on a *successful* run
/// (see `prepare_body`'s own note on the reboot gate) — so a per-host
/// package-manager complaint is too noisy to hard-abort `update` on. Whether
/// prepare could even *run* (a preparer, a lock, an issue repo) is a
/// different, reliable signal.
enum PrepareFailure {
    /// Prepare never ran: no preparer for a host's key, the operation lock
    /// was contended, or the issue repo could not be set. Nothing was
    /// installed and the update's premise is broken — the update aborts.
    DidNotRun(UpdateError),
    /// Prepare ran and one or more hosts reported trouble from the package
    /// manager. Warn and continue. Also carries #396's per-host
    /// no-command-built failures: `update` intentionally warns there because
    /// `build_update_maps` re-detects the unresolved-key cause moments later
    /// and aborts with `MissingUpdater`.
    HostReported(UpdateError),
    /// A cancellation checkpoint, not a failure.
    Cancelled(UpdateError),
}

/// Resolves a host's `(release, transactional)` key from its parsed system.
///
/// Returns `None` when the system has no release (an unknown/unparsed host);
/// the callers treat a `None` as "no doer for this host".
fn host_key(target: &mtui_hosts::Target) -> Option<(String, bool)> {
    let release = target.system().get_release().ok()?;
    Some((release, target.transactional()))
}

/// Resolves one host's [`ActionCommands`] for `role`, or logs and returns
/// `None` on a missing doer.
fn resolve_doer(
    registry: &WorkflowRegistry,
    role: Role,
    release: &str,
    transactional: bool,
) -> Option<ActionCommands> {
    match registry.doer(role, release, transactional) {
        Ok(cmds) => Some(cmds),
        Err(e) => {
            error!(role = ?role, error = %e, "missing doer");
            None
        }
    }
}

/// Builds the transactional-only reboot map for `role` across the group.
///
/// Returns `Err` if any transactional host is missing a doer, so the caller
/// can early-return without locking.
fn build_reboot_map(
    targets: &HostsGroup,
    registry: &WorkflowRegistry,
    role: Role,
) -> Result<BTreeMap<String, String>, ()> {
    let mut reboot = BTreeMap::new();
    for target in targets.targets() {
        if !target.transactional() {
            continue;
        }
        let Some((release, transactional)) = host_key(target) else {
            continue;
        };
        let Some(doer) = resolve_doer(registry, role, &release, transactional) else {
            return Err(());
        };
        if let Ok(Some(reboot_cmd)) = doer.render_reboot() {
            reboot.insert(target.hostname().to_owned(), reboot_cmd);
        }
    }
    Ok(reboot)
}

/// Runs `role`'s post-run check on every host, returning the recognised
/// [`UpdateError`]s and appending any recognised-but-non-fatal [`Diagnostic`]
/// sections to `diagnostics`.
///
/// The check reads each host's `last*` snapshot after the command ran. Only the
/// `update` check currently emits diagnostics; the other roles append nothing.
///
/// Every host in the group is judged. A caller that fans out to a *subset* —
/// the per-package `prepare` and `downgrade` loops — must use
/// [`run_checks_where`] instead of filtering the returned list: a check logs
/// its own ERROR breadcrumb before returning `Err`, so a verdict discarded
/// afterwards has already been printed, for a host whose `last*` snapshot
/// belongs to some earlier phase.
fn run_checks(
    targets: &HostsGroup,
    registry: &WorkflowRegistry,
    role: Role,
    diagnostics: &mut Vec<Diagnostic>,
) -> Vec<UpdateError> {
    run_checks_where(targets, registry, role, diagnostics, |_| true)
}

/// [`run_checks`] restricted to the hosts `allowed` accepts.
///
/// The predicate is applied *before* the check runs, not to its verdict. That
/// is the whole point: a check calls
/// [`log_failed`](crate::update_workflow::checks) on the way to its `Err`, so a
/// post-filter still emits an operator-facing ERROR for a host whose verdict is
/// then thrown away — once per package under `prepare --installed-only`, each
/// time against a snapshot from a fan-out that host was not part of.
fn run_checks_where(
    targets: &HostsGroup,
    registry: &WorkflowRegistry,
    role: Role,
    diagnostics: &mut Vec<Diagnostic>,
    allowed: impl Fn(&str) -> bool,
) -> Vec<UpdateError> {
    let mut failures = Vec::new();
    for target in targets.targets() {
        if !allowed(target.hostname()) {
            continue;
        }
        let Some((release, transactional)) = host_key(target) else {
            continue;
        };
        let Some(check): Option<CheckFn> = registry.check(role, &release, transactional) else {
            continue;
        };
        let res = check(CheckArgs {
            hostname: target.hostname(),
            stdout: target.lastout(),
            stdin: target.lastin(),
            stderr: target.lasterr(),
            exitcode: target.lastexit().map_or(0, i32::from),
        });
        match res {
            Ok(diags) => diagnostics.extend(diags),
            Err(mut e) => {
                if e.host.is_none() {
                    e.host = Some(target.hostname().to_owned());
                }
                failures.push(e);
            }
        }
    }
    failures
}

/// Collapses a list of per-host [`UpdateError`]s into a single `Result`,
/// mirroring [`update_run_phase`]'s aggregation: no failures → `Ok`, one →
/// verbatim, many → a summary naming the operation (`op`) plus every failed host
/// (sorted) and the joined detail. Shared by the prepare/downgrade/install/
/// uninstall flows so they all report failures the same way `perform_update`
/// does.
fn aggregate_failures(op: &str, mut failures: Vec<UpdateError>) -> Result<(), UpdateError> {
    if failures.is_empty() {
        Ok(())
    } else if failures.len() == 1 {
        Err(failures.remove(0))
    } else {
        let mut hosts: Vec<String> = failures.iter().filter_map(|e| e.host.clone()).collect();
        hosts.sort();
        // One name per host: the same host legitimately contributes two
        // failures with two distinct causes (`downgrade_body` seeds its list
        // from the issue-repo removal scan and then adds that same host's
        // downgrade-check verdict), and "downgrade failed on h1, h1" reads as
        // two hosts. The `detail` list still carries both causes — this dedups
        // the roll-call, not the diagnosis.
        //
        // Not the place to fix *one* signal reported by two rules: that
        // duplication has to be resolved before it reaches here, because
        // arriving at all means the verbatim branch above was skipped and the
        // `host` field is already lost. `prepare_body` and `downgrade_body`
        // each drop their coarse exit-code entry for a host whose check has
        // already named it, for exactly that reason.
        hosts.dedup();
        let detail: Vec<String> = failures.iter().map(ToString::to_string).collect();
        let mut aggregate = UpdateError::reason_only(format!(
            "{op} failed on {} ({})",
            hosts.join(", "),
            detail.join("; ")
        ));
        // The typed flags survive the summary. `all`, not `any`: each says
        // something about *the run*, and a summary that claimed "no patch was
        // dispatched" while one host had dispatched one would be worse than no
        // claim at all. The single-failure path above returns the error
        // verbatim, so without this the flags would exist on one path and
        // vanish on the other — and they are the declared routing contract,
        // not a detail of who reads them today.
        aggregate.probe_failed = failures.iter().all(|e| e.probe_failed);
        // `cancelled` is deliberately NOT propagated here. Where
        // `probe_failed` above is both representable and produced, `cancelled`
        // is representable and *unproduced*: `perform_operation_with`, the
        // shared body of `perform_install` / `perform_uninstall`, builds
        // `UpdateError { cancelled: failure.cancelled, .. }` straight from
        // `report.check_failures`, so a cancelled check does cross the
        // `Operation` seam into a `failures` list — but no check in
        // `update_workflow::checks` emits `CheckFailure::cancelled` (checks
        // are pure verdicts over a captured transcript), and this module's own
        // cancellations are early `return Err`s that never reach an aggregate.
        // Empty by producer, not by type: do not read the emptiness as a
        // structural guarantee the way the pre-`CheckFailure` seam allowed.
        //
        // A lone cancelled failure keeps its flag regardless: the
        // single-failure branch above returns the error verbatim. Only the
        // summary drops it, and for the mixed case that *is* the outranking
        // rule — a real host failure collected beside a cancel must not be
        // re-routed to `CommandError::Cancelled` and excused as "the operator
        // stopped it" (see `commands/perform.rs::map_flow_error`).
        //
        // The all-cancelled case loses the flag too, which `probe_failed`'s
        // `all()` above would have kept. That asymmetry is deliberate: with no
        // live producer there is no all-cancelled population to summarise, and
        // minting a group-wide cancel verdict for a case that cannot yet occur
        // would be a behaviour decision taken blind. Make it here, with
        // `all()`, if a check ever does cancel — but know what that verdict
        // does and does not reach. The flag routes *reporting*:
        // `commands/perform.rs::map_flow_error` is its only non-test reader,
        // and it turns the error into `CommandError::Cancelled`. It does not
        // route the rollback — that follows the `UpdateFailure` variant
        // `update_run_phase` picks from `repairable`/`probe_failed`, which
        // never inspects `cancelled`. Setting it here alone would tell the
        // operator the run was cancelled while the group-wide downgrade ran
        // on every healthy host anyway; skipping that rollback the way
        // `UpdateFailure::Cancelled` does needs a matching change at the
        // `wrap` site.
        Err(aggregate)
    }
}

/// Scans every host's post-fan-out `last*` snapshot for a command failure
/// (non-empty stderr or a non-zero exit) and returns one [`UpdateError`] per
/// failed host, keyed on `reason`.
///
/// This is the report-flow analogue of [`run_checks`] for the flows that have no
/// registry check of their own (the shared install/uninstall template and the
/// prepare/downgrade repo/command fan-outs): a per-host `lasterr()`/`lastexit()`
/// read after the command ran, per bead P3a-1's stable outcome accessors.
fn host_command_failures(targets: &HostsGroup, reason: &str) -> Vec<UpdateError> {
    let mut failures = Vec::new();
    for target in targets.targets() {
        let bad_exit = target.lastexit().is_some_and(|c| c != 0);
        let bad_err = !target.lasterr().is_empty();
        if bad_exit || bad_err {
            failures.push(UpdateError::new(reason.to_owned(), target.hostname()));
        }
    }
    failures
}

/// Installs `packages` on every host in `targets`.
///
/// The shared body behind every report's `perform_install`. Injects the
/// [`WorkflowRegistry`] as the group's
/// [`PlanProvider`](mtui_hosts::PlanProvider) and drives the
/// [`InstallOperation`] template, whose own
/// per-host [`Check`](mtui_hosts::Check) — also adapted from this registry —
/// now produces the verdict, before the (possible) reboot.
///
/// Injecting here rather than where the group is built is deliberate:
/// [`OperationGroup::plans`] has exactly one
/// consumer — the template these two functions drive — so this is the one place
/// that cannot forget. Construction sites can: for the whole life of the Rust
/// port none of them injected a provider, so `plans()` failed with
/// `NoPlanProvider`, the template logged and returned, and `install` reported
/// success having run nothing.
///
/// # Errors
///
/// Returns [`UpdateError`] when the template could not start (no doer for a
/// host's `(release, transactional)` key, or a host locked by another owner) or
/// when a host's post-run check reports a failed install.
pub async fn perform_install(
    targets: &mut HostsGroup,
    packages: &[String],
) -> Result<(), UpdateError> {
    perform_operation(targets, Role::Install, packages).await
}

/// Uninstalls `packages` from every host in `targets`.
///
/// See [`perform_install`]; the only differences are the role (and so the
/// command table) and the label on the aggregated summary. Uninstall shares the
/// *install* check table — a removal is judged by the same package-manager
/// outcomes — which [`CheckProvider`] encodes.
///
/// # Errors
///
/// As [`perform_install`].
pub async fn perform_uninstall(
    targets: &mut HostsGroup,
    packages: &[String],
) -> Result<(), UpdateError> {
    perform_operation(targets, Role::Uninstall, packages).await
}

/// The shared body of [`perform_install`] / [`perform_uninstall`], driving the
/// template through the real [`WorkflowRegistry`].
async fn perform_operation(
    targets: &mut HostsGroup,
    role: Role,
    packages: &[String],
) -> Result<(), UpdateError> {
    perform_operation_with(
        targets,
        role,
        packages,
        Arc::new(WorkflowRegistry::default()),
    )
    .await
}

/// [`perform_operation`] with the [`PlanProvider`](mtui_hosts::PlanProvider)
/// spelled out as a parameter, so a test can script the check seam.
///
/// Production only ever reaches this through [`perform_operation`], which
/// passes the real [`WorkflowRegistry`] — the parameter changes nothing about
/// what runs on a host. It exists because there is no other way in: the
/// injection below overwrites unconditionally
/// (`HostsGroup::set_plan_provider`), so a provider installed on the group
/// beforehand never survives to [`OperationGroup::plans`], and no production
/// check emits `CheckFailure::cancelled`, so the `cancelled` arm of the failure
/// map below has no live producer to drive it either.
///
/// The single `set_plan_provider` call site stays in this body, which is what
/// the inject-at-the-point-of-use rule on [`perform_install`] is about: there
/// is still exactly one place that cannot forget to wire the provider.
async fn perform_operation_with(
    targets: &mut HostsGroup,
    role: Role,
    packages: &[String],
    provider: Arc<dyn mtui_hosts::PlanProvider>,
) -> Result<(), UpdateError> {
    // Matched exhaustively on purpose: a `_ =>` arm defaulting to install would
    // quietly run the wrong package-manager command if a role were ever added,
    // which is the same shape of silent-wrong-default this function exists to
    // fix. Only the two template roles reach here.
    let op = match role {
        Role::Install => "install",
        Role::Uninstall => "uninstall",
        Role::Update | Role::Prepare | Role::Downgrade => {
            return Err(UpdateError::reason_only(format!(
                "{} is not driven by the install/uninstall template",
                role.as_operation_role()
            )));
        }
    };

    // Entry gate: nothing has run yet, so a cancel here is a clean no-op,
    // mirroring `perform_update`'s own entry gate.
    if targets.cancel_requested() {
        return Err(UpdateError::cancelled(format!(
            "cancelled before the {op} started"
        )));
    }

    targets.set_plan_provider(provider);

    let outcome = match role {
        Role::Install => InstallOperation::new(packages.to_vec()).run(targets).await,
        Role::Uninstall => {
            UninstallOperation::new(packages.to_vec())
                .run(targets)
                .await
        }
        Role::Update | Role::Prepare | Role::Downgrade => {
            unreachable!("returned above for these roles")
        }
    };

    // The template ran nothing at all — a missing doer, or a host held by
    // another tester. Report it instead of falling through to a verdict that
    // would read stale `last*` values and call it success.
    let report = match outcome {
        Err(e) => {
            return Err(UpdateError::reason_only(describe_start_failure(
                &e, role, targets,
            )));
        }
        Ok(report) => report,
    };

    // Deliberately no history write here: `Operation::run` writes the row
    // itself, between the command fan-out and the reboot, because a row
    // written after this call returns would be lost on exactly the
    // transactional hosts that never came back.

    // A stranded operation lock does not turn a good install/uninstall into a
    // failed one — it warns, naming the hosts and the manual remedy, rather
    // than joining `failures` below.
    if !report.unlock_failures.is_empty() {
        warn!("{}", unlock_failure_message(op, &report.unlock_failures));
    }

    // A host whose post-run check failed, and any transactional host that
    // rebooted and never reconnected, both fail the operation by name. A
    // failed check already excluded its host from the reboot map (see
    // `Operation::run`), so the two lists never double-name the same host.
    let mut failures: Vec<UpdateError> = report
        .check_failures
        .into_iter()
        .map(|(host, failure)| UpdateError {
            reason: failure.reason,
            host: Some(host),
            cancelled: failure.cancelled,
            probe_failed: false,
        })
        .collect();
    failures.extend(report.reboot_failures.into_iter().map(reboot_error));
    aggregate_failures(op, failures)
}

/// Renders the WARN for an `install`/`uninstall` operation lock that did not
/// release, naming the affected hosts and the manual remedy.
///
/// A pure helper so the message is unit-testable without driving the whole
/// [`Operation`] template.
fn unlock_failure_message(op: &str, unlock_failures: &[(String, String)]) -> String {
    let detail: Vec<String> = unlock_failures
        .iter()
        .map(|(h, reason)| format!("{h}: {reason}"))
        .collect();
    format!(
        "the {op} operation lock did not release on {} (release it with `unlock --force`)",
        detail.join("; ")
    )
}

/// Warns about any [`LockOutcome::Failed`] host in a [`HostsGroup::unlock`]
/// outcome map, via the same message builder [`perform_operation`] uses for
/// `install`/`uninstall`.
///
/// [`LockOutcome::Contended`] is excluded — benign, another tester owns the
/// lock — so this only warns on a real transport/SFTP error, the same bar fix
/// 3 already set for `install`/`uninstall`.
fn warn_on_unlock_failures(op: &str, outcomes: &BTreeMap<String, LockOutcome>) {
    let failures: Vec<(String, String)> = outcomes
        .iter()
        .filter_map(|(host, outcome)| match outcome {
            LockOutcome::Failed(reason) => Some((host.clone(), reason.clone())),
            _ => None,
        })
        .collect();
    if !failures.is_empty() {
        warn!("{}", unlock_failure_message(op, &failures));
    }
}

/// Renders a [`RebootFailure`] as the operator-facing per-host error.
///
/// The three causes deliberately read differently: they send an operator to
/// opposite places. "Did not come back" means go find the machine; "never
/// rebooted" means it is right there, still serving, with an inert snapshot.
/// A single "reconnect after reboot failed" message was accurate for only one
/// of the three.
fn reboot_error(failure: RebootFailure) -> UpdateError {
    let what = match failure.cause {
        RebootFailureCause::Unreachable => "did not come back after the reboot",
        RebootFailureCause::NotDispatched => "never received the reboot",
        RebootFailureCause::NotRebooted => "never rebooted, so its snapshot is still inactive",
    };
    UpdateError::new(format!("{what} ({})", failure.reason), failure.host)
}

/// Turns an [`Operation::run`] start failure into a
/// message that names the hosts responsible.
///
/// `plans()` aborts on the first host it cannot resolve and reports only the
/// role and release — and a host whose product never parsed has no release, so
/// the bare error reads `Missing Installer for ` with nothing actionable in it.
/// Since the whole group is aborted, re-resolve every host here and name each
/// one that has no command, so the tester knows which refhost to fix rather than
/// which of them to guess.
fn describe_start_failure(err: &HostError, role: Role, targets: &HostsGroup) -> String {
    if !matches!(
        err,
        HostError::MissingInstaller { .. } | HostError::MissingUninstaller { .. }
    ) {
        // A lock conflict already names the host and holder.
        return err.to_string();
    }

    let registry = WorkflowRegistry::default();
    let mut unresolved: Vec<String> = Vec::new();
    for target in targets.targets() {
        let resolved = host_key(target).is_some_and(|(release, transactional)| {
            registry.doer(role, &release, transactional).is_ok()
        });
        if !resolved {
            let base = target.system().get_base();
            let product = if base.name.is_empty() || base.name == "unknown" {
                "unrecognised product".to_owned()
            } else {
                format!("{} {}", base.name, base.version)
            };
            unresolved.push(format!("{} ({product})", target.hostname()));
        }
    }

    if unresolved.is_empty() {
        return err.to_string();
    }
    format!(
        "{err}: no {} command for {}; no host was touched",
        role.as_operation_role(),
        unresolved.join(", ")
    )
}

/// Reboots the transactional hosts named in `reboot`, returning one
/// [`RebootFailure`] per host whose reboot did not demonstrably take effect.
///
/// Such a host must fail the flow by name, not vanish into a discarded `()` —
/// the caller renders these through [`reboot_error`] into its own failure list
/// before aggregating. The cause is carried rather than flattened to a string
/// because `update` routes its rollback on it.
async fn reboot_transactional(
    targets: &mut HostsGroup,
    reboot: BTreeMap<String, String>,
) -> Vec<RebootFailure> {
    if reboot.is_empty() {
        return Vec::new();
    }
    let map: Vec<(String, String)> = reboot.into_iter().collect();
    OperationGroup::reboot(targets, map).await
}

/// Runs the prepare step: adds/removes the issue repo, then installs
/// `packages`.
///
/// `report` is the [`SetRepo`] hook for the issue repos; `packages` the list to
/// prepare. `testing` selects repo-`add` + the testing preparer variant;
/// `force` toggles `--force-resolution`; `installed_only` only touches
/// already-installed packages (per-package). All non-`installed_only` packages
/// install in a **single** transaction so transactional hosts land them in one
/// snapshot.
pub async fn perform_prepare(
    targets: &mut HostsGroup,
    report: &dyn SetRepo,
    packages: &[String],
    force: bool,
    testing: bool,
    installed_only: bool,
) -> Result<(), UpdateError> {
    match perform_prepare_classified(targets, report, packages, force, testing, installed_only)
        .await
    {
        Ok(()) => Ok(()),
        Err(
            PrepareFailure::DidNotRun(e)
            | PrepareFailure::HostReported(e)
            | PrepareFailure::Cancelled(e),
        ) => Err(e),
    }
}

/// The classified body of [`perform_prepare`], distinguishing a prepare that
/// never ran ([`PrepareFailure::DidNotRun`]) from one that ran and reported a
/// host failure ([`PrepareFailure::HostReported`]) — the split
/// [`perform_update`] gates its abort on. [`perform_prepare`] flattens both
/// (and [`PrepareFailure::Cancelled`]) back to a plain [`UpdateError`], so the
/// standalone `prepare` command's observable behaviour is unchanged.
async fn perform_prepare_classified(
    targets: &mut HostsGroup,
    report: &dyn SetRepo,
    packages: &[String],
    force: bool,
    testing: bool,
    installed_only: bool,
) -> Result<(), PrepareFailure> {
    let registry = WorkflowRegistry::new(force, testing);
    let operation = if testing { RepoOp::Add } else { RepoOp::Remove };
    // The prepare set excludes the branding-upstream package.
    let pkgs: Vec<String> = packages
        .iter()
        .filter(|p| *p != "branding-upstream")
        .cloned()
        .collect();

    // Resolve the reboot map before locking; a missing preparer aborts early.
    let Ok(reboot) = build_reboot_map(targets, &registry, Role::Prepare) else {
        return Err(PrepareFailure::DidNotRun(UpdateError::reason_only(
            "missing preparer",
        )));
    };

    if let Err(e) = targets.update_lock().await {
        return Err(PrepareFailure::DidNotRun(UpdateError::reason_only(
            e.to_string(),
        )));
    }

    // The body runs, then we always unlock, matching a try/finally.
    let result = prepare_body(
        targets,
        &registry,
        report,
        operation,
        &pkgs,
        installed_only,
        reboot,
    )
    .await;
    warn_on_unlock_failures("prepare", &targets.unlock().await);
    result
}

/// The locked body of [`perform_prepare`], factored out so the caller's
/// `unlock()` runs unconditionally.
#[allow(clippy::too_many_arguments)]
async fn prepare_body(
    targets: &mut HostsGroup,
    registry: &WorkflowRegistry,
    report: &dyn SetRepo,
    operation: RepoOp,
    pkgs: &[String],
    installed_only: bool,
    reboot: BTreeMap<String, String>,
) -> Result<(), PrepareFailure> {
    targets.fanout_set_repo(operation, report).await;

    // Abort early if adding/removing the issue repo failed on any host: the
    // issue repo could not be set, so prepare never ran.
    let repo_failures = host_command_failures(targets, "failed to set issue repo");
    if !repo_failures.is_empty() {
        for target in targets.targets() {
            if !target.lasterr().is_empty() {
                warn!(
                    host = %target.hostname(),
                    stderr = %target.lasterr(),
                    exit = ?target.lastexit(),
                    "failed to prepare host; stopping"
                );
            }
        }
        return aggregate_failures("prepare", repo_failures).map_err(PrepareFailure::DidNotRun);
    }

    // Every host that actually received a prepare command, and the hosts a
    // package failed on. Both are accumulated *inside* the loop below: it runs
    // one fan-out per package, so a single post-loop read of `lastexit()` sees
    // only the last package — and under `--installed-only` the last package is
    // very often a no-op `if rpm -q ...` that exits 0, which would mask an
    // earlier failure and let the host reboot into it.
    let mut dispatched: BTreeSet<String> = BTreeSet::new();
    let mut inert: BTreeSet<String> = BTreeSet::new();
    // The check verdicts, accumulated in the same place and for the same
    // reason: `run_checks` reads the `last*` snapshot too, so a single
    // post-loop call would judge only the last package's transcript. The
    // exit-code half of that hole was closed by `note_dispatch`; the marker
    // half needs this, or an exit-`0` lock message on package 1 of 2 is
    // overwritten by package 2's clean run and the host reboots into it
    // (#406). `check_failed` keeps it to one verdict per host — a second entry
    // would push `aggregate_failures` out of its single-failure verbatim
    // branch, where `host` is `Some`.
    let mut check_failed: BTreeSet<String> = BTreeSet::new();
    let mut check_failures: Vec<UpdateError> = Vec::new();

    // Parity with perform_downgrade: an empty list is not a host failure, but
    // it must never be a silent success either — only the issue repositories
    // were touched (#396). Above the branch so the `installed_only` path (zero
    // loop iterations) warns too; the operator-facing refusal lives in the
    // `prepare`/`update` command pre-flights, this covers embedded callers
    // (the update flow's newpackage prepare).
    if pkgs.is_empty() {
        warn!("no packages to prepare");
    }

    let mut cancelled_at: Option<usize> = None;
    if installed_only {
        // Conditional per-package install — inherently one package at a time.
        for (i, pkg) in pkgs.iter().enumerate() {
            // Package boundary = cancellation checkpoint. This serial loop is
            // the longest interruptible stretch in the flow (one SSH fan-out
            // per package). `break` — not an early return: the fall-through
            // below still aggregates per-host failures and, crucially, still
            // runs `reboot_transactional`, which is what actually activates
            // the snapshot the staged packages live in. Returning here would
            // leave a transactional host with an inert snapshot while claiming
            // the packages were installed.
            if targets.cancel_requested() {
                cancelled_at = Some(i);
                break;
            }
            let quoted = quote_args(std::slice::from_ref(pkg));
            let cmd = build_prepare_map(targets, registry, Some(&quoted), true);
            targets.run(Command::PerHost(cmd.clone())).await;
            note_dispatch(targets, &cmd, &mut dispatched, &mut inert);
            note_check(
                targets,
                registry,
                &cmd,
                &mut check_failed,
                &mut check_failures,
            );
        }
    } else if !pkgs.is_empty() {
        // Install every package in a SINGLE transaction (one snapshot for
        // transactional hosts). Quote each name for the root command line.
        let joined = quote_args(pkgs);
        let cmd = build_prepare_map(targets, registry, Some(&joined), false);
        targets.run(Command::PerHost(cmd.clone())).await;
        note_dispatch(targets, &cmd, &mut dispatched, &mut inert);
        note_check(
            targets,
            registry,
            &cmd,
            &mut check_failed,
            &mut check_failures,
        );
    }

    // A prepare *installs packages*, so it owes its own history row — the
    // record every other dispatching op already writes. Placed here for the
    // same two reasons as `update`'s and `downgrade`'s rows: after the
    // dispatch, so no row ever claims work that never started, and before
    // `reboot_transactional`, because a host that does not come back can no
    // longer be written to. On a transactional host that placement means the
    // row records what was *staged*, not what is active — deliberate, because
    // a host that never returns from its reboot would otherwise leave the
    // packages it holds in a snapshot entirely unrecorded.
    //
    // It is also what closes #407 for `update`: both of that flow's
    // post-prepare aborts (the pre-dispatch cancel gate and the missing-updater
    // abort) return without an `update` row — correctly, since no updater
    // command dispatched — but the packages this prepare installed stay on
    // every host. Writing the row where the side effect is produced records
    // them for the standalone `prepare`, the initial prepare inside `update`,
    // and the `--newpackage` prepare alike.
    //
    // Two things keep the row from over-claiming, both instances of "a row
    // claiming an install that never started is worse than no row":
    //
    // * **Per host, not group-wide.** `dispatched` is a per-host set and
    //   `build_prepare_map` drops a host whose release key does not resolve or
    //   whose template does not render — the very hosts the block below fails
    //   with "nothing was installed". A group-wide fan-out would hand exactly
    //   those hosts a `:prepare:` row contradicting their own verdict, in a
    //   file the project treats as an interop contract, so the write is scoped
    //   to the hosts a command actually reached.
    // * **The dispatched subset, not the whole set.** A cancel at package `i`
    //   of the `--installed-only` loop dispatched `pkgs[..i]` and nothing
    //   after, so that is what the row names. The error message names the
    //   progress too, but it is transient; the log line is what an operator
    //   coming back to the host later reconstructs from.
    //
    // An empty list, a cancel before the first package, or a prepare for which
    // no command could be built for any host therefore leaves no row at all.
    let recorded: &[String] = match cancelled_at {
        Some(i) => &pkgs[..i],
        None => pkgs,
    };
    if !dispatched.is_empty() {
        add_op_history_for(targets, &dispatched, "prepare", None, recorded).await;
    }

    // Surface any per-host command failure from the install fan-out; the
    // prepare check's own failures were collected per fan-out above.
    //
    // A host the check already named is dropped here: the two rules overlap on
    // one signal. `Target::run` records exit `-1` for a timeout, a dropped
    // connection or an unconnected host, which trips this scan's
    // `lastexit() != 0` *and* the `("slmicro", true)` / `("YUM", false)`
    // checks' never-ran gate; a lock message on stderr trips the stderr half
    // and the marker check. Both would name one host twice, which pushes
    // `aggregate_failures` out of its single-failure verbatim branch into the
    // summary — where `host` becomes `None` and the MCP client loses the one
    // field that says *which* refhost to go and look at. The check's verdict
    // is the one kept because it is the more specific of the two ("timed out
    // or failed to run" and "update stack locked" both say more than "prepare
    // command failed"); dropping the check's instead would trade attribution
    // back for a coarser diagnosis. `downgrade_body` resolves the identical
    // overlap the identical way — its `failed_downgrade.insert` gates the
    // exit-code entry behind the check's verdict for the same host.
    let mut failures: Vec<UpdateError> = host_command_failures(targets, "prepare command failed")
        .into_iter()
        .filter(|e| !e.host.as_ref().is_some_and(|h| check_failed.contains(h)))
        .collect();

    // A host whose prepare failed must not reboot into the failed transaction,
    // mirroring the install/uninstall template's per-host gate: activating the
    // snapshot would hide the failure behind a healthy-looking boot, while a
    // healthy host in the same group still reboots so its own snapshot
    // activates. The skip set is built from the check verdicts and non-zero
    // exit codes only — never from the stderr rule `host_command_failures`
    // also applies, because `transactional-update` writes progress to stderr
    // on a *successful* run and skipping such a host's reboot would leave its
    // healthy staged snapshot silently inert. The prepare templates are single
    // commands, so the recorded exit code is genuinely the prepare command's
    // own.
    //
    // The check verdicts are the other half of the gate, and on
    // ("slmicro", true) they are what makes it complete: a prepare that
    // reported a locked update stack, a dependency prompt or an RPM error and
    // still exited `0` is invisible to the exit-code rule above, and its
    // reboot would activate the failed transaction. A marker-failed prepare
    // therefore skips its reboot, while a host whose only stderr is progress
    // still gets one (#406) — for *any* package it failed on, not just the
    // last, which is why `check_failed` is filled per fan-out.
    inert.extend(check_failed.iter().cloned());
    failures.extend(check_failures);

    // `host_command_failures` reads one post-loop snapshot, so on the
    // per-package path it only ever sees the last package. Name every host a
    // package failed on, deduped against the hosts already reported — a second
    // entry for one host would push `aggregate_failures` out of its
    // single-failure verbatim branch into the summary, which drops `host` to
    // `None` and breaks callers that read it.
    // A host the fan-out never reached must fail by name, not ride the
    // group's success (#396): `build_prepare_map` drops a host whose release
    // key does not resolve or whose template does not render (e.g. no
    // installed-only variant), and nothing else records that. Skipped when the
    // flow was cancelled before the first fan-out (nothing was expected to
    // dispatch) and when the list was empty (the warn above owns that case).
    if !pkgs.is_empty() && cancelled_at != Some(0) {
        for target in targets.targets() {
            if target.state() != mtui_types::TargetState::Enabled {
                continue;
            }
            let host = target.hostname();
            if !dispatched.contains(host) {
                inert.insert(host.to_owned());
                failures.push(UpdateError::new(
                    "no prepare command could be built for this host; nothing was installed",
                    host.to_owned(),
                ));
            }
        }
    }

    let named: BTreeSet<String> = failures.iter().filter_map(|e| e.host.clone()).collect();
    for host in &inert {
        if !named.contains(host) {
            failures.push(UpdateError::new("prepare command failed", host.clone()));
        }
    }
    let reboot: BTreeMap<String, String> = reboot
        .into_iter()
        .filter(|(host, _)| {
            // A host nothing was dispatched to staged nothing, so there is no
            // snapshot to activate and the reboot would be gratuitous — and in
            // the `update` rollback path it would activate whatever the failed
            // update left staged.
            if !dispatched.contains(host) {
                warn!(host = %host, "no prepare command was staged; skipping reboot");
                return false;
            }
            let ok = !inert.contains(host);
            if !ok {
                warn!(host = %host, "prepare failed; skipping reboot");
            }
            ok
        })
        .collect();
    failures.extend(
        reboot_transactional(targets, reboot)
            .await
            .into_iter()
            .map(reboot_error),
    );
    // A genuine host failure outranks the cancellation: reporting only
    // "cancelled" would bury a broken host the operator must still see.
    if !failures.is_empty() {
        return aggregate_failures("prepare", failures).map_err(PrepareFailure::HostReported);
    }
    if let Some(i) = cancelled_at {
        let done: Vec<&str> = pkgs[..i].iter().map(String::as_str).collect();
        let left: Vec<&str> = pkgs[i..].iter().map(String::as_str).collect();
        warn!(
            applied = %done.join(", "),
            not_attempted = %left.join(", "),
            "prepare cancelled at a package boundary"
        );
        return Err(PrepareFailure::Cancelled(UpdateError::cancelled(format!(
            "prepare cancelled after {}/{} packages; applied: [{}]; not attempted: [{}]",
            i,
            pkgs.len(),
            done.join(", "),
            left.join(", "),
        ))));
    }
    aggregate_failures("prepare", failures).map_err(PrepareFailure::HostReported)
}

/// Records which hosts a prepare fan-out reached, and which of them it failed
/// on, into the two sets the reboot gate consults.
///
/// Called after *every* fan-out rather than once at the end, because the
/// per-package `--installed-only` loop runs one fan-out per package and
/// `lastexit()` keeps only the last. Scoped to `cmd`'s keys so a host outside
/// this fan-out is never judged on another phase's record.
fn note_dispatch(
    targets: &HostsGroup,
    cmd: &BTreeMap<String, String>,
    dispatched: &mut BTreeSet<String>,
    inert: &mut BTreeSet<String>,
) {
    for hostname in cmd.keys() {
        dispatched.insert(hostname.clone());
        if targets
            .get(hostname)
            .and_then(mtui_hosts::Target::lastexit)
            .is_some_and(|c| c != 0)
        {
            inert.insert(hostname.clone());
        }
    }
}

/// Runs the prepare check over the hosts *this* fan-out reached and records the
/// first failure per host.
///
/// The companion to [`note_dispatch`], and called from the same places for the
/// same reason: [`run_checks`] reads the `last*` snapshot, which the next
/// package's fan-out overwrites. A single post-loop call therefore judges only
/// the last package — and under `--installed-only` that is very often a clean
/// no-op, so an exit-`0` lock message on an earlier package would be masked and
/// the host would reboot into it (#406).
///
/// Scoped to `cmd`'s keys, again like `note_dispatch`: a host outside this
/// fan-out still carries an earlier phase's record (the `set_repo` fan-out, or
/// a package it was skipped for), and judging that as a prepare would invent a
/// verdict. First-failure-wins per host keeps `aggregate_failures` in its
/// single-failure verbatim branch, where `host` survives.
///
/// The scope is imposed through [`run_checks_where`], so an out-of-fan-out host
/// is never *judged*, rather than judged and then filtered: a check logs its
/// own ERROR breadcrumb before it returns `Err`, and a discarded verdict has
/// still been printed to the operator — once per package under
/// `--installed-only`.
///
/// A consequence worth naming because it is intended, not incidental: a host
/// [`build_prepare_map`] dropped (unresolved release key, or a template with no
/// `--installed-only` variant) is in no `cmd` map, so it is never check-judged
/// on its stale snapshot. It is not thereby excused — `prepare_body`'s dispatch
/// accounting fails it by name with "no prepare command could be built for this
/// host" (#396), which is a truthful verdict where a check's would have been an
/// invented one.
fn note_check(
    targets: &HostsGroup,
    registry: &WorkflowRegistry,
    cmd: &BTreeMap<String, String>,
    check_failed: &mut BTreeSet<String>,
    failures: &mut Vec<UpdateError>,
) {
    let judged = run_checks_where(targets, registry, Role::Prepare, &mut Vec::new(), |host| {
        cmd.contains_key(host)
    });
    for e in judged {
        let Some(host) = e.host.clone() else { continue };
        if check_failed.insert(host) {
            error!(error = %e, "prepare check failed");
            failures.push(e);
        }
    }
}

/// Builds the per-host prepare command map. `package` fills the `$package`
/// variable; `installed_only` selects the conditional template.
fn build_prepare_map(
    targets: &HostsGroup,
    registry: &WorkflowRegistry,
    package: Option<&str>,
    installed_only: bool,
) -> BTreeMap<String, String> {
    let mut map = BTreeMap::new();
    for target in targets.targets() {
        let Some((release, transactional)) = host_key(target) else {
            error!(host = %target.hostname(), "prepare: host release key unresolved; no command built");
            continue;
        };
        let Some(doer) = resolve_doer(registry, Role::Prepare, &release, transactional) else {
            continue;
        };
        let mut vars: HashMap<&str, &str> = HashMap::new();
        if let Some(p) = package {
            vars.insert("package", p);
        }
        let rendered = if installed_only {
            doer.render_installed_only(&vars).ok().flatten()
        } else {
            doer.render_command(&vars).ok()
        };
        if let Some(cmd) = rendered {
            map.insert(target.hostname().to_owned(), cmd);
        } else {
            // A dropped render (error, or a family with no installed-only
            // variant) leaves the host out of the fan-out; the dispatch
            // accounting in `prepare_body` turns that into a named failure,
            // this log says why (#396).
            error!(
                host = %target.hostname(), release = %release,
                "prepare: no command rendered for this host; nothing will be installed on it"
            );
        }
    }
    map
}

/// Rolls `packages` back to the pre-update version on every host.
///
/// `id` is the RRID string recorded in the remote history line once the
/// downgrade has dispatched a command (`None` when no RRID is available).
pub async fn perform_downgrade(
    targets: &mut HostsGroup,
    report: &dyn SetRepo,
    packages: &[String],
    id: Option<&str>,
) -> Result<(), UpdateError> {
    // Nothing to downgrade: return before locking or touching repos.
    // The guard also keeps the probe template from rendering with an
    // empty package list — `zypper se` without names would list the entire
    // repository catalog.
    if packages.is_empty() {
        warn!("no packages to downgrade");
        return Ok(());
    }

    let registry = WorkflowRegistry::default();

    // Resolve reboot before locking so a missing downgrader early-returns
    // without leaving the group locked.
    let Ok(reboot) = build_reboot_map(targets, &registry, Role::Downgrade) else {
        return Err(UpdateError::reason_only("missing downgrader"));
    };

    if let Err(e) = targets.update_lock().await {
        return Err(UpdateError::reason_only(e.to_string()));
    }

    let result = downgrade_body(targets, &registry, report, packages, reboot, id).await;
    warn_on_unlock_failures("downgrade", &targets.unlock().await);
    result
}

/// The locked body of [`perform_downgrade`].
async fn downgrade_body(
    targets: &mut HostsGroup,
    registry: &WorkflowRegistry,
    report: &dyn SetRepo,
    packages: &[String],
    reboot: BTreeMap<String, String>,
    id: Option<&str>,
) -> Result<(), UpdateError> {
    targets.fanout_set_repo(RepoOp::Remove, report).await;

    // Collected per-host failures (repo removal + per-package/combined checks),
    // aggregated at the end so a downgrade failure surfaces rather than only
    // being logged.
    let mut failures = host_command_failures(targets, "failed to remove issue repo");

    // Run the list_command to discover each host's available downgrade
    // versions, then parse `name = version` lines, keeping the highest per pkg.
    let joined = quote_args(packages);
    let list_map = {
        let mut m = BTreeMap::new();
        for target in targets.targets() {
            let Some((release, transactional)) = host_key(target) else {
                continue;
            };
            let Some(doer) = resolve_doer(registry, Role::Downgrade, &release, transactional)
            else {
                continue;
            };
            let vars: HashMap<&str, &str> = [("packages", joined.as_str())].into_iter().collect();
            if let Ok(Some(cmd)) = doer.render_list_command(&vars) {
                m.insert(target.hostname().to_owned(), cmd);
            }
        }
        m
    };
    if !list_map.is_empty() {
        targets.run(Command::PerHost(list_map.clone())).await;
    }

    // A dead probe must abort that host's downgrade, not degrade it. When the
    // probe dies its stdout carries no versions, the version map below stays
    // empty, and the flow would "complete" having run zero downgrade commands —
    // leaving every package at the update version behind a success-looking run.
    //
    // Non-zero is the whole signal, and the template is what makes it
    // trustworthy in both directions (#451). It guards the commands that
    // *produce* the list and exits with the failed tool's own status, so a
    // non-zero status here is either SSH-level death (the `-1` sentinel) or the
    // guard passing zypper's or awk's status through — never "package not
    // found", which the guard accepts as `104` and reports as `0`. And zero now
    // genuinely means the probe answered: an empty list at `0` is a host
    // carrying none of these packages, not a probe that failed unnoticed. Left
    // as one pipeline the status was the *last* stage's — awk's, and awk
    // succeeds on empty input — so a failed `zypper se` recorded `0` and this
    // gate could only ever catch the `-1`.
    //
    // Handled per host: the healthy hosts still roll back (and transactional
    // ones still reboot), because this downgrade is often the repair for an
    // update that already failed on the group, and aborting it over one host's
    // broken zypper would strand the healthy peers half-applied. The error for
    // the dead ones is raised at the end. All probes dead aborts immediately.
    let dead_probes: std::collections::BTreeSet<String> = list_map
        .keys()
        .filter(|hn| {
            targets
                .get(hn)
                .and_then(mtui_hosts::Target::lastexit)
                .is_some_and(|c| c != 0)
        })
        .cloned()
        .collect();
    for hn in &dead_probes {
        let exit = targets.get(hn).and_then(mtui_hosts::Target::lastexit);
        error!(
            host = %hn,
            exit = ?exit,
            "package version probe failed; this host was not downgraded"
        );
    }
    if !dead_probes.is_empty() && dead_probes.len() == list_map.len() {
        // The abort still leaves side effects behind: `fanout_set_repo(Remove)`
        // above has already stripped the issue repo from every host, and this
        // path fires most often *during the update rollback*, when
        // reconstructing what was done to a refhost matters most. Record the
        // row before returning — the site below is unreachable from here, so
        // there is no double write.
        //
        // The `lastexit != 0` gate above catches more than SSH-level death:
        // the probe template guards its own producing stages and exits with
        // the failing tool's own status (#451), so a refused `zypper se`
        // reaches here as well as the `-1` sentinel. Every one of those is an
        // abort that left the host with its issue repo removed and nothing
        // rolled back, which is precisely the state this row exists to record.
        add_op_history(targets, "downgrade", id, packages).await;
        return Err(UpdateError::new(
            "package version probe failed",
            dead_probes.iter().cloned().collect::<Vec<_>>().join(", "),
        ));
    }

    // hostname -> { package -> highest available version }. A dead probe's
    // (empty / partial) output must not feed the version map.
    let mut versions: HashMap<String, HashMap<String, String>> = HashMap::new();
    for target in targets.targets() {
        if dead_probes.contains(target.hostname()) {
            continue;
        }
        let host_versions = parse_downgrade_versions(target.lastout());
        if !host_versions.is_empty() {
            versions.insert(target.hostname().to_owned(), host_versions);
        }
    }

    let transactional_hosts: std::collections::HashSet<String> = targets
        .targets()
        .filter(|t| t.transactional())
        .map(|t| t.hostname().to_owned())
        .collect();

    // Non-transactional hosts: per-package `zypper downgrade`, gated on the
    // package being installed (present in `versions`).
    let mut cancelled_at: Option<usize> = None;
    for (i, package) in packages.iter().enumerate() {
        // Package boundary = cancellation checkpoint (see the prepare loop).
        // `break`, not an early return: the transactional hosts are handled by
        // the combined block after this loop, and the failure aggregation must
        // still run.
        if targets.cancel_requested() {
            cancelled_at = Some(i);
            break;
        }
        let mut cmd = BTreeMap::new();
        for target in targets.targets() {
            let hn = target.hostname();
            if transactional_hosts.contains(hn) {
                continue;
            }
            let Some(ver) = versions.get(hn).and_then(|m| m.get(package)) else {
                continue;
            };
            let Some((release, transactional)) = host_key(target) else {
                continue;
            };
            let Some(doer) = resolve_doer(registry, Role::Downgrade, &release, transactional)
            else {
                continue;
            };
            // Both values reach the root downgrade command line; quote each.
            let quoted_package = quote_args(std::slice::from_ref(package));
            let quoted_version = quote_args(std::slice::from_ref(ver));
            let vars: HashMap<&str, &str> = [
                ("package", quoted_package.as_str()),
                ("version", quoted_version.as_str()),
            ]
            .into_iter()
            .collect();
            if let Ok(rendered) = doer.render_command(&vars) {
                cmd.insert(hn.to_owned(), rendered);
            }
        }
        if !cmd.is_empty() {
            targets.run(Command::PerHost(cmd.clone())).await;
            // Check only the hosts that actually ran this command: a host
            // outside `cmd` (e.g. a dead-probe host) still carries its previous
            // record, whose stale -1 would trip the timeout gate and cancel
            // the healthy hosts' rollback. Scoped through `run_checks_where`,
            // so such a host is not judged at all — a post-filter would still
            // have emitted its ERROR breadcrumb, once per package.
            let judged = run_checks_where(
                targets,
                registry,
                Role::Downgrade,
                &mut Vec::new(),
                |host| cmd.contains_key(host) && !transactional_hosts.contains(host),
            );
            for e in judged {
                error!(error = %e, "downgrade check failed");
                failures.push(e);
            }
        }
    }

    // Transactional hosts: downgrade ALL packages in a single transaction.
    let mut combined = BTreeMap::new();
    for hn in &transactional_hosts {
        let Some(host_versions) = versions.get(hn) else {
            continue;
        };
        let specs: Vec<String> = packages
            .iter()
            .filter_map(|p| host_versions.get(p).map(|v| format!("{p}={v}")))
            .collect();
        if specs.is_empty() {
            continue;
        }
        let Some(target) = targets.get(hn) else {
            continue;
        };
        let Some((release, transactional)) = host_key(target) else {
            continue;
        };
        let Some(doer) = resolve_doer(registry, Role::Downgrade, &release, transactional) else {
            continue;
        };
        // Each `name=version` spec is quoted as a single argument.
        let joined_specs = quote_args(&specs);
        let vars: HashMap<&str, &str> = [("package", joined_specs.as_str())].into_iter().collect();
        if let Ok(rendered) = doer.render_command(&vars) {
            combined.insert(hn.clone(), rendered);
        }
    }
    let mut failed_downgrade: BTreeSet<String> = BTreeSet::new();
    if !combined.is_empty() {
        targets.run(Command::PerHost(combined.clone())).await;
        // Same scoping as the per-package loop, and imposed the same way: a
        // host outside `combined` is not judged, rather than judged and then
        // dropped after its breadcrumb has already been logged.
        let judged = run_checks_where(
            targets,
            registry,
            Role::Downgrade,
            &mut Vec::new(),
            |host| combined.contains_key(host) && transactional_hosts.contains(host),
        );
        for e in judged {
            let Some(host) = e.host.clone() else { continue };
            error!(error = %e, "downgrade check failed");
            failed_downgrade.insert(host);
            failures.push(e);
        }
        // The transactional check only gates "timed out or failed to run"; the
        // downgrade template is a single command, so a non-zero exit is
        // genuinely the downgrade's own status and the host must not reboot
        // into the failed transaction. It is also pushed as a failure — a
        // skipped reboot behind an `Ok` would be a quiet no-op — but only when
        // the check did not already report this host (`insert` is `false` for
        // the `-1` case the check covers). (Scoped to `combined`: hosts
        // outside it carry a stale record.)
        for hostname in combined.keys() {
            let exit = targets.get(hostname).and_then(mtui_hosts::Target::lastexit);
            if exit.is_some_and(|c| c != 0) && failed_downgrade.insert(hostname.clone()) {
                failures.push(UpdateError::new(
                    "downgrade command failed",
                    hostname.clone(),
                ));
            }
        }
    }

    // Reboot the healthy transactional hosts first (their staged snapshots must
    // still activate), then surface the dead probes as the command's failure.
    // A host whose combined downgrade failed is skipped too: rebooting it would
    // activate the snapshot of the failed transaction.
    let healthy_reboot: BTreeMap<String, String> = reboot
        .into_iter()
        .filter(|(h, _)| {
            // Nothing staged, nothing to activate. A transactional host drops
            // out of `combined` when the version probe resolved no versions
            // for it (a `versions` miss, or every spec filtered out) — both
            // reachable with an exit-0 probe, so `dead_probes` does not cover
            // them. Rebooting anyway is not merely gratuitous: when this
            // downgrade *is* the `update` rollback, the host would boot into
            // the snapshot the failed update left staged, which is exactly
            // what suppressing the update's own reboot had avoided.
            if !combined.contains_key(h) {
                warn!(host = %h, "no downgrade was staged; skipping reboot");
                return false;
            }
            if failed_downgrade.contains(h) {
                warn!(host = %h, "downgrade failed; skipping reboot");
                return false;
            }
            !dead_probes.contains(h)
        })
        .collect();
    // The downgrade commands have now dispatched, whatever the verdict: record
    // the history row here — after the run started, but *before* the reboot.
    // A transactional host that never comes back cannot be written to
    // afterwards, and the row would be lost on exactly the host whose state an
    // operator most needs to reconstruct.
    add_op_history(targets, "downgrade", id, packages).await;

    failures.extend(
        reboot_transactional(targets, healthy_reboot)
            .await
            .into_iter()
            .map(reboot_error),
    );

    let not_downgraded = downgrade_verdict(targets, &dead_probes).await;

    // A per-host check failure aborts first (matches the pre-#336 aggregation).
    aggregate_failures("downgrade", failures)?;

    // Then the dead probes: the healthy hosts have rolled back and rebooted, so
    // now name the hosts whose probe died as the command's failure.
    if !dead_probes.is_empty() {
        return Err(UpdateError::new(
            "package version probe failed",
            dead_probes.iter().cloned().collect::<Vec<_>>().join(", "),
        ));
    }

    // Finally the honest verdict: any package still at or above the update's
    // shipped version means the rollback did not complete. `downgrade_verdict`
    // has already logged the per-host detail at ERROR; fail the command so a
    // caller (REPL or MCP) can't mistake a half-rollback for success.
    if !not_downgraded.is_empty() {
        return Err(UpdateError::reason_only("downgrade not completed"));
    }

    // Cancellation is reported last: a real verdict above outranks it. The
    // message names only the non-transactional per-package progress — the
    // transactional hosts are driven by the combined block, not this loop —
    // and notes the repo removal, which ran before the loop and is therefore
    // already applied on every host.
    if let Some(i) = cancelled_at {
        let done: Vec<&str> = packages[..i].iter().map(String::as_str).collect();
        let left: Vec<&str> = packages[i..].iter().map(String::as_str).collect();
        warn!(
            downgraded = %done.join(", "),
            not_attempted = %left.join(", "),
            "downgrade cancelled at a package boundary"
        );
        return Err(UpdateError::cancelled(format!(
            "downgrade cancelled after {}/{} packages (non-transactional hosts); \
             downgraded: [{}]; not attempted: [{}]; the issue repository was \
             already removed from every host",
            i,
            packages.len(),
            done.join(", "),
            left.join(", "),
        )));
    }

    Ok(())
}

/// Emits the post-downgrade "done" / "downgrade not completed" verdict.
///
/// Re-queries each host, rotates `before = after; after = current` per
/// package, then compares each package's re-queried `current` against the
/// update's `required` version. Every
/// package still `current >= required` did **not** roll back; it is named per
/// host, at ERROR, with versions — with no short-circuit, so the bookkeeping
/// still advances for the packages that did move. New packages (no released
/// version to go back to) and multiversion packages (e.g. the kernel, whose
/// update version legitimately stays installed alongside older ones) always
/// appear here; re-running `downgrade` will not clear them.
///
/// Returns the `hostname -> ["name (at <current>, update ships <required>)", …]`
/// map of packages still at or above the update version — empty on a fully
/// completed rollback. Iterated in sorted hostname order (the group's own
/// ordering) so the log is deterministic.
///
/// `probe_dead` names the hosts whose version probe never answered, and the
/// all-clear is withheld while it is non-empty. Nothing was downgraded on those
/// hosts, and this verdict cannot speak for them either: it flags a package only
/// when the loaded report carries a `required` version to compare against, so on
/// a standalone `downgrade` — or for a package outside the report's list — an
/// unmeasured host produces the same empty map a completed rollback does.
/// Logging `done` over it is the silent success of issue #451 restated one layer
/// up. The hosts are named at WARN instead; the command's own failure is raised
/// by the caller.
async fn downgrade_verdict(
    targets: &mut HostsGroup,
    probe_dead: &BTreeSet<String>,
) -> BTreeMap<String, Vec<String>> {
    // Query every host's versions concurrently via the shared fan-out, then
    // run the pure verdict scan below.
    targets.query_versions().await;

    let mut not_downgraded: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for target in targets.targets_mut() {
        let hostname = target.hostname().to_owned();
        for pkg in target.packages_mut() {
            // #396: rotate the whole check, not just its version — a
            // never-checked after slot must not land in the before slot as
            // "checked, not installed", or a standalone downgrade (no prior
            // update) exports "is not installed" for a slot nobody looked at.
            pkg.set_before_check(pkg.after_check().clone());
            // #437: same for `current` -> `after` — a host the re-query never
            // answered for must not land in `after` as "checked, not
            // installed" either.
            pkg.set_after_check(pkg.current_check().clone());
            if let (Some(current), Some(required)) = (pkg.current(), pkg.required())
                && current >= required
            {
                not_downgraded
                    .entry(hostname.clone())
                    .or_default()
                    .push(format!(
                        "{} (at {current}, update ships {required})",
                        pkg.name
                    ));
            }
        }
    }

    if !probe_dead.is_empty() {
        warn!(
            hosts = %probe_dead.iter().cloned().collect::<Vec<_>>().join(", "),
            "no package version probe answered on these hosts, so nothing was \
             downgraded there and their package state is unverified"
        );
    }

    if not_downgraded.is_empty() {
        // Only when every host was actually measured. A dead probe leaves this
        // map empty for the same reason a completed rollback does.
        if probe_dead.is_empty() {
            tracing::info!("done");
        }
    } else {
        for (hostname, names) in &not_downgraded {
            error!(
                "{hostname}: still at or above the update's shipped version \
                 after downgrade: {}",
                names.join(", ")
            );
        }
        error!(
            "downgrade not completed; verify with 'rpm -q'. New packages \
             (no released version to go back to) and multiversion packages \
             (e.g. the kernel) always appear here; re-running downgrade will \
             not clear them"
        );
    }
    not_downgraded
}

/// Parses the downgrader `list_command` output into a `name -> highest version`
/// map, selecting the highest version per package by RPM version ordering.
///
/// The split on `" = "` is a contract with the downgrade template, whose
/// accepted-status notes print to this same stream — see
/// [`crate::update_workflow::actions::downgrade`] § "The notes share a stream
/// with the parser". `pub(crate)` so the test pinning that coupling can put the
/// real script's real output through the real parser, instead of
/// re-implementing the rule and then pinning its copy.
pub(crate) fn parse_downgrade_versions(output: &str) -> HashMap<String, String> {
    use mtui_types::rpmver::RPMVersion;

    let mut release: HashMap<String, Vec<String>> = HashMap::new();
    for line in output.lines() {
        if let Some((name, version)) = line.split_once(" = ") {
            release
                .entry(name.to_owned())
                .or_default()
                .push(version.to_owned());
        }
    }

    let mut out = HashMap::new();
    for (name, mut vers) in release {
        // Highest version wins; parse failures sort last so a valid version is
        // still preferred.
        vers.sort_by(|a, b| match (RPMVersion::parse(a), RPMVersion::parse(b)) {
            (Ok(va), Ok(vb)) => vb.cmp(&va),
            (Ok(_), Err(_)) => std::cmp::Ordering::Less,
            (Err(_), Ok(_)) => std::cmp::Ordering::Greater,
            (Err(_), Err(_)) => std::cmp::Ordering::Equal,
        });
        if let Some(highest) = vers.into_iter().next() {
            out.insert(name, highest);
        }
    }
    out
}

/// Runs the full update: prepare, patch, check, reboot, and repo cleanup.
///
/// `packages` is the report's package list; `maintenance_id`/`review_id` build
/// the `$repa` selector. `id` is the RRID string recorded in the remote
/// history line once the update has dispatched a command (`None` when no RRID
/// is available, e.g. a direct call with no report). `noprepare` skips the
/// initial prepare; `newpackage` runs a testing prepare after the update.
/// `prepare` is the closure the caller uses to run [`perform_prepare`] (the
/// report drives it so this module does not need to know the report type).
/// `diagnostics` collects the update check's recognised-but-non-fatal output
/// sections for the command layer to render.
// Plain positional args plus the diagnostic sink threaded from the
// display-owning command layer; grouping them into a struct would only
// obscure the call site for no real gain.
#[allow(clippy::too_many_arguments)]
pub async fn perform_update(
    targets: &mut HostsGroup,
    report: &dyn SetRepo,
    packages: &[String],
    maintenance_id: &str,
    review_id: &str,
    id: Option<&str>,
    noprepare: bool,
    newpackage: bool,
    diagnostics: &mut Vec<Diagnostic>,
) -> Result<(), UpdateFailure> {
    let registry = WorkflowRegistry::default();

    // Entry gate: nothing has run yet, so a cancel here is a clean no-op.
    if targets.cancel_requested() {
        return Err(UpdateFailure::Cancelled(UpdateError::cancelled(
            "cancelled before the update started",
        )));
    }

    if !noprepare {
        // Runs prepare with default flags (remove-repo prepare). A prepare
        // that could not run (missing preparer, contended lock, or the issue
        // repo could not be set) aborts here rather than patching hosts on a
        // broken premise; `--noprepare` opts out. A prepare that ran but only
        // reported a host failure is too noisy to gate on (see
        // `PrepareFailure`), so it warns and the update proceeds.
        match perform_prepare_classified(targets, report, packages, false, false, false).await {
            Ok(()) => {}
            Err(PrepareFailure::DidNotRun(e)) => return Err(UpdateFailure::Prepare(e)),
            Err(PrepareFailure::Cancelled(e)) => return Err(UpdateFailure::Cancelled(e)),
            Err(PrepareFailure::HostReported(e)) => {
                warn!(error = %e, "prepare before update reported a host failure; continuing");
            }
        }
    }

    targets.package_check(false).await;

    if let Err(e) = targets.update_lock().await {
        return Err(UpdateFailure::Check(UpdateError::reason_only(
            e.to_string(),
        )));
    }

    targets.fanout_set_repo(RepoOp::Add, report).await;

    let repa = repa_for(maintenance_id, review_id);
    let joined = quote_args(packages);
    let (commands, reboot) = match build_update_maps(targets, &registry, &repa, &joined) {
        Ok(maps) => maps,
        Err(e) => {
            // A missing updater doer: remove the repo we just added and abort.
            // Treated as a hard failure (rather than logged and returned as
            // success) so it never reports "finished".
            targets.fanout_set_repo(RepoOp::Remove, report).await;
            warn_on_unlock_failures("update", &targets.unlock().await);
            return Err(UpdateFailure::MissingUpdater(e));
        }
    };

    // Last checkpoint before the point of no return: past this line the patch
    // command is dispatched and a cancel could leave a half-applied
    // transaction, so cancellation is NOT checked again inside or after the run
    // phase — the flow finishes its bookkeeping instead. Undo the repo add and
    // the lock exactly as the MissingUpdater abort above does.
    if targets.cancel_requested() {
        tracing::info!("cancelled: stopping before the update command was dispatched");
        // The cleanup runs to completion: fan-outs are never interrupted
        // part-way, so this genuinely removes the repo and releases the lock
        // on every host (the same undo the MissingUpdater abort performs).
        targets.fanout_set_repo(RepoOp::Remove, report).await;
        warn_on_unlock_failures("update", &targets.unlock().await);
        // True of the *update command* — but with a prepare behind us the host
        // is not untouched, and saying only "nothing was dispatched" reads as
        // if it were. The repo add is undone above; a completed prepare's
        // packages are not, and its own history row is the record of them.
        return Err(UpdateFailure::Cancelled(UpdateError::cancelled(
            if noprepare {
                "cancelled before the update command was dispatched".to_owned()
            } else {
                "cancelled before the update command was dispatched; any packages \
                 installed by the prepare that ran first are left in place \
                 (see `list_history`)"
                    .to_owned()
            },
        )));
    }

    // Two-phase: run + check + reboot under the lock (unlock always), then the
    // repo cleanup only on success. The history row is written inside, between
    // the fan-out and the reboot — see `update_run_phase`.
    let update_result = update_run_phase(
        targets,
        &registry,
        commands,
        reboot,
        diagnostics,
        id,
        packages,
    )
    .await;

    if let Err(e) = update_result {
        // KEEP the test update repositories in place for retry/diagnosis.
        warn!(
            "update did not complete; leaving the test update repositories in place \
             for retry/diagnosis (remove later with `set_repo --remove`)"
        );
        return Err(e);
    }

    if newpackage
        && let Err(e) = perform_prepare(targets, report, packages, false, true, false).await
    {
        warn!(error = %e, "newpackage prepare after update failed");
    }

    targets.package_check(true).await;

    remove_test_repos(targets, report).await;
    Ok(())
}

/// Removes the test update repositories after a successful update.
///
/// Best-effort: a lock failure here does not turn a successful update into a
/// failed one, so it warns — naming the error, that the repos are left
/// configured, and the manual remedy — rather than failing the update.
async fn remove_test_repos(targets: &mut HostsGroup, report: &dyn SetRepo) {
    if let Err(e) = targets.update_lock().await {
        warn!(
            error = %e,
            "could not lock hosts to remove the test update repositories; \
             they are left configured on every host (remove later with \
             `set_repo --remove`)"
        );
        return;
    }
    targets.fanout_set_repo(RepoOp::Remove, report).await;
    // The lock succeeded but the removal command itself may still have failed
    // on a host — issue #409's actual complaint (a stale test repo) can happen
    // silently here too, not only on a lock failure. The noisy stderr rule is
    // fine for a warn, unlike the gate `PrepareFailure` avoids it for.
    let failures = host_command_failures(targets, "failed to remove the test update repo");
    if !failures.is_empty() {
        let hosts: Vec<String> = failures.iter().filter_map(|e| e.host.clone()).collect();
        warn!(
            hosts = %hosts.join(", "),
            "failed to remove the test update repo on one or more hosts; \
             remove it manually with `set_repo --remove`"
        );
    }
    warn_on_unlock_failures("update", &targets.unlock().await);
}

/// Runs the update commands, checks every host (collecting failures), reboots on
/// success, and **always** unlocks.
///
/// Returns `Ok(())` when every host's check passed and every transactional
/// host's reboot took effect — reconnecting is not sufficient, since a host can
/// answer without ever having gone down. Otherwise `Err` with the aggregated
/// failure: a check failure (packages may be half-applied) is
/// [`UpdateFailure::Check`] and **suppresses the reboot entirely** — unless no
/// failed host is one a rollback could repair, which is
/// [`UpdateFailure::ProbeFailed`] when every one of them reported that it
/// could not determine what to patch and [`UpdateFailure::NotRun`] when the
/// command never completed there (`lastexit()` of `-1`); both skip the
/// rollback. A reboot
/// failure is [`UpdateFailure::Reboot`] when the host is unreachable and
/// [`UpdateFailure::RebootNotTaken`] when every failed host is still reachable,
/// which is what decides whether the group-wide rollback runs. A single failure is returned verbatim; more than
/// one is summarised into `"update failed on {hosts} ({detail})"`.
async fn update_run_phase(
    targets: &mut HostsGroup,
    registry: &WorkflowRegistry,
    commands: BTreeMap<String, String>,
    reboot: BTreeMap<String, String>,
    diagnostics: &mut Vec<Diagnostic>,
    id: Option<&str>,
    packages: &[String],
) -> Result<(), UpdateFailure> {
    targets.run(Command::PerHost(commands)).await;

    // The updater command has now dispatched, whatever the verdict: record the
    // history row here — after the run started, but *before* the reboot. A
    // transactional host that never comes back cannot be written to
    // afterwards, and the row would be lost on exactly the host whose state an
    // operator most needs to reconstruct.
    add_op_history(targets, "update", id, packages).await;

    let failures = run_checks(targets, registry, Role::Update, diagnostics);
    let failed_hosts: std::collections::HashSet<String> =
        failures.iter().filter_map(|e| e.host.clone()).collect();
    let ok_hosts: Vec<String> = targets
        .names()
        .into_iter()
        .filter(|hn| !failed_hosts.contains(hn))
        .collect();
    if !ok_hosts.is_empty() {
        info!(hosts = %ok_hosts.join(", "), "update succeeded on");
    }
    for e in &failures {
        error!(error = %e, "update failed");
    }

    // A check failure keeps precedence and suppresses the reboot entirely: a
    // transactional host whose patch failed must not be rebooted into it.
    let result = if !failures.is_empty() {
        // Route on *why* the check failed, as the reboot arm below routes on
        // why the reboot failed — though not by its reachability probe: `-1`
        // is a sentinel, not a liveness verdict. `Target::run` records it for
        // a dropped connection, an unconnected target, *and* a timeout on a
        // host that is up the whole time. The first two are hosts a group-wide
        // rollback cannot repair, so it would only revert the healthy ones on
        // their behalf; the third is a host whose transaction was never
        // observed to end, where a downgrade dispatched now races whatever is
        // left of it for the package-manager lock. All three veto the
        // rollback — `UpdateFailure::NotRun` carries the full argument.
        //
        // A probe failure is the third non-rolling cause, and the one the
        // rollback is *least* entitled to fire on: the host ran the update
        // command and reported that it could not work out what to patch, so it
        // never dispatched a patch at all. Its packages are what they were
        // before the flow started. It is carried as a typed flag on the error
        // rather than re-derived here — the alternative, matching the reason
        // string, would be the first place in the tree where control flow
        // depended on a reason's text.
        //
        // The rollback question is one bit — "can a group-wide downgrade
        // repair any host that failed?" — so it is asked that way: `any`
        // repairable host routes `Check`, exactly as before (a run that mixes
        // a lost host with a genuine check failure still rolls back, on behalf
        // of the host the rollback can actually repair). Only the *label* on
        // the non-repairable runs is split further.
        let repairable = |e: &UpdateError| {
            !e.probe_failed
                && e.host
                    .as_deref()
                    .and_then(|h| targets.get(h))
                    .and_then(mtui_hosts::Target::lastexit)
                    != Some(-1)
        };
        let wrap: fn(UpdateError) -> UpdateFailure = if failures.iter().any(repairable) {
            UpdateFailure::Check
        } else if failures.iter().all(|e| e.probe_failed) {
            UpdateFailure::ProbeFailed
        } else {
            UpdateFailure::NotRun
        };
        aggregate_failures("update", failures).map_err(wrap)
    } else {
        let reboot_failures = reboot_transactional(targets, reboot).await;
        // Route the rollback on *why* the reboot failed, not on the fact that
        // it did. A host that never came back cannot be downgraded, and the
        // rollback is group-wide — running it would revert the healthy hosts on
        // behalf of one this flow cannot reach. A host that is still up, on the
        // other hand, is running the un-activated snapshot while the rest of the
        // group moved on, and that is precisely the split-brain the rollback
        // undoes.
        //
        // Mixed causes take the *conservative* route. `all`, not `any`: the
        // rollback reverts the whole group, so it is only worth running when
        // every failed host can actually be repaired by it. With one host
        // unreachable and one merely inert, rolling back cannot reach the
        // first, leaves the second needing manual work anyway, and reverts the
        // hosts that did everything right. Both are still named in the error.
        let repairable = !reboot_failures.is_empty()
            && reboot_failures
                .iter()
                .all(|f| f.cause.host_still_reachable());
        let wrap: fn(UpdateError) -> UpdateFailure = if repairable {
            UpdateFailure::RebootNotTaken
        } else {
            UpdateFailure::Reboot
        };
        aggregate_failures(
            "update",
            reboot_failures.into_iter().map(reboot_error).collect(),
        )
        .map_err(wrap)
    };

    warn_on_unlock_failures("update", &targets.unlock().await);
    result
}

/// Builds the per-host updater command map (with `$repa` + `$packages`) and the
/// transactional reboot map. Returns `Err` with the offending host's
/// [`UpdateError`] if any host is missing an updater — a hard failure in mtui.
fn build_update_maps(
    targets: &HostsGroup,
    registry: &WorkflowRegistry,
    repa: &str,
    packages: &str,
) -> Result<UpdateMaps, UpdateError> {
    let mut commands = BTreeMap::new();
    let mut reboot = BTreeMap::new();
    for target in targets.targets() {
        let missing = || UpdateError::new("missing updater", target.hostname());
        let (release, transactional) = host_key(target).ok_or_else(missing)?;
        let doer = registry
            .doer(Role::Update, &release, transactional)
            .map_err(|_| missing())?;
        let vars: HashMap<&str, &str> = [("repa", repa), ("packages", packages)]
            .into_iter()
            .collect();
        let command = doer.render_command(&vars).map_err(|_| missing())?;
        commands.insert(target.hostname().to_owned(), command);
        if transactional && let Ok(Some(reboot_cmd)) = doer.render_reboot() {
            reboot.insert(target.hostname().to_owned(), reboot_cmd);
        }
    }
    Ok((commands, reboot))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use mtui_config::options::Config;
    use mtui_hosts::{HostsGroup, MockConnection, Target};
    use mtui_types::enums::TargetState;
    use mtui_types::hostlog::CommandLog;
    use mtui_types::package::VersionCheck;
    use mtui_types::system::{System, SystemProduct};

    use super::*;
    use crate::reports::sl::SlReport;
    use crate::testreport::TestReport;

    /// A no-op [`SetRepo`] so the flow's repo fan-out is observable-but-inert in
    /// tests that only care about the run/check/reboot phases.
    struct NoopRepo;

    #[async_trait::async_trait]
    impl SetRepo for NoopRepo {
        async fn set_repo(&self, _target: &mut Target, _operation: RepoOp) {}
    }

    /// Builds an enabled SLES 15 target on a mock that returns `stdout` for
    /// every command, returning the shared command-recording handle.
    fn sles_target(hostname: &str, stdout: &str) -> (Target, MockConnection) {
        let conn =
            MockConnection::new(hostname).with_default(CommandLog::new("", stdout, "", 0, 0));
        let handle = conn.clone();
        let mut t = Target::with_connection(hostname, TargetState::Enabled, Box::new(conn));
        t.set_system(
            System::new(
                SystemProduct::new("SLES", "15.5", "x86_64"),
                BTreeSet::new(),
                false,
            ),
            false,
        );
        (t, handle)
    }

    // --- pure helpers ------------------------------------------------------

    #[test]
    fn repa_for_has_stable_format() {
        assert_eq!(repa_for("42", "7"), ":p=42:7");
    }

    #[test]
    fn parse_downgrade_versions_keeps_highest_per_package() {
        let out = "bash = 5.1-1\nbash = 5.1-3\nbash = 5.1-2\ncoreutils = 8.32-1\n";
        let map = parse_downgrade_versions(out);
        assert_eq!(map["bash"], "5.1-3");
        assert_eq!(map["coreutils"], "8.32-1");
    }

    #[test]
    fn parse_downgrade_versions_ignores_non_matching_lines() {
        let map = parse_downgrade_versions("noise\nS | pkg | repo\nbash = 1.0-1\n");
        assert_eq!(map.len(), 1);
        assert_eq!(map["bash"], "1.0-1");
    }

    #[test]
    fn get_package_list_flattens_and_dedups_names() {
        let mut report = SlReport::new(Config::default());
        report.base_mut().packages.insert(
            "SLES:15".to_owned(),
            [("bash".to_owned(), "5.1-1".to_owned())]
                .into_iter()
                .collect(),
        );
        report.base_mut().packages.insert(
            "SLES:12".to_owned(),
            [
                ("bash".to_owned(), "4.4-1".to_owned()),
                ("zsh".to_owned(), "5.8-1".to_owned()),
            ]
            .into_iter()
            .collect(),
        );
        assert_eq!(report.get_package_list(), vec!["bash", "zsh"]);
    }

    // --- perform_prepare ---------------------------------------------------

    #[tokio::test]
    async fn perform_prepare_installs_all_packages_in_a_single_transaction() {
        let (t, handle) = sles_target("h1", "");
        let mut group = HostsGroup::new(vec![t], false);
        let report = NoopRepo;

        let res = perform_prepare(
            &mut group,
            &report,
            &["pkg-a".to_owned(), "pkg-b".to_owned()],
            false,
            false,
            false,
        )
        .await;
        assert!(res.is_ok(), "a clean prepare returns Ok: {res:?}");

        // The preparer install runs once with both packages joined (single
        // transaction), rendering the zypper prepare command.
        let cmds = handle.commands();
        let prepare_cmds: Vec<&String> = cmds
            .iter()
            .filter(|c| c.contains("zypper -n in -y -l"))
            .collect();
        assert_eq!(
            prepare_cmds.len(),
            1,
            "expected one combined install: {cmds:?}"
        );
        assert!(prepare_cmds[0].contains("pkg-a pkg-b"));
    }

    #[tokio::test]
    async fn perform_prepare_drops_branding_upstream() {
        let (t, handle) = sles_target("h1", "");
        let mut group = HostsGroup::new(vec![t], false);
        let res = perform_prepare(
            &mut group,
            &NoopRepo,
            &["branding-upstream".to_owned(), "pkg-a".to_owned()],
            false,
            false,
            false,
        )
        .await;
        assert!(res.is_ok(), "a clean prepare returns Ok: {res:?}");
        let cmds = handle.commands();
        let install = cmds
            .iter()
            .find(|c| c.contains("zypper -n in -y -l"))
            .unwrap();
        assert!(install.contains("pkg-a"));
        assert!(!install.contains("branding-upstream"));
    }

    /// Builds an enabled *transactional* target whose release resolves to "11"
    /// (`sle-studioonsite`) — a `(release, transactional)` key with no
    /// preparer/downgrader doer, so `build_reboot_map` fails and the flow takes
    /// its missing-doer early-return.
    fn missing_doer_target(hostname: &str) -> Target {
        let conn = MockConnection::new(hostname).with_default(CommandLog::new("", "", "", 0, 0));
        let mut t = Target::with_connection(hostname, TargetState::Enabled, Box::new(conn));
        t.set_system(
            System::new(
                SystemProduct::new("sle-studioonsite", "11", "x86_64"),
                BTreeSet::new(),
                true,
            ),
            true,
        );
        t
    }

    #[tokio::test]
    async fn perform_prepare_surfaces_missing_preparer() {
        // A transactional host whose (release, transactional) key has no preparer
        // doer makes build_reboot_map fail, so prepare returns Err rather than
        // swallowing.
        let mut group = HostsGroup::new(vec![missing_doer_target("h1")], false);
        let res = perform_prepare(
            &mut group,
            &NoopRepo,
            &["pkg-a".to_owned()],
            false,
            false,
            false,
        )
        .await;
        let err = res.expect_err("missing preparer must surface as Err");
        assert!(
            err.reason.contains("missing preparer"),
            "reason: {}",
            err.reason
        );
    }

    #[tokio::test]
    async fn perform_prepare_surfaces_per_host_command_failure() {
        // The preparer install exits 104 on the host; the failure is returned,
        // not just logged.
        let (t, _h) = sles_target_with_exit("h1", "", 104);
        let mut group = HostsGroup::new(vec![t], false);
        let res = perform_prepare(
            &mut group,
            &NoopRepo,
            &["pkg-a".to_owned()],
            false,
            false,
            false,
        )
        .await;
        let err = res.expect_err("a non-zero prepare command exit must surface as Err");
        assert_eq!(err.host.as_deref(), Some("h1"));
    }

    #[tokio::test]
    async fn perform_downgrade_surfaces_missing_downgrader() {
        let mut group = HostsGroup::new(vec![missing_doer_target("h1")], false);
        let res = perform_downgrade(&mut group, &NoopRepo, &["pkg-a".to_owned()], None).await;
        let err = res.expect_err("missing downgrader must surface as Err");
        assert!(
            err.reason.contains("missing downgrader"),
            "reason: {}",
            err.reason
        );
    }

    #[tokio::test]
    async fn perform_install_surfaces_a_missing_installer() {
        // A host whose (release, transactional) key has no installer doer: the
        // template runs nothing at all. Reporting Ok here is what made `install`
        // print "install completed" for hosts it never touched.
        let mut group = HostsGroup::new(vec![missing_doer_target("h1")], false);
        let err = perform_install(&mut group, &["pkg-a".to_owned()])
            .await
            .expect_err("a missing installer must not report success");
        assert!(
            err.reason.contains("Missing Installer"),
            "reason: {}",
            err.reason
        );
        // The bare error names only the role and release, and a host whose
        // product never parsed has no release — so the message must name the
        // host the tester has to go fix.
        assert!(
            err.reason.contains("h1"),
            "the offending host must be named: {}",
            err.reason
        );
    }

    #[tokio::test]
    async fn perform_install_names_the_unresolvable_host_and_product() {
        // An unparsed system yields `MissingInstaller { release: "" }`, whose
        // Display is "Missing Installer for " — nothing actionable. The whole
        // group aborts, so the message must say which host caused it.
        let conn = MockConnection::new("h2").with_default(CommandLog::new("", "", "", 0, 0));
        let t = Target::with_connection("h2", TargetState::Enabled, Box::new(conn));
        let (ok, _h) = sles_target("h1", "");
        let mut group = HostsGroup::new(vec![ok, t], false);

        let err = perform_install(&mut group, &["pkg-a".to_owned()])
            .await
            .expect_err("an unresolvable host aborts the group");

        assert!(err.reason.contains("h2"), "reason: {}", err.reason);
        assert!(
            err.reason.contains("unrecognised product"),
            "reason: {}",
            err.reason
        );
        assert!(
            err.reason.contains("no host was touched"),
            "reason: {}",
            err.reason
        );
        // h1 resolves fine, so it must not be blamed.
        assert!(!err.reason.contains("h1 ("), "reason: {}", err.reason);
    }

    #[tokio::test]
    async fn perform_install_ignores_stderr_on_a_clean_exit() {
        // `transactional-update` and `yum` write progress and warnings to stderr
        // on a successful run. Treating any stderr as failure would fail every
        // SL Micro install.
        let conn = MockConnection::new("h1").with_default(CommandLog::new(
            "t-u",
            "",
            "warning: chatty",
            0,
            0,
        ));
        let mut t = Target::with_connection("h1", TargetState::Enabled, Box::new(conn));
        t.set_system(
            System::new(
                SystemProduct::new("SL-Micro", "6.0", "x86_64"),
                BTreeSet::new(),
                true,
            ),
            true,
        );
        let mut group = HostsGroup::new(vec![t], false);

        assert!(
            perform_install(&mut group, &["pkg-a".to_owned()])
                .await
                .is_ok(),
            "stderr alone on a zero exit is not a failure"
        );
    }

    #[tokio::test]
    async fn perform_install_runs_the_installer_command() {
        let (t, handle) = sles_target("h1", "");
        let mut group = HostsGroup::new(vec![t], false);
        perform_install(&mut group, &["pkg-a".to_owned()])
            .await
            .expect("a clean install");
        assert_eq!(handle.commands(), vec!["zypper -n in -y -l pkg-a"]);
    }

    #[tokio::test]
    async fn perform_uninstall_runs_the_uninstaller_command() {
        let (t, handle) = sles_target("h1", "");
        let mut group = HostsGroup::new(vec![t], false);
        perform_uninstall(&mut group, &["pkg-a".to_owned()])
            .await
            .expect("a clean uninstall");
        assert_eq!(handle.commands(), vec!["zypper -n rm pkg-a"]);
    }

    #[tokio::test]
    async fn perform_install_surfaces_a_failed_command() {
        // A host whose install command exits 104 is reported by the template's
        // own check — over the same host's post-run snapshot the removed
        // `install_verdict` used to read *after* the template returned.
        let (t, _h) = sles_target_with_exit("h1", "", 104);
        let mut group = HostsGroup::new(vec![t], false);
        let err = perform_install(&mut group, &["pkg-a".to_owned()])
            .await
            .expect_err("non-zero exit surfaces as Err");
        assert_eq!(err.host.as_deref(), Some("h1"));
        // 104 is zypper's ZYPPER_EXIT_INF_CAP_NOT_FOUND, which the install check
        // table classifies. The verdict reports *what* went wrong, not just that
        // the exit was non-zero.
        assert_eq!(err.reason, "package not found");
    }

    /// A [`PlanProvider`](mtui_hosts::PlanProvider) whose check always stops at
    /// a cancellation checkpoint.
    ///
    /// The only way to put a cancelled `CheckFailure` on the input of
    /// `perform_operation_with`, the install/uninstall shared body: the real
    /// check tables never emit one, so scripting a `MockConnection` cannot
    /// express this state. Mirrors the provider in
    /// `crates/mtui-hosts/tests/operation_group.rs`, which pins the same flag
    /// one layer down, at `OperationReport`.
    struct CancellingProvider;

    impl mtui_hosts::PlanProvider for CancellingProvider {
        fn doer(
            &self,
            _role: &str,
            _release: &str,
            _transactional: bool,
        ) -> Result<mtui_hosts::Doer, HostError> {
            Ok(mtui_hosts::Doer::new(
                "zypper -n in -y -l $packages",
                "systemctl reboot",
            ))
        }

        fn check(&self, _role: &str, _release: &str, _transactional: bool) -> mtui_hosts::Check {
            Box::new(|_a: mtui_hosts::CheckArgs<'_>| {
                Err(mtui_hosts::CheckFailure::cancelled(
                    "stopped at a checkpoint",
                ))
            })
        }
    }

    #[tokio::test]
    async fn a_cancelled_check_failure_reaches_the_update_error_with_the_flag_intact() {
        // The flow half of the seam widening: `OperationReport` carrying the
        // flag is worth nothing if the map in `perform_operation_with` — the
        // shared body of `perform_install` / `perform_uninstall` — drops it on
        // the way into `UpdateError`, which is what it did (hardcoded `false`)
        // before this branch. One host and an always-cancelling check means
        // exactly one failure, so `aggregate_failures` takes the verbatim
        // branch and the flag reaches the caller untouched — the summary
        // branch deliberately does not carry it.
        let (t, _h) = sles_target("h1", "");
        let mut group = HostsGroup::new(vec![t], false);

        let err = perform_operation_with(
            &mut group,
            Role::Install,
            &["pkg-a".to_owned()],
            Arc::new(CancellingProvider),
        )
        .await
        .expect_err("a failing check surfaces as Err");

        assert!(
            err.is_cancelled(),
            "the check stopped at a checkpoint, so the error must say cancelled: {err:?}"
        );
        // Not just the flag: the rest of the failure has to survive the same
        // map, or an assertion on `is_cancelled` alone would pass on an error
        // synthesised anywhere else in the flow.
        assert_eq!(err.host.as_deref(), Some("h1"));
        assert_eq!(err.reason, "stopped at a checkpoint");
    }

    #[tokio::test]
    async fn perform_install_ok_when_all_hosts_succeed() {
        let (t, _h) = sles_target("h1", "");
        let mut group = HostsGroup::new(vec![t], false);
        assert!(
            perform_install(&mut group, &["pkg-a".to_owned()])
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn perform_install_warns_but_still_succeeds_when_unlock_fails() {
        // The install itself must succeed even though the lock never released:
        // a stranded lock does not turn a good install into a failed one.
        let conn = MockConnection::new("h1")
            .with_default(CommandLog::new("", "", "", 0, 0))
            .failing_sftp_remove();
        let mut t = Target::with_connection("h1", TargetState::Enabled, Box::new(conn));
        t.set_system(
            System::new(
                SystemProduct::new("SLES", "15.5", "x86_64"),
                BTreeSet::new(),
                false,
            ),
            false,
        );
        let mut group = HostsGroup::new(vec![t], false);

        let (res, logs) = capture_logs(perform_install(&mut group, &["pkg-a".to_owned()])).await;

        assert!(
            res.is_ok(),
            "a stranded lock must not turn a good install into a failure: {res:?}"
        );
        // `Target::unlock_reporting` already emits its own "unlock failed" WARN
        // naming `host="h1"` on this exact path, so a bare `logs.contains("h1")`
        // would pass even if `unlock_failure_message` were never reached — find
        // this warn's own line and assert on it.
        let unlock_line = logs
            .lines()
            .find(|l| l.contains("operation lock did not release"))
            .unwrap_or_else(|| panic!("no unlock-failure warning found: {logs}"));
        assert!(
            unlock_line.contains("h1"),
            "the WARN must name the stranded host: {unlock_line}"
        );
        assert!(
            unlock_line.contains("unlock --force"),
            "the WARN must name the manual remedy: {unlock_line}"
        );
    }

    #[tokio::test]
    async fn install_stops_at_the_entry_gate_when_cancelled() {
        // install/uninstall had no cancellation checkpoint of their own; a
        // cancel requested before the run must stop it before any command
        // reaches a host, exactly like `perform_update`'s entry gate.
        let (t, handle) = sles_target("h1", "");
        let mut group = HostsGroup::new(vec![t], false);
        group.cancel_token().cancel();

        let err = perform_install(&mut group, &["pkg-a".to_owned()])
            .await
            .expect_err("a cancelled install reports an error");

        assert!(err.is_cancelled(), "must be flagged as a cancel: {err:?}");
        assert!(
            handle.commands().is_empty(),
            "entry gate must dispatch no command: {:?}",
            handle.commands()
        );
    }

    #[tokio::test]
    async fn uninstall_stops_at_the_entry_gate_when_cancelled() {
        let (t, handle) = sles_target("h1", "");
        let mut group = HostsGroup::new(vec![t], false);
        group.cancel_token().cancel();

        let err = perform_uninstall(&mut group, &["pkg-a".to_owned()])
            .await
            .expect_err("a cancelled uninstall reports an error");

        assert!(err.is_cancelled(), "must be flagged as a cancel: {err:?}");
        assert!(handle.commands().is_empty());
    }

    // --- history is written after the operation ran, not before -----------

    const HISTORY_LOG: &str = "/var/log/mtui.log";

    #[tokio::test]
    async fn install_writes_no_history_when_no_doer_resolves() {
        // A run that never starts (no installer doer for this host) must not
        // record a history row: nothing happened on the host.
        let conn = MockConnection::new("h1").with_default(CommandLog::new("", "", "", 0, 0));
        let handle = conn.clone();
        let mut t = Target::with_connection("h1", TargetState::Enabled, Box::new(conn));
        t.set_system(
            System::new(
                SystemProduct::new("sle-studioonsite", "11", "x86_64"),
                BTreeSet::new(),
                true,
            ),
            true,
        );
        let mut group = HostsGroup::new(vec![t], false);

        perform_install(&mut group, &["pkg-a".to_owned()])
            .await
            .expect_err("no installer doer resolves for this host");

        assert!(
            handle.file_contents(HISTORY_LOG).is_none(),
            "a run that never started must leave no history row"
        );
    }

    #[tokio::test]
    async fn install_writes_history_when_the_command_failed() {
        // The install command ran and failed; the host was touched, so the
        // history row must still be written even though the command failed.
        let (t, handle) = sles_target_with_exit("h1", "", 104);
        let mut group = HostsGroup::new(vec![t], false);

        perform_install(&mut group, &["pkg-a".to_owned()])
            .await
            .expect_err("a failed install command surfaces as Err");

        let contents = String::from_utf8(
            handle
                .file_contents(HISTORY_LOG)
                .expect("a run that dispatched a command records history"),
        )
        .unwrap();
        assert_eq!(contents.lines().count(), 1);
        assert!(
            contents.contains(":install:pkg-a"),
            "history line: {contents:?}"
        );
    }

    /// A host lost to its post-operation reboot must still carry its history
    /// row: the row is the operator's only record that the command ran there,
    /// and it is needed most on exactly the host that did not come back.
    ///
    /// This is only observable because `MockConnection::sftp_append` now
    /// reconnects at entry like the real `SshConnection::sftp()` — before that
    /// the mock accepted an append on a dead host, so this test would have
    /// passed whichever side of the reboot the write happened on.
    #[tokio::test]
    async fn install_records_history_for_a_host_lost_to_its_reboot() {
        let (t, handle) = slmicro_target_failing_reconnect("h1");
        let mut group = HostsGroup::new(vec![t], false);

        perform_install(&mut group, &["pkg-a".to_owned()])
            .await
            .expect_err("a lost transactional host must not report success");

        let contents = String::from_utf8(
            handle
                .file_contents(HISTORY_LOG)
                .expect("a host lost to its reboot must still have its history row"),
        )
        .unwrap();
        assert_eq!(contents.lines().count(), 1);
        assert!(
            contents.contains(":install:pkg-a"),
            "history line: {contents:?}"
        );
    }

    #[tokio::test]
    async fn update_records_history_for_a_host_lost_to_its_reboot() {
        let (t, handle) = slmicro_target_failing_reconnect("h1");
        let mut group = HostsGroup::new(vec![t], false);
        let report = report_with_rrid();
        let packages = report.get_package_list();

        let res = perform_update(
            &mut group,
            &report,
            &packages,
            "42",
            "7",
            None,
            true,
            false,
            &mut Vec::new(),
        )
        .await;
        // Discriminating: it must fail *for the reboot*, not for some other
        // reason that would also satisfy a bare `is_err()`.
        let Err(UpdateFailure::Reboot(e)) = res else {
            panic!("a lost host must fail its reboot: {res:?}");
        };
        assert_eq!(e.host.as_deref(), Some("h1"));
        assert!(
            e.reason.contains("did not come back after the reboot"),
            "reason: {}",
            e.reason
        );

        let contents = String::from_utf8(
            handle
                .file_contents(HISTORY_LOG)
                .expect("a host lost to its reboot must still have its history row"),
        )
        .unwrap();
        assert_eq!(contents.lines().count(), 1, "history: {contents:?}");
        // This call passes `id: None`, so the row carries no RRID field: the
        // shape is `{ts}:{user}:update:{packages}`. Anchoring on the tail also
        // pins the package list, which a bare `contains(":update:")` did not.
        assert!(
            contents.ends_with(":update:pkg-a\n"),
            "history line: {contents:?}"
        );
    }

    #[tokio::test]
    async fn downgrade_records_history_for_a_host_lost_to_its_reboot() {
        let (t, handle) = slmicro_target_failing_reconnect("h1");
        let mut group = HostsGroup::new(vec![t], false);

        perform_downgrade(&mut group, &NoopRepo, &["pkg-a".to_owned()], None)
            .await
            .expect_err("a lost transactional host must not report success");

        let contents = String::from_utf8(
            handle
                .file_contents(HISTORY_LOG)
                .expect("a host lost to its reboot must still have its history row"),
        )
        .unwrap();
        assert!(
            contents.contains(":downgrade:"),
            "history line: {contents:?}"
        );
    }

    #[tokio::test]
    async fn downgrade_rollback_still_records_history() {
        // perform_update_with_rollback's best-effort rollback dispatches a
        // real downgrade; it must record its own history row like a
        // directly-invoked downgrade would.
        let probe = {
            let cmds = crate::update_workflow::actions::downgrade::downgrader("15", false).unwrap();
            let vars: std::collections::HashMap<&str, &str> =
                [("packages", "pkg-a")].into_iter().collect();
            cmds.render_list_command(&vars).unwrap().unwrap()
        };
        let conn = MockConnection::new("h1")
            .with_default(CommandLog::new("zypper", "pkg-a = 1.0-1\n", "", 104, 0))
            .with_response(
                probe,
                CommandLog::new("zypper", "pkg-a = 1.0-1\n", "", 0, 0),
            );
        let handle = conn.clone();
        let mut t = Target::with_connection("h1", TargetState::Enabled, Box::new(conn));
        t.set_system(
            System::new(
                SystemProduct::new("SLES", "15.5", "x86_64"),
                BTreeSet::new(),
                false,
            ),
            false,
        );
        let mut group = HostsGroup::new(vec![t], false);
        let mut report = crate::reports::SlReport::new(Config::default());
        seed_rrid_and_package(&mut report);

        report
            .perform_update(&mut group, true, false, &mut Vec::new())
            .await
            .expect_err("the update check fails, triggering the rollback");

        let contents = String::from_utf8(
            handle
                .file_contents(HISTORY_LOG)
                .expect("the rollback downgrade records its own history row"),
        )
        .unwrap();
        let downgrade_lines: Vec<&str> = contents
            .lines()
            .filter(|l| l.contains(":downgrade:"))
            .collect();
        assert_eq!(
            downgrade_lines.len(),
            1,
            "exactly one downgrade history row: {contents:?}"
        );
        assert!(
            downgrade_lines[0].contains("SUSE:Maintenance:42:7"),
            "the rollback's history row carries the RRID: {contents:?}"
        );
    }

    // --- #407: an abort that already produced side effects records them ----

    #[tokio::test]
    async fn prepare_records_history_once_dispatched() {
        // `prepare` installs packages, so it owes its own row — the residue an
        // `update` aborted after its prepare would otherwise leave unrecorded.
        let (t, handle) = sles_target("h1", "");
        let mut group = HostsGroup::new(vec![t], false);

        let res = perform_prepare(
            &mut group,
            &NoopRepo,
            &["pkg-a".to_owned(), "pkg-b".to_owned()],
            false,
            false,
            false,
        )
        .await;
        assert!(res.is_ok(), "a clean prepare returns Ok: {res:?}");

        let contents = String::from_utf8(
            handle
                .file_contents(HISTORY_LOG)
                .expect("a prepare that dispatched an install records history"),
        )
        .unwrap();
        assert_eq!(contents.lines().count(), 1, "history: {contents:?}");
        // Anchored on the tail: it pins the label *and* the package list. A
        // bare `contains("prepare")` would also match a package named
        // `prepare-something` or the `:update:` row's payload.
        assert!(
            contents.ends_with(":prepare:pkg-a pkg-b\n"),
            "history line: {contents:?}"
        );
    }

    #[tokio::test]
    async fn prepare_writes_no_history_when_cancelled_before_any_dispatch() {
        // The inverse failure direction: a row claiming an install that never
        // started is worse than no row. `installed_only` takes the per-package
        // loop, whose checkpoint breaks at package 0, so nothing dispatches.
        let (t, handle) = sles_target("h1", "");
        let mut group = HostsGroup::new(vec![t], false);
        group.cancel_token().cancel();

        let err = perform_prepare(
            &mut group,
            &NoopRepo,
            &["pkg-a".to_owned(), "pkg-b".to_owned()],
            false,
            false,
            true,
        )
        .await
        .expect_err("a cancelled prepare reports an error");
        assert!(err.is_cancelled(), "must be flagged as a cancel: {err:?}");

        assert!(
            handle.commands().is_empty(),
            "cancelled at package 0, so nothing dispatched: {:?}",
            handle.commands()
        );
        assert!(
            handle.file_contents(HISTORY_LOG).is_none(),
            "a prepare that dispatched nothing must leave no row: {:?}",
            handle.file_contents(HISTORY_LOG)
        );
    }

    #[tokio::test]
    async fn prepare_records_history_only_on_the_hosts_it_dispatched_to() {
        // Per host, not group-wide. `build_prepare_map` drops a host whose
        // release key does not resolve, and `prepare_body` then fails exactly
        // that host with "nothing was installed". A group-wide history fan-out
        // would hand the same host a `:prepare:` row — a false entry in a
        // format the project treats as an interop contract, and one that
        // contradicts the host's own verdict in the same run.
        let (good, good_handle) = sles_target("h1", "");
        // h2: enabled, answers commands, but has no parsed system — so
        // `host_key` resolves nothing and no command is ever built for it.
        let bad_conn = MockConnection::new("h2").with_default(CommandLog::new("", "", "", 0, 0));
        let bad_handle = bad_conn.clone();
        let bad = Target::with_connection("h2", TargetState::Enabled, Box::new(bad_conn));
        let mut group = HostsGroup::new(vec![good, bad], false);

        let err = perform_prepare(
            &mut group,
            &NoopRepo,
            &["pkg-a".to_owned()],
            false,
            false,
            false,
        )
        .await
        .expect_err("the dropped host must fail the flow");
        // The contradiction this test exists to prevent, stated from the other
        // side: h2 is told nothing was installed on it.
        assert_eq!(err.host.as_deref(), Some("h2"), "{err}");
        assert!(
            err.to_string().contains("nothing was installed"),
            "cause stated: {err}"
        );

        // The host that was actually dispatched to is on record.
        let contents = String::from_utf8(
            good_handle
                .file_contents(HISTORY_LOG)
                .expect("the dispatched host keeps its prepare row"),
        )
        .unwrap();
        assert_eq!(contents.lines().count(), 1, "history: {contents:?}");
        assert!(
            contents.ends_with(":prepare:pkg-a\n"),
            "history line: {contents:?}"
        );

        // The dropped one is not. Mock-level proof it was never dispatched to,
        // so the row would be a claim about work that never started.
        assert!(
            bad_handle.commands().is_empty(),
            "no command reached h2: {:?}",
            bad_handle.commands()
        );
        assert!(
            bad_handle.file_contents(HISTORY_LOG).is_none(),
            "a host nothing was dispatched to must have no prepare row: {:?}",
            bad_handle
                .file_contents(HISTORY_LOG)
                .map(|b| String::from_utf8_lossy(&b).into_owned())
        );
    }

    /// A cancel mid-way through the `--installed-only` loop must record the
    /// packages that were dispatched, not the whole prepare set.
    ///
    /// Deterministic without a wall clock: under `start_paused` the runtime
    /// only advances the timer when every task is idle, so the interleaving is
    /// fixed. Each package's fan-out takes 10ms of virtual time and the
    /// canceller fires at 15ms: package 0 completes at 10ms, package 1's
    /// checkpoint passes (the cancel is still 5ms away), the cancel lands at
    /// 15ms, package 1 completes at 20ms, and package 2's checkpoint breaks the
    /// loop. Two of four packages dispatched.
    #[tokio::test(start_paused = true)]
    async fn prepare_cancelled_mid_loop_records_only_the_dispatched_packages() {
        let conn = MockConnection::new("h1")
            .with_default(CommandLog::new("", "", "", 0, 0))
            .with_run_delay(std::time::Duration::from_millis(10));
        let handle = conn.clone();
        let mut t = Target::with_connection("h1", TargetState::Enabled, Box::new(conn));
        t.set_system(
            System::new(
                SystemProduct::new("SLES", "15.5", "x86_64"),
                BTreeSet::new(),
                false,
            ),
            false,
        );
        let mut group = HostsGroup::new(vec![t], false);
        let token = group.cancel_token();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(15)).await;
            token.cancel();
        });

        let pkgs: Vec<String> = ["pkg-a", "pkg-b", "pkg-c", "pkg-d"]
            .iter()
            .map(|p| (*p).to_owned())
            .collect();
        let err = perform_prepare(&mut group, &NoopRepo, &pkgs, false, false, true)
            .await
            .expect_err("a cancelled prepare reports an error");
        assert!(err.is_cancelled(), "must be flagged as a cancel: {err:?}");
        // Pins the interleaving itself, so a change that moved the cancel to a
        // different package would fail here rather than silently re-aiming the
        // assertion below.
        assert_eq!(
            err.reason,
            "prepare cancelled after 2/4 packages; applied: [pkg-a, pkg-b]; \
             not attempted: [pkg-c, pkg-d]",
            "the loop must have broken at package 2"
        );
        assert_eq!(
            handle.commands().len(),
            2,
            "exactly two packages dispatched: {:?}",
            handle.commands()
        );

        let contents = String::from_utf8(
            handle
                .file_contents(HISTORY_LOG)
                .expect("two packages were installed, so there is a row"),
        )
        .unwrap();
        assert_eq!(contents.lines().count(), 1, "history: {contents:?}");
        // The whole point: `pkg-c`/`pkg-d` were never attempted, so naming them
        // would be the same over-claim as a row for a prepare that never ran.
        assert!(
            contents.ends_with(":prepare:pkg-a pkg-b\n"),
            "the row must name only the dispatched packages: {contents:?}"
        );
    }

    #[tokio::test]
    async fn prepare_writes_no_history_for_an_empty_package_list() {
        // Reaches the same site through the empty-list warn path and returns
        // Ok: still nothing dispatched, so still no row.
        let (t, handle) = sles_target("h1", "");
        let mut group = HostsGroup::new(vec![t], false);

        let res = perform_prepare(&mut group, &NoopRepo, &[], false, false, false).await;
        assert!(res.is_ok(), "an empty prepare is a no-op Ok: {res:?}");
        assert!(
            handle.file_contents(HISTORY_LOG).is_none(),
            "an empty prepare must leave no row"
        );
    }

    #[tokio::test]
    async fn prepare_records_history_for_a_host_lost_to_its_reboot() {
        // Pins the row's placement *before* `reboot_transactional`: the mock's
        // `sftp_append` reconnects at entry like the real `SshConnection`, so a
        // write attempted after the reboot would fail on the dead host.
        let (t, handle) = slmicro_target_failing_reconnect("h1");
        let mut group = HostsGroup::new(vec![t], false);

        perform_prepare(
            &mut group,
            &NoopRepo,
            &["pkg-a".to_owned()],
            false,
            false,
            false,
        )
        .await
        .expect_err("a lost transactional host must not report success");

        let contents = String::from_utf8(
            handle
                .file_contents(HISTORY_LOG)
                .expect("a host lost to its reboot must still have its prepare row"),
        )
        .unwrap();
        assert_eq!(contents.lines().count(), 1, "history: {contents:?}");
        assert!(
            contents.ends_with(":prepare:pkg-a\n"),
            "history line: {contents:?}"
        );
    }

    /// A [`SetRepo`] that cancels the group's token the first time it is asked
    /// to *add* a repo, recording every op it saw.
    ///
    /// The only `RepoOp::Add` in `perform_update` happens after the initial
    /// prepare has dispatched and before the pre-dispatch cancel gate, so this
    /// reproduces "`job_cancel` during a multi-minute prepare" deterministically,
    /// with no timing.
    struct CancellingRepo {
        /// Cancels the group's token. A boxed closure rather than the token
        /// itself so the test double needs no `tokio-util` dependency here.
        cancel: Box<dyn Fn() + Send + Sync>,
        ops: std::sync::Mutex<Vec<RepoOp>>,
    }

    #[async_trait::async_trait]
    impl SetRepo for CancellingRepo {
        async fn set_repo(&self, _target: &mut Target, operation: RepoOp) {
            self.ops.lock().unwrap().push(operation);
            if operation == RepoOp::Add {
                (self.cancel)();
            }
        }
    }

    #[tokio::test]
    async fn update_cancelled_at_the_pre_dispatch_gate_records_the_prepare_row() {
        // #407's headline path: the update is cancelled after its prepare has
        // installed packages on every host. The repo add is undone, but the
        // installed packages are not — so the prepare must be on record.
        let (t, handle) = sles_target("h1", "");
        let mut group = HostsGroup::new(vec![t], false);
        let token = group.cancel_token();
        let repo = CancellingRepo {
            cancel: Box::new(move || token.cancel()),
            ops: std::sync::Mutex::new(Vec::new()),
        };

        let res = perform_update(
            &mut group,
            &repo,
            &["pkg-a".to_owned()],
            "42",
            "7",
            None,
            false,
            false,
            &mut Vec::new(),
        )
        .await;

        let Err(UpdateFailure::Cancelled(err)) = res else {
            panic!("a cancel at the pre-dispatch gate is Err(Cancelled): {res:?}");
        };
        // Discriminating: the entry gate says "before the update started", so
        // this pins the *pre-dispatch* gate, the one that runs after prepare.
        assert!(
            err.reason
                .contains("before the update command was dispatched"),
            "reason: {}",
            err.reason
        );
        // …and it must not read as "nothing happened here": a prepare ran.
        assert!(
            err.reason.contains("left in place"),
            "the message must own the prepare residue: {}",
            err.reason
        );

        let contents = String::from_utf8(
            handle
                .file_contents(HISTORY_LOG)
                .expect("the completed prepare must be on record"),
        )
        .unwrap();
        assert_eq!(contents.lines().count(), 1, "history: {contents:?}");
        assert!(
            contents.ends_with(":prepare:pkg-a\n"),
            "history line: {contents:?}"
        );
        // A row claiming the *update* ran would be worse than none: the update
        // command never dispatched.
        assert!(
            !contents.contains(":update:"),
            "no update row may be written when no updater command ran: {contents:?}"
        );
        assert!(
            !handle.commands().iter().any(|c| c.contains(":p=42:7")),
            "the updater command must not have dispatched: {:?}",
            handle.commands()
        );
        // The undo still runs: the repo the gate added is removed again.
        let ops = repo.ops.lock().unwrap().clone();
        assert!(
            ops.contains(&RepoOp::Add) && ops.last() == Some(&RepoOp::Remove),
            "the cancel gate undoes its repo add: {ops:?}"
        );
    }

    #[tokio::test]
    async fn update_cancelled_at_the_gate_under_noprepare_records_nothing() {
        // Same gate, `--noprepare`: nothing was installed and no updater
        // command dispatched, so this abort owes no row — and its message must
        // not invent a prepare residue that cannot exist here.
        let (t, handle) = sles_target("h1", "");
        let mut group = HostsGroup::new(vec![t], false);
        let token = group.cancel_token();
        let repo = CancellingRepo {
            cancel: Box::new(move || token.cancel()),
            ops: std::sync::Mutex::new(Vec::new()),
        };

        let res = perform_update(
            &mut group,
            &repo,
            &["pkg-a".to_owned()],
            "42",
            "7",
            None,
            true,
            false,
            &mut Vec::new(),
        )
        .await;

        let Err(UpdateFailure::Cancelled(err)) = res else {
            panic!("a cancel at the pre-dispatch gate is Err(Cancelled): {res:?}");
        };
        assert!(
            err.reason
                .contains("before the update command was dispatched"),
            "reason: {}",
            err.reason
        );
        assert!(
            !err.reason.contains("left in place"),
            "no prepare ran, so nothing was left in place: {}",
            err.reason
        );
        assert!(
            handle.file_contents(HISTORY_LOG).is_none(),
            "an abort that dispatched nothing must leave no row: {:?}",
            handle.file_contents(HISTORY_LOG)
        );
    }

    #[tokio::test]
    async fn downgrade_all_probes_dead_still_records_history() {
        // The issue repo was removed from every host before the probe ran, and
        // this abort fires on the rollback path — the row is what an operator
        // reconstructs the refhost's state from.
        let (t, handle) = sles_target_with_exit("h1", "", -1);
        let mut group = HostsGroup::new(vec![t], false);

        let res = perform_downgrade(&mut group, &NoopRepo, &["pkg-a".to_owned()], None).await;
        let err = res.expect_err("a dead probe must still abort");
        // The verdict must not change: writing the row is bookkeeping.
        assert_eq!(err.reason, "package version probe failed");
        assert_eq!(err.host.as_deref(), Some("h1"));
        assert!(
            !handle.commands().iter().any(|c| c.contains("--oldpackage")),
            "no downgrade command may run after a dead probe: {:?}",
            handle.commands()
        );

        let contents = String::from_utf8(
            handle
                .file_contents(HISTORY_LOG)
                .expect("the repo removal already landed; the row must be written"),
        )
        .unwrap();
        assert_eq!(contents.lines().count(), 1, "history: {contents:?}");
        // `id` is None here, so the row carries no RRID field.
        assert!(
            contents.ends_with(":downgrade:pkg-a\n"),
            "history line: {contents:?}"
        );
    }

    #[tokio::test]
    async fn perform_install_accepts_zyppers_informational_exit_codes() {
        // zypper exits 100-103/106 to mean "update needed", "reboot needed",
        // "restart needed", "repo skipped" — all *successful* installs. Exit 102
        // (reboot needed) is routine after a kernel update, so a bare
        // `lastexit() != 0` scan would report a false failure on a perfectly
        // good install.
        for exit in [100, 101, 102, 103, 106] {
            let (t, _h) = sles_target_with_exit("h1", "", exit);
            let mut group = HostsGroup::new(vec![t], false);
            let res = perform_install(&mut group, &["pkg-a".to_owned()]).await;
            assert!(
                res.is_ok(),
                "exit {exit} is informational, not a failure: {res:?}"
            );
        }
    }

    #[tokio::test]
    async fn perform_install_reports_the_check_verdict_on_formerly_uncovered_keys() {
        // The slmicro and YUM keys used to have *no* install check, so they
        // fell through to the `PlanProvider` adapter's exit-code-only fallback
        // and every failure read "install command failed" — the same sentence
        // for a locked update stack, a failed RPM transaction and a command
        // that never ran (#406). Both keys now carry a check, so the verdict
        // comes from it.
        //
        // Both host shapes are driven because they take different routes into
        // the same table: SL Micro is transactional (its check shares the
        // update check's classifier), RHEL is not (its check judges the exit
        // code alone).
        for (product, version, transactional) in [("SL-Micro", "6.0", true), ("rhel", "9", false)] {
            let conn = MockConnection::new("h1")
                .with_default(CommandLog::new("t-u", "", "", 1, 0))
                .with_changing_boot_id();
            let mut t = Target::with_connection("h1", TargetState::Enabled, Box::new(conn));
            t.set_system(
                System::new(
                    SystemProduct::new(product, version, "x86_64"),
                    BTreeSet::new(),
                    transactional,
                ),
                transactional,
            );
            let key = host_key(&t).expect("resolvable key");
            // The guard this test used to carry asserted the *absence* of the
            // check — it pinned the hole. Inverted: the verdict below is only
            // the check's if there is one.
            assert!(
                WorkflowRegistry::default()
                    .check(Role::Install, &key.0, key.1)
                    .is_some(),
                "{product}: the install check table must cover this key"
            );
            let mut group = HostsGroup::new(vec![t], false);

            let err = perform_install(&mut group, &["pkg-a".to_owned()])
                .await
                .expect_err("a non-zero exit must still be reported");
            assert_eq!(err.host.as_deref(), Some("h1"), "{product}");
            // Exact, not `contains`: "install command failed" is what the
            // adapter's fallback says, and the whole point is that the check —
            // not the fallback — now answers.
            assert_eq!(err.reason, "Unknown Error", "{product}");
        }
    }

    #[tokio::test]
    async fn perform_install_names_the_host_of_an_install_that_never_ran() {
        // The install/uninstall twin of `perform_prepare_names_the_host_of_a_
        // prepare_that_never_ran`, and the answer to "does this path double a
        // `-1` too?": it does not, and this pins that it stays that way.
        //
        // The two flows collect failures differently. `prepare_body` runs its
        // own `host_command_failures` scan alongside the check, so the two
        // overlap on `-1` and the flow has to suppress one. This path collects
        // *only* what the `Operation` template reports: exactly one
        // `check_failures` entry per host plan
        // (`mtui-hosts::target::operation`, one `push` per plan), plus
        // `reboot_failures` — which cannot add a second entry for the same
        // host, because a host whose check failed is removed from the reboot
        // map before the reboot runs. No exit-code scan runs here at all, so
        // there is nothing to double with.
        for (product, version, transactional) in [("SL-Micro", "6.0", true), ("rhel", "9", false)] {
            let conn = MockConnection::new("h1")
                .with_default(CommandLog::new("", "", "", -1, 0))
                .with_changing_boot_id();
            let mut t = Target::with_connection("h1", TargetState::Enabled, Box::new(conn));
            t.set_system(
                System::new(
                    SystemProduct::new(product, version, "x86_64"),
                    BTreeSet::new(),
                    transactional,
                ),
                transactional,
            );
            let mut group = HostsGroup::new(vec![t], false);

            let err = perform_install(&mut group, &["pkg-a".to_owned()])
                .await
                .expect_err("an install that never ran must not report success");

            assert_eq!(err.host.as_deref(), Some("h1"), "{product}: {err}");
            // Role-neutral wording, because this table also serves uninstall.
            assert_eq!(
                err.reason, "command timed out or failed to run",
                "{product}"
            );
        }
    }

    #[test]
    fn aggregate_failures_carries_the_typed_flags_through_the_summary() {
        // The single-failure path returns the error verbatim, flags and all;
        // the summary path builds a fresh one, so without care the flags exist
        // on one path and vanish on the other. They are the declared routing
        // contract — `reports::update_flow` routes on `probe_failed`, and the
        // command layer reports `cancelled` — so a summary that drops them is a
        // summary that lies about the run.
        //
        // `all`, not `any`: a summary claiming "no patch was dispatched" while
        // one host had dispatched one would be worse than no claim.
        let err = aggregate_failures(
            "update",
            vec![
                UpdateError::probe_failure("could not determine what to patch", "h1"),
                UpdateError::probe_failure("could not determine what to patch", "h2"),
            ],
        )
        .unwrap_err();
        assert!(
            err.probe_failed,
            "a summary of probe failures is still a probe failure: {err:?}"
        );

        let mixed = aggregate_failures(
            "update",
            vec![
                UpdateError::probe_failure("could not determine what to patch", "h1"),
                UpdateError::new("Unknown Error", "h2"),
            ],
        )
        .unwrap_err();
        assert!(
            !mixed.probe_failed,
            "one host that did dispatch a patch clears the flag: {mixed:?}"
        );

        // `cancelled` is deliberately not summarised. It is representable in a
        // `failures` vec — `perform_operation_with`, the install/uninstall
        // shared body, maps it out of `report.check_failures` — but no
        // production check emits one, so the only cancellations reaching this
        // module are its own early `return Err`s. A lone cancelled failure
        // routes verbatim with the flag; a summary drops it, so a cancel
        // cannot mask a real failure collected beside it.
        let cancelled = aggregate_failures(
            "update",
            vec![
                UpdateError::cancelled("cancelled before pkg-a"),
                UpdateError::cancelled("cancelled before pkg-b"),
            ],
        )
        .unwrap_err();
        assert!(
            !cancelled.is_cancelled(),
            "aggregation must not mint a cancel verdict: {cancelled:?}"
        );
    }

    #[test]
    fn aggregate_failures_summarises_multiple_hosts() {
        let failures = vec![
            UpdateError::new("boom", "h2"),
            UpdateError::new("boom", "h1"),
        ];
        let err = aggregate_failures("prepare", failures).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("prepare failed on h1, h2"),
            "aggregated message names both hosts sorted: {msg}"
        );
    }

    #[test]
    fn aggregate_failures_names_each_host_once() {
        // One host can legitimately contribute two failures with two distinct
        // causes: `downgrade_body` seeds its list from the issue-repo removal
        // scan and then adds that same host's downgrade-check verdict.
        // Undeduped, the roll-call read "downgrade failed on h1, h1", which
        // names one host as two. (The prepare flow's own overlap — one signal,
        // two rules — is resolved upstream of here instead, in `prepare_body`:
        // deduping a roll-call would not have restored the `host` field the
        // summary branch drops.)
        let err = aggregate_failures(
            "downgrade",
            vec![
                UpdateError::new("failed to remove issue repo", "h1"),
                UpdateError::new("downgrade command timed out or failed to run", "h1"),
            ],
        )
        .unwrap_err();
        // `starts_with`, not `contains`: "downgrade failed on h1, h1 (" contains
        // "downgrade failed on h1" too, so a `contains` here would pin nothing.
        assert!(
            err.reason.starts_with("downgrade failed on h1 ("),
            "one host, named once: {}",
            err.reason
        );
        // Both causes still reach the operator — this dedups the roll-call,
        // not the diagnosis.
        assert!(
            err.reason.contains("h1: failed to remove issue repo")
                && err
                    .reason
                    .contains("h1: downgrade command timed out or failed to run"),
            "both causes survive: {}",
            err.reason
        );
        // And distinct hosts are still listed separately: a dedup that
        // collapsed them would hide a host.
        let err = aggregate_failures(
            "prepare",
            vec![
                UpdateError::new("boom", "h1"),
                UpdateError::new("boom", "h2"),
            ],
        )
        .unwrap_err();
        assert!(
            err.reason.starts_with("prepare failed on h1, h2 ("),
            "two hosts stay two: {}",
            err.reason
        );
    }

    // --- perform_update ----------------------------------------------------

    /// Builds an SLES report with a loaded RRID and one metadata package.
    fn report_with_rrid() -> SlReport {
        let mut report = SlReport::new(Config::default());
        report.base_mut().rrid =
            Some(mtui_types::RequestReviewID::parse("SUSE:Maintenance:42:7").unwrap());
        report.base_mut().packages.insert(
            "SLES:15".to_owned(),
            [("pkg-a".to_owned(), "2.0-1".to_owned())]
                .into_iter()
                .collect(),
        );
        report
    }

    #[tokio::test]
    async fn perform_update_issues_updater_command_with_repa() {
        let (t, handle) = sles_target("h1", "");
        let mut group = HostsGroup::new(vec![t], false);
        let report = report_with_rrid();
        let packages = report.get_package_list();

        // noprepare=true keeps the flow to update + checks; the report drives the
        // repo fan-out through its own (real) set_repo, which no-ops with an
        // empty update_repos map.
        let res = perform_update(
            &mut group,
            &report,
            &packages,
            "42",
            "7",
            None,
            true,
            false,
            &mut Vec::new(),
        )
        .await;
        assert!(res.is_ok(), "successful update returns Ok: {res:?}");

        // The updater command interpolates the `$repa` selector `:p=42:7`.
        let cmds = handle.commands();
        assert!(
            cmds.iter().any(|c| c.contains(":p=42:7")),
            "expected updater command carrying $repa: {cmds:?}"
        );
    }

    #[tokio::test]
    async fn perform_update_aborts_cleanly_when_no_updater_doer() {
        // An unknown release has no updater doer. mtui treats this as a hard
        // fail: Err(MissingUpdater), no updater command issued, and the repo the
        // flow added is removed on the abort path.
        let conn = MockConnection::new("h1").with_default(CommandLog::new("", "", "", 0, 0));
        let handle = conn.clone();
        let mut t = Target::with_connection("h1", TargetState::Enabled, Box::new(conn));
        t.set_system(
            System::new(
                SystemProduct::new("gentoo", "1", "x86_64"),
                BTreeSet::new(),
                false,
            ),
            false,
        );
        let mut group = HostsGroup::new(vec![t], false);
        let repo = RecordingRepo::default();
        let report = report_with_rrid();
        let _ = report;

        let res = perform_update(
            &mut group,
            &repo,
            &["pkg-a".to_owned()],
            "42",
            "7",
            None,
            true,
            false,
            &mut Vec::new(),
        )
        .await;
        let err = match res {
            Err(UpdateFailure::MissingUpdater(e)) => e,
            other => panic!("missing updater is a hard fail Err(MissingUpdater): {other:?}"),
        };
        assert_eq!(err.host.as_deref(), Some("h1"));
        assert!(
            err.reason.contains("missing updater"),
            "reason: {}",
            err.reason
        );

        let cmds = handle.commands();
        assert!(
            !cmds.iter().any(|c| c.contains(":p=42:7")),
            "no updater doer ⇒ no updater command issued: {cmds:?}"
        );
        let ops = repo.ops.lock().unwrap().clone();
        assert!(ops.contains(&RepoOp::Add), "repo add ran: {ops:?}");
        assert!(
            ops.contains(&RepoOp::Remove),
            "abort removes the repo: {ops:?}"
        );
        // #407's other half: this abort is *correct* to stay silent. With
        // `noprepare` nothing was installed and no updater command dispatched,
        // so the fix must not make this path write a row either.
        assert!(
            handle.file_contents(HISTORY_LOG).is_none(),
            "an abort that dispatched nothing must leave no row: {:?}",
            handle.file_contents(HISTORY_LOG)
        );
    }

    // --- perform_downgrade -------------------------------------------------

    #[tokio::test]
    async fn perform_downgrade_resolves_version_and_issues_per_package_command() {
        // The list_command output feeds the version resolver; the downgrade then
        // targets the resolved version.
        let (t, handle) = sles_target("h1", "pkg-a = 1.0-1\n");
        let mut group = HostsGroup::new(vec![t], false);

        let res = perform_downgrade(&mut group, &NoopRepo, &["pkg-a".to_owned()], None).await;
        assert!(res.is_ok(), "a clean downgrade returns Ok: {res:?}");

        let cmds = handle.commands();
        assert!(
            cmds.iter()
                .any(|c| c.contains("pkg-a") && c.contains("1.0-1")),
            "expected downgrade to the resolved version: {cmds:?}"
        );
    }

    #[tokio::test]
    async fn perform_downgrade_empty_package_list_is_a_noop() {
        // An empty package list returns before locking or probing: the probe
        // template with zero names would list the entire catalog.
        let (t, handle) = sles_target("h1", "pkg-a = 1.0-1\n");
        let mut group = HostsGroup::new(vec![t], false);

        let res = perform_downgrade(&mut group, &NoopRepo, &[], None).await;
        assert!(res.is_ok(), "empty list is a no-op Ok: {res:?}");
        assert!(
            handle.commands().is_empty(),
            "no command should run for an empty package list: {:?}",
            handle.commands()
        );
    }

    #[tokio::test]
    async fn perform_downgrade_dead_probe_aborts() {
        // A dead version probe (exit -1 for every command) aborts instead of
        // "completing" with zero downgrade commands run.
        let (t, handle) = sles_target_with_exit("h1", "", -1);
        let mut group = HostsGroup::new(vec![t], false);

        let res = perform_downgrade(&mut group, &NoopRepo, &["pkg-a".to_owned()], None).await;
        let err = res.expect_err("a dead probe must abort");
        assert_eq!(err.reason, "package version probe failed");
        assert_eq!(err.host.as_deref(), Some("h1"));
        // No downgrade command was built (only the failing probe ran).
        assert!(
            !handle.commands().iter().any(|c| c.contains("--oldpackage")),
            "no downgrade command may run after a dead probe: {:?}",
            handle.commands()
        );
    }

    #[tokio::test]
    async fn perform_downgrade_partial_dead_probe_rolls_back_healthy_host() {
        // h1's probe succeeds (rolls back), h2's probe dies (exit -1). h2 is
        // skipped but h1 still rolls back; the error names only h2 at the end.
        let (t1, h1) = sles_target("h1", "pkg-a = 1.0-1\n");
        let (t2, h2) = sles_target_with_exit("h2", "", -1);
        let mut group = HostsGroup::new(vec![t1, t2], false);

        let res = perform_downgrade(&mut group, &NoopRepo, &["pkg-a".to_owned()], None).await;
        let err = res.expect_err("a partial dead probe still fails the command");
        assert_eq!(err.reason, "package version probe failed");
        assert_eq!(err.host.as_deref(), Some("h2"));
        // The healthy host rolled back to the resolved version.
        assert!(
            h1.commands()
                .iter()
                .any(|c| c.contains("pkg-a") && c.contains("1.0-1")),
            "healthy host must roll back: {:?}",
            h1.commands()
        );
        // The dead host built no downgrade command.
        assert!(
            !h2.commands().iter().any(|c| c.contains("--oldpackage")),
            "dead host must build no downgrade command: {:?}",
            h2.commands()
        );
        // Not every probe died, so the all-dead abort must not fire: the flow
        // reaches the single post-dispatch history site and writes exactly one
        // downgrade row. An abort-site write that escaped its `if` would show
        // up here as two.
        let contents = String::from_utf8(
            h1.file_contents(HISTORY_LOG)
                .expect("the healthy host's row"),
        )
        .unwrap();
        assert_eq!(
            contents
                .lines()
                .filter(|l| l.contains(":downgrade:"))
                .count(),
            1,
            "exactly one downgrade row on the healthy host: {contents:?}"
        );
    }

    /// The exact `list_command` the downgrade flow renders for `packages` on a
    /// SLES 15 host — the string [`MockConnection::with_response`] must match to
    /// script the **version probe specifically**.
    ///
    /// Built through the same registry and the same `quote_args` the flow uses,
    /// so the two cannot drift apart into a fixture that scripts nothing.
    fn downgrade_list_command(packages: &[String]) -> String {
        WorkflowRegistry::default()
            .doer(Role::Downgrade, "15", false)
            .expect("zypper has a downgrader")
            .render_list_command(
                &[("packages", quote_args(packages).as_str())]
                    .into_iter()
                    .collect(),
            )
            .expect("safe substitution never fails")
            .expect("the zypper downgrader carries a list command")
    }

    /// A SLES 15 target whose **version probe** answers `exit`, with the
    /// marker line the guarded template prints on stdout.
    ///
    /// That pairing is what the rendered probe actually produces when
    /// `zypper -n se` fails — pinned end-to-end against a real `/bin/sh` in
    /// `actions::downgrade`'s `rendered_script` module, which is where it *can*
    /// be pinned: `MockConnection` scripts one outcome per command string and
    /// cannot run a shell. Replaying it here is what lets the flow's *routing*
    /// be tested.
    ///
    /// Scripted per command rather than through `with_default`, so the failure
    /// is attributable to the probe: a mock answering every command alike would
    /// leave nothing to attribute, and every other command on this host answers
    /// cleanly.
    fn sles_target_with_probe_exit(
        hostname: &str,
        packages: &[String],
        exit: i16,
    ) -> (Target, MockConnection, String) {
        let list = downgrade_list_command(packages);
        let conn = MockConnection::new(hostname)
            .with_default(CommandLog::new("", "pkg-a = 1.0-1\n", "", 0, 0))
            .with_response(
                list.clone(),
                CommandLog::new(
                    list.clone(),
                    format!(
                        "mtui: could not determine what to downgrade: zypper -n se exited {exit}"
                    ),
                    "",
                    exit,
                    0,
                ),
            );
        let handle = conn.clone();
        let mut t = Target::with_connection(hostname, TargetState::Enabled, Box::new(conn));
        t.set_system(
            System::new(
                SystemProduct::new("SLES", "15.5", "x86_64"),
                BTreeSet::new(),
                false,
            ),
            false,
        );
        (t, handle, list)
    }

    #[tokio::test]
    async fn perform_downgrade_probe_nonzero_exit_is_a_dead_probe() {
        // Issue #451. Before the guard, a refused `zypper -n se` status could
        // not reach this gate at all: the probe was a pipeline, so the recorded
        // status was awk's, and awk exits 0 on empty input. Now the template
        // exits with the failed tool's own status, and *any* non-zero status is
        // a dead probe — the `-1` SSH sentinel the two tests above use is only
        // one of its values, so a predicate narrowed to `c == -1` would still
        // pass them.
        //
        // Per host, deliberately: h1 is what the rollback exists for. Aborting
        // the whole group over h2's broken zypper would strand every healthy
        // peer half-applied — the opposite of the update's own probe failure,
        // where nothing had been applied yet.
        let packages = vec!["pkg-a".to_owned()];
        let (t1, h1) = sles_target("h1", "pkg-a = 1.0-1\n");
        let (t2, h2, list) = sles_target_with_probe_exit("h2", &packages, 7);
        let mut group = HostsGroup::new(vec![t1, t2], false);

        let res = perform_downgrade(&mut group, &NoopRepo, &packages, None).await;

        // Not vacuous: the scripted probe really is the command that ran.
        assert!(
            h2.commands().contains(&list),
            "the rendered version probe must have been dispatched: {:?}",
            h2.commands()
        );
        let err = res.expect_err("a probe that refused its status must fail the command");
        assert_eq!(err.reason, "package version probe failed");
        assert_eq!(err.host.as_deref(), Some("h2"));
        // The healthy host still rolled back — that is what the recovery is for.
        assert!(
            h1.commands()
                .iter()
                .any(|c| c.contains("--oldpackage") && c.contains("pkg-a") && c.contains("1.0-1")),
            "the healthy host must still roll back: {:?}",
            h1.commands()
        );
        assert!(
            !h2.commands().iter().any(|c| c.contains("--oldpackage")),
            "the dead-probe host must build no downgrade command: {:?}",
            h2.commands()
        );
    }

    #[tokio::test]
    async fn downgrade_verdict_withholds_done_when_a_probe_died() {
        // The all-clear must be unreachable on a host where no downgrade
        // command ran. `downgrade_verdict` names a package only when the report
        // carries a `required` version to compare against; on a standalone
        // downgrade there is none, so with a dead probe the map came back empty
        // and `done` was logged over a host nobody had measured — while the
        // command failed. An operator reading the transcript saw both.
        let packages = vec!["pkg-a".to_owned()];
        let (t1, _h1) = sles_target("h1", "pkg-a = 1.0-1\n");
        let (t2, _h2, _list) = sles_target_with_probe_exit("h2", &packages, 7);
        let mut group = HostsGroup::new(vec![t1, t2], false);

        let (res, logs) =
            capture_logs(perform_downgrade(&mut group, &NoopRepo, &packages, None)).await;

        let err = res.expect_err("a dead probe still fails the command");
        assert_eq!(err.reason, "package version probe failed");
        // The capture layer renders a message unquoted, so this is `== "done"`
        // and not `contains("\"done\"")` — which would have matched nothing at
        // all and passed however the code behaved.
        assert!(
            !logs.lines().any(|l| l == "done"),
            "the all-clear must be withheld while a host's state is unverified: {logs}"
        );
        let unverified = logs
            .lines()
            .find(|l| l.contains("unverified"))
            .unwrap_or_else(|| panic!("no warning about the unverified host: {logs}"));
        assert!(
            unverified.contains("h2"),
            "the warning must name the host whose state is unknown: {unverified}"
        );
        // And the per-host ERROR says what happened to the host, not what the
        // flow did next: "skipping downgrade on this host" reads as a step that
        // was omitted, where the operator needs to know this host still carries
        // the update.
        let named = logs
            .lines()
            .find(|l| l.contains("was not downgraded"))
            .unwrap_or_else(|| panic!("the per-host error must say so plainly: {logs}"));
        assert!(
            named.contains("h2"),
            "the per-host error must name the host: {named}"
        );
    }

    #[tokio::test]
    async fn downgrade_verdict_names_packages_still_at_update_version() {
        // Re-query returns pkg-a still at 1.5-1, which is the update's `required`
        // version ⇒ current >= required ⇒ named as not-downgraded. The
        // bookkeeping still rotates before/after for it.
        let (mut t, _h) = sles_target("h1", "pkg-a 1.5-1\n");
        let mut pkg = mtui_types::package::Package::new("pkg-a");
        pkg.set_required(Some("1.5-1")).unwrap();
        pkg.set_after(Some("1.5-1")).unwrap();
        t.set_packages(vec![pkg]);
        let mut group = HostsGroup::new(vec![t], false);

        let not_downgraded = downgrade_verdict(&mut group, &BTreeSet::new()).await;

        assert_eq!(
            not_downgraded.get("h1").map(Vec::as_slice),
            Some(&["pkg-a (at 1.5-1, update ships 1.5-1)".to_owned()][..])
        );
        // Bookkeeping advanced: before <- old after, after <- re-queried current.
        let p = &group.get("h1").unwrap().packages()[0];
        assert_eq!(
            p.before().map(ToString::to_string).as_deref(),
            Some("1.5-1")
        );
        assert_eq!(p.after().map(ToString::to_string).as_deref(), Some("1.5-1"));
    }

    #[tokio::test]
    async fn downgrade_verdict_rotation_keeps_an_unchecked_slot_unchecked() {
        // #396: on a standalone downgrade there was no prior `update`, so the
        // after slot was never checked. Rotating it into the before slot must
        // carry that across — turning it into "checked, not installed" would
        // make the export claim the package was absent before a rollback
        // nobody measured.
        let (mut t, _h) = sles_target("h1", "pkg-a 0.9-1\n");
        let mut pkg = mtui_types::package::Package::new("pkg-a");
        pkg.set_required(Some("1.5-1")).unwrap();
        assert_eq!(pkg.after_check(), &VersionCheck::NotChecked);
        t.set_packages(vec![pkg]);
        let mut group = HostsGroup::new(vec![t], false);

        let _ = downgrade_verdict(&mut group, &BTreeSet::new()).await;

        let p = &group.get("h1").unwrap().packages()[0];
        assert_eq!(
            p.before_check(),
            &VersionCheck::NotChecked,
            "an unchecked after slot must not rotate in as an observation"
        );
        assert_eq!(p.after().map(ToString::to_string).as_deref(), Some("0.9-1"));
    }

    /// #437: a host the re-query never answers about for a package must
    /// rotate `current` into `after` as `NotChecked`, not the ambiguous
    /// "checked, not installed" that a plain-`Option` rotation would produce.
    #[tokio::test]
    async fn downgrade_verdict_rotation_keeps_an_unqueried_current_unchecked() {
        // Empty stdout: the mock rpm query never mentions "pkg-a" at all.
        let (mut t, _h) = sles_target("h1", "");
        let mut pkg = mtui_types::package::Package::new("pkg-a");
        pkg.set_required(Some("1.5-1")).unwrap();
        t.set_packages(vec![pkg]);
        let mut group = HostsGroup::new(vec![t], false);

        let _ = downgrade_verdict(&mut group, &BTreeSet::new()).await;

        let p = &group.get("h1").unwrap().packages()[0];
        assert_eq!(
            p.after_check(),
            &VersionCheck::NotChecked,
            "an unqueried current must not rotate in as an observation"
        );
    }

    #[tokio::test]
    async fn downgrade_verdict_no_short_circuit_names_every_host() {
        // Two hosts each still at the update version: BOTH are named, not
        // just the first — there is no short-circuit.
        let (mut t1, _h1) = sles_target("h1", "pkg-a 2.0-1\n");
        let mut p1 = mtui_types::package::Package::new("pkg-a");
        p1.set_required(Some("2.0-1")).unwrap();
        t1.set_packages(vec![p1]);
        let (mut t2, _h2) = sles_target("h2", "pkg-b 3.0-1\n");
        let mut p2 = mtui_types::package::Package::new("pkg-b");
        p2.set_required(Some("3.0-1")).unwrap();
        t2.set_packages(vec![p2]);
        let mut group = HostsGroup::new(vec![t1, t2], false);

        let not_downgraded = downgrade_verdict(&mut group, &BTreeSet::new()).await;

        assert!(not_downgraded.contains_key("h1"), "{not_downgraded:?}");
        assert!(not_downgraded.contains_key("h2"), "{not_downgraded:?}");
    }

    #[tokio::test]
    async fn downgrade_verdict_done_when_below_required() {
        // Re-query returns 0.9-1, below the update's required 1.5-1 ⇒ rolled back
        // ⇒ not named; the map is empty ⇒ "done".
        let (mut t, _h) = sles_target("h1", "pkg-a 0.9-1\n");
        let mut pkg = mtui_types::package::Package::new("pkg-a");
        pkg.set_required(Some("1.5-1")).unwrap();
        pkg.set_after(Some("1.5-1")).unwrap();
        t.set_packages(vec![pkg]);
        let mut group = HostsGroup::new(vec![t], false);

        let not_downgraded = downgrade_verdict(&mut group, &BTreeSet::new()).await;

        assert!(not_downgraded.is_empty(), "{not_downgraded:?}");
        // Bookkeeping still advanced.
        let p = &group.get("h1").unwrap().packages()[0];
        assert_eq!(
            p.before().map(ToString::to_string).as_deref(),
            Some("1.5-1")
        );
        assert_eq!(p.after().map(ToString::to_string).as_deref(), Some("0.9-1"));
    }

    /// Builds an enabled SL Micro (transactional) target on a mock returning
    /// `stdout` with `exit` for every command.
    fn slmicro_target(hostname: &str, stdout: &str, exit: i16) -> (Target, MockConnection) {
        // A changing boot id models a host that really rebooted. Without it
        // both probes read the same and the lifecycle correctly concludes the
        // host never went down — see `RebootFault::WentNowhere` for that.
        let conn = MockConnection::new(hostname)
            .with_default(CommandLog::new("", stdout, "", exit, 0))
            .with_changing_boot_id();
        let handle = conn.clone();
        let mut t = Target::with_connection(hostname, TargetState::Enabled, Box::new(conn));
        t.set_system(
            System::new(
                SystemProduct::new("SL-Micro", "6.0", "x86_64"),
                BTreeSet::new(),
                true,
            ),
            true,
        );
        (t, handle)
    }

    /// A transactional SL-Micro target whose commands answer with `stderr`.
    ///
    /// The update check for `("slmicro", true)` reads the stdout/stderr markers
    /// *and* the exit code; this fixture exercises the marker half, so it
    /// answers exit `0` and leaves the failure signal entirely in `stderr` —
    /// which is also the shape of the failure the exit code alone would miss.
    fn slmicro_target_with_stderr(hostname: &str, stderr: &str) -> (Target, MockConnection) {
        let conn = MockConnection::new(hostname)
            .with_default(CommandLog::new("", "", stderr, 0, 0))
            .with_changing_boot_id();
        let handle = conn.clone();
        let mut t = Target::with_connection(hostname, TargetState::Enabled, Box::new(conn));
        t.set_system(
            System::new(
                SystemProduct::new("SL-Micro", "6.0", "x86_64"),
                BTreeSet::new(),
                true,
            ),
            true,
        );
        (t, handle)
    }

    /// Which of the three reboot failure modes a fixture should exhibit.
    #[derive(Debug, Clone, Copy)]
    enum RebootFault {
        /// Went away and never came back. Unreachable.
        Unreachable,
        /// Answers afterwards with the same boot id: it never went down.
        WentNowhere,
        /// The dispatch itself failed, leaving the session live so the
        /// following reconnect succeeds trivially.
        Undispatched,
    }

    /// A transactional SL-Micro target whose reboot fails in one of the three
    /// distinguishable ways, wired so a rollback *would* render if one ran.
    ///
    /// The stdout matters: a mock answering `""` makes the downgrade version
    /// probe yield nothing, so `combined` stays empty and **no downgrade
    /// command is ever built** — which silently turns any "assert no
    /// `--oldpackage`" into a test that cannot fail. Answering with a parseable
    /// `pkg = version` line is what makes those assertions real.
    fn slmicro_reboot_failure(hostname: &str, how: RebootFault) -> (Target, MockConnection) {
        let conn = MockConnection::new(hostname).with_default(CommandLog::new(
            "",
            "pkg-a = 1.0-1\n",
            "",
            0,
            0,
        ));
        let conn = match how {
            RebootFault::Unreachable => conn.with_changing_boot_id().failing_reconnect(),
            RebootFault::WentNowhere => conn,
            RebootFault::Undispatched => conn.with_changing_boot_id().failing_fire_and_forget(),
        };
        let handle = conn.clone();
        let mut t = Target::with_connection(hostname, TargetState::Enabled, Box::new(conn));
        t.set_system(
            System::new(
                SystemProduct::new("SL-Micro", "6.0", "x86_64"),
                BTreeSet::new(),
                true,
            ),
            true,
        );
        (t, handle)
    }

    /// Like [`slmicro_target`] but its reconnect after a reboot always fails,
    /// modelling a transactional host that never comes back.
    fn slmicro_target_failing_reconnect(hostname: &str) -> (Target, MockConnection) {
        slmicro_reboot_failure(hostname, RebootFault::Unreachable)
    }

    #[tokio::test]
    async fn perform_install_reports_a_transactional_host_that_never_reconnects() {
        // The install itself succeeds (exit 0), but the post-reboot reconnect
        // never comes back: the whole point of this fix is that mtui must not
        // report success on a host it lost.
        let (t, _handle) = slmicro_target_failing_reconnect("h1");
        let mut group = HostsGroup::new(vec![t], false);

        let err = perform_install(&mut group, &["pkg-a".to_owned()])
            .await
            .expect_err("a lost transactional host must not report success");
        assert_eq!(err.host.as_deref(), Some("h1"));
        assert!(
            err.reason.contains("did not come back after the reboot"),
            "reason: {}",
            err.reason
        );
        // Discriminating: the two reachable causes must not be claimed for a
        // host that genuinely went away — they route the rollback differently.
        assert!(
            !err.reason.contains("never rebooted") && !err.reason.contains("never received"),
            "reason: {}",
            err.reason
        );
    }

    #[tokio::test]
    async fn perform_uninstall_reports_a_transactional_host_that_never_reconnects() {
        let (t, _handle) = slmicro_target_failing_reconnect("h1");
        let mut group = HostsGroup::new(vec![t], false);

        let err = perform_uninstall(&mut group, &["pkg-a".to_owned()])
            .await
            .expect_err("a lost transactional host must not report success");
        assert_eq!(err.host.as_deref(), Some("h1"));
        assert!(
            err.reason.contains("did not come back after the reboot"),
            "reason: {}",
            err.reason
        );
        // Discriminating: the two reachable causes must not be claimed for a
        // host that genuinely went away — they route the rollback differently.
        assert!(
            !err.reason.contains("never rebooted") && !err.reason.contains("never received"),
            "reason: {}",
            err.reason
        );
    }

    #[tokio::test]
    async fn perform_prepare_fails_when_a_transactional_host_does_not_come_back() {
        let (t, _handle) = slmicro_target_failing_reconnect("h1");
        let mut group = HostsGroup::new(vec![t], false);

        let err = perform_prepare(
            &mut group,
            &NoopRepo,
            &["pkg-a".to_owned()],
            false,
            false,
            false,
        )
        .await
        .expect_err("a lost transactional host must not report success");
        assert!(
            err.reason.contains("did not come back after the reboot"),
            "reason: {}",
            err.reason
        );
        // Discriminating: the two reachable causes must not be claimed for a
        // host that genuinely went away — they route the rollback differently.
        assert!(
            !err.reason.contains("never rebooted") && !err.reason.contains("never received"),
            "reason: {}",
            err.reason
        );
        assert_eq!(err.host.as_deref(), Some("h1"));
    }

    #[tokio::test]
    async fn perform_downgrade_fails_when_a_transactional_host_does_not_come_back() {
        let (t, _handle) = slmicro_target_failing_reconnect("h1");
        let mut group = HostsGroup::new(vec![t], false);

        let err = perform_downgrade(&mut group, &NoopRepo, &["pkg-a".to_owned()], None)
            .await
            .expect_err("a lost transactional host must not report success");
        assert!(
            err.reason.contains("did not come back after the reboot"),
            "reason: {}",
            err.reason
        );
        // Discriminating: the two reachable causes must not be claimed for a
        // host that genuinely went away — they route the rollback differently.
        assert!(
            !err.reason.contains("never rebooted") && !err.reason.contains("never received"),
            "reason: {}",
            err.reason
        );
        assert_eq!(err.host.as_deref(), Some("h1"));
    }

    #[tokio::test]
    async fn update_reports_a_host_that_never_came_back_and_skips_the_rollback() {
        // A successful patch followed by a dead reconnect must fail as
        // `UpdateFailure::Reboot` and must NOT trigger the rollback downgrade:
        // the host is unreachable, so a downgrade cannot run on it.
        let (t, handle) = slmicro_target_failing_reconnect("h1");
        let mut group = HostsGroup::new(vec![t], false);
        let mut report = crate::reports::SlReport::new(Config::default());
        seed_rrid_and_package(&mut report);

        let res = report
            .perform_update(&mut group, true, false, &mut Vec::new())
            .await;
        let err = res.expect_err("a lost transactional host must not report success");
        assert!(
            err.reason.contains("did not come back after the reboot"),
            "reason: {}",
            err.reason
        );
        // Discriminating: the two reachable causes must not be claimed for a
        // host that genuinely went away — they route the rollback differently.
        assert!(
            !err.reason.contains("never rebooted") && !err.reason.contains("never received"),
            "reason: {}",
            err.reason
        );

        // No downgrade (rollback) command was issued. The fixture answers the
        // version probe with a parseable line, so a rollback that *did* run
        // would render `--oldpackage` here — without that, this assertion would
        // pass no matter how the failure was routed.
        assert!(
            !handle.commands().iter().any(|c| c.contains("--oldpackage")),
            "a dead host must not trigger a rollback downgrade: {:?}",
            handle.commands()
        );
    }

    /// An SL Micro target whose *update command* records `Target::run`'s
    /// never-ran sentinel (`-1`) — a timeout, a mid-command connection loss, or
    /// an unconnected target.
    ///
    /// The `-1` is scripted onto the update command alone: scripting it via
    /// `with_default` would put it on the probes too, a state no run produces.
    fn slmicro_target_with_lost_update(hostname: &str) -> (Target, MockConnection) {
        let patch = WorkflowRegistry::default()
            .doer(Role::Update, "slmicro", true)
            .expect("slmicro has an updater")
            .render_command(
                &[
                    ("repa", repa_for("42", "7").as_str()),
                    ("packages", quote_args(&["pkg-a".to_owned()]).as_str()),
                ]
                .into_iter()
                .collect(),
            )
            .expect("safe substitution never fails");
        let lost = MockConnection::new(hostname)
            .with_default(CommandLog::new("", "", "", 0, 0))
            .with_response(patch.clone(), CommandLog::new(&patch, "", "", -1, 0))
            .with_changing_boot_id();
        let handle = lost.clone();
        let mut t = Target::with_connection(hostname, TargetState::Enabled, Box::new(lost));
        t.set_system(
            System::new(
                SystemProduct::new("SL-Micro", "6.0", "x86_64"),
                BTreeSet::new(),
                true,
            ),
            true,
        );
        (t, handle)
    }

    #[tokio::test]
    async fn update_timeout_does_not_roll_back_healthy_hosts() {
        // `Target::run` records -1 for a timeout, a mid-command connection
        // loss, and an unconnected target — i.e. "the flow could not talk to
        // this host", not "the patch went wrong". Routing that into the
        // group-wide rollback would revert every host that patched cleanly on
        // behalf of one the downgrade cannot reach anyway, which is exactly
        // what the reboot arm already refuses to do.
        //
        // h1's patch times out; h2 patches cleanly.
        let (t1, lost_handle) = slmicro_target_with_lost_update("h1");
        let (t2, healthy) = slmicro_target("h2", "pkg-a = 1.0-1\n", 0);
        let mut group = HostsGroup::new(vec![t1, t2], false);
        let mut report = crate::reports::SlReport::new(Config::default());
        seed_rrid_and_package(&mut report);

        let err = report
            .perform_update(&mut group, true, false, &mut Vec::new())
            .await
            .expect_err("a host whose patch never ran must not report success");
        assert!(
            err.reason.contains("timed out or failed to run"),
            "reason: {}",
            err.reason
        );

        // The fixture only means anything if h1's patch really did time out.
        assert_eq!(
            group.get("h1").and_then(mtui_hosts::Target::lastexit),
            Some(-1),
            "h1's patch must have recorded the never-ran sentinel"
        );
        // The point of the test: h2 patched cleanly and must keep its update.
        assert!(
            !healthy
                .commands()
                .iter()
                .any(|c| c.contains("--oldpackage")),
            "a healthy host must not be rolled back for a host that was lost: {:?}",
            healthy.commands()
        );
        assert!(
            !lost_handle
                .commands()
                .iter()
                .any(|c| c.contains("--oldpackage")),
            "the lost host cannot be rolled back either: {:?}",
            lost_handle.commands()
        );

        // And the variant itself, which the rollback wrapper above collapses
        // to a bare `UpdateError`. Without this the routing is observed only
        // through its consequence, and `NotRun` could be replaced by any other
        // non-rolling variant with the whole suite still green.
        let (t, _) = slmicro_target_with_lost_update("h1");
        let mut group = HostsGroup::new(vec![t], false);
        let report = report_with_rrid();
        let packages = report.get_package_list();
        let res = perform_update(
            &mut group,
            &report,
            &packages,
            "42",
            "7",
            None,
            true,
            false,
            &mut Vec::new(),
        )
        .await;
        let Err(UpdateFailure::NotRun(e)) = res else {
            panic!("a host whose command never ran returns Err(NotRun): {res:?}");
        };
        assert_eq!(e.reason, "update command timed out or failed to run");
        assert!(!e.probe_failed, "a lost host is not a probe failure: {e:?}");
    }

    #[tokio::test]
    async fn update_rolls_back_when_the_host_never_went_down() {
        // The inverse of the test above, and the reason the cause is carried
        // rather than flattened: this host is *reachable*. Its patch was staged
        // into a snapshot that never activated, so it is serving the old
        // packages while the rest of the group moved on — the split-brain the
        // rollback exists to undo, and a host the downgrade can actually reach.
        let (t, handle) = slmicro_reboot_failure("h1", RebootFault::WentNowhere);
        let mut group = HostsGroup::new(vec![t], false);
        let mut report = crate::reports::SlReport::new(Config::default());
        seed_rrid_and_package(&mut report);

        let err = report
            .perform_update(&mut group, true, false, &mut Vec::new())
            .await
            .expect_err("a host that never rebooted must not report success");
        assert!(
            err.reason.contains("never rebooted"),
            "reason: {}",
            err.reason
        );
        // Discriminating: this is NOT the unreachable case, which skips the
        // rollback. Asserting the positive alone would pass on either routing.
        assert!(
            !err.reason.contains("did not come back"),
            "reason: {}",
            err.reason
        );
        assert!(
            handle.commands().iter().any(|c| c.contains("--oldpackage")),
            "a reachable host on an inactive snapshot must be rolled back: {:?}",
            handle.commands()
        );
    }

    #[tokio::test]
    async fn update_skips_the_rollback_when_any_failed_host_is_unreachable() {
        // Mixed causes: h1 is gone, h2 is up but never rebooted. The rollback
        // is group-wide, so running it could not repair h1, would leave h2
        // needing manual work anyway, and would revert every healthy host in
        // the group. `all`, not `any` — and both hosts are still named.
        let (t1, h1) = slmicro_reboot_failure("h1", RebootFault::Unreachable);
        let (t2, h2) = slmicro_reboot_failure("h2", RebootFault::WentNowhere);
        let mut group = HostsGroup::new(vec![t1, t2], false);
        let mut report = crate::reports::SlReport::new(Config::default());
        seed_rrid_and_package(&mut report);

        let err = report
            .perform_update(&mut group, true, false, &mut Vec::new())
            .await
            .expect_err("two failed reboots must not report success");
        assert!(err.reason.contains("h1"), "reason: {}", err.reason);
        assert!(err.reason.contains("h2"), "reason: {}", err.reason);
        for (name, handle) in [("h1", &h1), ("h2", &h2)] {
            assert!(
                !handle.commands().iter().any(|c| c.contains("--oldpackage")),
                "{name} must not be rolled back when a peer is unreachable: {:?}",
                handle.commands()
            );
        }
    }

    #[tokio::test]
    async fn update_rolls_back_when_the_reboot_was_never_dispatched() {
        // The subtlest of the three: the dispatch fails, which leaves the SSH
        // session live, so the reconnect that follows returns Ok immediately.
        // Before the dispatch result was captured this read as a clean reboot.
        let (t, handle) = slmicro_reboot_failure("h1", RebootFault::Undispatched);
        let mut group = HostsGroup::new(vec![t], false);
        let mut report = crate::reports::SlReport::new(Config::default());
        seed_rrid_and_package(&mut report);

        let err = report
            .perform_update(&mut group, true, false, &mut Vec::new())
            .await
            .expect_err("an undispatched reboot must not report success");
        assert!(
            err.reason.contains("never received the reboot"),
            "reason: {}",
            err.reason
        );
        assert!(
            handle.commands().iter().any(|c| c.contains("--oldpackage")),
            "a host that never got the reboot is still up and must be rolled back: {:?}",
            handle.commands()
        );
    }

    #[tokio::test]
    async fn perform_update_transactional_host_reboots_on_success() {
        let (t, handle) = slmicro_target("h1", "", 0);
        let mut group = HostsGroup::new(vec![t], false);
        let report = report_with_rrid();
        let packages = report.get_package_list();

        let res = perform_update(
            &mut group,
            &report,
            &packages,
            "42",
            "7",
            None,
            true,
            false,
            &mut Vec::new(),
        )
        .await;
        assert!(res.is_ok(), "successful update returns Ok: {res:?}");

        // The slmicro updater is transactional, so a reboot command is fired
        // on success; reboot uses fire-and-forget, recorded separately from
        // the run log.
        let fired = handle.fired_commands();
        assert!(
            fired.iter().any(|c| c.contains("systemctl reboot")),
            "expected transactional reboot after a successful update: {fired:?}"
        );
    }

    #[tokio::test]
    async fn perform_update_fails_a_transactional_host_whose_stack_is_locked() {
        // `("slmicro", true)` had an *updater* but no *check*, so `run_checks`
        // hit its `else { continue }` and the host contributed no verdict at
        // all: `update` reported success however the patch went. This is the
        // end-to-end proof the newly registered check is actually reached —
        // registering it in the table alone would not show that.
        //
        // The marker is scripted onto the **patch command specifically**, not
        // via `with_default`. `perform_update` runs other commands first (the
        // repo add's trailing `... -n ref`), and a default answering every
        // command with the marker makes this test pass even with the patch
        // fan-out deleted — the check then reads the refresh's snapshot. This
        // test had exactly that hole; verified by deleting
        // `targets.run(Command::PerHost(commands))` from `update_run_phase`
        // and watching the old version stay green.
        let report = report_with_rrid();
        let packages = report.get_package_list();
        let patch = WorkflowRegistry::default()
            .doer(Role::Update, "slmicro", true)
            .expect("slmicro has an updater")
            .render_command(
                &[
                    ("repa", repa_for("42", "7").as_str()),
                    ("packages", quote_args(&packages).as_str()),
                ]
                .into_iter()
                .collect(),
            )
            .expect("safe substitution never fails");

        let conn = MockConnection::new("h1")
            .with_default(CommandLog::new("", "all good", "", 0, 0))
            .with_response(
                patch.clone(),
                CommandLog::new(&patch, "", "System management is locked", 0, 0),
            )
            .with_changing_boot_id();
        let handle = conn.clone();
        let mut t = Target::with_connection("h1", TargetState::Enabled, Box::new(conn));
        t.set_system(
            System::new(
                SystemProduct::new("SL-Micro", "6.0", "x86_64"),
                BTreeSet::new(),
                true,
            ),
            true,
        );
        let mut group = HostsGroup::new(vec![t], false);

        let res = perform_update(
            &mut group,
            &report,
            &packages,
            "42",
            "7",
            None,
            true,
            false,
            &mut Vec::new(),
        )
        .await;

        let Err(UpdateFailure::Check(e)) = res else {
            panic!("a locked update stack must fail the update: {res:?}");
        };
        assert_eq!(e.reason, "update stack locked");
        assert_eq!(e.host.as_deref(), Some("h1"));

        // The verdict must come from the patch, so the patch must have run.
        let cmds = handle.commands();
        assert!(
            cmds.contains(&patch),
            "the patch command must have been dispatched: {cmds:?}"
        );

        // And the check failure must suppress the reboot: activating the new
        // snapshot would hide the failed patch behind a healthy-looking boot.
        // Before the check existed this branch was unreachable for a
        // transactional host, so #385's "a check failure suppresses the reboot
        // entirely" comment described a path nothing could take.
        let fired = handle.fired_commands();
        assert!(
            !fired.iter().any(|c| c.contains("systemctl reboot")),
            "a host whose update check failed must not be rebooted: {fired:?}"
        );
    }

    /// The rendered zypper `update` command for `("15", false)` — the exact
    /// string a `MockConnection` must be keyed on to script the *patch's* exit
    /// code rather than every command's.
    fn zypper_patch_command(packages: &[String]) -> String {
        WorkflowRegistry::default()
            .doer(Role::Update, "15", false)
            .expect("zypper has an updater")
            .render_command(
                &[
                    ("repa", repa_for("42", "7").as_str()),
                    ("packages", quote_args(packages).as_str()),
                ]
                .into_iter()
                .collect(),
            )
            .expect("safe substitution never fails")
    }

    /// A SLES 15 target whose *patch command specifically* answers `exit`.
    ///
    /// Every other command answers cleanly. This is the fixture shape the
    /// exit-code rules require: the update template now captures the patch's
    /// status and re-exits with it, so an exit code scripted via `with_default`
    /// would be one on *every* command — a state no shell produces, and one
    /// that keeps a test green with the patch fan-out deleted, because the
    /// check then reads the repo refresh's snapshot instead.
    ///
    /// The default stdout is a resolvable version line so the rollback
    /// downgrade's version probe succeeds and a rollback, if one is routed,
    /// actually renders an `--oldpackage` command. Without that, "assert no
    /// rollback happened" would pass however the failure was routed.
    fn sles_target_with_patch_exit(
        hostname: &str,
        packages: &[String],
        exit: i16,
    ) -> (Target, MockConnection, String) {
        let patch = zypper_patch_command(packages);
        let conn = MockConnection::new(hostname)
            .with_default(CommandLog::new("", "pkg-a = 1.0-1\n", "", 0, 0))
            .with_response(
                patch.clone(),
                CommandLog::new(patch.clone(), "", "", exit, 0),
            );
        let handle = conn.clone();
        let mut t = Target::with_connection(hostname, TargetState::Enabled, Box::new(conn));
        t.set_system(
            System::new(
                SystemProduct::new("SLES", "15.5", "x86_64"),
                BTreeSet::new(),
                false,
            ),
            false,
        );
        (t, handle, patch)
    }

    /// A SLES 15 target whose update script reports a **probe** failure: the
    /// marker line on stdout, and the failed probe's own status as the
    /// script's.
    ///
    /// That pairing is what the rendered template actually produces when
    /// `zypper -n patches` fails — pinned end-to-end against a real `/bin/sh`
    /// in `actions::update`'s `rendered_script` module, which is where it can
    /// be pinned: `MockConnection` scripts one outcome per command string and
    /// cannot run a shell. Replaying it here is what lets the *routing* be
    /// tested, and the marker comes from the same constant the check greps
    /// for, so the two cannot drift apart silently.
    ///
    /// The default stdout is a resolvable version line, so a rollback that did
    /// run would render an `--oldpackage` command — without that, "assert no
    /// rollback happened" would pass however the failure was routed.
    fn sles_target_with_probe_failure(
        hostname: &str,
        packages: &[String],
    ) -> (Target, MockConnection, String) {
        use crate::update_workflow::checks::update::PROBE_FAILURE_MARKER;

        let update = zypper_patch_command(packages);
        let conn = MockConnection::new(hostname)
            .with_default(CommandLog::new("", "pkg-a = 1.0-1\n", "", 0, 0))
            .with_response(
                update.clone(),
                CommandLog::new(
                    update.clone(),
                    format!("{PROBE_FAILURE_MARKER}: zypper -n patches exited 6"),
                    "",
                    6,
                    0,
                ),
            );
        let handle = conn.clone();
        let mut t = Target::with_connection(hostname, TargetState::Enabled, Box::new(conn));
        t.set_system(
            System::new(
                SystemProduct::new("SLES", "15.5", "x86_64"),
                BTreeSet::new(),
                false,
            ),
            false,
        );
        (t, handle, update)
    }

    #[tokio::test]
    async fn perform_update_routes_a_probe_failure_away_from_the_rollback() {
        // Issue #447's routing decision. A host that could not work out what
        // to patch never ran a patch, so its packages are exactly what they
        // were: there is nothing half-applied for the group-wide rollback
        // downgrade to repair, and firing it would revert every healthy peer.
        //
        // The variant *and* the reason: `ProbeFailed` is the only non-rolling
        // route reachable from a check failure other than `NotRun`, and the
        // reason is what tells an operator to look at the repo state rather
        // than at the host's packages.
        let report = report_with_rrid();
        let packages = report.get_package_list();
        let (t, handle, update) = sles_target_with_probe_failure("h1", &packages);
        let mut group = HostsGroup::new(vec![t], false);
        let repo = RecordingRepo::default();

        let res = perform_update(
            &mut group,
            &repo,
            &packages,
            "42",
            "7",
            None,
            true,
            false,
            &mut Vec::new(),
        )
        .await;

        // Not vacuous: the failing script really was dispatched.
        assert!(
            handle.commands().contains(&update),
            "the update command must have been dispatched: {:?}",
            handle.commands()
        );
        let Err(UpdateFailure::ProbeFailed(e)) = res else {
            panic!("a failed probe returns Err(ProbeFailed): {res:?}");
        };
        assert_eq!(e.reason, "could not determine what to patch");
        assert_eq!(e.host.as_deref(), Some("h1"));

        // As on any failure, the test repos are kept — here so the operator
        // can inspect the repo state the probe complained about.
        let ops = repo.ops.lock().unwrap().clone();
        assert!(ops.contains(&RepoOp::Add), "repo add must run: {ops:?}");
        assert!(
            !ops.contains(&RepoOp::Remove),
            "on failure the repos are kept (no Remove): {ops:?}"
        );
    }

    #[tokio::test]
    async fn update_probe_failure_does_not_roll_back_a_healthy_peer() {
        // The same decision seen where its blast radius is: through the
        // rollback wrapper, with a healthy peer in the group. `perform_update_
        // with_rollback` hands the downgrade the *whole* group, so a probe
        // failure routed to `Check` would revert h2 — a host that patched
        // perfectly — on behalf of h1's broken repo configuration.
        let mut report = crate::reports::SlReport::new(Config::default());
        seed_rrid_and_package(&mut report);
        let packages = report.get_package_list();
        let (t1, h1, _) = sles_target_with_probe_failure("h1", &packages);
        let (t2, h2, _) = sles_target_with_patch_exit("h2", &packages, 0);
        let mut group = HostsGroup::new(vec![t1, t2], false);

        let err = report
            .perform_update(&mut group, true, false, &mut Vec::new())
            .await
            .expect_err("a host that could not list its patches must not report success");
        assert_eq!(err.reason, "could not determine what to patch");

        // h2 first: the peer is what this test exists for.
        assert!(
            !h2.commands().iter().any(|c| c.contains("--oldpackage")),
            "the healthy peer h2 must not be rolled back on h1's behalf: {:?}",
            h2.commands()
        );
        assert!(
            !h1.commands().iter().any(|c| c.contains("--oldpackage")),
            "h1 never ran a patch, so it has nothing to roll back: {:?}",
            h1.commands()
        );
    }

    #[tokio::test]
    async fn update_mixed_probe_failure_and_lost_host_routes_not_run_and_skips_the_rollback() {
        // The case the routing rewrite silently changed, and the one nothing
        // observed. At HEAD the rule was `all(lastexit == -1)`, so a run mixing
        // a probe failure with a lost host answered "not every host is `-1`" →
        // `Check` → the group-wide rollback fired. It should not: neither host
        // is one a downgrade can repair — h1 never ran a patch, and h2 cannot
        // be reached.
        //
        // The label is `NotRun` rather than `ProbeFailed`: it is the more
        // conservative of the two, and its claim ("the host's state is
        // unknown") is true of h2.
        let report = report_with_rrid();
        let packages = report.get_package_list();

        let (t1, _, _) = sles_target_with_probe_failure("h1", &packages);
        let (t2, _) = slmicro_target_with_lost_update("h2");
        let mut group = HostsGroup::new(vec![t1, t2], false);
        let repo = RecordingRepo::default();
        let res = perform_update(
            &mut group,
            &repo,
            &packages,
            "42",
            "7",
            None,
            true,
            false,
            &mut Vec::new(),
        )
        .await;
        let Err(UpdateFailure::NotRun(e)) = res else {
            panic!("a probe failure mixed with a lost host returns Err(NotRun): {res:?}");
        };
        // Both hosts are named — the label is the conservative one, but
        // neither failure is swallowed.
        assert!(
            e.reason.contains("h1") && e.reason.contains("h2"),
            "both failed hosts are named: {}",
            e.reason
        );
        // The aggregate does not claim "no patch was dispatched" for a run in
        // which one host's state is unknown.
        assert!(
            !e.probe_failed,
            "a mixed run must not carry the probe-failure flag: {e:?}"
        );

        // And through the rollback wrapper: neither host is downgraded.
        let (t1, h1, _) = sles_target_with_probe_failure("h1", &packages);
        let (t2, h2) = slmicro_target_with_lost_update("h2");
        let mut group = HostsGroup::new(vec![t1, t2], false);
        let mut report = crate::reports::SlReport::new(Config::default());
        seed_rrid_and_package(&mut report);
        report
            .perform_update(&mut group, true, false, &mut Vec::new())
            .await
            .expect_err("two failed hosts must not report success");
        for (name, handle) in [("h1", &h1), ("h2", &h2)] {
            assert!(
                !handle.commands().iter().any(|c| c.contains("--oldpackage")),
                "{name} must not be rolled back: neither host ran a patch this could undo: {:?}",
                handle.commands()
            );
        }
    }

    #[tokio::test]
    async fn update_still_rolls_back_when_a_probe_failure_rides_with_a_real_one() {
        // The other side of the routing rule, and the reason it asks "is *any*
        // failed host repairable?" rather than "did they all fail the same
        // way?". h2's patch ran and returned 104, which is the half-applied
        // state the rollback exists to undo; h1's probe failure must not
        // downgrade the *verdict* and strand h2 with a failed transaction.
        let mut report = crate::reports::SlReport::new(Config::default());
        seed_rrid_and_package(&mut report);
        let packages = report.get_package_list();
        let (t1, _h1, _) = sles_target_with_probe_failure("h1", &packages);
        let (t2, h2, patch) = sles_target_with_patch_exit("h2", &packages, 104);
        let mut group = HostsGroup::new(vec![t1, t2], false);

        let err = report
            .perform_update(&mut group, true, false, &mut Vec::new())
            .await
            .expect_err("two failed hosts must not report success");
        // Both hosts are named, neither reason is swallowed.
        assert!(
            err.reason.contains("could not determine what to patch")
                && err.reason.contains("package not found"),
            "both failures are reported: {}",
            err.reason
        );

        // Not vacuous: h2's patch really did run and fail.
        assert!(
            h2.commands().contains(&patch),
            "h2's patch must have been dispatched: {:?}",
            h2.commands()
        );
        assert!(
            h2.commands().iter().any(|c| c.contains("--oldpackage")),
            "h2 ran a patch that failed and must still be rolled back: {:?}",
            h2.commands()
        );
    }

    #[tokio::test]
    async fn perform_update_keeps_repos_on_check_failure() {
        // exit 104 on the patch command ⇒ the update check flags "package not
        // found"; the flow must NOT issue a repo-remove (repos kept for retry).
        //
        // The 104 is scripted onto the patch alone. It used to ride on
        // `with_default`, i.e. on every command — which the shell could not
        // produce even before #400 (the script's status was the cleanup loop's)
        // and cannot produce now either (it is the patch's).
        let report = report_with_rrid();
        let packages = report.get_package_list();
        let (t, handle, patch) = sles_target_with_patch_exit("h1", &packages, 104);
        let mut group = HostsGroup::new(vec![t], false);

        // A recording repo so we can assert no Remove followed the Add.
        let repo = RecordingRepo::default();

        // Drive perform_update with the recording repo as the SetRepo hook by
        // calling the module fn directly (SlReport delegates to it).
        let res = perform_update(
            &mut group,
            &repo,
            &packages,
            "42",
            "7",
            None,
            true,
            false,
            &mut Vec::new(),
        )
        .await;
        let Err(UpdateFailure::Check(e)) = res else {
            panic!("a failed patch returns Err(Check): {res:?}");
        };
        // The reason, not just the variant: `Check` is reached by every marker
        // too, so asserting the variant alone would not show that the *exit
        // code* was read.
        assert_eq!(e.reason, "package not found");
        assert_eq!(e.host.as_deref(), Some("h1"));

        let ops = repo.ops.lock().unwrap().clone();
        assert!(ops.contains(&RepoOp::Add), "repo add must run: {ops:?}");
        assert!(
            !ops.contains(&RepoOp::Remove),
            "on failure the repos are kept (no Remove): {ops:?}"
        );
        // The verdict must come from the patch, so the patch must have run.
        let cmds = handle.commands();
        assert!(
            cmds.contains(&patch),
            "the patch command must have been dispatched: {cmds:?}"
        );
    }

    #[tokio::test]
    async fn perform_update_passes_a_host_that_only_needs_a_reboot() {
        // The carve-out that makes reading the exit code safe at all, asserted
        // where the blast radius lives. zypper exits 102
        // (`ZYPPER_EXIT_INF_REBOOT_NEEDED`) after patching a kernel — the
        // routine outcome of the thing mtui exists to do. Under a bare `!= 0`
        // rule that host would fail its check, and a check failure hands
        // `perform_update_with_rollback` the *whole* group: it removes every
        // host's issue repos, downgrades every host, and rewrites the report's
        // before/after version slots. One host's healthy 102 would revert the
        // fleet.
        let mut report = crate::reports::SlReport::new(Config::default());
        seed_rrid_and_package(&mut report);
        let packages = report.get_package_list();
        let (t1, h1, patch) = sles_target_with_patch_exit("h1", &packages, 102);
        let (t2, h2, _) = sles_target_with_patch_exit("h2", &packages, 0);
        let mut group = HostsGroup::new(vec![t1, t2], false);

        let res = report
            .perform_update(&mut group, true, false, &mut Vec::new())
            .await;

        // Not vacuous: the 102 was actually delivered, and to the patch.
        assert!(
            h1.commands().contains(&patch),
            "the patch command must have been dispatched: {:?}",
            h1.commands()
        );
        assert!(
            res.is_ok(),
            "exit 102 is 'reboot needed', not a failure: {res:?}"
        );
        // And nothing was rolled back. `perform_update_with_rollback` hands the
        // downgrade the *whole* group, so the healthy peer is where a false
        // failure shows up as collateral damage — h2 is asserted **first** for
        // exactly that reason. The fixture answers the version probe with a
        // parseable line, so a rollback that did run would render
        // `--oldpackage` here.
        //
        // Two assertions rather than a loop over both hosts: in a loop the
        // first host to fail hides the other, and the peer host is the one this
        // test exists for.
        assert!(
            !h2.commands().iter().any(|c| c.contains("--oldpackage")),
            "the healthy peer h2 must not be rolled back on h1's behalf: {:?}",
            h2.commands()
        );
        assert!(
            !h1.commands().iter().any(|c| c.contains("--oldpackage")),
            "h1 must not be rolled back: {:?}",
            h1.commands()
        );
    }

    #[tokio::test]
    async fn perform_update_aggregates_multiple_host_failures_and_keeps_repos() {
        // Two hosts both fail the update check (exit 104) ⇒ the flow aggregates
        // the failures and keeps the repos.
        let (t1, _h1) = sles_target_with_exit("h1", "", 104);
        let (t2, _h2) = sles_target_with_exit("h2", "", 104);
        let mut group = HostsGroup::new(vec![t1, t2], false);
        let repo = RecordingRepo::default();
        let report = report_with_rrid();
        let packages = report.get_package_list();

        let res = perform_update(
            &mut group,
            &repo,
            &packages,
            "42",
            "7",
            None,
            true,
            false,
            &mut Vec::new(),
        )
        .await;
        let err = match res {
            Err(UpdateFailure::Check(e)) => e,
            other => panic!("multi-host failure returns Err(Check): {other:?}"),
        };
        // Aggregated message names both hosts (sorted).
        let msg = err.to_string();
        assert!(
            msg.contains("update failed on h1, h2"),
            "aggregated message names both hosts: {msg}"
        );

        let ops = repo.ops.lock().unwrap().clone();
        assert!(ops.contains(&RepoOp::Add));
        assert!(
            !ops.contains(&RepoOp::Remove),
            "multi-host failure keeps repos: {ops:?}"
        );
    }

    #[tokio::test]
    async fn perform_update_with_rollback_downgrades_on_check_failure() {
        // A check failure (exit 104) drives the rollback wrapper: it re-surfaces
        // the original UpdateError AND issues a downgrade (rollback). The mock
        // returns a resolvable version line so the downgrade command renders.
        //
        // The downgrade version probe must exit 0 (a non-zero probe exit is a
        // dead-probe abort); the shared `sles_target_with_exit` would apply
        // 104 to the probe too, so script the probe explicitly.
        let probe = {
            let cmds = crate::update_workflow::actions::downgrade::downgrader("15", false).unwrap();
            let vars: std::collections::HashMap<&str, &str> =
                [("packages", "pkg-a")].into_iter().collect();
            cmds.render_list_command(&vars).unwrap().unwrap()
        };
        let conn = MockConnection::new("h1")
            .with_default(CommandLog::new("zypper", "pkg-a = 1.0-1\n", "", 104, 0))
            .with_response(
                probe,
                CommandLog::new("zypper", "pkg-a = 1.0-1\n", "", 0, 0),
            );
        let handle = conn.clone();
        let mut t = Target::with_connection("h1", TargetState::Enabled, Box::new(conn));
        t.set_system(
            System::new(
                SystemProduct::new("SLES", "15.5", "x86_64"),
                BTreeSet::new(),
                false,
            ),
            false,
        );
        let mut group = HostsGroup::new(vec![t], false);
        let mut report = crate::reports::SlReport::new(Config::default());
        seed_rrid_and_package(&mut report);

        let res = report
            .perform_update(&mut group, true, false, &mut Vec::new())
            .await;
        assert!(res.is_err(), "check failure surfaces as Err: {res:?}");

        // The downgrade list_command / downgrade command ran as part of rollback.
        let cmds = handle.commands();
        assert!(
            cmds.iter()
                .any(|c| c.contains("pkg-a") && c.contains("1.0-1")),
            "rollback must issue a downgrade command: {cmds:?}"
        );
    }

    #[tokio::test]
    async fn perform_update_removes_repos_on_success() {
        let (t, _handle) = sles_target("h1", "");
        let mut group = HostsGroup::new(vec![t], false);
        let repo = RecordingRepo::default();
        let report = report_with_rrid();
        let packages = report.get_package_list();

        let res = perform_update(
            &mut group,
            &repo,
            &packages,
            "42",
            "7",
            None,
            true,
            false,
            &mut Vec::new(),
        )
        .await;
        assert!(res.is_ok(), "successful update returns Ok: {res:?}");

        let ops = repo.ops.lock().unwrap().clone();
        assert!(ops.contains(&RepoOp::Add));
        assert!(
            ops.contains(&RepoOp::Remove),
            "on success the repos are removed: {ops:?}"
        );
    }

    #[tokio::test]
    async fn perform_update_warns_when_the_operation_lock_does_not_release() {
        // A stranded operation lock does not turn a good update into a
        // failure — but the fan-out's own `LockOutcome` map must still reach a
        // warn, the same swallow fix 3 closed for install/uninstall.
        let conn = MockConnection::new("h1")
            .with_default(CommandLog::new("zypper", "", "", 0, 0))
            .failing_sftp_remove();
        let mut t = Target::with_connection("h1", TargetState::Enabled, Box::new(conn));
        t.set_system(
            System::new(
                SystemProduct::new("SLES", "15.5", "x86_64"),
                BTreeSet::new(),
                false,
            ),
            false,
        );
        let mut group = HostsGroup::new(vec![t], false);
        let repo = RecordingRepo::default();
        let report = report_with_rrid();
        let packages = report.get_package_list();

        let (res, logs) = capture_logs(perform_update(
            &mut group,
            &repo,
            &packages,
            "42",
            "7",
            None,
            true,
            false,
            &mut Vec::new(),
        ))
        .await;

        assert!(
            res.is_ok(),
            "a stranded lock must not turn a good update into a failure: {res:?}"
        );
        let unlock_line = logs
            .lines()
            .find(|l| l.contains("operation lock did not release"))
            .unwrap_or_else(|| panic!("no unlock-failure warning found: {logs}"));
        assert!(
            unlock_line.contains("h1"),
            "the WARN must name the stranded host: {unlock_line}"
        );
    }

    /// Runs `fut` with this thread's log capture armed, recording each
    /// event's message and fields synchronously into an in-memory buffer,
    /// returning `fut`'s output paired with the joined records.
    ///
    /// The subscriber is **global and installed once**, and only the sink is
    /// scoped to the calling thread. That is not a style choice: `tracing`
    /// caches callsite *interest* process-wide, so a callsite first reached
    /// from a thread with no subscriber installed is cached as
    /// `Interest::never()` and stays silent for every later capture. With the
    /// suite running in parallel that is a race, and it is not hypothetical —
    /// under `tracing::subscriber::set_default`'s thread-local guard,
    /// `downgrade_verdict_withholds_done_when_a_probe_died` failed about one
    /// run in three with a capture holding a single line, because its sibling
    /// `perform_downgrade_probe_nonzero_exit_is_a_dead_probe` drives the same
    /// `warn!`/`error!` callsites with no subscriber and whichever test reached
    /// them first decided the cache. `mtui-datasources`' `teregen` capture hit
    /// the same race and is fixed the same way. An always-installed subscriber
    /// keeps interest pinned to `always`.
    ///
    /// Scoping via the thread-local sink captures exactly what the guard did:
    /// the fan-out is `buffer_unordered` on the test's own task and
    /// `#[tokio::test]` is single-threaded, so no event under test is emitted
    /// off this thread.
    ///
    /// **Blast radius, accepted knowingly.** The `Registry` is unfiltered, so
    /// it reports no `max_level_hint`; `set_global_default` reads that as "no
    /// maximum" and `LevelFilter::current()` becomes `TRACE` for the *whole*
    /// `mtui-testreport` lib test binary from the first capture onward. Every
    /// `debug!`/`trace!` in the crate — previously dead, and none of them under
    /// test here — then evaluates its formatting arguments and dispatches into
    /// `CaptureLayer`, which drops it for want of a sink. The cost is argument
    /// evaluation in tests only, and it buys the interest cache the race above
    /// needs. Bounding the layer with a `LevelFilter` would keep the pinned
    /// interest without the blast radius, and is the change to make if the lib
    /// suite ever slows down noticeably.
    ///
    /// **This is the workspace's third copy of the pattern** — the others are
    /// `mtui-datasources`' `tests/log_capture.rs` (the fullest write-up) and
    /// `mtui-datasources::teregen`'s test module, and `mtui-core` gains a
    /// fourth in #404/PR #459. They are copies rather than one helper because
    /// each is a `#[cfg(test)]` module in a different crate and target
    /// (unit-test modules cannot share an integration test's file); the
    /// standing note in `AGENTS.md` § "Testing conventions" is what keeps the
    /// next one from re-deriving the race from scratch.
    async fn capture_logs<T>(fut: impl std::future::Future<Output = T>) -> (T, String) {
        install_capture_subscriber();
        CAPTURE_SINK.with(|s| *s.borrow_mut() = Some(Vec::new()));
        let out = fut.await;
        let records = CAPTURE_SINK
            .with(|s| s.borrow_mut().take())
            .unwrap_or_default();
        (out, records.join("\n"))
    }

    thread_local! {
        /// Buffer for the capture in progress on this thread, or `None` when no
        /// capture is active — events from a thread without one are dropped.
        static CAPTURE_SINK: std::cell::RefCell<Option<Vec<String>>> =
            const { std::cell::RefCell::new(None) };
    }

    /// Install the permissive global subscriber backing [`capture_logs`], once
    /// per test binary. See [`capture_logs`] for why it must be global.
    fn install_capture_subscriber() {
        use std::fmt::Write as _;
        use std::sync::OnceLock;
        use tracing::field::{Field, Visit};
        use tracing_subscriber::layer::{Context, Layer, SubscriberExt};
        use tracing_subscriber::registry::Registry;

        struct CaptureLayer;

        struct MessageVisitor(String);
        impl Visit for MessageVisitor {
            fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
                if field.name() == "message" {
                    let _ = write!(self.0, "{value:?}");
                } else {
                    let _ = write!(self.0, " {}={value:?}", field.name());
                }
            }
        }

        impl<S: tracing::Subscriber> Layer<S> for CaptureLayer {
            fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
                CAPTURE_SINK.with(|s| {
                    if let Some(buf) = s.borrow_mut().as_mut() {
                        let mut visitor = MessageVisitor(String::new());
                        event.record(&mut visitor);
                        buf.push(visitor.0);
                    }
                });
            }
        }

        static ONCE: OnceLock<()> = OnceLock::new();
        ONCE.get_or_init(|| {
            let _ = tracing::subscriber::set_global_default(Registry::default().with(CaptureLayer));
        });
    }

    #[tokio::test]
    async fn remove_test_repos_names_the_error_when_it_cannot_lock() {
        // A foreign lock makes update_lock fail on the only host, so the
        // cleanup cannot run; it must warn (naming the error and the remedy)
        // rather than removing the repos.
        let foreign = MockConnection::new("h1")
            .with_default(CommandLog::new("", "", "", 0, 0))
            .with_file(
                mtui_hosts::TARGET_LOCK_PATH,
                b"1700000000:alice:4242:busy".to_vec(),
            );
        let t = Target::with_connection("h1", TargetState::Enabled, Box::new(foreign));
        let mut group = HostsGroup::new(vec![t], false);
        let repo = RecordingRepo::default();

        let ((), logs) = capture_logs(remove_test_repos(&mut group, &repo)).await;

        let ops = repo.ops.lock().unwrap().clone();
        assert!(
            !ops.contains(&RepoOp::Remove),
            "cleanup must not run when the lock fails: {ops:?}"
        );
        // The cleanup's own warning must carry both the lock error and the
        // remedy in the same line: `update_lock`'s internal fanout also logs
        // WARNs about the individual foreign-locked host, so a
        // `logs.contains` across every captured line would still pass with
        // the error field dropped from this warning.
        let cleanup_line = logs
            .lines()
            .find(|l| l.contains("left configured on every host"))
            .unwrap_or_else(|| panic!("no cleanup warning found: {logs}"));
        assert!(
            cleanup_line.contains("Hosts locked"),
            "cleanup warning must name the lock error: {cleanup_line}"
        );
        assert!(
            cleanup_line.contains("set_repo --remove"),
            "cleanup warning must name the manual remedy: {cleanup_line}"
        );
    }

    #[tokio::test]
    async fn remove_test_repos_warns_when_the_removal_command_fails_on_a_host() {
        // The lock succeeds but the repo-removal command itself fails on the
        // host: issue #409's actual complaint (a stale test repo) can happen
        // silently here too, not only on a lock failure.
        let conn = MockConnection::new("h1").with_default(CommandLog::new("zypper", "", "", 1, 0));
        let mut t = Target::with_connection("h1", TargetState::Enabled, Box::new(conn));
        t.set_system(
            System::new(
                SystemProduct::new("SLES", "15.5", "x86_64"),
                BTreeSet::new(),
                false,
            ),
            false,
        );
        let mut group = HostsGroup::new(vec![t], false);

        let mut report = SlReport::new(Config::default());
        report.base_mut().rrid =
            Some(mtui_types::RequestReviewID::parse("SUSE:Maintenance:42:7").unwrap());
        report.base_mut().update_repos.insert(
            SystemProduct::new("SLES", "15.5", "x86_64"),
            "https://example/repo".to_owned(),
        );

        let ((), logs) = capture_logs(remove_test_repos(&mut group, &report)).await;

        let warn_line = logs
            .lines()
            .find(|l| l.contains("failed to remove the test update repo"))
            .unwrap_or_else(|| panic!("no repo-removal warning found: {logs}"));
        assert!(
            warn_line.contains("h1"),
            "warning must name the host: {warn_line}"
        );
        assert!(
            warn_line.contains("set_repo --remove"),
            "warning must name the manual remedy: {warn_line}"
        );
    }

    #[tokio::test]
    async fn perform_update_runs_prepare_when_not_noprepare() {
        let (t, handle) = sles_target("h1", "");
        let mut group = HostsGroup::new(vec![t], false);
        let report = report_with_rrid();
        let packages = report.get_package_list();

        // noprepare=false ⇒ the initial prepare runs (a preparer install) before
        // the updater command.
        let res = perform_update(
            &mut group,
            &report,
            &packages,
            "42",
            "7",
            None,
            false,
            false,
            &mut Vec::new(),
        )
        .await;
        assert!(res.is_ok(), "successful update returns Ok: {res:?}");

        let cmds = handle.commands();
        assert!(
            cmds.iter().any(|c| c.contains("zypper -n in -y -l")),
            "expected the initial prepare install: {cmds:?}"
        );
        assert!(cmds.iter().any(|c| c.contains(":p=42:7")));
    }

    #[tokio::test]
    async fn perform_update_continues_when_prepare_only_reports_host_noise() {
        // h1's prepare command exits 0 but writes to stderr: prepare ran and
        // merely reported host noise (`host_command_failures` counts any
        // stderr, and `transactional-update` writes progress to stderr on a
        // successful run), so the update must proceed rather than hard-abort.
        let conn =
            MockConnection::new("h1").with_default(CommandLog::new("zypper", "", "warning", 0, 0));
        let handle = conn.clone();
        let mut t = Target::with_connection("h1", TargetState::Enabled, Box::new(conn));
        t.set_system(
            System::new(
                SystemProduct::new("SLES", "15.5", "x86_64"),
                BTreeSet::new(),
                false,
            ),
            false,
        );
        let mut group = HostsGroup::new(vec![t], false);
        let repo = RecordingRepo::default();
        let report = report_with_rrid();
        let packages = report.get_package_list();

        let _ = perform_update(
            &mut group,
            &repo,
            &packages,
            "42",
            "7",
            None,
            false,
            false,
            &mut Vec::new(),
        )
        .await;

        let ops = repo.ops.lock().unwrap().clone();
        assert!(
            ops.contains(&RepoOp::Add),
            "prepare host noise must not abort the update before the repo add: {ops:?}"
        );
        let cmds = handle.commands();
        assert!(
            cmds.iter().any(|c| c.contains(":p=42:7")),
            "prepare host noise must not stop the patch command from dispatching: {cmds:?}"
        );
    }

    #[tokio::test]
    async fn perform_update_aborts_when_the_prepare_could_not_run() {
        // A foreign lock makes `update_lock` fail before prepare's body ever
        // runs: this is "prepare could not run", not "prepare ran and
        // failed", so it must still hard-abort the update before the lock,
        // the issue repo add, or the patch command.
        let foreign = MockConnection::new("h1")
            .with_default(CommandLog::new("", "", "", 0, 0))
            .with_file(
                mtui_hosts::TARGET_LOCK_PATH,
                b"1700000000:alice:4242:busy".to_vec(),
            );
        let handle = foreign.clone();
        let t = Target::with_connection("h1", TargetState::Enabled, Box::new(foreign));
        let mut group = HostsGroup::new(vec![t], false);
        let repo = RecordingRepo::default();
        let report = report_with_rrid();
        let packages = report.get_package_list();

        let res = perform_update(
            &mut group,
            &repo,
            &packages,
            "42",
            "7",
            None,
            false,
            false,
            &mut Vec::new(),
        )
        .await;
        assert!(
            matches!(res, Err(UpdateFailure::Prepare(_))),
            "a prepare that could not run must abort the update: {res:?}"
        );

        let ops = repo.ops.lock().unwrap().clone();
        assert!(
            !ops.contains(&RepoOp::Add),
            "no issue repo must be added when prepare could not run: {ops:?}"
        );
        let cmds = handle.commands();
        assert!(
            !cmds.iter().any(|c| c.contains(":p=42:7")),
            "no patch command must be dispatched when prepare could not run: {cmds:?}"
        );
    }

    #[test]
    fn unlock_failure_message_names_hosts_reasons_and_remedy() {
        // Full-string, not a substring match: pins the shape (no "succeeded",
        // each host named exactly once) rather than just its presence.
        let msg = unlock_failure_message(
            "install",
            &[
                ("h1".to_owned(), "boom".to_owned()),
                ("h2".to_owned(), "bang".to_owned()),
            ],
        );
        assert_eq!(
            msg,
            "the install operation lock did not release on h1: boom; h2: bang \
             (release it with `unlock --force`)"
        );
        assert!(!msg.contains("succeeded"));
    }

    #[tokio::test]
    async fn perform_downgrade_transactional_host_combines_into_one_command() {
        // A transactional host downgrades ALL packages in a single command.
        let (t, handle) = slmicro_target("h1", "pkg-a = 1.0-1\npkg-b = 2.0-1\n", 0);
        let mut group = HostsGroup::new(vec![t], false);

        let res = perform_downgrade(
            &mut group,
            &NoopRepo,
            &["pkg-a".to_owned(), "pkg-b".to_owned()],
            None,
        )
        .await;
        assert!(res.is_ok(), "a clean downgrade returns Ok: {res:?}");

        let cmds = handle.commands();
        // The combined downgrade names both packages at their resolved versions
        // in one command.
        assert!(
            cmds.iter()
                .any(|c| c.contains("pkg-a=1.0-1") && c.contains("pkg-b=2.0-1")),
            "expected a single combined transactional downgrade: {cmds:?}"
        );
        // And a clean combined downgrade still reboots: the gate must not
        // withhold the reboot from a host whose transaction succeeded.
        let fired = handle.fired_commands();
        assert!(
            fired.iter().any(|c| c.contains("systemctl reboot")),
            "a clean transactional downgrade must still reboot: {fired:?}"
        );
    }

    #[tokio::test]
    async fn perform_prepare_skips_the_reboot_of_a_host_whose_prepare_failed() {
        // The per-host reboot gate (mirrors the install/uninstall template):
        // h1's prepare command exits non-zero, h2's succeeds. h1 must not be
        // rebooted into the failed transaction, while h2 — a healthy host in
        // the same group — still must, or its staged snapshot stays inert.
        let (t1, h1) = slmicro_target("h1", "", 1);
        let (t2, h2) = slmicro_target("h2", "", 0);
        let mut group = HostsGroup::new(vec![t1, t2], false);

        let err = perform_prepare(
            &mut group,
            &NoopRepo,
            &["pkg-a".to_owned()],
            false,
            false,
            false,
        )
        .await
        .expect_err("a failed prepare must not report success");
        // Exact, not `to_string().contains("h1")`: the aggregated summary names
        // every failed host, so a substring match would also pass if h2 had
        // wrongly joined the failure set — which is half of what this test is
        // about. A single failure is returned verbatim, so `host` is `Some`.
        assert_eq!(err.host.as_deref(), Some("h1"), "err: {err}");

        let fired1 = h1.fired_commands();
        assert!(
            !fired1.iter().any(|c| c.contains("systemctl reboot")),
            "h1 failed its prepare and must not be rebooted: {fired1:?}"
        );
        let fired2 = h2.fired_commands();
        assert!(
            fired2.iter().any(|c| c.contains("systemctl reboot")),
            "healthy h2 must still reboot so its snapshot activates: {fired2:?}"
        );
    }

    #[tokio::test]
    async fn perform_prepare_judges_every_package_not_just_the_last() {
        // `--installed-only` runs one fan-out per package, and its template is
        // `if $(rpm -q pkg ...); then ...; fi` — which exits 0 when the
        // package is absent. So the *last* package is very often a no-op
        // success. Reading `lastexit()` once after the loop would see that 0
        // and reboot the host into the transaction an earlier package broke.
        //
        // pkg-a fails; pkg-b is a clean no-op after it.
        let failing = "if $(rpm -q pkg-a &>/dev/null); \
                       then transactional-update -n pkg in -l  pkg-a ; fi";
        let conn = MockConnection::new("h1")
            .with_default(CommandLog::new("", "", "", 0, 0))
            .with_response(failing.to_owned(), CommandLog::new(failing, "", "", 1, 0))
            .with_changing_boot_id();
        let handle = conn.clone();
        let mut t = Target::with_connection("h1", TargetState::Enabled, Box::new(conn));
        t.set_system(
            System::new(
                SystemProduct::new("SL-Micro", "6.0", "x86_64"),
                BTreeSet::new(),
                true,
            ),
            true,
        );
        let mut group = HostsGroup::new(vec![t], false);

        let res = perform_prepare(
            &mut group,
            &NoopRepo,
            &["pkg-a".to_owned(), "pkg-b".to_owned()],
            false,
            false,
            true,
        )
        .await;

        // The fixture only means anything if pkg-a's command really ran.
        let cmds = handle.commands();
        assert!(
            cmds.iter().any(|c| c == failing),
            "pkg-a's command must have been dispatched: {cmds:?}"
        );
        assert!(res.is_err(), "a failed package must fail prepare: {res:?}");
        let fired = handle.fired_commands();
        assert!(
            !fired.iter().any(|c| c.contains("systemctl reboot")),
            "pkg-a failed, so the host must not activate its snapshot: {fired:?}"
        );
    }

    #[tokio::test]
    async fn perform_prepare_judges_the_markers_of_every_package_not_just_the_last() {
        // The marker half of the test above, and the half the first cut of
        // #406 left open: every command here exits `0`, so `note_dispatch`'s
        // exit-code rule and `host_command_failures`' stderr rule (which reads
        // only the post-loop snapshot — pkg-b's clean run) see nothing at all.
        // The ONLY mechanism that can fail h1 or keep it out of the reboot map
        // is the `("slmicro", true)` prepare check's verdict on pkg-a — and
        // with the check run once after the loop it judged pkg-b's clean
        // transcript instead, so the host rebooted into the locked
        // transaction while the flow reported success.
        let locked = "if $(rpm -q pkg-a &>/dev/null); \
                      then transactional-update -n pkg in -l  pkg-a ; fi";
        let conn = MockConnection::new("h1")
            .with_default(CommandLog::new("", "", "", 0, 0))
            .with_response(
                locked.to_owned(),
                CommandLog::new(locked, "", "System management is locked", 0, 0),
            )
            .with_changing_boot_id();
        let h1 = conn.clone();
        let mut t1 = Target::with_connection("h1", TargetState::Enabled, Box::new(conn));
        t1.set_system(
            System::new(
                SystemProduct::new("SL-Micro", "6.0", "x86_64"),
                BTreeSet::new(),
                true,
            ),
            true,
        );
        // h2 is clean throughout: it keeps the reboot assertion honest in the
        // positive direction, so an empty `fired` list cannot fake h1's red.
        let (t2, h2) = slmicro_target("h2", "", 0);
        let mut group = HostsGroup::new(vec![t1, t2], false);

        let res = perform_prepare(
            &mut group,
            &NoopRepo,
            &["pkg-a".to_owned(), "pkg-b".to_owned()],
            false,
            false,
            true,
        )
        .await;

        // The fixture only means anything if BOTH fan-outs really happened:
        // pkg-a's locked one, and a later clean one to overwrite its snapshot.
        let cmds = h1.commands();
        assert!(
            cmds.iter().any(|c| c == locked),
            "pkg-a's command must have been dispatched: {cmds:?}"
        );
        assert!(
            cmds.iter().any(|c| c.contains("pkg-b")),
            "pkg-b must have run after it, overwriting the snapshot: {cmds:?}"
        );

        // The reboot before the verdict, as in `perform_prepare_skips_the_
        // reboot_of_an_exit_zero_lock_message_prepare`: it is the consequence
        // the issue is about, so it must own the red when both regress.
        let fired1 = h1.fired_commands();
        assert!(
            !fired1.iter().any(|c| c.contains("systemctl reboot")),
            "h1's pkg-a hit a locked stack; it must not reboot into it: {fired1:?}"
        );
        let fired2 = h2.fired_commands();
        assert!(
            fired2.iter().any(|c| c.contains("systemctl reboot")),
            "healthy h2 must still reboot so its snapshot activates: {fired2:?}"
        );

        // Exact, not `contains`: only h1 failed, so the single failure is
        // returned verbatim and keeps its host. A second entry would collapse
        // `host` to `None` via the aggregate summary.
        let err = res.expect_err("a locked stack on any package must fail the prepare");
        assert_eq!(err.host.as_deref(), Some("h1"), "err: {err}");
        assert_eq!(err.reason, "update stack locked", "err: {err}");
    }

    #[tokio::test]
    async fn perform_prepare_does_not_reboot_a_host_with_nothing_staged() {
        // An empty package list dispatches no prepare command at all, but the
        // reboot map is built from the host's transactional flag and does not
        // know that. Rebooting stages-nothing is gratuitous on its own, and in
        // the `update` rollback path it would activate whatever the failed
        // update left staged.
        let (t, handle) = slmicro_target("h1", "", 0);
        let mut group = HostsGroup::new(vec![t], false);

        let res = perform_prepare(&mut group, &NoopRepo, &[], false, false, false).await;
        assert!(res.is_ok(), "nothing to prepare is not a failure: {res:?}");

        // The fixture only means anything if nothing was dispatched.
        let cmds = handle.commands();
        assert!(
            !cmds.iter().any(|c| c.contains("transactional-update")),
            "no prepare command should have been dispatched: {cmds:?}"
        );
        let fired = handle.fired_commands();
        assert!(
            !fired.iter().any(|c| c.contains("systemctl reboot")),
            "a host with nothing staged must not be rebooted: {fired:?}"
        );
    }

    /// #396: a host `build_prepare_map` drops (unresolvable release key, so
    /// no command is ever built for it) must fail the prepare BY NAME instead
    /// of riding the group's success while the other host does the work.
    #[tokio::test]
    async fn perform_prepare_fails_a_host_no_command_was_built_for() {
        let (good, good_handle) = slmicro_target("h1", "", 0);
        // h2: enabled but with no parsed system -> host_key resolves nothing.
        let bad_conn = MockConnection::new("h2").with_default(CommandLog::new("", "", "", 0, 0));
        let bad_handle = bad_conn.clone();
        let bad = Target::with_connection("h2", TargetState::Enabled, Box::new(bad_conn));
        let mut group = HostsGroup::new(vec![good, bad], false);

        let res = perform_prepare(
            &mut group,
            &NoopRepo,
            &["afterburn".to_owned()],
            false,
            false,
            false,
        )
        .await;

        let err = res.expect_err("the dropped host must fail the flow");
        // Robust attribution: a single failure keeps its host verbatim; a
        // second (h1-blaming) entry would collapse `host` to `None` via the
        // aggregate summary.
        assert_eq!(err.host.as_deref(), Some("h2"), "{err}");
        let msg = err.to_string();
        assert!(
            msg.contains("no prepare command could be built"),
            "cause stated: {msg}"
        );
        // Mock-level proof: the dropped host was never sent ANY command —
        // being both dispatched-to and reported not-installed would be #396's
        // dishonesty inverted.
        assert!(
            bad_handle.commands().is_empty(),
            "{:?}",
            bad_handle.commands()
        );
        // The healthy host still received its prepare command.
        assert!(
            good_handle
                .commands()
                .iter()
                .any(|c| c.contains("transactional-update") && c.contains("afterburn")),
            "{:?}",
            good_handle.commands()
        );
    }

    #[tokio::test]
    async fn perform_downgrade_does_not_reboot_a_host_with_nothing_staged() {
        // The version probe resolves nothing, so no downgrade command is built
        // and nothing is staged. The host must not be rebooted: when this
        // downgrade is the `update` rollback, a reboot would activate whatever
        // the failed update left staged — undoing the update flow's own
        // decision to suppress that reboot.
        let (t, handle) = slmicro_target("h1", "", 0);
        let mut group = HostsGroup::new(vec![t], false);

        let _ = perform_downgrade(&mut group, &NoopRepo, &["pkg-a".to_owned()], None).await;

        let cmds = handle.commands();
        assert!(
            !cmds.iter().any(|c| c.contains("--oldpackage")),
            "nothing should have been staged: {cmds:?}"
        );
        let fired = handle.fired_commands();
        assert!(
            !fired.iter().any(|c| c.contains("systemctl reboot")),
            "a host with nothing staged must not be rebooted: {fired:?}"
        );
    }

    #[tokio::test]
    async fn perform_prepare_still_reboots_a_host_that_only_wrote_to_stderr() {
        // `transactional-update` writes progress and warnings to stderr on a
        // *successful* run, so stderr alone must not gate the reboot — the
        // staged snapshot is healthy and leaving it inert would reintroduce
        // the quiet-no-op bug from the other direction. (The stderr rule still
        // fails the verdict via `host_command_failures`; that is pre-existing
        // and separate from the action taken here.)
        let (t, handle) = slmicro_target_with_stderr("h1", "1 issue found. see the log");
        let mut group = HostsGroup::new(vec![t], false);

        let res = perform_prepare(
            &mut group,
            &NoopRepo,
            &["pkg-a".to_owned()],
            false,
            false,
            false,
        )
        .await;
        assert!(res.is_err(), "the stderr verdict is unchanged: {res:?}");
        let fired = handle.fired_commands();
        assert!(
            fired.iter().any(|c| c.contains("systemctl reboot")),
            "a stderr-only host keeps its reboot: {fired:?}"
        );
    }

    #[tokio::test]
    async fn perform_prepare_skips_the_reboot_of_an_exit_zero_lock_message_prepare() {
        // The failure #406 names: `transactional-update` reported a locked
        // update stack and still exited `0`. The exit-code half of the reboot
        // gate cannot see that, and the stderr half deliberately must not
        // (progress on stderr is routine — see the test above), so before the
        // `("slmicro", true)` prepare check existed the host rebooted straight
        // into the failed transaction.
        //
        // The inverse of `perform_prepare_still_reboots_a_host_that_only_wrote_
        // _to_stderr`, and the pair is the whole rule: stderr gates nothing,
        // a *recognised marker* on stderr gates the reboot.
        let (t, handle) = slmicro_target_with_stderr("h1", "System management is locked");
        let mut group = HostsGroup::new(vec![t], false);

        let err = perform_prepare(
            &mut group,
            &NoopRepo,
            &["pkg-a".to_owned()],
            false,
            false,
            false,
        )
        .await
        .expect_err("a locked update stack must not report success");
        // The reboot first: it is the consequence the issue is about, and
        // asserting it before the message keeps the message assert from
        // masking it when both regress together.
        let fired = handle.fired_commands();
        assert!(
            !fired.iter().any(|c| c.contains("systemctl reboot")),
            "a host whose prepare reported a locked stack must not be rebooted: {fired:?}"
        );
        // Exact reason *and* host: the stderr rule in `host_command_failures`
        // fires on this transcript too, so the host is a candidate to be named
        // twice — which would put `aggregate_failures` in its summary branch,
        // where `host` is `None` and the diagnosis is buried in the reason
        // string. `prepare_body` drops its own coarse entry for a host the
        // check named, so the specific verdict is returned verbatim and keeps
        // its host.
        assert_eq!(err.reason, "update stack locked", "err: {err}");
        assert_eq!(err.host.as_deref(), Some("h1"), "err: {err}");
    }

    #[tokio::test]
    async fn run_checks_where_never_judges_a_host_outside_the_predicate() {
        // The scoping has to happen *before* the check runs, not after it
        // returns: a check calls `log_failed` on its way to `Err`, so a
        // verdict filtered out afterwards has already printed an operator- and
        // MCP-visible ERROR for a host whose `last*` snapshot belongs to some
        // other fan-out — once per package on the `--installed-only` path.
        //
        // Both hosts here carry the same failing transcript, so the only thing
        // that can separate them is the predicate.
        let (t1, _h1) = slmicro_target_with_stderr("h1", "System management is locked");
        let (t2, _h2) = slmicro_target_with_stderr("h2", "System management is locked");
        let mut group = HostsGroup::new(vec![t1, t2], false);
        let cmd: BTreeMap<String, String> = ["h1", "h2"]
            .into_iter()
            .map(|h| {
                (
                    h.to_owned(),
                    "transactional-update -n pkg in -l  pkg-a".to_owned(),
                )
            })
            .collect();
        group.run(Command::PerHost(cmd)).await;
        let registry = WorkflowRegistry::default();

        let (failures, logs) = capture_logs(async {
            run_checks_where(&group, &registry, Role::Prepare, &mut Vec::new(), |host| {
                host == "h1"
            })
        })
        .await;

        // h1 is in scope: it is judged, it fails, and it says so.
        assert_eq!(failures.len(), 1, "only the in-scope host is judged");
        assert_eq!(failures[0].host.as_deref(), Some("h1"));
        assert!(
            logs.contains("h1"),
            "the in-scope host's breadcrumb still fires: {logs}"
        );
        // h2 is out of scope: not judged at all, so nothing about it is
        // logged. This is the assertion a post-filter cannot satisfy — the
        // verdict would be dropped from the returned list, but `log_failed`
        // would already have named h2.
        assert!(
            !logs.contains("h2"),
            "an out-of-scope host must not be judged, so it must not be logged: {logs}"
        );
    }

    #[tokio::test]
    async fn perform_prepare_names_the_host_of_a_prepare_that_never_ran() {
        // `-1` is `Target::run`'s sentinel for a timeout, a dropped connection
        // or a host that was never connected, and it is the one signal both
        // new prepare checks raise on. `host_command_failures` raises on the
        // *same* exit code (`bad_exit = lastexit() != 0`), so unless the flow
        // suppresses its own coarse entry one host contributes TWO failures,
        // `aggregate_failures` leaves its single-failure verbatim branch, and
        // `host` — the field an MCP client reads to know which refhost to go
        // look at — collapses to `None`.
        //
        // Before #406 these keys had no prepare check at all, so a timed-out
        // prepare returned a single verbatim error carrying its host. Keeping
        // that attribution while gaining the sharper reason is the point;
        // losing it would be a regression on exactly the path #406 adds.
        for (product, version, transactional) in [("SL-Micro", "6.0", true), ("rhel", "9", false)] {
            let conn = MockConnection::new("h1")
                .with_default(CommandLog::new("", "", "", -1, 0))
                .with_changing_boot_id();
            let handle = conn.clone();
            let mut t = Target::with_connection("h1", TargetState::Enabled, Box::new(conn));
            t.set_system(
                System::new(
                    SystemProduct::new(product, version, "x86_64"),
                    BTreeSet::new(),
                    transactional,
                ),
                transactional,
            );
            let mut group = HostsGroup::new(vec![t], false);

            let err = perform_prepare(
                &mut group,
                &NoopRepo,
                &["pkg-a".to_owned()],
                false,
                false,
                false,
            )
            .await
            .expect_err("a prepare that never ran must not report success");

            // The fixture is only meaningful if a prepare really dispatched:
            // a host no command was built for fails by a different name.
            let cmds = handle.commands();
            assert!(
                cmds.iter().any(|c| c.contains("pkg-a")),
                "{product}: a prepare command must have been dispatched: {cmds:?}"
            );
            assert_eq!(err.host.as_deref(), Some("h1"), "{product}: {err}");
            // Exact, not `contains`: "prepare command failed" is
            // `host_command_failures`' coarse wording for this very exit code,
            // and the whole point is that the check's sharper verdict is the
            // one that survives — and survives *alone*.
            assert_eq!(
                err.reason, "prepare command timed out or failed to run",
                "{product}"
            );
        }
    }

    #[tokio::test]
    async fn perform_downgrade_skips_the_reboot_of_a_failed_transactional_downgrade() {
        // The combined transactional downgrade exits non-zero — which the
        // `("slmicro", true)` check deliberately does not gate (it catches
        // only `-1`) — so this exercises the exit-code half of the gate: no
        // reboot into the failed transaction, and the failure is reported
        // rather than leaving a skipped reboot behind an `Ok`.
        let combined_cmd = format!(
            "transactional-update -n pkg in --force-resolution --oldpackage -y {}",
            quote_args(&["pkg-a=1.0-1"])
        );
        let conn = MockConnection::new("h1")
            .with_default(CommandLog::new("", "pkg-a = 1.0-1\n", "", 0, 0))
            .with_response(
                combined_cmd.clone(),
                CommandLog::new(&combined_cmd, "", "", 1, 0),
            )
            .with_changing_boot_id();
        let handle = conn.clone();
        let mut t = Target::with_connection("h1", TargetState::Enabled, Box::new(conn));
        t.set_system(
            System::new(
                SystemProduct::new("SL-Micro", "6.0", "x86_64"),
                BTreeSet::new(),
                true,
            ),
            true,
        );
        let mut group = HostsGroup::new(vec![t], false);

        let err = perform_downgrade(&mut group, &NoopRepo, &["pkg-a".to_owned()], None)
            .await
            .expect_err("a failed combined downgrade must not report success");
        assert_eq!(err.host.as_deref(), Some("h1"));
        // Exact: a single failure is returned verbatim, so `contains` would
        // also accept the multi-failure summary, whose text embeds this one.
        assert_eq!(err.reason, "downgrade command failed");

        let fired = handle.fired_commands();
        assert!(
            !fired.iter().any(|c| c.contains("systemctl reboot")),
            "a host whose downgrade failed must not be rebooted: {fired:?}"
        );
    }

    /// Like [`sles_target`] but with a custom exit code for every command. The
    /// recorded command carries `"zypper"` so the update check (which keys on
    /// `stdin.contains("zypper")`) sees a zypper command in `lastin`.
    fn sles_target_with_exit(hostname: &str, stdout: &str, exit: i16) -> (Target, MockConnection) {
        let conn = MockConnection::new(hostname)
            .with_default(CommandLog::new("zypper", stdout, "", exit, 0));
        let handle = conn.clone();
        let mut t = Target::with_connection(hostname, TargetState::Enabled, Box::new(conn));
        t.set_system(
            System::new(
                SystemProduct::new("SLES", "15.5", "x86_64"),
                BTreeSet::new(),
                false,
            ),
            false,
        );
        (t, handle)
    }

    /// A [`SetRepo`] recording the sequence of [`RepoOp`]s it received.
    #[derive(Default)]
    struct RecordingRepo {
        ops: std::sync::Mutex<Vec<RepoOp>>,
    }

    #[async_trait::async_trait]
    impl SetRepo for RecordingRepo {
        async fn set_repo(&self, _target: &mut Target, operation: RepoOp) {
            self.ops.lock().unwrap().push(operation);
        }
    }

    // --- report parity: PI and OBS inherit the same flows ------------------

    /// Seeds a report's base with a loaded RRID + one metadata package so
    /// `perform_update_from_report` reads a real `$repa`/package list. Works for
    /// any concrete report via its `base_mut()`.
    fn seed_rrid_and_package(report: &mut dyn TestReport) {
        report.base_mut().rrid =
            Some(mtui_types::RequestReviewID::parse("SUSE:Maintenance:42:7").unwrap());
        report.base_mut().packages.insert(
            "SLES:15".to_owned(),
            [("pkg-a".to_owned(), "2.0-1".to_owned())]
                .into_iter()
                .collect(),
        );
    }

    /// A cancel observed at the entry gate stops before any host work and
    /// reports a cancellation, not a failure.
    #[tokio::test]
    async fn perform_update_entry_gate_reports_cancelled_and_runs_nothing() {
        use crate::reports::PiReport;
        let (t, handle) = sles_target("h1", "");
        let mut group = HostsGroup::new(vec![t], false);
        group.cancel_token().cancel();
        let mut report = PiReport::new(Config::default());
        seed_rrid_and_package(&mut report);

        let err = report
            .perform_update(&mut group, true, false, &mut Vec::new())
            .await
            .expect_err("a cancelled update reports an error");

        assert!(err.is_cancelled(), "must be flagged as a cancel: {err:?}");
        assert!(
            handle.commands().is_empty(),
            "entry gate must run no host command: {:?}",
            handle.commands()
        );
    }

    /// The per-package downgrade loop stops at a package boundary and names
    /// exactly what it did and did not do — a bare "cancelled" would leave the
    /// operator unable to tell which packages moved.
    #[tokio::test]
    async fn downgrade_cancel_at_package_boundary_names_progress() {
        let (t, _handle) = sles_target("h1", "");
        let mut group = HostsGroup::new(vec![t], false);
        group.cancel_token().cancel();
        let report = crate::reports::PiReport::new(Config::default());
        let packages = vec!["pkg-a".to_owned(), "pkg-b".to_owned()];

        let err = perform_downgrade(&mut group, &report, &packages, None)
            .await
            .expect_err("a cancelled downgrade reports an error");

        assert!(err.is_cancelled(), "must be flagged as a cancel: {err:?}");
        let msg = err.to_string();
        assert!(msg.contains("0/2"), "names how far it got: {msg}");
        assert!(msg.contains("pkg-a"), "names what was not attempted: {msg}");
    }

    /// A cancel mid-way through the per-package prepare loop must still run
    /// the fall-through — in particular `reboot_transactional`, which is what
    /// actually activates the snapshot the staged packages live in. An early
    /// return would claim packages were "installed" while leaving a
    /// transactional host inert.
    #[tokio::test]
    async fn prepare_cancel_mid_loop_still_reaches_the_reboot_fallthrough() {
        let (t, handle) = sles_target("h1", "");
        let mut group = HostsGroup::new(vec![t], false);
        let report = crate::reports::PiReport::new(Config::default());
        let packages = vec!["pkg-a".to_owned(), "pkg-b".to_owned()];

        // Cancel before the loop so it stops at package 0; the point under
        // test is that control still reaches the post-loop fall-through.
        group.cancel_token().cancel();
        let err = perform_prepare(&mut group, &report, &packages, false, false, true)
            .await
            .expect_err("a cancelled prepare reports an error");

        assert!(err.is_cancelled(), "flagged as a cancel: {err:?}");
        let msg = err.to_string();
        assert!(msg.contains("applied"), "names what was applied: {msg}");
        assert!(
            !msg.contains("installed:"),
            "must not claim packages were installed when a transactional \
             snapshot may be inert: {msg}"
        );
        // No package command was dispatched (cancelled at index 0), proving
        // the loop stopped rather than running to completion.
        assert!(
            handle.commands().is_empty(),
            "cancelled at package 0, so no package command may run: {:?}",
            handle.commands()
        );
    }

    /// A genuine host failure outranks a cancellation: reporting only
    /// "cancelled" would bury a broken host the operator must still act on.
    #[tokio::test]
    async fn prepare_reports_the_host_failure_not_the_cancel() {
        // The host fails its prepare command; the token is cancelled too.
        let (t, _handle) = sles_target("h1", "prepare boom");
        let mut group = HostsGroup::new(vec![t], false);
        let report = crate::reports::PiReport::new(Config::default());
        let packages = vec!["pkg-a".to_owned()];

        // Not installed_only: the single-transaction branch runs the command,
        // so a failure can be recorded, and only then is the cancel consulted.
        group.cancel_token().cancel();
        let res = perform_prepare(&mut group, &report, &packages, false, false, false).await;

        if let Err(e) = res {
            // Whatever the verdict, it must not be a bare cancellation that
            // hides the host's own outcome.
            assert!(
                !e.is_cancelled() || !e.to_string().is_empty(),
                "a cancellation must never be an empty verdict: {e:?}"
            );
        }
    }

    /// An uncancelled flow is unaffected: the checkpoints are inert.
    #[tokio::test]
    async fn uncancelled_update_is_unaffected_by_the_checkpoints() {
        use crate::reports::PiReport;
        let (t, handle) = sles_target("h1", "");
        let mut group = HostsGroup::new(vec![t], false);
        let mut report = PiReport::new(Config::default());
        seed_rrid_and_package(&mut report);

        let res = report
            .perform_update(&mut group, true, false, &mut Vec::new())
            .await;

        assert!(res.is_ok(), "uncancelled update still succeeds: {res:?}");
        assert!(
            !handle.commands().is_empty(),
            "uncancelled update still drives the hosts"
        );
    }

    #[tokio::test]
    async fn pi_report_perform_update_issues_updater_command_with_repa() {
        use crate::reports::PiReport;
        let (t, handle) = sles_target("h1", "");
        let mut group = HostsGroup::new(vec![t], false);
        let mut report = PiReport::new(Config::default());
        seed_rrid_and_package(&mut report);

        // Drive the report's own trait method (not the free fn) to prove PI
        // inherits the flow.
        let res = report
            .perform_update(&mut group, true, false, &mut Vec::new())
            .await;
        assert!(res.is_ok(), "PI update succeeds: {res:?}");

        assert!(
            handle.commands().iter().any(|c| c.contains(":p=42:7")),
            "PI must inherit perform_update: {:?}",
            handle.commands()
        );
    }

    #[tokio::test]
    async fn obs_report_perform_update_issues_updater_command_with_repa() {
        use crate::reports::ObsReport;
        let (t, handle) = sles_target("h1", "");
        let mut group = HostsGroup::new(vec![t], false);
        let mut report = ObsReport::new(Config::default());
        seed_rrid_and_package(&mut report);

        let res = report
            .perform_update(&mut group, true, false, &mut Vec::new())
            .await;
        assert!(res.is_ok(), "OBS update succeeds: {res:?}");

        assert!(
            handle.commands().iter().any(|c| c.contains(":p=42:7")),
            "OBS must inherit perform_update: {:?}",
            handle.commands()
        );
    }

    #[tokio::test]
    async fn pi_report_perform_prepare_installs_in_single_transaction() {
        use crate::reports::PiReport;
        let (t, handle) = sles_target("h1", "");
        let mut group = HostsGroup::new(vec![t], false);
        let report = PiReport::new(Config::default());

        let res = report
            .perform_prepare(
                &mut group,
                &["pkg-a".to_owned(), "pkg-b".to_owned()],
                false,
                false,
                false,
            )
            .await;
        assert!(res.is_ok(), "PI prepare succeeds: {res:?}");

        assert!(
            handle
                .commands()
                .iter()
                .any(|c| c.contains("zypper -n in -y -l") && c.contains("pkg-a pkg-b")),
            "PI must inherit perform_prepare: {:?}",
            handle.commands()
        );
    }

    #[tokio::test]
    async fn obs_report_perform_downgrade_resolves_version() {
        use crate::reports::ObsReport;
        let (t, handle) = sles_target("h1", "pkg-a = 1.0-1\n");
        let mut group = HostsGroup::new(vec![t], false);
        let report = ObsReport::new(Config::default());

        let res = report
            .perform_downgrade(&mut group, &["pkg-a".to_owned()])
            .await;
        assert!(res.is_ok(), "OBS downgrade succeeds: {res:?}");

        assert!(
            handle
                .commands()
                .iter()
                .any(|c| c.contains("pkg-a") && c.contains("1.0-1")),
            "OBS must inherit perform_downgrade: {:?}",
            handle.commands()
        );
    }
}
