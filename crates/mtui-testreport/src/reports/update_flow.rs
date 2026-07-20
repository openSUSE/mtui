//! The bespoke (non-template) update flows: `perform_prepare`,
//! `perform_downgrade`, `perform_update`.
//!
//! ## Reference
//!
//! Ports upstream `HostsGroup.perform_prepare` / `perform_downgrade` /
//! `perform_update`. Unlike install/uninstall (which route through the shared
//! [`Operation`](mtui_hosts::Operation) template), these three are deliberately
//! open-coded upstream — they have per-package loops, `set_repo` add/remove
//! fan-outs, package-version comparison, and (for `update`) a two-phase
//! try/finally that guarantees repo cleanup on success while **keeping** the
//! test repos on failure for retry/diagnosis.
//!
//! ## Crate boundary
//!
//! Upstream hangs these off `HostsGroup`, but that class also owns
//! `get_package_list` / `set_repo`, which in the Rust split live in
//! `mtui-testreport`. Putting the flows here (as the concrete reports'
//! `perform_*` bodies, alongside `perform_install`) keeps `mtui-hosts` free of a
//! `mtui-testreport` dependency and reuses the report's own [`SetRepo`] hook and
//! package list. The flows resolve each host's command templates directly from
//! the [`WorkflowRegistry`] (`ActionCommands` + `CheckFn`) — the same tables the
//! `PlanProvider` adapter uses — keyed on `(system.get_release(),
//! transactional)`, mirroring upstream `Target.doer(role)` / `Target.check(role)`.

use std::collections::{BTreeMap, HashMap};

use mtui_hosts::{Command, HostsGroup, OperationGroup, RepoOp, SetRepo};
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
    /// A concrete target has no updater doer (upstream `MissingUpdaterError`);
    /// unlike upstream — which logs and returns as if successful — mtui-rs treats
    /// this as a hard failure so a target that cannot be updated never reports
    /// "finished".
    MissingUpdater(UpdateError),
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
    let maintenance_id = rrid.maintenance_id.clone();
    let review_id = rrid.review_id.to_string();
    let packages = report.get_package_list();
    perform_update(
        targets,
        report,
        &packages,
        &maintenance_id,
        &review_id,
        noprepare,
        newpackage,
        diagnostics,
    )
    .await
}

/// Drives [`perform_update_from_report`] and, on a *check* failure, rolls the
/// packages back via [`perform_downgrade`] before re-surfacing the original
/// error — the port of upstream `testreport.perform_update`'s
/// `except UpdateError: ... perform_downgrade(...); raise`.
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
        Err(UpdateFailure::MissingUpdater(e)) => {
            // Hard fail, but nothing was installed → no rollback.
            error!(error = %e, "update failed");
            Err(e)
        }
        Err(UpdateFailure::Check(e)) => {
            error!("Update failed");
            warn!("Error while updating. Rolling back changes");
            let pkgs = report.get_package_list();
            let id = report.base().rrid.as_ref().map(ToString::to_string);
            add_op_history(targets, "downgrade", id.as_deref(), &pkgs).await;
            // Rollback is best-effort; a failed downgrade must never bury the
            // original update error, so its result is logged, not returned.
            if let Err(de) = perform_downgrade(targets, report, &pkgs).await {
                warn!(error = %de, "rollback downgrade failed");
            }
            Err(e)
        }
    }
}

/// Records a workflow op in every target's remote history file before fan-out.
///
/// Ports upstream `testreport.perform_*`'s `targets.add_history([...])` calls
/// (`test_reports/testreport.py`). `id_field` carries the RRID for ops that log
/// it (`update`/`downgrade`) and is `None` for `install`/`uninstall`, matching
/// upstream's differing field lists. The op label and package list complete the
/// colon-joined line written by [`HostsGroup::add_history`].
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

/// The `$repa` maintenance-selector for an update, mirroring upstream
/// `f":p={rrid.maintenance_id}:{rrid.review_id}"`.
fn repa_for(maintenance_id: &str, review_id: &str) -> String {
    format!(":p={maintenance_id}:{review_id}")
}

/// Resolves a host's `(release, transactional)` key from its parsed system.
///
/// Returns `None` when the system has no release (an unknown/unparsed host),
/// which upstream surfaces as the role's `Missing*Error`; the callers treat a
/// `None` as "no doer for this host".
fn host_key(target: &mtui_hosts::Target) -> Option<(String, bool)> {
    let release = target.system().get_release().ok()?;
    Some((release, target.transactional()))
}

/// Resolves one host's [`ActionCommands`] for `role`, or logs and returns `None`
/// on a missing doer (mirroring upstream's `logger.error("%s", e)` +
/// early-return semantics).
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
/// Mirrors upstream's `reboot = {t.hostname: doer["reboot"].substitute() for t
/// in group if t.transactional}`. Returns `Err` if any transactional host is
/// missing a doer (upstream lets the `Missing*Error` abort before the lock),
/// so the caller can early-return without locking.
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
/// [`UpdateError`]s (upstream iterates `t.check(role)(...)`) and appending any
/// recognised-but-non-fatal [`Diagnostic`] sections to `diagnostics`.
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

/// Reports whether a shared install/uninstall [`Operation`](mtui_hosts::Operation)
/// fan-out succeeded on every host.
///
/// The template's own per-host check lives in `mtui-hosts` and only logs; this
/// gives the report-level `perform_install`/`perform_uninstall` a returned
/// verdict by scanning each host's post-fan-out `lasterr()`/`lastexit()`
/// snapshot (bead P3a-1's stable outcome accessors). `op` labels the aggregated
/// summary. Public so the report impls (SL/PI/OBS) can call it after
/// `Operation::run`.
pub fn install_verdict(op: &str, targets: &HostsGroup) -> Result<(), UpdateError> {
    aggregate_failures(
        op,
        host_command_failures(targets, &format!("{op} command failed")),
    )
}

/// Reboots the transactional hosts named in `reboot` (upstream `group._reboot`).
async fn reboot_transactional(targets: &mut HostsGroup, reboot: BTreeMap<String, String>) {
    if reboot.is_empty() {
        return;
    }
    let map: Vec<(String, String)> = reboot.into_iter().collect();
    OperationGroup::reboot(targets, map).await;
}

/// Ports upstream `HostsGroup.perform_prepare`.
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
    // Upstream drops branding-upstream from the prepare set.
    let pkgs: Vec<String> = packages
        .iter()
        .filter(|p| *p != "branding-upstream")
        .cloned()
        .collect();

    // Resolve the reboot map before locking; a missing preparer aborts early
    // (upstream's `except MissingPreparerError: return`).
    let Ok(reboot) = build_reboot_map(targets, &registry, Role::Prepare) else {
        return Err(UpdateError::reason_only("missing preparer"));
    };

    if let Err(e) = targets.update_lock().await {
        return Err(UpdateError::reason_only(e.to_string()));
    }

    // From here upstream guarantees `unlock()` via `finally`; we mirror that by
    // running the body then always unlocking.
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
/// `unlock()` runs unconditionally (upstream's `finally`).
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

    if installed_only {
        // Conditional per-package install — inherently one package at a time.
        for pkg in pkgs {
            let quoted = quote_args(std::slice::from_ref(pkg));
            let cmd = build_prepare_map(targets, registry, Some(&quoted), true);
            targets.run(Command::PerHost(cmd)).await;
        }
    } else if !pkgs.is_empty() {
        // Install every package in a SINGLE transaction (one snapshot for
        // transactional hosts). Quote each name for the root command line.
        let joined = quote_args(pkgs);
        let cmd = build_prepare_map(targets, registry, Some(&joined), false);
        targets.run(Command::PerHost(cmd)).await;
    }

    // Surface any per-host command failure from the install fan-out plus the
    // prepare check's own failures. The prepare check emits no diagnostics; the
    // sink is discarded.
    let mut failures = host_command_failures(targets, "prepare command failed");
    for e in run_checks(targets, registry, Role::Prepare, &mut Vec::new()) {
        error!(error = %e, "prepare check failed");
        failures.push(e);
    }
    reboot_transactional(targets, reboot).await;
    aggregate_failures("prepare", failures)
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

/// Ports upstream `HostsGroup.perform_downgrade`.
pub async fn perform_downgrade(
    targets: &mut HostsGroup,
    report: &dyn SetRepo,
    packages: &[String],
) -> Result<(), UpdateError> {
    // Nothing to downgrade: return before locking or touching repos (upstream
    // PR #336). The guard also keeps the probe template from rendering with an
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

    let result = downgrade_body(targets, &registry, report, packages, reboot).await;
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

    // A dead probe must abort that host's downgrade, not degrade it (upstream
    // PR #336). When the probe dies (an SSH no-output timeout records exit -1)
    // its stdout is empty, the version map below stays empty, and the flow would
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
    for package in packages {
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
            // the healthy hosts' rollback (upstream PR #336).
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
    if !combined.is_empty() {
        targets.run(Command::PerHost(combined.clone())).await;
        // Same scoping as the per-package loop: only check the transactional
        // hosts that ran this combined command.
        for e in run_checks(targets, registry, Role::Downgrade, &mut Vec::new()) {
            let host = e.host.as_deref().unwrap_or("");
            if combined.contains_key(host) && transactional_hosts.contains(host) {
                error!(error = %e, "downgrade check failed");
                failures.push(e);
            }
        }
    }

    // Reboot the healthy transactional hosts first (their staged snapshots must
    // still activate), then surface the dead probes as the command's failure
    // (upstream PR #336).
    let healthy_reboot: BTreeMap<String, String> = reboot
        .into_iter()
        .filter(|(h, _)| !dead_probes.contains(h))
        .collect();
    reboot_transactional(targets, healthy_reboot).await;

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

    Ok(())
}

/// Emits the post-downgrade "done" / "downgrade not completed" verdict.
///
/// Ports upstream PR #336's `commands/downgrade.py` post-loop: re-query each
/// host, rotate `before = after; after = current` per package, then compare each
/// package's re-queried `current` against the update's `required` version. Every
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
    // Query every host's versions concurrently (serial hosts one at a time)
    // via the shared fan-out, then run the pure verdict scan below.
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
/// map, matching upstream's `(.*) = (.*)` regex + `sorted(..., key=RPMVersion,
/// reverse=True)[0]` selection.
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

/// Ports upstream `HostsGroup.perform_update`.
///
/// `packages` is the report's package list; `maintenance_id`/`review_id` build
/// the `$repa` selector. `noprepare` skips the initial prepare; `newpackage`
/// runs a testing prepare after the update. `prepare` is the closure the caller
/// uses to run [`perform_prepare`] (the report drives it so this module does not
/// need to know the report type). `diagnostics` collects the update check's
/// recognised-but-non-fatal output sections for the command layer to render.
// Ports upstream's positional `perform_update` signature plus the diagnostic
// sink threaded from the display-owning command layer; grouping into a struct
// would obscure the 1:1 upstream mapping for no real gain.
#[allow(clippy::too_many_arguments)]
pub async fn perform_update(
    targets: &mut HostsGroup,
    report: &dyn SetRepo,
    packages: &[String],
    maintenance_id: &str,
    review_id: &str,
    noprepare: bool,
    newpackage: bool,
    diagnostics: &mut Vec<Diagnostic>,
) -> Result<(), UpdateFailure> {
    let registry = WorkflowRegistry::default();

    if !noprepare {
        // Upstream: `perform_prepare(get_package_list(), testreport)` (default
        // flags: remove-repo prepare). Prepare is best-effort within the update
        // flow (upstream logs and proceeds), so a failure is logged, not
        // returned.
        if let Err(e) = perform_prepare(targets, report, packages, false, false, false).await {
            warn!(error = %e, "prepare before update failed");
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
            // MissingUpdaterError: remove the repo we just added and abort. Unlike
            // upstream (log + return as success), a target with no updater doer is
            // a hard failure so it never reports "finished".
            targets.fanout_set_repo(RepoOp::Remove, report).await;
            targets.unlock().await;
            return Err(UpdateFailure::MissingUpdater(e));
        }
    };

    // Two-phase: run + check + reboot under the lock (unlock always), then the
    // repo cleanup only on success.
    let update_result = update_run_phase(targets, &registry, commands, reboot, diagnostics).await;

    if let Err(e) = update_result {
        // KEEP the test update repositories in place for retry/diagnosis.
        warn!(
            "update did not complete; leaving the test update repositories in place \
             for retry/diagnosis (remove later with `set_repo --remove`)"
        );
        return Err(UpdateFailure::Check(e));
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
/// success, and **always** unlocks (upstream's inner `finally`).
///
/// Returns `Ok(())` when every host's check passed, otherwise `Err` with the
/// aggregated [`UpdateError`]. The aggregation mirrors upstream
/// (`hostgroup.py:661-667`): a single failure is returned verbatim; more than
/// one is summarised into `"update failed on {hosts} ({detail})"`.
async fn update_run_phase(
    targets: &mut HostsGroup,
    registry: &WorkflowRegistry,
    commands: BTreeMap<String, String>,
    reboot: BTreeMap<String, String>,
    diagnostics: &mut Vec<Diagnostic>,
) -> Result<(), UpdateError> {
    targets.run(Command::PerHost(commands)).await;

    let mut failures = run_checks(targets, registry, Role::Update, diagnostics);
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

    let result = if failures.is_empty() {
        reboot_transactional(targets, reboot).await;
        Ok(())
    } else if failures.len() == 1 {
        // Preserve the exact single-host error the caller used to get.
        Err(failures.remove(0))
    } else {
        let hosts: Vec<String> = {
            let mut h: Vec<String> = failures.iter().filter_map(|e| e.host.clone()).collect();
            h.sort();
            h
        };
        let detail: Vec<String> = failures.iter().map(ToString::to_string).collect();
        Err(UpdateError::reason_only(format!(
            "update failed on {} ({})",
            hosts.join(", "),
            detail.join("; ")
        )))
    };

    targets.unlock().await;
    result
}

/// Builds the per-host updater command map (with `$repa` + `$packages`) and the
/// transactional reboot map. Returns `Err` with the offending host's
/// [`UpdateError`] if any host is missing an updater (upstream's
/// `MissingUpdaterError`) — a hard failure in mtui-rs.
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
    use mtui_types::enums::{ExecutionMode, TargetState};
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
        let mut t = Target::with_connection(
            hostname,
            TargetState::Enabled,
            ExecutionMode::Parallel,
            Box::new(conn),
        );
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
    fn repa_for_matches_upstream_format() {
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
        let mut t = Target::with_connection(
            hostname,
            TargetState::Enabled,
            ExecutionMode::Parallel,
            Box::new(conn),
        );
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
        let res = perform_downgrade(&mut group, &NoopRepo, &["pkg-a".to_owned()]).await;
        let err = res.expect_err("missing downgrader must surface as Err");
        assert!(
            err.reason.contains("missing downgrader"),
            "reason: {}",
            err.reason
        );
    }

    #[tokio::test]
    async fn install_verdict_surfaces_per_host_command_failure() {
        // A host left with a non-zero lastexit after an install fan-out is
        // reported by install_verdict (the report-level install/uninstall hook).
        let (t, _h) = sles_target_with_exit("h1", "", 104);
        let mut group = HostsGroup::new(vec![t], false);
        // Run one command so the host records its (failing) last* snapshot.
        group.run(Command::All("zypper in".to_owned())).await;
        let err = install_verdict("install", &group).expect_err("non-zero exit surfaces as Err");
        assert_eq!(err.host.as_deref(), Some("h1"));
        assert!(
            err.reason.contains("install command failed"),
            "reason: {}",
            err.reason
        );
    }

    #[tokio::test]
    async fn install_verdict_ok_when_all_hosts_succeed() {
        let (t, _h) = sles_target("h1", "");
        let mut group = HostsGroup::new(vec![t], false);
        group.run(Command::All("zypper in".to_owned())).await;
        assert!(install_verdict("install", &group).is_ok());
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
        // An unknown release has no updater doer. mtui-rs treats this as a hard
        // fail: Err(MissingUpdater), no updater command issued, and the repo the
        // flow added is removed on the abort path.
        let conn = MockConnection::new("h1").with_default(CommandLog::new("", "", "", 0, 0));
        let handle = conn.clone();
        let mut t = Target::with_connection(
            "h1",
            TargetState::Enabled,
            ExecutionMode::Parallel,
            Box::new(conn),
        );
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

        let res = perform_downgrade(&mut group, &NoopRepo, &["pkg-a".to_owned()]).await;
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
        // template with zero names would list the entire catalog (upstream #336).
        let (t, handle) = sles_target("h1", "pkg-a = 1.0-1\n");
        let mut group = HostsGroup::new(vec![t], false);

        let res = perform_downgrade(&mut group, &NoopRepo, &[]).await;
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
        // "completing" with zero downgrade commands run (upstream PR #336).
        let (t, handle) = sles_target_with_exit("h1", "", -1);
        let mut group = HostsGroup::new(vec![t], false);

        let res = perform_downgrade(&mut group, &NoopRepo, &["pkg-a".to_owned()]).await;
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
        // skipped but h1 still rolls back; the error names only h2 at the end
        // (upstream PR #336).
        let (t1, h1) = sles_target("h1", "pkg-a = 1.0-1\n");
        let (t2, h2) = sles_target_with_exit("h2", "", -1);
        let mut group = HostsGroup::new(vec![t1, t2], false);

        let res = perform_downgrade(&mut group, &NoopRepo, &["pkg-a".to_owned()]).await;
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
        // version ⇒ current >= required ⇒ named as not-downgraded (upstream
        // PR #336). The bookkeeping still rotates before/after for it.
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
        // Two hosts each still at the update version: BOTH are named, not just
        // the first (upstream PR #336 removed the short-circuit).
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
        let conn =
            MockConnection::new(hostname).with_default(CommandLog::new("", stdout, "", exit, 0));
        let handle = conn.clone();
        let mut t = Target::with_connection(
            hostname,
            TargetState::Enabled,
            ExecutionMode::Parallel,
            Box::new(conn),
        );
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
            true,
            false,
            &mut Vec::new(),
        )
        .await;
        assert!(res.is_ok(), "successful update returns Ok: {res:?}");

        // The slmicro updater is transactional, so a reboot command is fired on
        // success (upstream `_reboot`); reboot uses fire-and-forget, recorded
        // separately from the run log.
        let fired = handle.fired_commands();
        assert!(
            fired.iter().any(|c| c.contains("systemctl reboot")),
            "expected transactional reboot after a successful update: {fired:?}"
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
            true,
            false,
            &mut Vec::new(),
        )
        .await;
        let err = match res {
            Err(UpdateFailure::Check(e)) => e,
            other => panic!("multi-host failure returns Err(Check): {other:?}"),
        };
        // Aggregated message names both hosts (sorted) — upstream's summary form.
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
        // The downgrade version probe must exit 0 (a non-zero probe exit is now a
        // dead-probe abort, upstream PR #336); the shared `sles_target_with_exit`
        // would apply 104 to the probe too, so script the probe explicitly.
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
        let mut t = Target::with_connection(
            "h1",
            TargetState::Enabled,
            ExecutionMode::Parallel,
            Box::new(conn),
        );
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
    async fn perform_downgrade_transactional_host_combines_into_one_command() {
        // A transactional host downgrades ALL packages in a single command.
        let (t, handle) = slmicro_target("h1", "pkg-a = 1.0-1\npkg-b = 2.0-1\n", 0);
        let mut group = HostsGroup::new(vec![t], false);

        let res = perform_downgrade(
            &mut group,
            &NoopRepo,
            &["pkg-a".to_owned(), "pkg-b".to_owned()],
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
    }

    /// Like [`sles_target`] but with a custom exit code for every command. The
    /// recorded command carries `"zypper"` so the update check (which keys on
    /// `stdin.contains("zypper")`) sees a zypper command in `lastin`.
    fn sles_target_with_exit(hostname: &str, stdout: &str, exit: i16) -> (Target, MockConnection) {
        let conn = MockConnection::new(hostname)
            .with_default(CommandLog::new("zypper", stdout, "", exit, 0));
        let handle = conn.clone();
        let mut t = Target::with_connection(
            hostname,
            TargetState::Enabled,
            ExecutionMode::Parallel,
            Box::new(conn),
        );
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
