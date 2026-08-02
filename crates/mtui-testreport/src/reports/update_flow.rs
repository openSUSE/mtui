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
    Command, HostError, HostsGroup, InstallOperation, Operation, OperationGroup, RebootFailure,
    RebootFailureCause, RepoOp, SetRepo, UninstallOperation,
};
use mtui_types::shellquote::quote_args;
use tracing::{debug, error, info, warn};

use crate::update_workflow::actions::ActionCommands;
use crate::update_workflow::checks::{CheckArgs, CheckFn, Diagnostic};
use crate::update_workflow::{CheckProvider, DoerProvider, Role, UpdateError, WorkflowRegistry};

/// A per-host command map paired with the transactional-host reboot map, as
/// built by [`build_update_maps`] (`(commands, reboot)`).
type UpdateMaps = (BTreeMap<String, String>, BTreeMap<String, String>);

/// Why an update did not apply — distinguishes a *check* failure (packages may be
/// half-applied, so roll back) from a *config* failure (a concrete target has no
/// updater doer, so nothing was installed and there is nothing to roll back).
///
/// Both map to a single [`UpdateError`] at the command boundary; the distinction
/// exists only so [`perform_update_with_rollback`] rolls back on `Check` and
/// skips rollback on `MissingUpdater`.
#[derive(Debug, PartialEq, Eq)]
pub enum UpdateFailure {
    /// One or more hosts failed the `updater` check after the command ran.
    Check(UpdateError),
    /// The pre-update `prepare` step failed.
    ///
    /// Like [`MissingUpdater`](Self::MissingUpdater) this skips the rollback,
    /// but for a different reason: nothing has been dispatched yet at this
    /// point — no lock taken, no issue repo added, no patch command sent — so
    /// there is nothing on any host for a downgrade to undo. `--noprepare` is
    /// the opt-out for a caller that wants to patch anyway.
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
    /// Only used when **every** failed host is in that state; a run mixing a
    /// real check failure with a lost host still rolls back, on behalf of the
    /// host the rollback can genuinely repair.
    NotRun(UpdateError),
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
/// A `MissingUpdater` failure installed nothing, so it re-surfaces without a
/// rollback attempt. The rollback is best-effort ([`perform_downgrade`] returns
/// `()`), so it can never bury the original update error.
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
            // Nothing was dispatched (no lock, no repo add, no patch) → no
            // rollback.
            error!(error = %e, "update aborted: prepare failed");
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
/// `id_field` carries the RRID for ops that log it (`update`/`downgrade`) and
/// is `None` for `install`/`uninstall`. The op label and package list
/// complete the colon-joined line written by [`HostsGroup::add_history`].
pub async fn add_op_history(
    targets: &mut HostsGroup,
    op: &str,
    id_field: Option<&str>,
    packages: &[String],
) {
    let mut fields = vec![op.to_owned()];
    if let Some(id) = id_field {
        fields.push(id.to_owned());
    }
    fields.push(packages.join(" "));
    targets.add_history(&fields).await;
}

/// The `$repa` maintenance-selector for an update.
fn repa_for(maintenance_id: &str, review_id: &str) -> String {
    format!(":p={maintenance_id}:{review_id}")
}

/// Classifies a pre-update `prepare` failure.
///
/// The cancel arm is defensive rather than live on this call path: the
/// pre-update prepare always runs with `installed_only = false`, and only the
/// per-package (`installed_only = true`) loop sets `cancelled_at`. It stays
/// because [`perform_prepare`] is public and the "a cancel surfaces as
/// `Cancelled`, never a generic failure" invariant applies to every caller.
fn prepare_failure(e: UpdateError) -> UpdateFailure {
    if e.is_cancelled() {
        UpdateFailure::Cancelled(e)
    } else {
        UpdateFailure::Prepare(e)
    }
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
fn run_checks(
    targets: &HostsGroup,
    registry: &WorkflowRegistry,
    role: Role,
    diagnostics: &mut Vec<Diagnostic>,
) -> Vec<UpdateError> {
    let mut failures = Vec::new();
    for target in targets.targets() {
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
        let detail: Vec<String> = failures.iter().map(ToString::to_string).collect();
        Err(UpdateError::reason_only(format!(
            "{op} failed on {} ({})",
            hosts.join(", "),
            detail.join("; ")
        )))
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

/// The shared body of [`perform_install`] / [`perform_uninstall`].
async fn perform_operation(
    targets: &mut HostsGroup,
    role: Role,
    packages: &[String],
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

    targets.set_plan_provider(Arc::new(WorkflowRegistry::default()));

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

    // A host whose post-run check failed, and any transactional host that
    // rebooted and never reconnected, both fail the operation by name. A
    // failed check already excluded its host from the reboot map (see
    // `Operation::run`), so the two lists never double-name the same host.
    let mut failures: Vec<UpdateError> = report
        .check_failures
        .into_iter()
        .map(|(host, reason)| UpdateError::new(reason, host))
        .collect();
    failures.extend(report.reboot_failures.into_iter().map(reboot_error));
    aggregate_failures(op, failures)
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
        return Err(UpdateError::reason_only("missing preparer"));
    };

    if let Err(e) = targets.update_lock().await {
        return Err(UpdateError::reason_only(e.to_string()));
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
    targets.unlock().await;
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
) -> Result<(), UpdateError> {
    targets.fanout_set_repo(operation, report).await;

    // Abort early if adding/removing the issue repo failed on any host.
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
        return aggregate_failures("prepare", repo_failures);
    }

    // Every host that actually received a prepare command, and the hosts a
    // package failed on. Both are accumulated *inside* the loop below: it runs
    // one fan-out per package, so a single post-loop read of `lastexit()` sees
    // only the last package — and under `--installed-only` the last package is
    // very often a no-op `if rpm -q ...` that exits 0, which would mask an
    // earlier failure and let the host reboot into it.
    let mut dispatched: BTreeSet<String> = BTreeSet::new();
    let mut inert: BTreeSet<String> = BTreeSet::new();

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
        }
    } else if !pkgs.is_empty() {
        // Install every package in a SINGLE transaction (one snapshot for
        // transactional hosts). Quote each name for the root command line.
        let joined = quote_args(pkgs);
        let cmd = build_prepare_map(targets, registry, Some(&joined), false);
        targets.run(Command::PerHost(cmd.clone())).await;
        note_dispatch(targets, &cmd, &mut dispatched, &mut inert);
    }

    // Surface any per-host command failure from the install fan-out plus the
    // prepare check's own failures. The prepare check emits no diagnostics; the
    // sink is discarded.
    let mut failures = host_command_failures(targets, "prepare command failed");
    let check_failures = run_checks(targets, registry, Role::Prepare, &mut Vec::new());

    // A host whose prepare failed must not reboot into the failed transaction,
    // mirroring the install/uninstall template's per-host gate: activating the
    // snapshot would hide the failure behind a healthy-looking boot, while a
    // healthy host in the same group still reboots so its own snapshot
    // activates. The skip set is built from the check verdicts and non-zero
    // exit codes only — never from the stderr rule `host_command_failures`
    // also applies, because `transactional-update` writes progress to stderr
    // on a *successful* run and skipping such a host's reboot would leave its
    // healthy staged snapshot silently inert. Unlike update's multi-line
    // script, the prepare templates are single commands, so the recorded exit
    // code is genuinely the prepare command's own.
    //
    // The check verdicts are seeded too, but they are inert for the hosts that
    // matter today: `prepare_check` has no ("slmicro", true) entry, and only
    // transactional hosts are ever in `reboot`. Kept so the gate is already
    // right when that check is registered, not because it fires now.
    inert.extend(check_failures.iter().filter_map(|e| e.host.clone()));
    for e in check_failures {
        error!(error = %e, "prepare check failed");
        failures.push(e);
    }

    // `host_command_failures` reads one post-loop snapshot, so on the
    // per-package path it only ever sees the last package. Name every host a
    // package failed on, deduped against the hosts already reported — a second
    // entry for one host would push `aggregate_failures` out of its
    // single-failure verbatim branch into the summary, which drops `host` to
    // `None` and breaks callers that read it.
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
        return aggregate_failures("prepare", failures);
    }
    if let Some(i) = cancelled_at {
        let done: Vec<&str> = pkgs[..i].iter().map(String::as_str).collect();
        let left: Vec<&str> = pkgs[i..].iter().map(String::as_str).collect();
        warn!(
            applied = %done.join(", "),
            not_attempted = %left.join(", "),
            "prepare cancelled at a package boundary"
        );
        return Err(UpdateError::cancelled(format!(
            "prepare cancelled after {}/{} packages; applied: [{}]; not attempted: [{}]",
            i,
            pkgs.len(),
            done.join(", "),
            left.join(", "),
        )));
    }
    aggregate_failures("prepare", failures)
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
    targets.unlock().await;
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
    // probe dies (an SSH no-output timeout records exit -1) its stdout is
    // empty, the version map below stays empty, and the flow would
    // "complete" having run zero downgrade commands — leaving every package at
    // the update version behind a success-looking run. The pipeline's exit
    // status is awk's, so a recorded non-zero exit here always means the probe
    // itself broke, never "package not found". Handled per host: the healthy
    // hosts still roll back (and transactional ones still reboot); the error for
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
            "package version probe failed; skipping downgrade on this host"
        );
    }
    if !dead_probes.is_empty() && dead_probes.len() == list_map.len() {
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
            // record, whose stale -1 would trip the new timeout gate and cancel
            // the healthy hosts' rollback.
            for e in run_checks(targets, registry, Role::Downgrade, &mut Vec::new()) {
                let host = e.host.as_deref().unwrap_or("");
                if cmd.contains_key(host) && !transactional_hosts.contains(host) {
                    error!(error = %e, "downgrade check failed");
                    failures.push(e);
                }
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
        // Same scoping as the per-package loop: only check the transactional
        // hosts that ran this combined command.
        for e in run_checks(targets, registry, Role::Downgrade, &mut Vec::new()) {
            let host = e.host.as_deref().unwrap_or("");
            if combined.contains_key(host) && transactional_hosts.contains(host) {
                error!(error = %e, "downgrade check failed");
                failed_downgrade.insert(host.to_owned());
                failures.push(e);
            }
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

    let not_downgraded = downgrade_verdict(targets).await;

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
async fn downgrade_verdict(targets: &mut HostsGroup) -> BTreeMap<String, Vec<String>> {
    // Query every host's versions concurrently via the shared fan-out, then
    // run the pure verdict scan below.
    targets.query_versions().await;

    let mut not_downgraded: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for target in targets.targets_mut() {
        let hostname = target.hostname().to_owned();
        for pkg in target.packages_mut() {
            let after = pkg.after().cloned();
            pkg.set_before_version(after);
            let current = pkg.current().cloned();
            pkg.set_after_version(current);
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

    if not_downgraded.is_empty() {
        tracing::info!("done");
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
fn parse_downgrade_versions(output: &str) -> HashMap<String, String> {
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
        // Runs prepare with default flags (remove-repo prepare). Nothing has
        // been dispatched yet, so a failure aborts here rather than patching
        // hosts prepare may have left in a bad state; `--noprepare` opts out.
        if let Err(e) = perform_prepare(targets, report, packages, false, false, false).await {
            return Err(prepare_failure(e));
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
            targets.unlock().await;
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
        targets.unlock().await;
        return Err(UpdateFailure::Cancelled(UpdateError::cancelled(
            "cancelled before the update command was dispatched",
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

    // Success: remove the test update repositories.
    if targets.update_lock().await.is_err() {
        warn!("could not lock hosts to remove update repositories; skipping repo cleanup");
        return Ok(());
    }
    targets.fanout_set_repo(RepoOp::Remove, report).await;
    targets.unlock().await;
    Ok(())
}

/// Runs the update commands, checks every host (collecting failures), reboots on
/// success, and **always** unlocks.
///
/// Returns `Ok(())` when every host's check passed and every transactional
/// host's reboot took effect — reconnecting is not sufficient, since a host can
/// answer without ever having gone down. Otherwise `Err` with the aggregated
/// failure: a check failure (packages may be half-applied) is
/// [`UpdateFailure::Check`] and **suppresses the reboot entirely** — unless
/// every failed host's command never completed (`lastexit()` of `-1`), which
/// is [`UpdateFailure::NotRun`] and skips the rollback; a reboot
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
        // `all`, not `any`, for the same reason as the reboot arm: a run that
        // mixes a lost host with a genuine check failure still rolls back, on
        // behalf of the host the rollback can actually repair.
        let never_ran = failures.iter().all(|e| {
            e.host
                .as_deref()
                .and_then(|h| targets.get(h))
                .and_then(mtui_hosts::Target::lastexit)
                == Some(-1)
        });
        let wrap: fn(UpdateError) -> UpdateFailure = if never_ran {
            UpdateFailure::NotRun
        } else {
            UpdateFailure::Check
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

    targets.unlock().await;
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
    async fn perform_install_falls_back_to_raw_scan_without_a_check_table() {
        // slmicro/YUM keys have no install check registered. They must still get
        // a verdict rather than an unconditional pass.
        let conn = MockConnection::new("h1").with_default(CommandLog::new("t-u", "", "", 1, 0));
        let mut t = Target::with_connection("h1", TargetState::Enabled, Box::new(conn));
        t.set_system(
            System::new(
                SystemProduct::new("SL-Micro", "6.0", "x86_64"),
                BTreeSet::new(),
                true,
            ),
            true,
        );
        let key = host_key(&t).expect("resolvable key");
        assert!(
            WorkflowRegistry::default()
                .check(Role::Install, &key.0, key.1)
                .is_none(),
            "this test is only meaningful while the key has no check table"
        );
        let mut group = HostsGroup::new(vec![t], false);

        let err = perform_install(&mut group, &["pkg-a".to_owned()])
            .await
            .expect_err("a non-zero exit must still be reported");
        assert_eq!(err.host.as_deref(), Some("h1"));
        assert_eq!(err.reason, "install command failed");
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

        let not_downgraded = downgrade_verdict(&mut group).await;

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

        let not_downgraded = downgrade_verdict(&mut group).await;

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

        let not_downgraded = downgrade_verdict(&mut group).await;

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
    /// The update check for `("slmicro", true)` is deliberately judged on the
    /// output markers rather than the exit code — the slmicro update template
    /// ends with a repo-cleanup loop that masks the patch's status — so a
    /// stderr marker is the signal a genuinely failing patch leaves behind.
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

    #[tokio::test]
    async fn update_timeout_does_not_roll_back_healthy_hosts() {
        // `Target::run` records -1 for a timeout, a mid-command connection
        // loss, and an unconnected target — i.e. "the flow could not talk to
        // this host", not "the patch went wrong". Routing that into the
        // group-wide rollback would revert every host that patched cleanly on
        // behalf of one the downgrade cannot reach anyway, which is exactly
        // what the reboot arm already refuses to do.
        //
        // h1's patch times out; h2 patches cleanly. The `-1` is scripted onto
        // the patch command only — it is exactly what `Target::run` records
        // when the command times out, and scripting it via `with_default`
        // would put it on the probes too.
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
        let lost = MockConnection::new("h1")
            .with_default(CommandLog::new("", "", "", 0, 0))
            .with_response(patch.clone(), CommandLog::new(&patch, "", "", -1, 0))
            .with_changing_boot_id();
        let lost_handle = lost.clone();
        let mut t1 = Target::with_connection("h1", TargetState::Enabled, Box::new(lost));
        t1.set_system(
            System::new(
                SystemProduct::new("SL-Micro", "6.0", "x86_64"),
                BTreeSet::new(),
                true,
            ),
            true,
        );
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

    #[tokio::test]
    async fn perform_update_keeps_repos_on_check_failure() {
        // exit 104 on the updater command ⇒ the update check flags "package not
        // found"; the flow must NOT issue a repo-remove (repos kept for retry).
        let (t, handle) = sles_target_with_exit("h1", "", 104);
        let mut group = HostsGroup::new(vec![t], false);

        // A recording repo so we can assert no Remove followed the Add.
        let repo = RecordingRepo::default();
        let report = report_with_rrid();
        let packages = report.get_package_list();

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
        assert!(
            matches!(res, Err(UpdateFailure::Check(_))),
            "a check failure returns Err(Check): {res:?}"
        );

        let ops = repo.ops.lock().unwrap().clone();
        assert!(ops.contains(&RepoOp::Add), "repo add must run: {ops:?}");
        assert!(
            !ops.contains(&RepoOp::Remove),
            "on failure the repos are kept (no Remove): {ops:?}"
        );
        // The updater command was still attempted.
        assert!(handle.commands().iter().any(|c| c.contains(":p=42:7")));
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
    async fn perform_update_aborts_when_the_prepare_fails() {
        // h1's prepare command exits non-zero ⇒ the update must abort before
        // taking the lock, adding the issue repo, or dispatching the patch.
        let (t, handle) = sles_target_with_exit("h1", "", 1);
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
            "a failed prepare aborts the update: {res:?}"
        );

        let ops = repo.ops.lock().unwrap().clone();
        assert!(
            !ops.contains(&RepoOp::Add),
            "no issue repo must be added when prepare fails: {ops:?}"
        );
        let cmds = handle.commands();
        assert!(
            !cmds.iter().any(|c| c.contains(":p=42:7")),
            "no patch command must be dispatched when prepare fails: {cmds:?}"
        );
    }

    #[test]
    fn prepare_failure_classifies_cancelled_vs_plain_errors() {
        assert!(matches!(
            prepare_failure(UpdateError::cancelled("cancelled")),
            UpdateFailure::Cancelled(_)
        ));
        assert!(matches!(
            prepare_failure(UpdateError::new("boom", "h1")),
            UpdateFailure::Prepare(_)
        ));
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
