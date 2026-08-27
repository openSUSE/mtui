//! The bespoke (non-template) update flows: `perform_prepare`,
//! `perform_downgrade`, `perform_update`.
//!
//! Unlike install/uninstall (which route through the shared [`Operation`]
//! template), these three are deliberately open-coded: they have per-package
//! loops, `set_repo` add/remove fan-outs, package-version comparison, and (for
//! `update`) a two-phase try/finally that cleans the test repos up on success
//! while **keeping** them on failure for retry/diagnosis.
//!
//! They live here — as the concrete reports' `perform_*` bodies, alongside
//! `perform_install` — because they need `get_package_list` / `set_repo`, which
//! keeps `mtui-hosts` free of a `mtui-testreport` dependency and reuses the
//! report's own [`SetRepo`] hook and package list. Each host's command
//! templates come straight from the [`WorkflowRegistry`] (`ActionCommands` +
//! `CheckFn`) — the same tables the `PlanProvider` adapter uses — keyed on
//! `(system.get_release(), transactional)`.

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
/// The variants answer one question — **can a group-wide downgrade repair any
/// host that failed?** — because that is what [`perform_update_with_rollback`]
/// has to decide, and the rollback reverts *every* host in the group. `Check`
/// and `RebootNotTaken` answer yes; the rest answer no for reasons an operator
/// needs kept apart. All collapse to a single [`UpdateError`] at the command
/// boundary.
///
/// "No patch was dispatched" is not "the host is untouched": an abort after a
/// *completed* `prepare` leaves that prepare's packages installed, so the
/// prepare writes its own `/var/log/mtui.log` row (#407).
#[derive(Debug, PartialEq, Eq)]
pub enum UpdateFailure {
    /// One or more hosts failed the `updater` check after the command ran.
    Check(UpdateError),
    /// The pre-update `prepare` step could not run: no preparer for a host's
    /// key, the operation lock was contended, or the issue repo could not be
    /// set. No patch was dispatched, so no rollback; `--noprepare` is the
    /// opt-out. A prepare that *ran* and only reported a package-manager
    /// failure warns and lets the update proceed (see `PrepareFailure`).
    Prepare(UpdateError),
    /// A concrete target has no updater doer; a hard failure rather than a
    /// logged success, so a target that cannot be updated never reports
    /// "finished".
    MissingUpdater(UpdateError),
    /// Cooperative cancellation was requested (MCP `job_cancel`) and the flow
    /// stopped at a step boundary. Skips the rollback: a rollback is itself a
    /// multi-minute downgrade, so it would *extend* the work the caller just
    /// asked to stop.
    Cancelled(UpdateError),
    /// A transactional host rebooted after a successful patch and did not
    /// reconnect. Skips the rollback: the host is unreachable, and the rollback
    /// is group-wide, so running it would revert the *healthy* hosts on behalf
    /// of one that cannot be reached either way.
    Reboot(UpdateError),
    /// A transactional host was patched but its reboot never took effect while
    /// the host stayed **reachable**: the command was never dispatched, or the
    /// host answered with an unchanged boot id. Unlike [`Reboot`](Self::Reboot)
    /// this *does* roll back — it is up, serving the old snapshot while the
    /// group runs the new packages (the split-brain the rollback undoes), and
    /// the downgrade can reach it.
    RebootNotTaken(UpdateError),
    /// The update command never ran to completion on any host that failed —
    /// it timed out, or the connection dropped part-way (`Target::run`'s `-1`).
    ///
    /// Skips the rollback, but not by [`Reboot`](Self::Reboot)'s reachability
    /// argument: `-1` is a sentinel, not a liveness verdict. Either the flow
    /// lost the host, and a group-wide downgrade would revert the *healthy*
    /// hosts on behalf of one it cannot reach either way; or the command
    /// outlived its timeout on a host that is up, and since rpm masks signals
    /// inside its transaction, dispatching the downgrade now fires a second
    /// transaction at the one host whose first was never observed to end.
    /// Either way the state is unknown, not known-bad as under
    /// [`Check`](Self::Check).
    ///
    /// Used when **no** failed host is repairable and at least one is `-1`,
    /// including a mix with [`ProbeFailed`](Self::ProbeFailed) hosts, since
    /// this is the more conservative label. A mix with a real check failure
    /// still rolls back, on behalf of the repairable host.
    NotRun(UpdateError),
    /// Every failed host ran the update command and reported that it could not
    /// work out what to patch, so none dispatched a patch
    /// (`checks::update`'s `probe_failure`).
    ///
    /// Skips the rollback for a *stronger* reason than
    /// [`NotRun`](Self::NotRun)'s: a definite verdict, not the absence of one.
    /// Nothing is half-applied, so the group-wide rollback would revert every
    /// healthy peer over a probe that broke on one host. It is the operator's
    /// `zypper` view that needs attention (no repositories, a ZYpp lock, a
    /// broken awk), not the packages. A run mixing one of these with a `-1`
    /// host is labelled [`NotRun`](Self::NotRun).
    ProbeFailed(UpdateError),
    /// The pre-update `prepare` refused one or more hosts because their
    /// products compose none of the update's packages, so no package baseline
    /// was established on them and the patch left them out. The hosts that do
    /// compose it were patched normally; when every enabled host was refused,
    /// nothing was dispatched at all.
    ///
    /// Skips the rollback: an excluded host received no patch to undo, and the
    /// group-wide downgrade would revert the peers that updated correctly.
    Uncomposed(UpdateError),
}

/// Drives [`perform_update`] from a concrete report, reading the package list
/// and `$repa` selector (`maintenance_id` / `review_id`) off the report's RRID.
///
/// The shared body behind every report's `perform_update` override, so SL / PI
/// / OBS each delegate in one line. `report` supplies both the
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
/// Every other failure installed nothing, so each re-surfaces without a
/// rollback attempt. The rollback is best-effort, but it *can* fail —
/// [`perform_downgrade`] returns a `Result`, and a host whose version probe
/// never answered raises it (#451). The call site logs that error at WARN and
/// re-surfaces the original update error, so a failed rollback can never bury
/// the failure it was trying to repair; do not simplify the call away.
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
            error!(
                error = %e,
                "update aborted: prepare could not run (rerun with --noprepare to patch anyway)"
            );
            Err(e)
        }
        Err(UpdateFailure::MissingUpdater(e)) => {
            error!(error = %e, "update failed");
            Err(e)
        }
        Err(UpdateFailure::Cancelled(e)) => {
            info!(reason = %e, "update cancelled");
            Err(e)
        }
        Err(UpdateFailure::Reboot(e) | UpdateFailure::NotRun(e)) => {
            error!(error = %e, "update failed");
            Err(e)
        }
        Err(UpdateFailure::ProbeFailed(e)) => {
            error!(error = %e, "update failed: could not determine what to patch");
            Err(e)
        }
        Err(UpdateFailure::Uncomposed(e)) => {
            error!(error = %e, "update did not run on every host");
            Err(e)
        }
        Err(UpdateFailure::Check(e) | UpdateFailure::RebootNotTaken(e)) => {
            error!("Update failed");
            warn!("Error while updating. Rolling back changes");
            let pkgs = report.get_package_list();
            let id = report.base().rrid.as_ref().map(ToString::to_string);
            // The downgrade's own per-package checkpoint must not see a cancel
            // that landed during the run phase: it would abort the rollback at
            // package 0 and leave exactly the half-applied state it undoes.
            let token = targets.suspend_cancellation();
            // Best-effort: a failed downgrade must never bury the original
            // update error, so its result is logged, not returned.
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
/// `downgrade`) and is `None` for `prepare`. `install`/`uninstall` write a row
/// of the same shape from `OperationGroup::run` instead, so this function's
/// callers are not the full list of ops in `/var/log/mtui.log`.
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
/// For an op whose fan-out did not reach the whole group. `prepare` drops a
/// host whose release key does not resolve or whose template does not render,
/// and fails it with "nothing was installed", so a group-wide row would
/// contradict its own verdict in a file other tools parse (#407).
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
/// `transactional-update` writes progress to stderr on a *successful* run (see
/// `prepare_body`'s note on the reboot gate), so a per-host package-manager
/// complaint is too noisy to hard-abort `update` on. Whether prepare could even
/// *run* is a different, reliable signal.
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
    ///
    /// `uncomposed` names, as `host -> its products`, the hosts refused for the
    /// one reason the "too noisy to gate on" argument does not cover: their
    /// products compose none of the packages, so nothing was installed on them
    /// at all. `update` excludes exactly those from its patch — without the
    /// split it cannot tell them from the package-manager noise this variant
    /// exists to tolerate.
    HostReported {
        error: UpdateError,
        uncomposed: BTreeMap<String, String>,
    },
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

/// Builds the transactional-only reboot map for `role` over `selected`
/// (`None` is the whole group).
///
/// Returns `Err` if any transactional host **in scope** is missing a doer, so
/// the caller can early-return without locking.
///
/// Scoping does not change which host is rebooted — [`prepare_body`]'s retain
/// filter already refuses a host nothing was dispatched to — it changes what
/// the operator is told: an out-of-scope host left in the map draws that
/// filter's "skipping reboot" WARN, naming a host the caller had already
/// reported as excluded from this run.
fn build_reboot_map(
    targets: &HostsGroup,
    registry: &WorkflowRegistry,
    role: Role,
    selected: Option<&BTreeSet<String>>,
) -> Result<BTreeMap<String, String>, ()> {
    let mut reboot = BTreeMap::new();
    for target in targets.targets() {
        if !target.transactional() {
            continue;
        }
        if selected.is_some_and(|s| !s.contains(target.hostname())) {
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

/// Runs `role`'s post-run check on the hosts `allowed` accepts, returning the
/// recognised [`UpdateError`]s and appending any recognised-but-non-fatal
/// [`Diagnostic`] sections to `diagnostics`.
///
/// The check reads each host's `last*` snapshot after the command ran. Only the
/// `update` check currently emits diagnostics.
///
/// Every caller fans out to a subset of the group, so the predicate is
/// mandatory, and it is applied *before* the check runs rather than to its
/// verdict: a check calls [`log_failed`](crate::update_workflow::checks) on the
/// way to its `Err`, so a post-filter still emits an operator-facing ERROR for
/// a host whose verdict is then thrown away — once per package under
/// `prepare --installed-only`, against a snapshot from a fan-out that host was
/// not part of.
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

/// Collapses a list of per-host [`UpdateError`]s into a single `Result`: no
/// failures → `Ok`, one → verbatim, many → a summary naming the operation
/// (`op`) plus every failed host (sorted) and the joined detail. Shared by the
/// prepare/downgrade/install/uninstall flows so they all report failures the
/// same way `perform_update` does.
fn aggregate_failures(op: &str, mut failures: Vec<UpdateError>) -> Result<(), UpdateError> {
    if failures.is_empty() {
        Ok(())
    } else if failures.len() == 1 {
        Err(failures.remove(0))
    } else {
        let mut hosts: Vec<String> = failures.iter().filter_map(|e| e.host.clone()).collect();
        hosts.sort();
        // One host can contribute two failures with two distinct causes
        // (`downgrade_body` seeds its list from the issue-repo removal scan,
        // then adds that host's check verdict), and "failed on h1, h1" reads as
        // two hosts. `detail` still carries both causes. Not the place to fix
        // one signal reported by two rules: arriving here means the verbatim
        // branch was skipped and `host` is already lost, which is why
        // `prepare_body` and `downgrade_body` each drop their coarse exit-code
        // entry for a host their check already named.
        hosts.dedup();
        let detail: Vec<String> = failures.iter().map(ToString::to_string).collect();
        let mut aggregate = UpdateError::reason_only(format!(
            "{op} failed on {} ({})",
            hosts.join(", "),
            detail.join("; ")
        ));
        // The typed flags are the declared routing contract, so they must
        // survive the summary as well as the verbatim branch. `all`, not `any`:
        // a summary claiming "no patch was dispatched" while one host had
        // dispatched one would be worse than no claim at all.
        aggregate.probe_failed = failures.iter().all(|e| e.probe_failed);
        // `cancelled` is deliberately NOT propagated — THE AUTHORITATIVE
        // STATEMENT OF THAT CLAIM LIVES HERE, at the omission it justifies.
        // `perform_operation_with` builds `UpdateError { cancelled:
        // failure.cancelled, .. }` from `report.check_failures`, so a cancelled
        // check *can* cross the `Operation` seam into a `failures` list — but
        // no check in `update_workflow::checks` emits `CheckFailure::cancelled`
        // and this module's own cancellations are early `return Err`s. Empty by
        // producer, not by type: do not read that as a structural guarantee.
        //
        // A lone cancelled failure keeps its flag via the verbatim branch; only
        // the summary drops it, which for the mixed case *is* the outranking
        // rule — a real host failure collected beside a cancel must not be
        // re-routed to `CommandError::Cancelled` and excused as "the operator
        // stopped it" (`commands/perform.rs::map_flow_error`, the flag's only
        // non-test reader). It routes *reporting*, not the rollback, which
        // follows the `UpdateFailure` variant `update_run_phase` picks.
        Err(aggregate)
    }
}

/// Scans every host's post-fan-out `last*` snapshot for a command failure
/// (non-empty stderr or a non-zero exit) and returns one [`UpdateError`] per
/// failed host, keyed on `reason`.
///
/// The analogue of [`run_checks_where`] for the flows with no registry check
/// of their own: the shared install/uninstall template and the
/// prepare/downgrade repo/command fan-outs.
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
/// [`InstallOperation`] template, whose own per-host
/// [`Check`](mtui_hosts::Check) produces the verdict before the reboot.
///
/// Injecting here rather than where the group is built is deliberate:
/// [`OperationGroup::plans`] has exactly one consumer, so this is the one place
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
/// See [`perform_install`]; only the role and the summary label differ.
/// Uninstall shares the *install* check table ([`CheckProvider`]) — a removal
/// is judged by the same package-manager outcomes.
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
/// Production must not reach this except through [`perform_operation`]. The
/// parameter exists because there is no other way in: the injection below
/// overwrites unconditionally (`HostsGroup::set_plan_provider`), so a provider
/// installed on the group beforehand never survives to
/// [`OperationGroup::plans`]. That single call site stays in this body, keeping
/// [`perform_install`]'s inject-at-the-point-of-use rule intact. On cancelled
/// check failures see the `cancelled` comment in [`aggregate_failures`].
async fn perform_operation_with(
    targets: &mut HostsGroup,
    role: Role,
    packages: &[String],
    provider: Arc<dyn mtui_hosts::PlanProvider>,
) -> Result<(), UpdateError> {
    // Exhaustive on purpose: a `_ =>` arm defaulting to install would quietly
    // run the wrong package-manager command if a role were ever added.
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

    // Entry gate: nothing has run yet, so a cancel here is a clean no-op.
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

    // The template ran nothing at all (missing doer, or a host held by another
    // tester). Falling through would read stale `last*` values as success.
    let report = match outcome {
        Err(e) => {
            return Err(UpdateError::reason_only(describe_start_failure(
                &e, role, targets,
            )));
        }
        Ok(report) => report,
    };

    // No history write here: `Operation::run` writes the row itself, between
    // the fan-out and the reboot, because a row written after this call returns
    // would be lost on the transactional hosts that never came back.

    // A stranded operation lock does not turn a good install/uninstall into a
    // failed one — warn rather than joining `failures` below.
    if !report.unlock_failures.is_empty() {
        warn!("{}", unlock_failure_message(op, &report.unlock_failures));
    }

    // A failed check already excluded its host from the reboot map (see
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
/// lock — so this only warns on a real transport/SFTP error.
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
fn reboot_error(failure: RebootFailure) -> UpdateError {
    let what = match failure.cause {
        RebootFailureCause::Unreachable => "did not come back after the reboot",
        RebootFailureCause::NotDispatched => "never received the reboot",
        RebootFailureCause::NotRebooted => "never rebooted, so its snapshot is still inactive",
    };
    UpdateError::new(format!("{what} ({})", failure.reason), failure.host)
}

/// Turns an [`Operation::run`] start failure into a message that names the
/// hosts responsible.
///
/// `plans()` aborts on the first host it cannot resolve and reports only the
/// role and release — and a host whose product never parsed has no release, so
/// the bare error reads `Missing Installer for `, with nothing actionable in
/// it. The whole group is aborted, so re-resolve every host here and name each
/// one that has no command.
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
/// Such a host must fail the flow by name, not vanish into a discarded `()`;
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
/// `report` is the [`SetRepo`] hook for the issue repos, and also supplies the
/// update's composition, which narrows every host's list to what its own
/// products compose (see [`narrow_to_composed`]); `packages` the list to
/// prepare. `testing` selects repo-`add` + the testing preparer variant;
/// `force` toggles `--force-resolution`; `installed_only` narrows the list to
/// what each host already carries. Every host installs its list in a **single**
/// transaction either way, so transactional hosts land it in one snapshot.
pub async fn perform_prepare(
    targets: &mut HostsGroup,
    report: &dyn SetRepo,
    packages: &[String],
    force: bool,
    testing: bool,
    installed_only: bool,
) -> Result<(), UpdateError> {
    flatten_prepare_failure(
        perform_prepare_classified(
            targets,
            report,
            None,
            packages,
            force,
            testing,
            installed_only,
        )
        .await,
    )
}

/// [`perform_prepare`] restricted to `hosts` — every fan-out, the operation
/// lock and [`build_reboot_map`]'s pre-lock missing-preparer scan included.
///
/// For a flow that has already excluded a host from the work it is doing: the
/// group-wide variant would add the test repo to a host reported as excluded
/// and leave it there (the update's cleanup is scoped, see
/// [`remove_test_repos`]), and its contended lock would abort the prepare for
/// every eligible peer.
#[allow(clippy::too_many_arguments)]
async fn perform_prepare_for(
    targets: &mut HostsGroup,
    hosts: &BTreeSet<String>,
    report: &dyn SetRepo,
    packages: &[String],
    force: bool,
    testing: bool,
    installed_only: bool,
) -> Result<(), UpdateError> {
    flatten_prepare_failure(
        perform_prepare_classified(
            targets,
            report,
            Some(hosts),
            packages,
            force,
            testing,
            installed_only,
        )
        .await,
    )
}

/// Drops [`PrepareFailure`]'s classification, which only [`perform_update`]'s
/// abort gate reads.
fn flatten_prepare_failure(result: Result<(), PrepareFailure>) -> Result<(), UpdateError> {
    match result {
        Ok(()) => Ok(()),
        Err(
            PrepareFailure::DidNotRun(e)
            | PrepareFailure::HostReported { error: e, .. }
            | PrepareFailure::Cancelled(e),
        ) => Err(e),
    }
}

/// Shared implementation for [`perform_prepare`] / [`perform_prepare_for`]
/// (`selected` `None` is the whole group), and the classified body both flatten:
/// it distinguishes a prepare that never ran ([`PrepareFailure::DidNotRun`])
/// from one that ran and reported a host failure
/// ([`PrepareFailure::HostReported`]) — the split [`perform_update`] gates its
/// abort on, and the reason it calls this directly.
#[allow(clippy::too_many_arguments)]
async fn perform_prepare_classified(
    targets: &mut HostsGroup,
    report: &dyn SetRepo,
    selected: Option<&BTreeSet<String>>,
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
    let Ok(reboot) = build_reboot_map(targets, &registry, Role::Prepare, selected) else {
        return Err(PrepareFailure::DidNotRun(UpdateError::reason_only(
            "missing preparer",
        )));
    };

    let locked = match selected {
        Some(hosts) => targets.update_lock_selected(hosts).await,
        None => targets.update_lock().await,
    };
    if let Err(e) = locked {
        return Err(PrepareFailure::DidNotRun(UpdateError::reason_only(
            e.to_string(),
        )));
    }

    // try/finally: the body runs, then we always unlock.
    let result = prepare_body(
        targets,
        &registry,
        report,
        selected,
        operation,
        &pkgs,
        installed_only,
        reboot,
    )
    .await;
    // Exactly the set locked above: releasing a host we never locked would
    // report a failure for a lock nobody took.
    let unlocked = match selected {
        Some(hosts) => targets.unlock_selected(hosts).await,
        None => targets.unlock().await,
    };
    warn_on_unlock_failures("prepare", &unlocked);
    result
}

/// The locked body of [`perform_prepare`], factored out so the caller's
/// `unlock()` runs unconditionally.
///
/// `selected` scopes every fan-out and every by-host scan below; `None` is the
/// whole group. Everything past the repo fan-out is driven by `lists`, so
/// narrowing that one map carries the scope through `narrow_to_composed`,
/// `narrow_to_installed` and `build_prepare_map`. `reboot` is the exception:
/// it arrives already scoped from [`build_reboot_map`], because the retain
/// filter below warns a host by name *before* `dispatched` (⊆ `lists`) gets to
/// drop it.
#[allow(clippy::too_many_arguments)]
async fn prepare_body(
    targets: &mut HostsGroup,
    registry: &WorkflowRegistry,
    report: &dyn SetRepo,
    selected: Option<&BTreeSet<String>>,
    operation: RepoOp,
    pkgs: &[String],
    installed_only: bool,
    reboot: BTreeMap<String, String>,
) -> Result<(), PrepareFailure> {
    let in_scope = |host: &str| selected.is_none_or(|s| s.contains(host));

    match selected {
        Some(hosts) => targets.fanout_set_repo_for(hosts, operation, report).await,
        None => targets.fanout_set_repo(operation, report).await,
    }

    // The issue repo could not be set on some host, so prepare never ran.
    // Post-filtered: the scan only reads `last*`, and out of scope that is some
    // earlier phase's record.
    let repo_failures: Vec<UpdateError> =
        host_command_failures(targets, "failed to set issue repo")
            .into_iter()
            .filter(|e| e.host.as_ref().is_none_or(|h| in_scope(h)))
            .collect();
    if !repo_failures.is_empty() {
        for target in targets.targets() {
            if in_scope(target.hostname()) && !target.lasterr().is_empty() {
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

    // Hosts a prepare command reached, and hosts a package failed on. Both are
    // accumulated *inside* the loop below, which runs one fan-out per package:
    // a single post-loop read of `lastexit()` sees only the last package, and
    // under `--installed-only` that is very often a no-op `if rpm -q …` exiting
    // 0, masking an earlier failure and letting the host reboot into it.
    let mut dispatched: BTreeSet<String> = BTreeSet::new();
    let mut inert: BTreeSet<String> = BTreeSet::new();
    // The check verdicts, per fan-out for the same reason: the checks read
    // the `last*` snapshot too. `note_dispatch` closes the exit-code half of
    // that hole; this closes the marker half, or an exit-`0` lock message on
    // package 1 of 2 is overwritten by package 2's clean run and the host
    // reboots into it (#406). `check_failed` keeps it to one verdict per host —
    // a second entry would push `aggregate_failures` out of its single-failure
    // verbatim branch, where `host` is `Some`.
    let mut check_failed: BTreeSet<String> = BTreeSet::new();
    let mut check_failures: Vec<UpdateError> = Vec::new();

    // An empty list is not a host failure, but it must never be a silent
    // success either — only the issue repositories were touched (#396). Above
    // the branch so the `installed_only` path (zero iterations) warns too; the
    // operator-facing refusal lives in the `prepare`/`update` command
    // pre-flights, this covers embedded callers.
    if pkgs.is_empty() {
        warn!("no packages to prepare");
    }

    // One entry per enabled host; `--installed` narrows a host's own entry.
    // One `transactional-update` call is one snapshot, and N calls between
    // reboots leave all but the last inactive (#501).
    let mut lists: BTreeMap<String, Vec<String>> = targets
        .targets()
        .filter(|t| t.state() == mtui_types::TargetState::Enabled && in_scope(t.hostname()))
        .map(|t| (t.hostname().to_owned(), pkgs.to_vec()))
        .collect();

    // Hosts we deliberately did not dispatch to. Each already has its own
    // verdict, so neither the failure scan nor the "no command could be built"
    // rule below may judge them a second time, and their failures are collected
    // apart from that scan.
    let mut accounted: BTreeSet<String> = BTreeSet::new();
    let mut accounted_failures: Vec<UpdateError> = Vec::new();
    // Kept apart from `accounted`, which also holds the `--installed` probe's
    // dead and skipped hosts: only these have no package baseline, and only
    // they are excluded from an embedded `update`.
    let mut uncomposed: BTreeMap<String, String> = BTreeMap::new();

    // Before the `--installed` probe, so a host the update composes nothing
    // for is never probed either. Skipped on an empty list for the same reason
    // the probe is: every host would then compose "none of" it, and an empty
    // list is not a host failure (the warn above owns that case).
    if !pkgs.is_empty() {
        for (host, products) in narrow_to_composed(targets, report, &mut lists) {
            // Named refusal, not a fallback to the full list: the full list is
            // exactly the `zypper 104` this narrowing exists to prevent, and a
            // silent skip would let the group's success speak for this host (#396).
            accounted_failures.push(UpdateError::new(
                format!(
                    "this host's products ({products}) compose none of the requested packages \
                     ({requested}); nothing was installed",
                    requested = pkgs.join(", ")
                ),
                host.clone(),
            ));
            accounted.insert(host.clone());
            uncomposed.insert(host, products);
        }
    }

    // Both checkpoints fall through — never an early return past
    // `reboot_transactional`.
    let mut cancelled = false;
    if installed_only && !pkgs.is_empty() {
        if targets.cancel_requested() {
            cancelled = true;
        } else {
            let outcome = narrow_to_installed(targets, registry, &mut lists).await;
            for host in &outcome.dead {
                accounted.insert(host.clone());
                accounted_failures.push(UpdateError::new("package probe failed", host.clone()));
            }
            for host in &outcome.skipped {
                accounted.insert(host.clone());
                info!(
                    host = %host,
                    "none of the requested packages is installed; nothing to prepare"
                );
            }
            // The probe is a fan-out and is never gated on the token; the
            // checkpoint sits after it, before the dispatch it protects.
            if targets.cancel_requested() {
                cancelled = true;
            }
        }
    }

    if !cancelled {
        let cmd = build_prepare_map(targets, registry, &lists);
        if !cmd.is_empty() {
            targets.run(Command::PerHost(cmd.clone())).await;
        }
        note_dispatch(targets, &cmd, &mut dispatched, &mut inert);
        note_check(
            targets,
            registry,
            &cmd,
            &mut check_failed,
            &mut check_failures,
        );
    }

    // A prepare installs packages, so it owes its own history row — and it is
    // what closes #407 for `update`, whose post-prepare aborts correctly write
    // no `update` row while this prepare's packages stay on every host. Placed
    // after the dispatch (no row may claim work that never started) and before
    // `reboot_transactional` (a host that does not come back can no longer be
    // written to), so on a transactional host it records what was *staged*
    // rather than leaving those packages unrecorded entirely.
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
    // * **The host's own list, not the requested set.** Under `--installed`
    //   two hosts of one group receive different lists, and a row naming a
    //   package that host never installed is the same over-claim.
    //
    // An empty list, a cancel before the dispatch, or a prepare for which no
    // command could be built for any host therefore leaves no row at all.
    //
    // Grouped by list: one fan-out per distinct list, so up to one per host
    // when every host narrowed differently.
    let mut by_list: BTreeMap<Vec<String>, BTreeSet<String>> = BTreeMap::new();
    for host in &dispatched {
        if let Some(list) = lists.get(host) {
            by_list
                .entry(list.clone())
                .or_default()
                .insert(host.clone());
        }
    }
    for (list, hosts) in by_list {
        add_op_history_for(targets, &hosts, "prepare", None, &list).await;
    }

    // Surface any per-host command failure from the install fan-out; the
    // prepare check's own failures were collected with it above.
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
    //
    // An `accounted` host is dropped for a second, distinct reason: no install
    // command was built for it, so its last snapshot is the `--installed`
    // probe's. Scoring that as a prepare invents a verdict — a dead probe would
    // be named twice (the coarser reason on top of "package probe failed"), and
    // a skipped one named at all, since `rpm -qa` warns on stderr while exiting
    // `0` and this scan's stderr half fires on that. Same rule `note_dispatch`
    // and `note_check` impose by scoping to the fan-out's own command map.
    //
    // An out-of-scope host is dropped for a third: it was in no fan-out of this
    // prepare at all, so its `last*` is some earlier phase's — an rpm warning
    // from `package_check` is enough to fail a prepare it was never part of.
    let mut failures: Vec<UpdateError> = host_command_failures(targets, "prepare command failed")
        .into_iter()
        .filter(|e| {
            e.host.as_ref().is_none_or(|h| in_scope(h))
                && !e
                    .host
                    .as_ref()
                    .is_some_and(|h| check_failed.contains(h) || accounted.contains(h))
        })
        .collect();
    failures.extend(accounted_failures);

    // A host whose prepare failed must not reboot into the failed transaction,
    // while a healthy peer still must. The skip set is built from the check
    // verdicts and non-zero exit codes only — never from the stderr rule
    // `host_command_failures` also applies, because `transactional-update`
    // writes progress to stderr on a *successful* run and skipping that host's
    // reboot would leave a healthy snapshot silently inert. The prepare
    // templates are single commands, so the exit code is the prepare's own.
    //
    // The check verdicts are the other half of the gate, and on
    // ("slmicro", true) they are what makes it complete: a prepare that
    // reported a locked update stack, a dependency prompt or an RPM error and
    // still exited `0` is invisible to the exit-code rule above, and its
    // reboot would activate the failed transaction. A marker-failed prepare
    // therefore skips its reboot, while a host whose only stderr is progress
    // still gets one (#406).
    inert.extend(check_failed.iter().cloned());
    failures.extend(check_failures);

    // A host the fan-out never reached must fail by name, not ride the
    // group's success (#396): `build_prepare_map` drops a host whose release
    // key does not resolve or whose template does not render, and nothing else
    // records that. Skipped when the flow was cancelled before the dispatch
    // (nothing was expected to dispatch) and when the list was empty (the warn
    // above owns that case); `accounted` excuses the hosts that already carry
    // their own verdict, which is what keeps this from being a second entry for
    // one host.
    if !pkgs.is_empty() && !cancelled {
        for target in targets.targets() {
            if target.state() != mtui_types::TargetState::Enabled || !in_scope(target.hostname()) {
                continue;
            }
            let host = target.hostname();
            if !dispatched.contains(host) && !accounted.contains(host) {
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
            // Nothing staged, nothing to activate — and in the `update`
            // rollback path a reboot would activate whatever the failed update
            // left staged.
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
    // A genuine host failure outranks the cancellation, which would otherwise
    // bury a broken host the operator must still see.
    if !failures.is_empty() {
        return aggregate_failures("prepare", failures)
            .map_err(|error| PrepareFailure::HostReported { error, uncomposed });
    }
    if cancelled {
        return Err(PrepareFailure::Cancelled(UpdateError::cancelled(
            "prepare cancelled before any package was installed",
        )));
    }
    aggregate_failures("prepare", failures)
        .map_err(|error| PrepareFailure::HostReported { error, uncomposed })
}

/// Records which hosts a prepare fan-out reached, and which of them it failed
/// on, into the two sets the reboot gate consults.
///
/// Called after *every* fan-out, not once at the end: the per-package
/// `--installed-only` loop runs one fan-out per package and `lastexit()` keeps
/// only the last. Scoped to `cmd`'s keys so a host outside this fan-out is
/// never judged on another phase's record.
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
/// The companion to [`note_dispatch`], for the same reason:
/// [`run_checks_where`] reads the `last*` snapshot, so a single post-loop call
/// would judge only the last package — under `--installed-only` very often a
/// clean no-op, masking an exit-`0` lock message on an earlier one and letting
/// the host reboot into it (#406). First-failure-wins per host keeps
/// `aggregate_failures` in its single-failure verbatim branch, where `host`
/// survives.
///
/// Scoped to `cmd`'s keys through [`run_checks_where`], so an out-of-fan-out
/// host is never *judged* on an earlier phase's record rather than judged and
/// then filtered after its ERROR breadcrumb has fired. A host
/// [`build_prepare_map`] dropped is therefore never check-judged, but it is not
/// excused: `prepare_body`'s dispatch accounting fails it by name with "no
/// prepare command could be built for this host" (#396).
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

/// Builds the per-host prepare command map from each host's own list.
///
/// A host absent from `lists`, or carrying an empty one, gets no command: an
/// empty list would render an argument-less install. The caller owns that
/// host's verdict (see [`prepare_body`]'s `accounted`); the ERROR logs here stay
/// for the two cases the caller cannot see — an unresolved release key and a
/// template that does not render (#396).
fn build_prepare_map(
    targets: &HostsGroup,
    registry: &WorkflowRegistry,
    lists: &BTreeMap<String, Vec<String>>,
) -> BTreeMap<String, String> {
    let mut map = BTreeMap::new();
    for target in targets.targets() {
        let Some(list) = lists.get(target.hostname()) else {
            continue;
        };
        if list.is_empty() {
            continue;
        }
        let Some((release, transactional)) = host_key(target) else {
            error!(host = %target.hostname(), "prepare: host release key unresolved; no command built");
            continue;
        };
        let Some(doer) = resolve_doer(registry, Role::Prepare, &release, transactional) else {
            continue;
        };
        // Quoting lives here alone, so no entry can reach the template
        // unquoted.
        let joined = quote_args(list);
        let vars: HashMap<&str, &str> = [("package", joined.as_str())].into_iter().collect();
        if let Ok(cmd) = doer.render_command(&vars) {
            map.insert(target.hostname().to_owned(), cmd);
        } else {
            // `prepare_body`'s dispatch accounting turns the drop into a named
            // failure; this log says why (#396).
            error!(
                host = %target.hostname(), release = %release,
                "prepare: no command rendered for this host; nothing will be installed on it"
            );
        }
    }
    map
}

/// A host's flattened products as one stable `name-version.arch, ...` string.
fn fmt_products(products: &BTreeSet<mtui_types::SystemProduct>) -> String {
    products
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

/// Narrows each host's list in `lists` to the packages that host's own products
/// compose (base plus addons), returning `host -> its products` for the hosts
/// left with nothing, for the caller to fail by name.
///
/// A composition that names none of the host's products keeps the full list and
/// warns: narrowing on an index that does not describe the host would drop
/// every package, and keeping it silently would be indistinguishable from
/// today.
fn narrow_to_composed(
    targets: &HostsGroup,
    report: &dyn SetRepo,
    lists: &mut BTreeMap<String, Vec<String>>,
) -> BTreeMap<String, String> {
    let mut refused = BTreeMap::new();
    // The `!known` fallback below would keep the full list anyway; this returns
    // early for the *log*, so a report that simply carries no composition does
    // not warn once per host and drown the case that matters.
    let Some(composed) = report.composition().filter(|c| !c.is_empty()) else {
        return refused;
    };

    for target in targets.targets() {
        let host = target.hostname();
        let Some(list) = lists.get_mut(host) else {
            continue;
        };
        let products = target.system().flatten();
        let mut known = false;
        let mut composes: BTreeSet<&String> = BTreeSet::new();
        for product in &products {
            if let Some(names) = composed.get(product) {
                known = true;
                composes.extend(names);
            }
        }
        if !known {
            warn!(
                host = %host,
                products = %fmt_products(&products),
                "no product of this host is named in the update's composition; preparing the \
                 full package list"
            );
            continue;
        }

        let dropped: Vec<String> = list
            .iter()
            .filter(|p| !composes.contains(p))
            .cloned()
            .collect();
        list.retain(|p| composes.contains(p));
        if !dropped.is_empty() {
            // INFO, not DEBUG: a package installable only through a capability
            // is dropped here too, and this line is the only thing that makes
            // that visible.
            info!(
                host = %host, dropped = %dropped.join(", "),
                "these packages are not composed for this host's products; not installing them"
            );
        }
        if list.is_empty() {
            lists.remove(host);
            refused.insert(host.to_owned(), fmt_products(&products));
        }
    }
    refused
}

/// The hosts [`narrow_to_installed`] did not hand on to the install, split by
/// why. Both are accounted for, so neither may be judged on the probe's
/// snapshot.
struct ProbeOutcome {
    /// Probe exited non-zero — the `-1` never-ran sentinel included.
    dead: BTreeSet<String>,
    /// Probe answered `0` and the host carries none of the list. A skip, not a
    /// failure.
    skipped: BTreeSet<String>,
}

/// Narrows each host's list in `lists` to the packages that host already
/// carries, dropping the hosts left with nothing.
///
/// Unlike `downgrade_body`'s version probe there is no all-dead shortcut and no
/// history row: prepare has dispatched nothing at this point, so the caller's
/// ordinary fall-through already names every dead host and writes no row.
async fn narrow_to_installed(
    targets: &mut HostsGroup,
    registry: &WorkflowRegistry,
    lists: &mut BTreeMap<String, Vec<String>>,
) -> ProbeOutcome {
    let mut outcome = ProbeOutcome {
        dead: BTreeSet::new(),
        skipped: BTreeSet::new(),
    };
    let mut probe_map = BTreeMap::new();
    for target in targets.targets() {
        if !lists.contains_key(target.hostname()) {
            continue;
        }
        let Some((release, transactional)) = host_key(target) else {
            continue;
        };
        let Some(doer) = resolve_doer(registry, Role::Prepare, &release, transactional) else {
            continue;
        };
        if let Ok(Some(cmd)) = doer.render_list_command(&HashMap::new()) {
            probe_map.insert(target.hostname().to_owned(), cmd);
        }
    }
    if probe_map.is_empty() {
        return outcome;
    }
    targets.run(Command::PerHost(probe_map.clone())).await;

    for hostname in probe_map.keys() {
        let Some(target) = targets.get(hostname) else {
            continue;
        };
        // Non-zero is the whole signal, and `rpm -qa` is what makes it
        // trustworthy: it exits `0` on success, so non-zero is the probe
        // failing rather than "this host carries none of them" (#451). The
        // `-1` never-ran sentinel is one of its values, not the predicate.
        if target.lastexit().is_some_and(|c| c != 0) {
            error!(
                host = %hostname,
                exit = ?target.lastexit(),
                "installed-package probe failed; this host was not prepared"
            );
            outcome.dead.insert(hostname.clone());
            lists.remove(hostname);
            continue;
        }
        // Exact line equality: `pkg-a` must not match `pkg-a-devel`.
        let installed: std::collections::HashSet<&str> =
            target.lastout().lines().map(str::trim_end).collect();
        let Some(list) = lists.get_mut(hostname) else {
            continue;
        };
        list.retain(|p| installed.contains(p.as_str()));
        if list.is_empty() {
            lists.remove(hostname);
            outcome.skipped.insert(hostname.clone());
        }
    }
    outcome
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
    // Return before locking or touching repos. Also keeps the probe template
    // from rendering with no package names, where `zypper se` would list the
    // entire repository catalog.
    if packages.is_empty() {
        warn!("no packages to downgrade");
        return Ok(());
    }

    let registry = WorkflowRegistry::default();

    // Resolve reboot before locking so a missing downgrader early-returns
    // without leaving the group locked.
    // Downgrade has no scoped variant; the whole group is the scope.
    let Ok(reboot) = build_reboot_map(targets, &registry, Role::Downgrade, None) else {
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

    // Per-host failures (repo removal + per-package/combined checks) are
    // aggregated at the end, so a downgrade failure surfaces rather than only
    // being logged.
    let mut failures = host_command_failures(targets, "failed to remove issue repo");

    // Discover each host's available downgrade versions, keeping the highest
    // per package.
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

    // A dead probe must abort that host's downgrade, not degrade it: its stdout
    // carries no versions, so the flow would "complete" having run zero
    // downgrade commands and left every package at the update version.
    //
    // Non-zero is the whole signal, and the guarded template is what makes it
    // trustworthy in both directions (#451): it guards the commands that
    // *produce* the list and exits with the failed tool's own status, so a
    // non-zero status is SSH-level death (the `-1` sentinel) or zypper's/awk's
    // own — never "package not found", which the guard accepts as `104` and
    // reports as `0`. And `0` genuinely means the probe answered. (As one
    // pipeline the status was awk's, which succeeds on empty input, so a failed
    // `zypper se` recorded `0`.)
    //
    // Handled per host: this downgrade is often the repair for an update that
    // already failed on the group, so aborting over one host's broken zypper
    // would strand the healthy peers half-applied. They still roll back (and
    // reboot); the dead ones are raised at the end. All probes dead aborts now.
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
        // The abort still leaves side effects: `fanout_set_repo(Remove)` above
        // has already stripped the issue repo from every host, and this path
        // fires most often during the update rollback, where reconstructing
        // what was done to a refhost matters most. Record the row before
        // returning — the site below is unreachable from here, so there is no
        // double write.
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
        // `break`, not an early return: the combined transactional block and
        // the failure aggregation after this loop must still run.
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
            // Only the hosts that ran this command: a host outside `cmd` (e.g.
            // a dead-probe one) still carries a stale `-1` that would trip the
            // timeout gate and cancel the healthy hosts' rollback. Scoped
            // through `run_checks_where` so it is not judged at all — a
            // post-filter would still emit its breadcrumb, once per package.
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
        // Same scoping as the per-package loop, and for the same reason.
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
        // The transactional check only gates "timed out or failed to run", but
        // the downgrade template is a single command, so a non-zero exit is the
        // downgrade's own status and the host must not reboot into the failed
        // transaction. Pushed as a failure too — a skipped reboot behind an
        // `Ok` would be a quiet no-op — unless the check already named this
        // host (`insert` is `false` for the `-1` case it covers). Scoped to
        // `combined`, since hosts outside it carry a stale record.
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

    // Reboot the healthy transactional hosts first — their staged snapshots
    // must still activate — then surface the dead probes as the command's
    // failure. A host whose combined downgrade failed is skipped too:
    // rebooting it would activate the failed transaction's snapshot.
    let healthy_reboot: BTreeMap<String, String> = reboot
        .into_iter()
        .filter(|(h, _)| {
            // Nothing staged. A transactional host drops out of `combined`
            // when the probe resolved no versions for it, which an exit-0 probe
            // can do, so `dead_probes` does not cover it. When this downgrade
            // *is* the `update` rollback, rebooting would activate the snapshot
            // the failed update left staged.
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
    // Recorded after the run started but *before* the reboot: a transactional
    // host that never comes back cannot be written to afterwards, and that is
    // the host whose state an operator most needs to reconstruct.
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

    // Then the dead probes, now that the healthy hosts have rolled back.
    if !dead_probes.is_empty() {
        return Err(UpdateError::new(
            "package version probe failed",
            dead_probes.iter().cloned().collect::<Vec<_>>().join(", "),
        ));
    }

    // `downgrade_verdict` has already logged the per-host detail; fail so a
    // caller cannot mistake a half-rollback for success.
    if !not_downgraded.is_empty() {
        return Err(UpdateError::reason_only("downgrade not completed"));
    }

    // Cancellation last: a real verdict above outranks it. The message names
    // only the non-transactional per-package progress (transactional hosts go
    // through the combined block) and notes the repo removal, which ran before
    // the loop and so applies to every host.
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
/// Re-queries each host, rotates `before = after; after = current` per package,
/// then names every package still `current >= required` at ERROR — with no
/// short-circuit, so the bookkeeping advances for the ones that did move. New
/// packages (no released version to go back to) and multiversion packages (e.g.
/// the kernel) always appear here; re-running `downgrade` will not clear them.
///
/// Returns the `hostname -> ["name (at <current>, update ships <required>)", …]`
/// map, empty on a completed rollback, in sorted hostname order.
///
/// The all-clear is withheld while `probe_dead` is non-empty: this verdict
/// flags a package only when the report carries a `required` version, so on a
/// standalone `downgrade` an unmeasured host yields the same empty map a
/// completed rollback does, and logging `done` over it would restate #451's
/// silent success one layer up. Those hosts are named at WARN instead.
async fn downgrade_verdict(
    targets: &mut HostsGroup,
    probe_dead: &BTreeSet<String>,
) -> BTreeMap<String, Vec<String>> {
    targets.query_versions().await;

    let mut not_downgraded: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for target in targets.targets_mut() {
        let hostname = target.hostname().to_owned();
        for pkg in target.packages_mut() {
            // Rotate the whole check, not just its version: a never-checked
            // slot must not land in the next one as "checked, not installed",
            // or a standalone downgrade exports "is not installed" for a slot
            // nobody looked at (#396, and #437 for `current` -> `after`).
            pkg.set_before_check(pkg.after_check().clone());
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
        // Only when every host was measured: a dead probe leaves this map empty
        // for the same reason a completed rollback does.
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
/// real script's output through the real parser.
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
/// the `$repa` selector. `id` is the RRID recorded in the remote history line
/// once a command has dispatched (`None` for a direct call with no report).
/// `noprepare` skips the initial prepare; `newpackage` runs a testing prepare
/// after the update. `diagnostics` collects the update check's
/// recognised-but-non-fatal output sections for the command layer.
// Grouping these into a struct would only obscure the call site.
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

    let mut uncomposed: BTreeMap<String, String> = BTreeMap::new();
    if !noprepare {
        // Default flags (remove-repo prepare). A prepare that could not run
        // aborts rather than patching hosts on a broken premise; `--noprepare`
        // opts out. One that ran and only reported a host failure is too noisy
        // to gate on (see `PrepareFailure`), so it warns and the update goes on.
        match perform_prepare_classified(targets, report, None, packages, false, false, false).await
        {
            Ok(()) => {}
            Err(PrepareFailure::DidNotRun(e)) => return Err(UpdateFailure::Prepare(e)),
            Err(PrepareFailure::Cancelled(e)) => return Err(UpdateFailure::Cancelled(e)),
            Err(PrepareFailure::HostReported {
                error,
                uncomposed: refused,
            }) => {
                warn!(error = %error, "prepare before update reported a host failure; continuing");
                uncomposed = refused;
            }
        }
    }

    // A refused host has no package baseline, so patching it would patch on the
    // premise the refusal denied. It is dropped from the fan-out below and
    // named in the verdict — a skip the group's success spoke for is #396
    // itself, one layer up.
    let excluded: Vec<UpdateError> = uncomposed
        .iter()
        .map(|(host, products)| {
            UpdateError::new(
                format!(
                    "this host's products ({products}) compose none of the update's packages; \
                     it was excluded from the update"
                ),
                host.clone(),
            )
        })
        .collect();
    if !uncomposed.is_empty()
        && targets
            .targets()
            .filter(|t| t.state() == mtui_types::TargetState::Enabled)
            .all(|t| uncomposed.contains_key(t.hostname()))
    {
        // Nothing left to patch, and nothing locked or added yet. `excluded` is
        // non-empty here, so this is always the `Err` arm.
        return aggregate_failures("update", excluded).map_err(UpdateFailure::Uncomposed);
    }

    // The set the rest of the flow operates on, fixed once here rather than
    // filtered again at each fan-out. Every step below is group-wide by
    // default, and leaving a refused host in one is a distinct defect per step:
    // `update_lock` fails closed on *any* member, so a refused host with a
    // contended or unreadable lock aborts the update for the peers that do
    // compose it; the repo fan-out reconfigures a host already reported as
    // excluded; and `build_update_maps` turns a refused host's missing updater
    // into the whole group's `MissingUpdater`. Filtering after the fact — as
    // the `commands`/`reboot` retain used to — is too late for all three.
    let eligible: BTreeSet<String> = targets
        .names()
        .into_iter()
        .filter(|host| !uncomposed.contains_key(host))
        .collect();

    targets.package_check(false).await;

    if let Err(e) = targets.update_lock_selected(&eligible).await {
        return Err(UpdateFailure::Check(UpdateError::reason_only(
            e.to_string(),
        )));
    }

    targets
        .fanout_set_repo_for(&eligible, RepoOp::Add, report)
        .await;

    let repa = repa_for(maintenance_id, review_id);
    let joined = quote_args(packages);
    let (commands, reboot) = match build_update_maps(targets, &registry, &repa, &joined, &eligible)
    {
        Ok(maps) => maps,
        Err(e) => {
            // Remove the repo we just added and abort. A hard failure rather
            // than a logged success, so it never reports "finished".
            targets
                .fanout_set_repo_for(&eligible, RepoOp::Remove, report)
                .await;
            warn_on_unlock_failures("update", &targets.unlock_selected(&eligible).await);
            return Err(UpdateFailure::MissingUpdater(e));
        }
    };

    // Last checkpoint before the point of no return: past this line a cancel
    // could leave a half-applied transaction, so cancellation is NOT checked
    // again inside or after the run phase — the flow finishes its bookkeeping
    // instead. Fan-outs are never interrupted part-way, so the cleanup below
    // genuinely undoes the repo add and the lock.
    if targets.cancel_requested() {
        tracing::info!("cancelled: stopping before the update command was dispatched");
        targets
            .fanout_set_repo_for(&eligible, RepoOp::Remove, report)
            .await;
        warn_on_unlock_failures("update", &targets.unlock_selected(&eligible).await);
        // With a prepare behind us the host is not untouched: the repo add is
        // undone, a completed prepare's packages are not.
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
    // repo cleanup only on success. The history row is written inside
    // `update_run_phase`, between the fan-out and the reboot.
    let update_result = update_run_phase(
        targets,
        &registry,
        &eligible,
        commands,
        reboot,
        diagnostics,
        id,
        packages,
    )
    .await;

    if let Err(e) = update_result {
        warn!(
            "update did not complete; leaving the test update repositories in place \
             for retry/diagnosis (remove later with `set_repo --remove`)"
        );
        return Err(e);
    }

    // Scoped like every other step: this one *adds* the test repo, and
    // `remove_test_repos` below only removes it from `eligible`, so a
    // group-wide add here strands it on the excluded host (#409). It also
    // composes nothing for that host, so there is nothing to prepare on it.
    if newpackage
        && let Err(e) =
            perform_prepare_for(targets, &eligible, report, packages, false, true, false).await
    {
        warn!(error = %e, "newpackage prepare after update failed");
    }

    targets.package_check(true).await;

    remove_test_repos(targets, &eligible, report).await;
    aggregate_failures("update", excluded).map_err(UpdateFailure::Uncomposed)
}

/// Removes the test update repositories after a successful update.
///
/// Scoped to `eligible`, the hosts the update added a repo to: a host it
/// excluded never received one, and locking it here would let its contention
/// strand the repos on every peer (#409) over a cleanup it does not need.
///
/// Best-effort: a lock failure here does not turn a successful update into a
/// failed one, so it warns — naming the error, that the repos are left
/// configured, and the manual remedy.
async fn remove_test_repos(
    targets: &mut HostsGroup,
    eligible: &BTreeSet<String>,
    report: &dyn SetRepo,
) {
    if let Err(e) = targets.update_lock_selected(eligible).await {
        warn!(
            error = %e,
            "could not lock hosts to remove the test update repositories; \
             they are left configured on every host (remove later with \
             `set_repo --remove`)"
        );
        return;
    }
    targets
        .fanout_set_repo_for(eligible, RepoOp::Remove, report)
        .await;
    // The lock succeeded but the removal command may still have failed on a
    // host — #409's complaint (a stale test repo) can happen silently here too,
    // not only on a lock failure. The noisy stderr rule is fine for a warn.
    //
    // Post-filtered rather than scoped inside, unlike `run_checks_where`: this
    // scan only reads `last*` and logs nothing on the way, so an excluded host
    // dropped here has not already had a verdict printed for it.
    let failures: Vec<UpdateError> =
        host_command_failures(targets, "failed to remove the test update repo")
            .into_iter()
            .filter(|e| e.host.as_ref().is_some_and(|h| eligible.contains(h)))
            .collect();
    if !failures.is_empty() {
        let hosts: Vec<String> = failures.iter().filter_map(|e| e.host.clone()).collect();
        warn!(
            hosts = %hosts.join(", "),
            "failed to remove the test update repo on one or more hosts; \
             remove it manually with `set_repo --remove`"
        );
    }
    warn_on_unlock_failures("update", &targets.unlock_selected(eligible).await);
}

/// Runs the update commands, checks the hosts they reached (collecting
/// failures), reboots on success, and **always** unlocks.
///
/// `commands` and `reboot` are the whole scope: a host the caller excluded is
/// absent from both and is neither judged, rebooted, nor counted a success.
/// `locked` is the set the caller took the operation lock on, which the final
/// unlock must match exactly — releasing a host we never locked would report a
/// stranded lock against a host that has none.
///
/// Returns `Ok(())` when every check passed and every transactional host's
/// reboot took effect — reconnecting is not sufficient, since a host can answer
/// without ever having gone down.
///
/// Otherwise `Err` with the aggregated failure, which **suppresses the reboot
/// entirely** on a check failure. The [`UpdateFailure`] variant is what routes
/// the caller's rollback; a single failure is returned verbatim, more than one
/// is summarised into `"update failed on {hosts} ({detail})"`.
#[allow(clippy::too_many_arguments)]
async fn update_run_phase(
    targets: &mut HostsGroup,
    registry: &WorkflowRegistry,
    locked: &BTreeSet<String>,
    commands: BTreeMap<String, String>,
    reboot: BTreeMap<String, String>,
    diagnostics: &mut Vec<Diagnostic>,
    id: Option<&str>,
    packages: &[String],
) -> Result<(), UpdateFailure> {
    // Scoped to this fan-out's own map, as `note_dispatch`/`note_check` scope
    // `prepare_body`'s: an absent host's last snapshot is another phase's.
    let dispatched: BTreeSet<String> = commands.keys().cloned().collect();
    targets.run(Command::PerHost(commands)).await;

    // Recorded after the run started but *before* the reboot: a transactional
    // host that never comes back cannot be written to afterwards, and that is
    // the host whose state an operator most needs to reconstruct.
    add_op_history_for(targets, &dispatched, "update", id, packages).await;

    let failures = run_checks_where(targets, registry, Role::Update, diagnostics, |h| {
        dispatched.contains(h)
    });
    let failed_hosts: std::collections::HashSet<String> =
        failures.iter().filter_map(|e| e.host.clone()).collect();
    let ok_hosts: Vec<String> = targets
        .names()
        .into_iter()
        .filter(|hn| dispatched.contains(hn) && !failed_hosts.contains(hn))
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
        // `-1` and a probe failure both veto the rollback
        // (`UpdateFailure::NotRun` / `ProbeFailed` carry the arguments). The
        // probe verdict is a typed flag rather than re-derived here; matching
        // the reason string would be the first place in the tree where control
        // flow read a message's text.
        //
        // The rollback question is one bit — "can a group-wide downgrade repair
        // *any* host that failed?" — so a run mixing a lost host with a genuine
        // check failure still rolls back, on behalf of the repairable host.
        // Only the label on the non-repairable runs is split further.
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
        // Route on *why* the reboot failed, not on the fact that it did (see
        // `Reboot` vs `RebootNotTaken`). `all`, not `any`: with one host
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

    warn_on_unlock_failures("update", &targets.unlock_selected(locked).await);
    result
}

/// Builds the per-host updater command map (with `$repa` + `$packages`) and the
/// transactional reboot map, over the hosts in `eligible`. Returns `Err` with
/// the offending host's [`UpdateError`] if one of them is missing an updater —
/// a hard failure in mtui.
///
/// A host outside `eligible` is not resolved at all, not resolved and then
/// dropped: the update has already excluded it, and a `MissingUpdater` raised
/// on its behalf would abort the peers it was excluded to spare.
fn build_update_maps(
    targets: &HostsGroup,
    registry: &WorkflowRegistry,
    repa: &str,
    packages: &str,
    eligible: &BTreeSet<String>,
) -> Result<UpdateMaps, UpdateError> {
    let mut commands = BTreeMap::new();
    let mut reboot = BTreeMap::new();
    for target in targets
        .targets()
        .filter(|t| eligible.contains(t.hostname()))
    {
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
    use mtui_hosts::{
        Check, CheckArgs, CheckFailure, Doer, HostsGroup, MockConnection, TARGET_LOCK_PATH, Target,
    };
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

    // --- prepare --installed (#501) ----------------------------------------

    /// The `--installed` probe, verbatim. Every test below arms on it appearing
    /// in `commands()`: a fixture that never answered it would prove nothing.
    const INSTALLED_PROBE: &str = "rpm -qa --qf '%{NAME}\\n'";

    /// An enabled transactional SL-Micro target answering the `--installed`
    /// probe with `stdout`/`exit`, and every other command cleanly.
    ///
    /// Scripted per command rather than through `with_default`, so the probe is
    /// attributable: a mock answering everything alike leaves nothing to
    /// attribute.
    fn slmicro_target_with_probe(
        hostname: &str,
        stdout: &str,
        exit: i16,
    ) -> (Target, MockConnection) {
        let conn = MockConnection::new(hostname)
            .with_default(CommandLog::new("", "", "", 0, 0))
            .with_response(
                INSTALLED_PROBE.to_owned(),
                CommandLog::new(INSTALLED_PROBE, stdout, "", exit, 0),
            )
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

    /// [`slmicro_target_with_probe`] whose probe exits `0` but also writes
    /// `stderr` — `rpm -qa` warns about an rpmdb it had to convert or rebuild
    /// and still succeeds.
    fn slmicro_target_with_noisy_probe(
        hostname: &str,
        stdout: &str,
        stderr: &str,
    ) -> (Target, MockConnection) {
        let conn = MockConnection::new(hostname)
            .with_default(CommandLog::new("", "", "", 0, 0))
            .with_response(
                INSTALLED_PROBE.to_owned(),
                CommandLog::new(INSTALLED_PROBE, stdout, stderr, 0, 0),
            )
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

    /// [`sles_target`]'s non-transactional zypper shape, answering the
    /// `--installed` probe with `stdout`.
    fn sles_target_with_probe(hostname: &str, stdout: &str) -> (Target, MockConnection) {
        let conn = MockConnection::new(hostname)
            .with_default(CommandLog::new("", "", "", 0, 0))
            .with_response(
                INSTALLED_PROBE.to_owned(),
                CommandLog::new(INSTALLED_PROBE, stdout, "", 0, 0),
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
        (t, handle)
    }

    /// The argument tokens after `-l` of the single command starting with
    /// `prefix`, as a set. A token set, not a substring guard: `contains`
    /// cannot tell "installs pkg-a" from "installs pkg-a and pkg-b".
    fn install_args(handle: &MockConnection, prefix: &str) -> BTreeSet<String> {
        let cmds = handle.commands();
        let matching: Vec<&String> = cmds.iter().filter(|c| c.starts_with(prefix)).collect();
        assert_eq!(matching.len(), 1, "expected one install command: {cmds:?}");
        matching[0]
            .split_whitespace()
            .skip_while(|t| *t != "-l")
            .skip(1)
            .map(str::to_owned)
            .collect()
    }

    #[tokio::test]
    async fn prepare_installed_only_runs_one_transaction_per_transactional_host() {
        // #501. Per-package `transactional-update pkg in` opens one snapshot
        // each, so every package but the last stays inactive after the reboot
        // while the flow reports success. One call, one snapshot.
        //
        // The host carries BOTH requested packages, so a per-package loop over
        // its own narrowed list would still dispatch twice — the narrowing is
        // the sibling test's subject, not this one's.
        let (t, handle) = slmicro_target_with_probe("h1", "pkg-a\npkg-b\nzsh\n", 0);
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
        assert!(
            res.is_ok(),
            "a clean --installed prepare returns Ok: {res:?}"
        );

        let cmds = handle.commands();
        assert!(
            cmds.iter().any(|c| c == INSTALLED_PROBE),
            "the probe must have run, or nothing below is attributable: {cmds:?}"
        );
        assert_eq!(
            cmds.iter()
                .filter(|c| c.starts_with("transactional-update -n pkg in -l"))
                .count(),
            1,
            "one transaction, not one per package: {cmds:?}"
        );
        assert!(
            !cmds.iter().any(|c| c.contains("if $(rpm -q")),
            "the conditional wrapper is gone: {cmds:?}"
        );
        let fired = handle.fired_commands();
        assert_eq!(
            fired
                .iter()
                .filter(|c| c.contains("systemctl reboot"))
                .count(),
            1,
            "one snapshot, one reboot: {fired:?}"
        );
    }

    #[tokio::test]
    async fn prepare_installed_only_installs_only_what_the_probe_listed() {
        let (t, handle) = slmicro_target_with_probe("h1", "pkg-a\nzsh\n", 0);
        let mut group = HostsGroup::new(vec![t], false);

        perform_prepare(
            &mut group,
            &NoopRepo,
            &["pkg-a".to_owned(), "pkg-b".to_owned()],
            false,
            false,
            true,
        )
        .await
        .expect("a clean --installed prepare returns Ok");

        assert!(
            handle.commands().iter().any(|c| c == INSTALLED_PROBE),
            "the probe must have run: {:?}",
            handle.commands()
        );
        assert_eq!(
            install_args(&handle, "transactional-update -n pkg in -l"),
            ["pkg-a".to_owned()].into_iter().collect::<BTreeSet<_>>(),
            "pkg-b is not installed on this host, so it must not be requested"
        );
    }

    #[tokio::test]
    async fn prepare_installed_only_matches_names_exactly() {
        // `pkg-a-devel` is not `pkg-a`. A containment match would install a
        // package the host does not carry — the sibling test above cannot
        // catch that, because its probe answers with a whole name.
        let (t, handle) = slmicro_target_with_probe("h1", "pkg-a-devel\n", 0);
        let mut group = HostsGroup::new(vec![t], false);

        let res = perform_prepare(
            &mut group,
            &NoopRepo,
            &["pkg-a".to_owned()],
            false,
            false,
            true,
        )
        .await;
        assert!(res.is_ok(), "a genuine skip is not a failure: {res:?}");

        let cmds = handle.commands();
        assert!(
            cmds.iter().any(|c| c == INSTALLED_PROBE),
            "the probe must have run: {cmds:?}"
        );
        assert!(
            !cmds.iter().any(|c| c.contains("pkg in")),
            "nothing may be installed: {cmds:?}"
        );
        let fired = handle.fired_commands();
        assert!(
            !fired.iter().any(|c| c.contains("systemctl reboot")),
            "nothing was staged, so nothing to activate: {fired:?}"
        );
    }

    #[tokio::test]
    async fn prepare_installed_only_skips_a_host_with_nothing_installed() {
        // A host carrying none of the list is a skip, not a "no prepare command
        // could be built" failure — the #396 rule must not judge it a second
        // time.
        let (t, handle) = slmicro_target_with_probe("h1", "zsh\n", 0);
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
        assert!(res.is_ok(), "a host with nothing to prepare is Ok: {res:?}");

        let cmds = handle.commands();
        assert!(
            cmds.iter().any(|c| c == INSTALLED_PROBE),
            "the probe must have run: {cmds:?}"
        );
        assert!(
            !cmds.iter().any(|c| c.contains("pkg in")),
            "nothing may be installed: {cmds:?}"
        );
        assert!(
            !handle
                .fired_commands()
                .iter()
                .any(|c| c.contains("systemctl reboot")),
            "nothing was staged: {:?}",
            handle.fired_commands()
        );
        assert!(
            handle.file_contents(HISTORY_LOG).is_none(),
            "a host nothing was installed on must have no prepare row"
        );
    }

    #[tokio::test]
    async fn prepare_installed_only_skips_a_host_whose_probe_warned_on_stderr() {
        // The skip must survive `host_command_failures`' stderr half. A skipped
        // host's last snapshot IS its probe, and `rpm -qa` warns on stderr while
        // exiting `0`, so a group-wide read scores that warning as the prepare of
        // a host no install command was ever built for.
        let (t, handle) = slmicro_target_with_noisy_probe(
            "h1",
            "zsh\n",
            "warning: Found bdb Packages database while attempting sqlite backend\n",
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
        assert!(
            res.is_ok(),
            "a warning on the probe's stderr is not a failed prepare: {res:?}"
        );

        let cmds = handle.commands();
        assert_eq!(
            cmds,
            vec![INSTALLED_PROBE.to_owned()],
            "the probe ran and nothing else did"
        );
    }

    #[tokio::test]
    async fn prepare_installed_only_fails_a_host_whose_probe_failed() {
        // #451: a probe that fails must fail the host, never degrade it. `7` is
        // a real non-zero that is not the `-1` sentinel, so a predicate
        // narrowed to the sentinel still lets this through.
        let (t, handle) = slmicro_target_with_probe("h1", "", 7);
        let mut group = HostsGroup::new(vec![t], false);

        let err = perform_prepare(
            &mut group,
            &NoopRepo,
            &["pkg-a".to_owned()],
            false,
            false,
            true,
        )
        .await
        .expect_err("a dead probe must not report success");

        let cmds = handle.commands();
        assert!(
            cmds.iter().any(|c| c == INSTALLED_PROBE),
            "the probe must have run: {cmds:?}"
        );
        // Exact: one failure, so `aggregate_failures` returns it verbatim and
        // `host` survives. A second entry would collapse it to `None`.
        assert_eq!(err.host.as_deref(), Some("h1"), "err: {err}");
        assert_eq!(err.reason, "package probe failed", "err: {err}");
        assert!(
            !cmds.iter().any(|c| c.contains("pkg in")),
            "nothing may be installed: {cmds:?}"
        );
        assert!(
            !handle
                .fired_commands()
                .iter()
                .any(|c| c.contains("systemctl reboot")),
            "nothing was staged: {:?}",
            handle.fired_commands()
        );
        assert!(
            handle.file_contents(HISTORY_LOG).is_none(),
            "a dead-probe host must have no prepare row"
        );
    }

    #[tokio::test]
    async fn prepare_installed_only_probe_never_ran_fails_by_name() {
        // The `-1` sentinel half. Kept separate from the test above precisely
        // because narrowing the predicate to `c == -1` leaves THIS one green,
        // while deleting the gate outright leaves that one green.
        let (t, handle) = slmicro_target_with_probe("h1", "", -1);
        let mut group = HostsGroup::new(vec![t], false);

        let err = perform_prepare(
            &mut group,
            &NoopRepo,
            &["pkg-a".to_owned()],
            false,
            false,
            true,
        )
        .await
        .expect_err("a probe that never ran must not report success");

        let cmds = handle.commands();
        assert!(
            cmds.iter().any(|c| c == INSTALLED_PROBE),
            "the probe must have run: {cmds:?}"
        );
        assert_eq!(err.host.as_deref(), Some("h1"), "err: {err}");
        assert_eq!(err.reason, "package probe failed", "err: {err}");
        assert!(
            !cmds.iter().any(|c| c.contains("pkg in")),
            "nothing may be installed: {cmds:?}"
        );
        assert!(
            !handle
                .fired_commands()
                .iter()
                .any(|c| c.contains("systemctl reboot")),
            "nothing was staged: {:?}",
            handle.fired_commands()
        );
        assert!(
            handle.file_contents(HISTORY_LOG).is_none(),
            "a dead-probe host must have no prepare row"
        );
    }

    #[tokio::test]
    async fn prepare_installed_only_on_zypper_installs_the_installed_subset_in_one_call() {
        // The narrowing is not transactional-only: `--installed` on a zypper
        // host also becomes one call over the subset, which is what `prepare`
        // without `-i` already did.
        let (t, handle) = sles_target_with_probe("h1", "pkg-a\nzsh\n");
        let mut group = HostsGroup::new(vec![t], false);

        perform_prepare(
            &mut group,
            &NoopRepo,
            &["pkg-a".to_owned(), "pkg-b".to_owned()],
            false,
            false,
            true,
        )
        .await
        .expect("a clean --installed prepare returns Ok");

        let cmds = handle.commands();
        assert!(
            cmds.iter().any(|c| c == INSTALLED_PROBE),
            "the probe must have run: {cmds:?}"
        );
        assert_eq!(
            install_args(&handle, "zypper -n in -y -l"),
            ["pkg-a".to_owned()].into_iter().collect::<BTreeSet<_>>(),
        );
        assert!(
            !cmds.iter().any(|c| c.contains("if $(rpm -q")),
            "the conditional wrapper is gone: {cmds:?}"
        );
        assert!(
            handle.fired_commands().is_empty(),
            "a non-transactional host fires no reboot: {:?}",
            handle.fired_commands()
        );
    }

    #[tokio::test]
    async fn prepare_installed_only_records_the_hosts_own_list_in_history() {
        // Two hosts of one group receive different lists, so a group-wide row
        // would name packages h1 never installed.
        let (t1, h1) = slmicro_target_with_probe("h1", "pkg-a\n", 0);
        let (t2, h2) = slmicro_target_with_probe("h2", "pkg-a\npkg-b\n", 0);
        let mut group = HostsGroup::new(vec![t1, t2], false);

        perform_prepare(
            &mut group,
            &NoopRepo,
            &["pkg-a".to_owned(), "pkg-b".to_owned()],
            false,
            false,
            true,
        )
        .await
        .expect("a clean --installed prepare returns Ok");

        for handle in [&h1, &h2] {
            assert!(
                handle.commands().iter().any(|c| c == INSTALLED_PROBE),
                "both probes must have run: {:?}",
                handle.commands()
            );
        }
        // Anchored on the tail: it pins the label *and* the list. A bare
        // `contains("prepare")` would also match a package named
        // `prepare-something`.
        for (handle, tail) in [(&h1, ":prepare:pkg-a\n"), (&h2, ":prepare:pkg-a pkg-b\n")] {
            let contents =
                String::from_utf8(handle.file_contents(HISTORY_LOG).expect("a prepare row"))
                    .unwrap();
            assert_eq!(contents.lines().count(), 1, "history: {contents:?}");
            assert!(contents.ends_with(tail), "history line: {contents:?}");
        }
    }

    // --- prepare's per-host composition (#500) -----------------------------

    /// The seven synthetic package names the composition tests narrow from.
    fn seven_packages() -> Vec<String> {
        [
            "pkg-a", "pkg-b", "pkg-c", "pkg-d", "pkg-e", "pkg-f", "pkg-g",
        ]
        .iter()
        .map(|s| (*s).to_owned())
        .collect()
    }

    fn names(v: &[&str]) -> BTreeSet<String> {
        v.iter().map(|s| (*s).to_owned()).collect()
    }

    /// A [`SetRepo`] whose `composition()` answers a scripted index and which
    /// records the [`RepoOp`]s its fan-out received, per host.
    #[derive(Default)]
    struct ComposingRepo {
        composed: HashMap<SystemProduct, BTreeSet<String>>,
        ops: std::sync::Mutex<Vec<(String, RepoOp)>>,
    }

    impl ComposingRepo {
        fn new(entries: Vec<(SystemProduct, BTreeSet<String>)>) -> Self {
            Self {
                composed: entries.into_iter().collect(),
                ops: std::sync::Mutex::default(),
            }
        }

        fn ops(&self) -> Vec<RepoOp> {
            self.ops.lock().unwrap().iter().map(|(_, op)| *op).collect()
        }

        /// The ops `host` alone received, in order.
        fn ops_for(&self, host: &str) -> Vec<RepoOp> {
            self.ops
                .lock()
                .unwrap()
                .iter()
                .filter(|(h, _)| h == host)
                .map(|(_, op)| *op)
                .collect()
        }
    }

    #[async_trait::async_trait]
    impl SetRepo for ComposingRepo {
        async fn set_repo(&self, target: &mut Target, operation: RepoOp) {
            self.ops
                .lock()
                .unwrap()
                .push((target.hostname().to_owned(), operation));
        }

        fn composition(&self) -> Option<&HashMap<SystemProduct, BTreeSet<String>>> {
            Some(&self.composed)
        }
    }

    /// The package tokens of the one prepare command this handle received.
    ///
    /// A token *set*, not a substring: `contains("pkg-a pkg-b")` discriminates
    /// only by accident of `get_package_list`'s sort. Anchored with
    /// `starts_with`, because the slmicro *updater* script also contains
    /// `pkg in`.
    fn prepared_packages(handle: &MockConnection) -> BTreeSet<String> {
        let cmds = handle.commands();
        let mut installs = cmds
            .iter()
            .filter(|c| c.starts_with("transactional-update -n pkg in -l"));
        let cmd = installs
            .next()
            .unwrap_or_else(|| panic!("no prepare command was dispatched: {cmds:?}"));
        assert!(
            installs.next().is_none(),
            "one transaction per host, not one per package: {cmds:?}"
        );
        cmd.split_whitespace()
            .skip_while(|t| *t != "-l")
            .skip(1)
            .map(ToOwned::to_owned)
            .collect()
    }

    /// An enabled transactional SL-Micro host carrying `product` and `addons`,
    /// answering every command cleanly.
    ///
    /// `with_default` answers every command identically, so this fixture cannot
    /// attribute a response to the command that drew it: a test that grows an
    /// `--installed` probe must switch to `with_response` on the exact
    /// `rpm -qa --qf` string.
    fn composed_host(
        hostname: &str,
        product: SystemProduct,
        addons: BTreeSet<SystemProduct>,
    ) -> (Target, MockConnection) {
        slmicro_target_on(hostname, product, addons, "", "", 0)
    }

    #[tokio::test]
    async fn perform_prepare_installs_only_the_packages_composed_for_the_host() {
        let (t, handle) = composed_host(
            "h1",
            SystemProduct::new("SL-Micro", "6.1", "x86_64"),
            BTreeSet::new(),
        );
        let mut group = HostsGroup::new(vec![t], false);
        let repo = ComposingRepo::new(vec![(
            SystemProduct::new("SL-Micro", "6.1", "x86_64"),
            names(&["pkg-a", "pkg-b"]),
        )]);

        perform_prepare(&mut group, &repo, &seven_packages(), false, false, false)
            .await
            .expect("a host that composes part of the list prepares that part");

        assert_eq!(prepared_packages(&handle), names(&["pkg-a", "pkg-b"]));
    }

    #[tokio::test]
    async fn perform_prepare_filters_per_host_and_arch() {
        // The two arches of one product ship different binary sets, so the
        // intersection is per host — hoisting it out of the loop would hand
        // both hosts whichever host was seen first.
        let (t1, h1) = composed_host(
            "h1",
            SystemProduct::new("SL-Micro", "6.1", "x86_64"),
            BTreeSet::new(),
        );
        let (t2, h2) = composed_host(
            "h2",
            SystemProduct::new("SL-Micro", "6.1", "aarch64"),
            BTreeSet::new(),
        );
        let mut group = HostsGroup::new(vec![t1, t2], false);
        let repo = ComposingRepo::new(vec![
            (
                SystemProduct::new("SL-Micro", "6.1", "x86_64"),
                names(&["pkg-a", "pkg-b"]),
            ),
            (
                SystemProduct::new("SL-Micro", "6.1", "aarch64"),
                names(&["pkg-b", "pkg-c"]),
            ),
        ]);

        perform_prepare(&mut group, &repo, &seven_packages(), false, false, false)
            .await
            .expect("both hosts compose part of the list");

        let (got1, got2) = (prepared_packages(&h1), prepared_packages(&h2));
        assert_eq!(got1, names(&["pkg-a", "pkg-b"]));
        assert_eq!(got2, names(&["pkg-b", "pkg-c"]));
        assert_ne!(got1, got2, "each host gets its own arch's set");
    }

    #[tokio::test]
    async fn perform_prepare_unions_an_addons_composition() {
        // SL-Micro-Extras is unusable as a *base* product (`System::get_release`
        // has no arm for it), but `System::flatten()` includes addons, so a
        // package composed only for the addon is still composed for this host.
        let (t, handle) = composed_host(
            "h1",
            SystemProduct::new("SL-Micro", "6.1", "x86_64"),
            [SystemProduct::new("SL-Micro-Extras", "6.1", "x86_64")]
                .into_iter()
                .collect(),
        );
        let mut group = HostsGroup::new(vec![t], false);
        let repo = ComposingRepo::new(vec![
            (
                SystemProduct::new("SL-Micro", "6.1", "x86_64"),
                names(&["pkg-a"]),
            ),
            (
                SystemProduct::new("SL-Micro-Extras", "6.1", "x86_64"),
                names(&["pkg-b"]),
            ),
        ]);

        perform_prepare(&mut group, &repo, &seven_packages(), false, false, false)
            .await
            .expect("the host composes two of the list");

        assert_eq!(prepared_packages(&handle), names(&["pkg-a", "pkg-b"]));
    }

    #[tokio::test]
    async fn perform_prepare_keeps_the_full_list_without_composition() {
        // A report whose metadata carries no `binaries` block must behave
        // exactly as before this narrowing existed.
        let (t, handle) = composed_host(
            "h1",
            SystemProduct::new("SL-Micro", "6.1", "x86_64"),
            BTreeSet::new(),
        );
        let mut group = HostsGroup::new(vec![t], false);

        let (res, logs) = capture_logs(perform_prepare(
            &mut group,
            &ComposingRepo::default(),
            &seven_packages(),
            false,
            false,
            false,
        ))
        .await;
        res.expect("no composition is not a reason to fail a host");

        assert_eq!(
            prepared_packages(&handle),
            seven_packages().into_iter().collect::<BTreeSet<_>>()
        );
        // Arms the negative below: a capture that recorded nothing would make
        // "the warning is absent" unfailable.
        assert!(
            logs.contains("transactional-update -n pkg in"),
            "nothing captured: {logs}"
        );
        // "There is no composition" is not "the composition does not describe
        // this host": warning per host on the former would drown the latter.
        assert!(
            !logs.contains("no product of this host is named"),
            "an absent composition must be silent: {logs}"
        );
    }

    #[tokio::test]
    async fn perform_prepare_with_an_empty_list_is_not_a_composition_refusal() {
        // Every host composes "none of" an empty list, so narrowing it would
        // turn the existing empty-list no-op into a failure on every host.
        let (t, handle) = composed_host(
            "h1",
            SystemProduct::new("SL-Micro", "6.1", "x86_64"),
            BTreeSet::new(),
        );
        let mut group = HostsGroup::new(vec![t], false);
        let repo = ComposingRepo::new(vec![(
            SystemProduct::new("SL-Micro", "6.1", "x86_64"),
            names(&["pkg-a"]),
        )]);

        perform_prepare(&mut group, &repo, &[], false, false, false)
            .await
            .expect("an empty prepare is a no-op Ok, composition or not");

        assert!(
            !handle
                .commands()
                .iter()
                .any(|c| c.starts_with("transactional-update -n pkg in")),
            "nothing to install: {:?}",
            handle.commands()
        );
    }

    #[tokio::test]
    async fn perform_prepare_warns_when_no_host_product_keys_into_the_composition() {
        // The composition describes 6.0; the host is 6.1. Narrowing on an index
        // that does not describe this host would drop everything, so it keeps
        // the full list — but silently keeping it makes the whole fix
        // indistinguishable from today's failure, hence the warning.
        let (t, handle) = composed_host(
            "h1",
            SystemProduct::new("SL-Micro", "6.1", "x86_64"),
            BTreeSet::new(),
        );
        let mut group = HostsGroup::new(vec![t], false);
        let repo = ComposingRepo::new(vec![(
            SystemProduct::new("SL-Micro", "6.0", "x86_64"),
            names(&["pkg-a"]),
        )]);

        let (res, logs) = capture_logs(perform_prepare(
            &mut group,
            &repo,
            &seven_packages(),
            false,
            false,
            false,
        ))
        .await;
        res.expect("an index that does not describe the host is not a host failure");

        assert_eq!(
            prepared_packages(&handle),
            seven_packages().into_iter().collect::<BTreeSet<_>>()
        );
        let line = logs
            .lines()
            .find(|l| l.contains("no product of this host is named in the update's composition"))
            .unwrap_or_else(|| panic!("no fallback warning captured: {logs}"));
        assert!(line.contains("h1"), "names the host: {line}");
        assert!(line.contains("SL-Micro-6.1.x86_64"), "names it: {line}");
    }

    #[tokio::test]
    async fn perform_prepare_refuses_a_host_whose_products_compose_none_of_the_list() {
        // The refusal, not a fallback to the full list: the full list is
        // exactly the `zypper 104` this narrowing exists to prevent.
        let (t, handle) = composed_host(
            "h1",
            SystemProduct::new("SL-Micro", "6.1", "x86_64"),
            BTreeSet::new(),
        );
        let mut group = HostsGroup::new(vec![t], false);
        let repo = ComposingRepo::new(vec![(
            SystemProduct::new("SL-Micro", "6.1", "x86_64"),
            names(&["pkg-y", "pkg-z"]),
        )]);

        let err = perform_prepare(&mut group, &repo, &seven_packages(), false, false, false)
            .await
            .expect_err("a host that composes nothing must be named, not skipped");

        assert_eq!(err.host.as_deref(), Some("h1"), "err: {err:?}");
        assert!(
            err.reason
                .contains("compose none of the requested packages"),
            "reason: {}",
            err.reason
        );
        assert!(
            !handle
                .commands()
                .iter()
                .any(|c| c.starts_with("transactional-update -n pkg in")),
            "nothing may be dispatched: {:?}",
            handle.commands()
        );
        assert!(
            !handle
                .fired_commands()
                .iter()
                .any(|c| c.contains("systemctl reboot")),
            "nothing was staged, so nothing to activate: {:?}",
            handle.fired_commands()
        );
        assert!(
            handle.file_contents(HISTORY_LOG).is_none(),
            "nothing installed, so no row"
        );
    }

    #[tokio::test]
    async fn prepare_records_each_hosts_own_list_in_history() {
        // The per-host-composition instance of the rule `--installed` also
        // pins: a group-wide row names packages a host never installed.
        let (t1, h1) = composed_host(
            "h1",
            SystemProduct::new("SL-Micro", "6.1", "x86_64"),
            BTreeSet::new(),
        );
        let (t2, h2) = composed_host(
            "h2",
            SystemProduct::new("SL-Micro", "6.1", "aarch64"),
            BTreeSet::new(),
        );
        let mut group = HostsGroup::new(vec![t1, t2], false);
        let repo = ComposingRepo::new(vec![
            (
                SystemProduct::new("SL-Micro", "6.1", "x86_64"),
                names(&["pkg-a", "pkg-b"]),
            ),
            (
                SystemProduct::new("SL-Micro", "6.1", "aarch64"),
                names(&["pkg-b", "pkg-c"]),
            ),
        ]);

        perform_prepare(&mut group, &repo, &seven_packages(), false, false, false)
            .await
            .expect("both hosts compose part of the list");

        for (handle, tail) in [
            (&h1, ":prepare:pkg-a pkg-b\n"),
            (&h2, ":prepare:pkg-b pkg-c\n"),
        ] {
            let contents =
                String::from_utf8(handle.file_contents(HISTORY_LOG).expect("a prepare row"))
                    .unwrap();
            assert_eq!(contents.lines().count(), 1, "history: {contents:?}");
            assert!(contents.ends_with(tail), "history line: {contents:?}");
        }
    }

    #[tokio::test]
    async fn perform_update_prepares_with_the_host_list() {
        // `update`'s embedded prepare reaches the composition through the same
        // `SetRepo` seam, so it needs no wiring of its own — and a real report
        // type is used here rather than a stub, to pin that the override
        // forwards `base.composed`.
        let (t, handle) = composed_host(
            "h1",
            SystemProduct::new("SL-Micro", "6.1", "x86_64"),
            BTreeSet::new(),
        );
        let mut group = HostsGroup::new(vec![t], false);
        let mut report = SlReport::new(Config::default());
        report.base_mut().composed = [(
            SystemProduct::new("SL-Micro", "6.1", "x86_64"),
            names(&["pkg-a", "pkg-b"]),
        )]
        .into_iter()
        .collect();

        let _ = perform_update(
            &mut group,
            &report,
            &seven_packages(),
            "42",
            "7",
            None,
            false,
            false,
            &mut Vec::new(),
        )
        .await;

        let cmds = handle.commands();
        let prepare = cmds
            .iter()
            .find(|c| c.starts_with("transactional-update -n pkg in -l"))
            .unwrap_or_else(|| panic!("no embedded prepare: {cmds:?}"));
        let got: BTreeSet<String> = prepare
            .split_whitespace()
            .skip_while(|t| *t != "-l")
            .skip(1)
            .map(ToOwned::to_owned)
            .collect();
        assert_eq!(got, names(&["pkg-a", "pkg-b"]));
        // Arms the assertion above: an empty `commands()` would make the
        // `find` panic, but a prepare that ran and an update that never
        // followed would still let a wrong narrowing look right.
        assert!(
            cmds.iter().any(|c| c.contains("-t patch")),
            "the updater must have followed the prepare: {cmds:?}"
        );
    }

    /// Every command the slmicro updater template dispatches carries this line;
    /// the prepare template does not, so it tells "patched" from "prepared".
    fn was_patched(handle: &MockConnection) -> bool {
        handle
            .commands()
            .iter()
            .any(|c| c.contains("mtui_patch_rows"))
    }

    #[tokio::test]
    async fn perform_update_excludes_the_host_its_prepare_refused() {
        // The refusal reaches the patch, or `update` installs the update on a
        // host for which the prepare established no package baseline — the
        // failure the standalone refusal exists to report.
        let (t1, h1) = composed_host(
            "h1",
            SystemProduct::new("SL-Micro", "6.1", "x86_64"),
            BTreeSet::new(),
        );
        let (t2, h2) = composed_host(
            "h2",
            SystemProduct::new("SL-Micro", "6.1", "aarch64"),
            BTreeSet::new(),
        );
        let mut group = HostsGroup::new(vec![t1, t2], false);
        let repo = ComposingRepo::new(vec![
            (
                SystemProduct::new("SL-Micro", "6.1", "x86_64"),
                names(&["pkg-a"]),
            ),
            (
                SystemProduct::new("SL-Micro", "6.1", "aarch64"),
                names(&["pkg-z"]),
            ),
        ]);

        let (res, logs) = capture_logs(perform_update(
            &mut group,
            &repo,
            &["pkg-a".to_owned()],
            "42",
            "7",
            None,
            false,
            false,
            &mut Vec::new(),
        ))
        .await;

        assert!(
            !was_patched(&h2),
            "the refused host must not be patched: {:?}",
            h2.commands()
        );
        // Arms the assertion above: excluding every host would satisfy it too.
        assert!(
            was_patched(&h1),
            "the composing host still gets its patch: {:?}",
            h1.commands()
        );
        // Nothing was staged on it, so the reboot that activates a staged
        // snapshot must not fire — and a reboot failure escalates to
        // `RebootNotTaken`, which rolls the whole group back.
        assert!(
            h2.fired_commands().is_empty(),
            "the excluded host must not be rebooted: {:?}",
            h2.fired_commands()
        );
        assert!(
            !h1.fired_commands().is_empty(),
            "arms the assertion above: the patched host does reboot"
        );
        assert!(
            h2.file_contents(HISTORY_LOG).is_none(),
            "nothing ran on it, so no row: {:?}",
            h2.file_contents(HISTORY_LOG)
                .map(|b| String::from_utf8_lossy(&b).into_owned())
        );
        let rows = String::from_utf8(h1.file_contents(HISTORY_LOG).expect("h1 has rows")).unwrap();
        assert!(rows.contains(":update:pkg-a"), "h1 history: {rows:?}");
        // #396 itself: the group's roll-call must not speak for the host left
        // out of it.
        let roll_call = logs
            .lines()
            .find(|l| l.contains("update succeeded on"))
            .unwrap_or_else(|| panic!("no roll-call line: {logs}"));
        assert!(
            roll_call.contains("h1"),
            "names the patched host: {roll_call}"
        );
        assert!(
            !roll_call.contains("h2"),
            "must not name the excluded host: {roll_call}"
        );
        assert!(
            matches!(res, Err(UpdateFailure::Uncomposed(_))),
            "an excluded host is a failed update, not a silent skip: {res:?}"
        );
    }

    #[tokio::test]
    async fn perform_update_does_not_check_the_host_it_excluded() {
        // An excluded host's last snapshot is an earlier phase's, from a
        // fan-out it was not judged by. Scoring that as the update's verdict
        // would fail a clean update with `Check` — the class that fires the
        // group-wide rollback, downgrading every peer that patched correctly.
        let (t1, _h1) = composed_host(
            "h1",
            SystemProduct::new("SL-Micro", "6.1", "x86_64"),
            BTreeSet::new(),
        );
        // Every command h2 answers exits non-zero, which the `("slmicro", true)`
        // update check reads as a failed patch.
        let (mut t2, h2) = slmicro_target_on(
            "h2",
            SystemProduct::new("SL-Micro", "6.1", "aarch64"),
            BTreeSet::new(),
            "",
            "",
            1,
        );
        // Gives h2 the stale snapshot: `package_check` queries versions on
        // every host, excluded ones included, and is the last thing to touch
        // h2 before the update's own check would read `lastexit`.
        t2.set_packages(vec![mtui_types::package::Package::new("pkg-a")]);
        let mut group = HostsGroup::new(vec![t1, t2], false);
        let repo = ComposingRepo::new(vec![
            (
                SystemProduct::new("SL-Micro", "6.1", "x86_64"),
                names(&["pkg-a"]),
            ),
            (
                SystemProduct::new("SL-Micro", "6.1", "aarch64"),
                names(&["pkg-z"]),
            ),
        ]);

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

        // Arms the case: a host that was patched would owe a real verdict, and
        // the scoping would prove nothing.
        assert!(
            !was_patched(&h2),
            "the refused host must not be patched: {:?}",
            h2.commands()
        );
        assert!(
            matches!(res, Err(UpdateFailure::Uncomposed(_))),
            "the exclusion is the verdict, not a check failure it never earned: {res:?}"
        );
    }

    #[tokio::test]
    async fn perform_update_names_the_excluded_host_in_its_error() {
        // The group's success must not speak for the host left out (#396): the
        // verdict names it, its products, and that it was excluded.
        let (t1, _h1) = composed_host(
            "h1",
            SystemProduct::new("SL-Micro", "6.1", "x86_64"),
            BTreeSet::new(),
        );
        let (t2, _h2) = composed_host(
            "h2",
            SystemProduct::new("SL-Micro", "6.1", "aarch64"),
            BTreeSet::new(),
        );
        let mut group = HostsGroup::new(vec![t1, t2], false);
        let repo = ComposingRepo::new(vec![
            (
                SystemProduct::new("SL-Micro", "6.1", "x86_64"),
                names(&["pkg-a"]),
            ),
            (
                SystemProduct::new("SL-Micro", "6.1", "aarch64"),
                names(&["pkg-z"]),
            ),
        ]);

        let err = perform_update(
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
        .await
        .expect_err("an update that skipped a host must not report success");

        let UpdateFailure::Uncomposed(e) = err else {
            panic!("the exclusion is its own class, not the noisy one: {err:?}");
        };
        assert_eq!(e.host.as_deref(), Some("h2"), "err: {e:?}");
        assert!(
            e.reason.contains("SL-Micro-6.1.aarch64")
                && e.reason.contains("excluded from the update"),
            "reason: {}",
            e.reason
        );
    }

    #[tokio::test]
    async fn perform_update_with_every_host_refused_touches_no_repo() {
        // Nothing left to patch: the update stops before it locks the group and
        // adds a test repo it would only have to remove again.
        let (t, handle) = composed_host(
            "h1",
            SystemProduct::new("SL-Micro", "6.1", "x86_64"),
            BTreeSet::new(),
        );
        let mut group = HostsGroup::new(vec![t], false);
        let repo = ComposingRepo::new(vec![(
            SystemProduct::new("SL-Micro", "6.1", "x86_64"),
            names(&["pkg-z"]),
        )]);

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

        assert!(
            matches!(res, Err(UpdateFailure::Uncomposed(_))),
            "res: {res:?}"
        );
        assert!(
            !was_patched(&handle),
            "no host composes the update: {:?}",
            handle.commands()
        );
        // The prepare's own `Remove` is expected; an `Add` means the update ran
        // its repo phase over a group it had already excluded entirely.
        assert_eq!(
            repo.ops(),
            vec![RepoOp::Remove],
            "only the prepare's repo removal may have run"
        );
    }

    /// One `update` over h1 (composes `pkg-a`) + h2 (composes only `pkg-z`, so
    /// it is refused), returning the [`ComposingRepo`] that recorded every op
    /// and the captured logs.
    async fn update_over_one_refused_host(newpackage: bool) -> (ComposingRepo, String) {
        let (t1, _h1) = composed_host(
            "h1",
            SystemProduct::new("SL-Micro", "6.1", "x86_64"),
            BTreeSet::new(),
        );
        let (t2, _h2) = composed_host(
            "h2",
            SystemProduct::new("SL-Micro", "6.1", "aarch64"),
            BTreeSet::new(),
        );
        let mut group = HostsGroup::new(vec![t1, t2], false);
        let repo = ComposingRepo::new(vec![
            (
                SystemProduct::new("SL-Micro", "6.1", "x86_64"),
                names(&["pkg-a"]),
            ),
            (
                SystemProduct::new("SL-Micro", "6.1", "aarch64"),
                names(&["pkg-z"]),
            ),
        ]);

        let (res, logs) = capture_logs(perform_update(
            &mut group,
            &repo,
            &["pkg-a".to_owned()],
            "42",
            "7",
            None,
            false,
            newpackage,
            &mut Vec::new(),
        ))
        .await;

        assert!(
            matches!(res, Err(UpdateFailure::Uncomposed(_))),
            "res: {res:?}"
        );
        (repo, logs)
    }

    #[tokio::test]
    async fn perform_update_does_not_reconfigure_the_repos_of_the_host_it_excluded() {
        // The exclusion is reported to the operator as "this host was left out
        // of the update"; adding the test repo to it anyway (and removing it
        // again on the way out) makes that report false.
        let (repo, _logs) = update_over_one_refused_host(false).await;

        assert_eq!(
            repo.ops_for("h2"),
            vec![RepoOp::Remove],
            "the excluded host may only see the prepare's own removal"
        );
        // Arms the assertion above: excluding the whole group would satisfy it.
        assert_eq!(
            repo.ops_for("h1"),
            vec![RepoOp::Remove, RepoOp::Add, RepoOp::Remove],
            "the composing host still gets the update's add and cleanup"
        );
    }

    #[tokio::test]
    async fn perform_update_newpackage_does_not_strand_the_test_repo_on_the_excluded_host() {
        // `--newpackage` runs a *testing* prepare (repo `Add`) after the
        // update, and the cleanup that follows is scoped to the eligible set.
        // Group-wide, that add is the one op nothing ever removes: the excluded
        // host keeps the test repo forever (#409).
        let (repo, logs) = update_over_one_refused_host(true).await;

        assert_eq!(
            repo.ops_for("h2"),
            vec![RepoOp::Remove],
            "the newpackage prepare must not add the test repo to the excluded host"
        );
        // The other half of the scoping: the excluded host is out of the
        // prepare's package lists too, so it is not refused a second time for
        // the composition its exclusion already reported.
        assert!(
            !logs.contains("newpackage prepare after update failed"),
            "the excluded host must not fail the newpackage prepare it is not part of: {logs}"
        );
        // The reboot map is scoped too, so the excluded host draws no WARN for
        // a fan-out it was never in. The one that remains is the initial
        // group-wide prepare's, where h2 is in scope and genuinely refused.
        assert_eq!(
            logs.matches("skipping reboot").count(),
            1,
            "only the initial prepare may warn about h2's reboot: {logs}"
        );
        // Arms the assertion above, and pins that the second `Add` — the
        // newpackage prepare's — still happens where it belongs.
        assert_eq!(
            repo.ops_for("h1"),
            vec![RepoOp::Remove, RepoOp::Add, RepoOp::Add, RepoOp::Remove],
            "the composing host gets the update's add, the newpackage add, and the cleanup"
        );
    }

    /// A [`ComposingRepo`] that overwrites `contended`'s operation lockfile
    /// with another owner's line on every repo fan-out (idempotent).
    ///
    /// `update`'s embedded prepare takes the very group lock the update then
    /// takes again, so a foreign lock seeded before the call aborts the
    /// *prepare* and the update's own lock is never reached. The prepare's repo
    /// fan-out runs between the two acquisitions, which is exactly where a
    /// competing owner claims the host in production.
    struct ContendingRepo {
        composed: HashMap<SystemProduct, BTreeSet<String>>,
        contended: MockConnection,
        ops: std::sync::Mutex<Vec<(String, RepoOp)>>,
    }

    impl ContendingRepo {
        fn new(entries: Vec<(SystemProduct, BTreeSet<String>)>, contended: MockConnection) -> Self {
            Self {
                composed: entries.into_iter().collect(),
                contended,
                ops: std::sync::Mutex::default(),
            }
        }

        fn ops_for(&self, host: &str) -> Vec<RepoOp> {
            self.ops
                .lock()
                .unwrap()
                .iter()
                .filter(|(h, _)| h == host)
                .map(|(_, op)| *op)
                .collect()
        }
    }

    #[async_trait::async_trait]
    impl SetRepo for ContendingRepo {
        async fn set_repo(&self, target: &mut Target, operation: RepoOp) {
            self.ops
                .lock()
                .unwrap()
                .push((target.hostname().to_owned(), operation));
            let _ = self
                .contended
                .clone()
                .with_file(TARGET_LOCK_PATH, b"1700000000:otheruser:99999".to_vec());
        }

        fn composition(&self) -> Option<&HashMap<SystemProduct, BTreeSet<String>>> {
            Some(&self.composed)
        }
    }

    #[tokio::test]
    async fn perform_update_survives_a_contended_lock_on_the_host_it_excluded() {
        // `update_lock` fails closed on *any* member of the group, so a refused
        // host left in it hands one contended lock the power to abort the
        // update for every peer that does compose it.
        let (t1, h1) = composed_host(
            "h1",
            SystemProduct::new("SL-Micro", "6.1", "x86_64"),
            BTreeSet::new(),
        );
        let (t2, h2) = composed_host(
            "h2",
            SystemProduct::new("SL-Micro", "6.1", "aarch64"),
            BTreeSet::new(),
        );
        let mut group = HostsGroup::new(vec![t1, t2], false);
        let repo = ContendingRepo::new(
            vec![
                (
                    SystemProduct::new("SL-Micro", "6.1", "x86_64"),
                    names(&["pkg-a"]),
                ),
                (
                    SystemProduct::new("SL-Micro", "6.1", "aarch64"),
                    names(&["pkg-z"]),
                ),
            ],
            h2.clone(),
        );

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

        assert!(
            was_patched(&h1),
            "the eligible peer must still be patched: {:?}",
            h1.commands()
        );
        assert!(
            matches!(res, Err(UpdateFailure::Uncomposed(_))),
            "the excluded host's contention is not the group's verdict: {res:?}"
        );
        // The foreign line is still there, so nothing unlocked a host this run
        // never locked.
        assert_eq!(
            h2.file_contents(TARGET_LOCK_PATH).as_deref(),
            Some(&b"1700000000:otheruser:99999"[..]),
            "the excluded host's foreign lock must be left alone"
        );
    }

    #[tokio::test]
    async fn perform_update_newpackage_ignores_the_excluded_host_stale_stderr() {
        // Between the update and the `--newpackage` prepare the only group-wide
        // step left is `package_check`'s `rpm -q`, so its stderr is the excluded
        // host's whole `last*` record by then. The prepare's post-fan-out scan
        // reads exactly that record; group-wide it turns one rpm warning on a
        // host the fan-out skipped into "failed to set issue repo" and aborts
        // the prepare for the peer that is eligible.
        let (t1, h1) = composed_host(
            "h1",
            SystemProduct::new("SL-Micro", "6.1", "x86_64"),
            BTreeSet::new(),
        );
        // A `with_default`, not a scripted `rpm -q`: the refused host runs no
        // command at all before `package_check`, so its `last*` is empty until
        // then either way, and this does not depend on the query's exact
        // wording.
        let conn = MockConnection::new("h2")
            .with_default(CommandLog::new(
                "",
                "pkg-z 1-1\n",
                "warning: found bdb Packages database",
                0,
                0,
            ))
            .with_changing_boot_id();
        let mut t2 = Target::with_connection("h2", TargetState::Enabled, Box::new(conn));
        t2.set_system(
            System::new(
                SystemProduct::new("SL-Micro", "6.1", "aarch64"),
                BTreeSet::new(),
                true,
            ),
            true,
        );
        t2.set_packages(vec![mtui_types::package::Package::new("pkg-z")]);
        let mut group = HostsGroup::new(vec![t1, t2], false);
        let repo = ComposingRepo::new(vec![
            (
                SystemProduct::new("SL-Micro", "6.1", "x86_64"),
                names(&["pkg-a"]),
            ),
            (
                SystemProduct::new("SL-Micro", "6.1", "aarch64"),
                names(&["pkg-z"]),
            ),
        ]);

        let (res, logs) = capture_logs(perform_update(
            &mut group,
            &repo,
            &["pkg-a".to_owned()],
            "42",
            "7",
            None,
            false,
            true,
            &mut Vec::new(),
        ))
        .await;

        assert!(
            matches!(res, Err(UpdateFailure::Uncomposed(_))),
            "res: {res:?}"
        );
        // Not the op sequence: the repo fan-out runs *before* the scan, so h1
        // gets its `Add` either way and only the install behind it is lost.
        assert!(
            !logs.contains("newpackage prepare after update failed"),
            "one rpm warning on the excluded host must not fail the prepare: {logs}"
        );
        let installs = h1
            .commands()
            .iter()
            // Anchored: the updater script body contains `pkg in` too.
            .filter(|c| c.starts_with("transactional-update -n pkg in -l"))
            .count();
        assert_eq!(
            installs,
            2,
            "the eligible peer keeps both installs (initial prepare + newpackage): {:?}",
            h1.commands()
        );
    }

    #[tokio::test]
    async fn perform_update_newpackage_survives_a_contended_lock_on_the_host_it_excluded() {
        // The `--newpackage` prepare takes its own operation lock. Group-wide
        // and fail-closed, so the excluded host's contention aborted it before
        // its repo fan-out and the eligible peer never got the post-update
        // testing repo it exists to add.
        let (t1, h1) = composed_host(
            "h1",
            SystemProduct::new("SL-Micro", "6.1", "x86_64"),
            BTreeSet::new(),
        );
        let (t2, h2) = composed_host(
            "h2",
            SystemProduct::new("SL-Micro", "6.1", "aarch64"),
            BTreeSet::new(),
        );
        let mut group = HostsGroup::new(vec![t1, t2], false);
        let repo = ContendingRepo::new(
            vec![
                (
                    SystemProduct::new("SL-Micro", "6.1", "x86_64"),
                    names(&["pkg-a"]),
                ),
                (
                    SystemProduct::new("SL-Micro", "6.1", "aarch64"),
                    names(&["pkg-z"]),
                ),
            ],
            h2.clone(),
        );

        let res = perform_update(
            &mut group,
            &repo,
            &["pkg-a".to_owned()],
            "42",
            "7",
            None,
            false,
            true,
            &mut Vec::new(),
        )
        .await;

        assert!(
            matches!(res, Err(UpdateFailure::Uncomposed(_))),
            "the excluded host's contention is not the group's verdict: {res:?}"
        );
        assert_eq!(
            repo.ops_for("h1"),
            vec![RepoOp::Remove, RepoOp::Add, RepoOp::Add, RepoOp::Remove],
            "the second `Add` is the newpackage prepare's; without it the lock aborted"
        );
        assert_eq!(
            repo.ops_for("h2"),
            vec![RepoOp::Remove],
            "and the excluded host is still left as the initial prepare found it"
        );
        // Nothing unlocked a host this run never locked.
        assert_eq!(
            h2.file_contents(TARGET_LOCK_PATH).as_deref(),
            Some(&b"1700000000:otheruser:99999"[..]),
            "the excluded host's foreign lock must be left alone"
        );
        assert!(
            was_patched(&h1),
            "the eligible peer must still be patched: {:?}",
            h1.commands()
        );
    }

    #[tokio::test]
    async fn perform_update_ignores_a_missing_updater_on_the_host_it_excluded() {
        // `build_update_maps` resolved an updater for every host and only then
        // dropped the refused ones, so a refused host with no supported updater
        // failed the whole group with `MissingUpdater` — the peers it was
        // excluded to spare included.
        let (t1, h1) = composed_host(
            "h1",
            SystemProduct::new("SL-Micro", "6.1", "x86_64"),
            BTreeSet::new(),
        );
        // Unknown release, so no updater doer resolves — and non-transactional,
        // so `build_reboot_map`'s pre-lock preparer scan skips it and the
        // prepare gets far enough to refuse it for its composition instead.
        let conn = MockConnection::new("h2").with_default(CommandLog::new("", "", "", 0, 0));
        let h2 = conn.clone();
        let mut t2 = Target::with_connection("h2", TargetState::Enabled, Box::new(conn));
        t2.set_system(
            System::new(
                SystemProduct::new("gentoo", "1", "x86_64"),
                BTreeSet::new(),
                false,
            ),
            false,
        );
        let mut group = HostsGroup::new(vec![t1, t2], false);
        let repo = ComposingRepo::new(vec![
            (
                SystemProduct::new("SL-Micro", "6.1", "x86_64"),
                names(&["pkg-a"]),
            ),
            (
                SystemProduct::new("gentoo", "1", "x86_64"),
                names(&["pkg-z"]),
            ),
        ]);

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

        assert!(
            was_patched(&h1),
            "the eligible peer must still be patched: {:?}",
            h1.commands()
        );
        assert!(
            !h2.commands().iter().any(|c| c.contains(":p=42:7")),
            "no updater command may reach the excluded host: {:?}",
            h2.commands()
        );
        assert!(
            matches!(res, Err(UpdateFailure::Uncomposed(_))),
            "an updater the update never needed must not fail the group: {res:?}"
        );
    }

    /// A cancel that lands while the probe fan-out is in flight must stop at
    /// the post-probe checkpoint, dispatching nothing.
    ///
    /// Deterministic without a wall clock: under `start_paused` the runtime
    /// advances the timer only when every task is idle. The probe starts at 0ms
    /// and completes at 10ms; the canceller fires at 5ms, so the fan-out itself
    /// is never gated (gating one would leave a host's stale `last*` snapshot
    /// sailing through the checks) and the checkpoint after it is what stops
    /// the flow.
    ///
    /// This replaces the retired
    /// `prepare_cancelled_mid_loop_records_only_the_dispatched_packages`. That
    /// test pinned "the history row names only the dispatched subset", which is
    /// not re-pinned here: with prepare all-or-nothing per host, a partially
    /// applied prepare has no producer and the property ceases to exist.
    #[tokio::test(start_paused = true)]
    async fn prepare_installed_only_cancel_after_the_probe_dispatches_nothing() {
        let conn = MockConnection::new("h1")
            .with_default(CommandLog::new("", "", "", 0, 0))
            .with_run_delay(std::time::Duration::from_millis(10))
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
        let token = group.cancel_token();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            token.cancel();
        });

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

        assert_eq!(
            handle.commands(),
            vec![INSTALLED_PROBE.to_owned()],
            "the probe ran and nothing else did"
        );
        assert!(
            !handle
                .fired_commands()
                .iter()
                .any(|c| c.contains("systemctl reboot")),
            "nothing was staged: {:?}",
            handle.fired_commands()
        );
        assert!(
            handle.file_contents(HISTORY_LOG).is_none(),
            "nothing dispatched, so no row"
        );
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
        // No preparer doer for this key makes `build_reboot_map` fail, so
        // prepare returns Err rather than swallowing.
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
        // The failure is returned, not just logged.
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
        // No installer doer for this key, so the template runs nothing at all.
        // Reporting Ok here is what made `install` print "install completed"
        // for hosts it never touched.
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
        // product never parsed has no release.
        assert!(
            err.reason.contains("h1"),
            "the offending host must be named: {}",
            err.reason
        );
    }

    #[tokio::test]
    async fn perform_install_names_the_unresolvable_host_and_product() {
        // An unparsed system yields `MissingInstaller { release: "" }`, whose
        // Display is "Missing Installer for " — nothing actionable.
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
        // The verdict comes from the template's own check.
        let (t, _h) = sles_target_with_exit("h1", "", 104);
        let mut group = HostsGroup::new(vec![t], false);
        let err = perform_install(&mut group, &["pkg-a".to_owned()])
            .await
            .expect_err("non-zero exit surfaces as Err");
        assert_eq!(err.host.as_deref(), Some("h1"));
        // 104 is zypper's ZYPPER_EXIT_INF_CAP_NOT_FOUND: the verdict reports
        // *what* went wrong, not just that the exit was non-zero.
        assert_eq!(err.reason, "package not found");
    }

    /// A [`PlanProvider`](mtui_hosts::PlanProvider) whose check always stops at
    /// a cancellation checkpoint.
    ///
    /// The only way to put a cancelled `CheckFailure` on the input of
    /// `perform_operation_with`: the real check tables never emit one, so a
    /// `MockConnection` cannot express this state. Mirrors the provider in
    /// `crates/mtui-hosts/tests/operation_group.rs`, which pins the same flag
    /// one layer down, at `OperationReport`.
    struct CancellingProvider;

    impl mtui_hosts::PlanProvider for CancellingProvider {
        fn doer(
            &self,
            _role: &str,
            _release: &str,
            _transactional: bool,
        ) -> Result<Doer, HostError> {
            Ok(Doer::new(
                "zypper -n in -y -l $packages",
                "systemctl reboot",
            ))
        }

        fn check(&self, _role: &str, _release: &str, _transactional: bool) -> Check {
            Box::new(|_a: CheckArgs<'_>| Err(CheckFailure::cancelled("stopped at a checkpoint")))
        }
    }

    #[tokio::test]
    async fn a_cancelled_check_failure_reaches_the_update_error_with_the_flag_intact() {
        // `OperationReport` carrying the flag is worth nothing if
        // `perform_operation_with`'s map drops it on the way into
        // `UpdateError`. One host and an always-cancelling check means exactly
        // one failure, so `aggregate_failures` takes the verbatim branch — the
        // summary branch deliberately does not carry the flag.
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
        // Not just the flag: an assertion on `is_cancelled` alone would pass on
        // an error synthesised anywhere else in the flow.
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
        // `Target::unlock_reporting` emits its own WARN naming `host="h1"` on
        // this path, so a bare `logs.contains("h1")` would pass even if
        // `unlock_failure_message` were never reached.
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
        // A cancel requested before the run must stop it before any command
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
    /// row: it is the operator's only record that the command ran there, and it
    /// is needed most on exactly the host that did not come back.
    ///
    /// Only observable because `MockConnection::sftp_append` reconnects at
    /// entry like the real `SshConnection::sftp()`; a mock that accepted an
    /// append on a dead host would pass whichever side of the reboot the write
    /// happened on.
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
        // `id: None`, so the row is `{ts}:{user}:update:{packages}`. Anchoring
        // on the tail also pins the package list.
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
        // The best-effort rollback dispatches a real downgrade, so it must
        // record its own history row like a directly-invoked one would.
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
        // Anchored on the tail so it pins the label *and* the package list; a
        // bare `contains("prepare")` would match a package named
        // `prepare-something` or the `:update:` row's payload.
        assert!(
            contents.ends_with(":prepare:pkg-a pkg-b\n"),
            "history line: {contents:?}"
        );
    }

    #[tokio::test]
    async fn prepare_installed_only_cancel_before_the_probe_runs_nothing() {
        // The inverse failure direction: a row claiming an install that never
        // started is worse than no row. The `commands().is_empty()` assertion
        // below is what forces the PRE-probe checkpoint to exist — without one
        // the probe fan-out would run and this would have to be weakened.
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
        assert_eq!(
            err.reason, "prepare cancelled before any package was installed",
            "names how far it got: {err}"
        );

        assert!(
            handle.commands().is_empty(),
            "cancelled before the probe, so nothing dispatched: {:?}",
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
        // release key does not resolve and `prepare_body` fails exactly that
        // host with "nothing was installed", so a group-wide history fan-out
        // would hand it a `:prepare:` row contradicting its own verdict — in a
        // format the project treats as an interop contract.
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
        // The other side of the contradiction: h2 is told nothing was
        // installed on it.
        assert_eq!(err.host.as_deref(), Some("h2"), "{err}");
        assert!(
            err.to_string().contains("nothing was installed"),
            "cause stated: {err}"
        );

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
    /// reproduces "`job_cancel` during a multi-minute prepare" with no timing.
    struct CancellingRepo {
        /// Cancels the group's token. A boxed closure rather than the token
        /// itself, so the double needs no `tokio-util` dependency here.
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
        // The entry gate says "before the update started", so this pins the
        // *pre-dispatch* gate, the one that runs after prepare.
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
        // A row claiming the *update* ran would be worse than none.
        assert!(
            !contents.contains(":update:"),
            "no update row may be written when no updater command ran: {contents:?}"
        );
        assert!(
            !handle.commands().iter().any(|c| c.contains(":p=42:7")),
            "the updater command must not have dispatched: {:?}",
            handle.commands()
        );
        let ops = repo.ops.lock().unwrap().clone();
        assert!(
            ops.contains(&RepoOp::Add) && ops.last() == Some(&RepoOp::Remove),
            "the cancel gate undoes its repo add: {ops:?}"
        );
    }

    #[tokio::test]
    async fn update_cancelled_at_the_gate_under_noprepare_records_nothing() {
        // Same gate under `--noprepare`: this abort owes no row, and its
        // message must not invent a prepare residue that cannot exist here.
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
        // 100-103/106 mean "update needed", "reboot needed", "restart needed",
        // "repo skipped" — all *successful* installs. 102 is routine after a
        // kernel update, so a bare `lastexit() != 0` scan would report a false
        // failure on a perfectly good install.
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
        // Without an install check these keys fell through to the
        // `PlanProvider` adapter's exit-code-only fallback, so a locked update
        // stack, a failed RPM transaction and a command that never ran all read
        // "install command failed" (#406). Both host shapes are driven because
        // they take different routes into the same table: SL Micro is
        // transactional (its check shares the update check's classifier), RHEL
        // is not (its check judges the exit code alone).
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
            // The verdict below is only the check's if there is one.
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
        // The install/uninstall twin of
        // `perform_prepare_names_the_host_of_a_prepare_that_never_ran`, pinning
        // that this path does *not* double a `-1`: it collects only what the
        // `Operation` template reports — one `check_failures` entry per host
        // plan (`mtui-hosts::target::operation`) plus `reboot_failures`, which
        // cannot double a host because a failed check removes it from the
        // reboot map first. Unlike `prepare_body`, no exit-code scan runs here.
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
        // The summary path builds a fresh error, so without care the flags
        // exist on the verbatim path and vanish on this one. They are the
        // declared routing contract (`reports::update_flow` routes on
        // `probe_failed`, the command layer reports `cancelled`). `all`, not
        // `any`: a summary claiming "no patch was dispatched" while one host
        // had dispatched one would be worse than no claim.
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

        // `cancelled` is deliberately not summarised, so a cancel cannot mask
        // a real failure collected beside it. On what does and does not produce
        // one, see the `cancelled` comment in `aggregate_failures`.
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
        // scan and then adds that host's downgrade-check verdict. Undeduped the
        // roll-call reads "downgrade failed on h1, h1". (The prepare flow's own
        // overlap — one signal, two rules — is resolved upstream in
        // `prepare_body` instead: deduping a roll-call would not restore the
        // `host` field the summary branch drops.)
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
        // Both causes still reach the operator: this dedups the roll-call, not
        // the diagnosis.
        assert!(
            err.reason.contains("h1: failed to remove issue repo")
                && err
                    .reason
                    .contains("h1: downgrade command timed out or failed to run"),
            "both causes survive: {}",
            err.reason
        );
        // A dedup that collapsed distinct hosts would hide one.
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

        // noprepare=true keeps the flow to update + checks; the report's real
        // `set_repo` no-ops with an empty `update_repos` map.
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

        let cmds = handle.commands();
        assert!(
            cmds.iter().any(|c| c.contains(":p=42:7")),
            "expected updater command carrying $repa: {cmds:?}"
        );
    }

    #[tokio::test]
    async fn perform_update_aborts_cleanly_when_no_updater_doer() {
        // An unknown release has no updater doer: a hard fail, no updater
        // command issued, and the repo the flow added removed on the abort path.
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
        // #407's other half: with `noprepare` nothing was installed and no
        // updater command dispatched, so this abort is correct to stay silent.
        assert!(
            handle.file_contents(HISTORY_LOG).is_none(),
            "an abort that dispatched nothing must leave no row: {:?}",
            handle.file_contents(HISTORY_LOG)
        );
    }

    // --- perform_downgrade -------------------------------------------------

    #[tokio::test]
    async fn perform_downgrade_resolves_version_and_issues_per_package_command() {
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
        // Aborts instead of "completing" with zero downgrade commands run.
        let (t, handle) = sles_target_with_exit("h1", "", -1);
        let mut group = HostsGroup::new(vec![t], false);

        let res = perform_downgrade(&mut group, &NoopRepo, &["pkg-a".to_owned()], None).await;
        let err = res.expect_err("a dead probe must abort");
        assert_eq!(err.reason, "package version probe failed");
        assert_eq!(err.host.as_deref(), Some("h1"));
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
        assert!(
            h1.commands()
                .iter()
                .any(|c| c.contains("pkg-a") && c.contains("1.0-1")),
            "healthy host must roll back: {:?}",
            h1.commands()
        );
        assert!(
            !h2.commands().iter().any(|c| c.contains("--oldpackage")),
            "dead host must build no downgrade command: {:?}",
            h2.commands()
        );
        // Not every probe died, so the all-dead abort must not fire: exactly one
        // downgrade row. An abort-site write that escaped its `if` shows as two.
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
    /// That pairing is what the rendered probe produces when `zypper -n se`
    /// fails; `MockConnection` cannot run a shell, so it is pinned end-to-end
    /// in `actions::downgrade`'s `rendered_script` module and only replayed
    /// here, to test the flow's *routing*. Scripted per command rather than
    /// through `with_default`, so the failure is attributable to the probe.
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
        // #451: the guarded template exits with the failed tool's own status,
        // so *any* non-zero status is a dead probe. The `-1` SSH sentinel the
        // two tests above use is only one of its values, so a predicate
        // narrowed to `c == -1` would still pass them.
        //
        // Per host, deliberately: aborting the whole group over h2's broken
        // zypper would strand every healthy peer half-applied — the opposite of
        // the update's own probe failure, where nothing had been applied yet.
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
        // `downgrade_verdict` names a package only when the report carries a
        // `required` version; on a standalone downgrade there is none, so a
        // dead probe leaves the map empty and `done` would be logged over a
        // host nobody measured — while the command failed.
        let packages = vec!["pkg-a".to_owned()];
        let (t1, _h1) = sles_target("h1", "pkg-a = 1.0-1\n");
        let (t2, _h2, _list) = sles_target_with_probe_exit("h2", &packages, 7);
        let mut group = HostsGroup::new(vec![t1, t2], false);

        let (res, logs) =
            capture_logs(perform_downgrade(&mut group, &NoopRepo, &packages, None)).await;

        let err = res.expect_err("a dead probe still fails the command");
        assert_eq!(err.reason, "package version probe failed");
        // The capture layer renders a message unquoted, so this is `== "done"`;
        // `contains("\"done\"")` would match nothing and pass regardless.
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
        // The per-host ERROR says what happened to the host, not what the flow
        // did next: the operator needs to know this host still carries the
        // update, not that a step was omitted.
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
        // current >= required ⇒ named as not-downgraded, while the bookkeeping
        // still rotates before/after for it.
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
        let p = &group.get("h1").unwrap().packages()[0];
        assert_eq!(
            p.before().map(ToString::to_string).as_deref(),
            Some("1.5-1")
        );
        assert_eq!(p.after().map(ToString::to_string).as_deref(), Some("1.5-1"));
    }

    #[tokio::test]
    async fn downgrade_verdict_rotation_keeps_an_unchecked_slot_unchecked() {
        // #396: with no prior `update` the after slot was never checked, and
        // rotating it in as "checked, not installed" would make the export
        // claim the package was absent before a rollback nobody measured.
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
        // BOTH are named, not just the first — there is no short-circuit.
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
        // 0.9-1 is below the required 1.5-1 ⇒ rolled back ⇒ not named ⇒ "done".
        let (mut t, _h) = sles_target("h1", "pkg-a 0.9-1\n");
        let mut pkg = mtui_types::package::Package::new("pkg-a");
        pkg.set_required(Some("1.5-1")).unwrap();
        pkg.set_after(Some("1.5-1")).unwrap();
        t.set_packages(vec![pkg]);
        let mut group = HostsGroup::new(vec![t], false);

        let not_downgraded = downgrade_verdict(&mut group, &BTreeSet::new()).await;

        assert!(not_downgraded.is_empty(), "{not_downgraded:?}");
        let p = &group.get("h1").unwrap().packages()[0];
        assert_eq!(
            p.before().map(ToString::to_string).as_deref(),
            Some("1.5-1")
        );
        assert_eq!(p.after().map(ToString::to_string).as_deref(), Some("0.9-1"));
    }

    /// Builds an enabled SL Micro (transactional) target carrying `product` and
    /// `addons`, on a mock returning `stdout`/`stderr` with `exit` for every
    /// command.
    ///
    /// The general form; the two fixtures below pin the default product.
    fn slmicro_target_on(
        hostname: &str,
        product: SystemProduct,
        addons: BTreeSet<SystemProduct>,
        stdout: &str,
        stderr: &str,
        exit: i16,
    ) -> (Target, MockConnection) {
        // A changing boot id models a host that really rebooted. Without it
        // both probes read the same and the lifecycle correctly concludes the
        // host never went down — see `RebootFault::WentNowhere` for that.
        let conn = MockConnection::new(hostname)
            .with_default(CommandLog::new("", stdout, stderr, exit, 0))
            .with_changing_boot_id();
        let handle = conn.clone();
        let mut t = Target::with_connection(hostname, TargetState::Enabled, Box::new(conn));
        t.set_system(System::new(product, addons, true), true);
        (t, handle)
    }

    /// [`slmicro_target_on`] for the default SL-Micro 6.0 x86_64 product.
    fn slmicro_target(hostname: &str, stdout: &str, exit: i16) -> (Target, MockConnection) {
        slmicro_target_on(
            hostname,
            SystemProduct::new("SL-Micro", "6.0", "x86_64"),
            BTreeSet::new(),
            stdout,
            "",
            exit,
        )
    }

    /// A transactional SL-Micro target whose commands answer with `stderr`.
    ///
    /// The `("slmicro", true)` check reads the stdout/stderr markers *and* the
    /// exit code; this fixture exercises the marker half by answering exit `0`
    /// and leaving the failure signal entirely in `stderr` — the shape the exit
    /// code alone would miss.
    fn slmicro_target_with_stderr(hostname: &str, stderr: &str) -> (Target, MockConnection) {
        slmicro_target_on(
            hostname,
            SystemProduct::new("SL-Micro", "6.0", "x86_64"),
            BTreeSet::new(),
            "",
            stderr,
            0,
        )
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
    /// The stdout matters: a mock answering `""` makes the version probe yield
    /// nothing, so `combined` stays empty, **no downgrade command is ever
    /// built**, and any "assert no `--oldpackage`" becomes a test that cannot
    /// fail. A parseable `pkg = version` line is what makes those real.
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
        // The install succeeds but the post-reboot reconnect never comes back:
        // mtui must not report success on a host it lost.
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
        // Not the reachable causes: they route the rollback differently.
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
        // Not the reachable causes: they route the rollback differently.
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
        // Not the reachable causes: they route the rollback differently.
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
        // Not the reachable causes: they route the rollback differently.
        assert!(
            !err.reason.contains("never rebooted") && !err.reason.contains("never received"),
            "reason: {}",
            err.reason
        );
        assert_eq!(err.host.as_deref(), Some("h1"));
    }

    #[tokio::test]
    async fn update_reports_a_host_that_never_came_back_and_skips_the_rollback() {
        // A successful patch followed by a dead reconnect must NOT trigger the
        // rollback: the host is unreachable, so a downgrade cannot run on it.
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
        // Not the reachable causes: they route the rollback differently.
        assert!(
            !err.reason.contains("never rebooted") && !err.reason.contains("never received"),
            "reason: {}",
            err.reason
        );

        // The fixture answers the version probe with a parseable line, so a
        // rollback that *did* run would render `--oldpackage`; without that
        // this assertion would pass however the failure was routed.
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
        // `-1` means "the flow could not talk to this host", not "the patch
        // went wrong". Routing it into the group-wide rollback would revert
        // every host that patched cleanly (h2) on behalf of one the downgrade
        // cannot reach anyway (h1).
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

        // The variant itself, which the rollback wrapper above collapses to a
        // bare `UpdateError`: without this `NotRun` could be swapped for any
        // other non-rolling variant with the suite still green.
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
        // rather than flattened: this host is *reachable*, serving the old
        // packages from a snapshot that never activated while the group moved
        // on — the split-brain the rollback exists to undo.
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
        // NOT the unreachable case, which skips the rollback: asserting the
        // positive alone would pass on either routing.
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
        // Mixed causes: h1 is gone, h2 is up but never rebooted. A group-wide
        // rollback could not repair h1, would leave h2 needing manual work
        // anyway, and would revert every healthy host. `all`, not `any` — and
        // both hosts are still named.
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

        // The reboot is fire-and-forget, recorded separately from the run log.
        let fired = handle.fired_commands();
        assert!(
            fired.iter().any(|c| c.contains("systemctl reboot")),
            "expected transactional reboot after a successful update: {fired:?}"
        );
    }

    #[tokio::test]
    async fn perform_update_fails_a_transactional_host_whose_stack_is_locked() {
        // End-to-end proof that the `("slmicro", true)` check is reached:
        // without one, the check lookup hits its `else { continue }` and `update`
        // reports success however the patch went. The marker is scripted onto
        // the **patch command specifically** because `perform_update` runs
        // other commands first (the repo add's trailing `… -n ref`), so a
        // `with_default` marker keeps the test green even with the patch
        // fan-out deleted — the check then reads the refresh's snapshot.
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

        // The check failure must suppress the reboot: activating the new
        // snapshot would hide the failed patch behind a healthy-looking boot.
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
    /// Every other command answers cleanly. The update template captures the
    /// patch's status and re-exits with it, so an exit code scripted via
    /// `with_default` would be one on *every* command — a state no shell
    /// produces, and one that keeps a test green with the patch fan-out
    /// deleted, because the check then reads the repo refresh's snapshot.
    ///
    /// The default stdout is a resolvable version line, so a routed rollback
    /// renders an `--oldpackage` command; without it "assert no rollback
    /// happened" would pass however the failure was routed.
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
    /// That pairing is what the rendered template produces when
    /// `zypper -n patches` fails; `MockConnection` cannot run a shell, so it is
    /// pinned end-to-end in `actions::update`'s `rendered_script` module and
    /// only replayed here, to test the *routing*. The marker comes from the
    /// same constant the check greps for, so the two cannot drift silently.
    ///
    /// The default stdout is a resolvable version line, so a rollback that did
    /// run renders an `--oldpackage` command; without it "assert no rollback
    /// happened" would pass however the failure was routed.
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
        // #447's routing decision: a host that could not work out what to patch
        // never ran one, so nothing is half-applied for the group-wide rollback
        // to repair and firing it would revert every healthy peer. Asserting
        // the variant *and* the reason — the reason is what tells an operator
        // to look at the repo state rather than at the host's packages.
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
        // The same decision where its blast radius is: `perform_update_with_
        // rollback` hands the downgrade the *whole* group, so a probe failure
        // routed to `Check` would revert h2 — which patched perfectly — on
        // behalf of h1's broken repo configuration.
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
        // Neither host is one a downgrade can repair — h1 never ran a patch,
        // h2 cannot be reached — so the group-wide rollback must not fire. A
        // rule of `all(lastexit == -1)` would answer "not every host is `-1`"
        // and route `Check`. The label is `NotRun` rather than `ProbeFailed`:
        // the more conservative claim ("state unknown"), which is true of h2.
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
        assert!(
            e.reason.contains("h1") && e.reason.contains("h2"),
            "both failed hosts are named: {}",
            e.reason
        );
        // No "no patch was dispatched" claim for a run in which one host's
        // state is unknown.
        assert!(
            !e.probe_failed,
            "a mixed run must not carry the probe-failure flag: {e:?}"
        );

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
        // Why the rule asks "is *any* failed host repairable?" rather than "did
        // they all fail the same way?": h2's 104 is the half-applied state the
        // rollback exists to undo, and h1's probe failure must not downgrade
        // the verdict and strand h2 with a failed transaction.
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
        // 104 on the patch ⇒ the check flags "package not found"; the flow must
        // NOT issue a repo-remove (repos kept for retry). Scripted onto the
        // patch alone, since the shell cannot produce that status on every
        // command.
        let report = report_with_rrid();
        let packages = report.get_package_list();
        let (t, handle, patch) = sles_target_with_patch_exit("h1", &packages, 104);
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
        let Err(UpdateFailure::Check(e)) = res else {
            panic!("a failed patch returns Err(Check): {res:?}");
        };
        // The reason too: `Check` is reached by every marker, so the variant
        // alone would not show that the *exit code* was read.
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
        // The carve-out that makes reading the exit code safe at all. zypper
        // exits 102 (`ZYPPER_EXIT_INF_REBOOT_NEEDED`) after patching a kernel —
        // the routine outcome of the thing mtui exists to do. Under a bare
        // `!= 0` rule that host would fail its check, and a check failure hands
        // the rollback the *whole* group: one host's healthy 102 would remove
        // every host's issue repos, downgrade every host, and rewrite the
        // report's before/after slots.
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
        // Nothing was rolled back. The healthy peer is where a false failure
        // shows up as collateral damage, so h2 is asserted **first** and not in
        // a loop, where the first host to fail would hide it. The fixture
        // answers the version probe with a parseable line, so a rollback that
        // did run would render `--oldpackage` here.
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
        // The mock returns a resolvable version line so the downgrade command
        // renders. The probe must exit 0 (a non-zero probe exit is a dead-probe
        // abort) and `sles_target_with_exit` would apply 104 to it too, so
        // script the probe explicitly.
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
        // A stranded operation lock does not turn a good update into a failure,
        // but the fan-out's own `LockOutcome` map must still reach a warn.
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
    /// The subscriber is **global and installed once**; only the sink is
    /// thread-scoped. `tracing` caches callsite *interest* process-wide, so a
    /// callsite first reached from a thread with no subscriber is cached
    /// `Interest::never()` and stays silent for every later capture — under
    /// `set_default`'s thread-local guard that race made
    /// `downgrade_verdict_withholds_done_when_a_probe_died` fail about one run
    /// in three. Thread-scoping the sink loses nothing: `#[tokio::test]` is
    /// single-threaded.
    ///
    /// **Blast radius, accepted knowingly.** The unfiltered `Registry` reports
    /// no `max_level_hint`, so `LevelFilter::current()` becomes `TRACE` for the
    /// whole lib test binary and every `debug!`/`trace!` evaluates its
    /// arguments before being dropped for want of a sink. Bound the layer with
    /// a `LevelFilter` if the suite ever slows noticeably.
    ///
    /// **The workspace's third copy of the pattern** — see
    /// `mtui-datasources`' `tests/log_capture.rs` (the fullest write-up) and
    /// `mtui-datasources::teregen`'s test module; `mtui-core` gains a fourth in
    /// #404/PR #459. Copies rather than one helper because a `#[cfg(test)]`
    /// module cannot share an integration test's file.
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
        // A foreign lock makes `update_lock` fail on the only host, so the
        // cleanup must warn (naming the error and the remedy) instead.
        let foreign = MockConnection::new("h1")
            .with_default(CommandLog::new("", "", "", 0, 0))
            .with_file(
                mtui_hosts::TARGET_LOCK_PATH,
                b"1700000000:alice:4242:busy".to_vec(),
            );
        let t = Target::with_connection("h1", TargetState::Enabled, Box::new(foreign));
        let mut group = HostsGroup::new(vec![t], false);
        let repo = RecordingRepo::default();

        let ((), logs) = capture_logs(remove_test_repos(&mut group, &names(&["h1"]), &repo)).await;

        let ops = repo.ops.lock().unwrap().clone();
        assert!(
            !ops.contains(&RepoOp::Remove),
            "cleanup must not run when the lock fails: {ops:?}"
        );
        // Both the lock error and the remedy must be on the *same* line:
        // `update_lock`'s internal fan-out also WARNs about the foreign-locked
        // host, so a `logs.contains` across every captured line would pass with
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
        // #409's complaint (a stale test repo) can arise from the removal
        // command failing, not only from a lock failure.
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

        let ((), logs) =
            capture_logs(remove_test_repos(&mut group, &names(&["h1"]), &report)).await;

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

        // noprepare=false ⇒ the initial prepare install runs before the patch.
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
        // Exit 0 with stderr is host noise, not "prepare could not run"
        // (`host_command_failures` counts any stderr, and
        // `transactional-update` writes progress there on success), so the
        // update must proceed rather than hard-abort.
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
        // A foreign lock makes `update_lock` fail before prepare's body runs:
        // "prepare could not run", not "prepare ran and failed", so the update
        // hard-aborts before the lock, the repo add, or the patch command.
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
        // Full-string: pins the shape (no "succeeded", each host named once).
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
        assert!(
            cmds.iter()
                .any(|c| c.contains("pkg-a=1.0-1") && c.contains("pkg-b=2.0-1")),
            "expected a single combined transactional downgrade: {cmds:?}"
        );
        // The gate must not withhold the reboot from a succeeded transaction.
        let fired = handle.fired_commands();
        assert!(
            fired.iter().any(|c| c.contains("systemctl reboot")),
            "a clean transactional downgrade must still reboot: {fired:?}"
        );
    }

    #[tokio::test]
    async fn perform_prepare_skips_the_reboot_of_a_host_whose_prepare_failed() {
        // The per-host reboot gate: h1 must not reboot into its failed
        // transaction, while healthy h2 still must, or its staged snapshot
        // stays inert.
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
        // Exact, not `to_string().contains("h1")`: a substring match would also
        // pass if h2 had wrongly joined the failure set, which is half of what
        // this test is about.
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
    async fn perform_prepare_does_not_reboot_a_host_with_nothing_staged() {
        // The reboot map is built from the host's transactional flag and does
        // not know an empty package list dispatched nothing. In the `update`
        // rollback path a reboot would activate whatever the failed update left
        // staged.
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
        // A second (h1-blaming) entry would collapse `host` to `None` via the
        // aggregate summary.
        assert_eq!(err.host.as_deref(), Some("h2"), "{err}");
        let msg = err.to_string();
        assert!(
            msg.contains("no prepare command could be built"),
            "cause stated: {msg}"
        );
        // Being both dispatched-to and reported not-installed would be #396's
        // dishonesty inverted.
        assert!(
            bad_handle.commands().is_empty(),
            "{:?}",
            bad_handle.commands()
        );
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
        // The probe resolves nothing, so nothing is staged. When this downgrade
        // is the `update` rollback, a reboot would activate whatever the failed
        // update left staged — undoing the update flow's own decision to
        // suppress that reboot.
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
        // `transactional-update` writes progress to stderr on a *successful*
        // run, so stderr alone must not gate the reboot: the staged snapshot is
        // healthy and leaving it inert is the quiet no-op from the other
        // direction. (The stderr rule still fails the verdict via
        // `host_command_failures`; that is separate from the action here.)
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
        // #406: `transactional-update` reported a locked update stack and still
        // exited `0`, which the exit-code half of the reboot gate cannot see
        // and the stderr half deliberately must not (progress on stderr is
        // routine — see the test above). Together the pair is the whole rule:
        // stderr gates nothing, a *recognised marker* on stderr gates the
        // reboot.
        let (t, handle) = slmicro_target_with_stderr("h1", "System management is locked");
        // h2 is clean throughout: it keeps the reboot assertion honest in the
        // positive direction, so an empty `fired` list cannot fake h1's red.
        let (t2, h2) = slmicro_target("h2", "", 0);
        let mut group = HostsGroup::new(vec![t, t2], false);

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
        // The reboot first: it is the consequence the issue is about, so the
        // message assert must not mask it when both regress together.
        let fired = handle.fired_commands();
        assert!(
            !fired.iter().any(|c| c.contains("systemctl reboot")),
            "a host whose prepare reported a locked stack must not be rebooted: {fired:?}"
        );
        let fired2 = h2.fired_commands();
        assert!(
            fired2.iter().any(|c| c.contains("systemctl reboot")),
            "healthy h2 must still reboot so its snapshot activates: {fired2:?}"
        );
        // Exact reason *and* host: the stderr rule in `host_command_failures`
        // fires on this transcript too, so h1 is a candidate to be named twice,
        // which would put `aggregate_failures` in its summary branch where
        // `host` is `None`. `prepare_body` drops its coarse entry for a host
        // the check named, so the specific verdict survives verbatim.
        assert_eq!(err.reason, "update stack locked", "err: {err}");
        assert_eq!(err.host.as_deref(), Some("h1"), "err: {err}");
    }

    #[tokio::test]
    async fn run_checks_where_never_judges_a_host_outside_the_predicate() {
        // Scoping must happen *before* the check runs: a check calls
        // `log_failed` on its way to `Err`, so a verdict filtered out
        // afterwards has already printed an operator-visible ERROR for a host
        // whose snapshot belongs to another fan-out — once per package on the
        // `--installed-only` path. Both hosts carry the same failing
        // transcript, so only the predicate can separate them.
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

        assert_eq!(failures.len(), 1, "only the in-scope host is judged");
        assert_eq!(failures[0].host.as_deref(), Some("h1"));
        assert!(
            logs.contains("h1"),
            "the in-scope host's breadcrumb still fires: {logs}"
        );
        // The assertion a post-filter cannot satisfy: the verdict would be
        // dropped from the list, but `log_failed` would already have named h2.
        assert!(
            !logs.contains("h2"),
            "an out-of-scope host must not be judged, so it must not be logged: {logs}"
        );
    }

    #[tokio::test]
    async fn perform_prepare_names_the_host_of_a_prepare_that_never_ran() {
        // Both prepare checks raise on `-1`, and `host_command_failures` raises
        // on the same exit code, so unless the flow suppresses its coarse entry
        // one host contributes TWO failures, `aggregate_failures` leaves its
        // verbatim branch, and `host` — the field an MCP client reads to know
        // which refhost to look at — collapses to `None`. Keeping that
        // attribution while gaining #406's sharper reason is the point.
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
            // `host_command_failures`' coarse wording for this exit code, and
            // the check's sharper verdict must survive *alone*.
            assert_eq!(
                err.reason, "prepare command timed out or failed to run",
                "{product}"
            );
        }
    }

    #[tokio::test]
    async fn perform_downgrade_skips_the_reboot_of_a_failed_transactional_downgrade() {
        // The `("slmicro", true)` check catches only `-1`, so a non-zero
        // combined downgrade exercises the exit-code half of the gate: no
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
        // Exact: `contains` would also accept the multi-failure summary, whose
        // text embeds this one.
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

    /// A genuine host failure outranks a cancellation: reporting only
    /// "cancelled" would bury a broken host the operator must still act on.
    ///
    /// Both conditions have to hold at once for the rule to be reachable, and
    /// only `--installed` produces that: the probe fails the host by name while
    /// the post-probe checkpoint sets `cancelled`. It is also what pins the
    /// fall-through — an early `return` on the cancel would skip the failure
    /// scan this asserts on.
    #[tokio::test(start_paused = true)]
    async fn prepare_reports_the_host_failure_not_the_cancel() {
        let conn = MockConnection::new("h1")
            .with_default(CommandLog::new("", "", "", 0, 0))
            .with_response(
                INSTALLED_PROBE.to_owned(),
                CommandLog::new(INSTALLED_PROBE, "", "", 7, 0),
            )
            .with_run_delay(std::time::Duration::from_millis(10))
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
        // Cancelled while the probe is in flight, so the pre-probe checkpoint
        // lets it run and the post-probe one fires.
        let token = group.cancel_token();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            token.cancel();
        });

        let err = perform_prepare(
            &mut group,
            &NoopRepo,
            &["pkg-a".to_owned()],
            false,
            false,
            true,
        )
        .await
        .expect_err("a dead probe must not report success");

        assert!(
            handle.commands().iter().any(|c| c == INSTALLED_PROBE),
            "the probe must have run: {:?}",
            handle.commands()
        );
        assert!(
            !err.is_cancelled(),
            "the host's own failure must outrank the cancel: {err:?}"
        );
        assert_eq!(err.host.as_deref(), Some("h1"), "err: {err}");
        assert_eq!(err.reason, "package probe failed", "err: {err}");
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
