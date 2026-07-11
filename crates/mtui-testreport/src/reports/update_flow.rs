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
            perform_downgrade(targets, report, &pkgs).await;
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
) {
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
        return;
    };

    if targets.update_lock().await.is_err() {
        return;
    }

    // From here upstream guarantees `unlock()` via `finally`; we mirror that by
    // running the body then always unlocking.
    prepare_body(
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
) {
    targets.fanout_set_repo(operation, report).await;

    // Abort early if adding/removing the issue repo failed on any host.
    for target in targets.targets() {
        if !target.lasterr().is_empty() {
            warn!(
                host = %target.hostname(),
                stderr = %target.lasterr(),
                exit = ?target.lastexit(),
                "failed to prepare host; stopping"
            );
            return;
        }
    }

    if installed_only {
        // Conditional per-package install — inherently one package at a time.
        for pkg in pkgs {
            let cmd = build_prepare_map(targets, registry, Some(pkg), true);
            targets.run(Command::PerHost(cmd)).await;
        }
    } else if !pkgs.is_empty() {
        // Install every package in a SINGLE transaction (one snapshot for
        // transactional hosts).
        let joined = pkgs.join(" ");
        let cmd = build_prepare_map(targets, registry, Some(&joined), false);
        targets.run(Command::PerHost(cmd)).await;
    }

    // The prepare check emits no diagnostics; discard the sink.
    for e in run_checks(targets, registry, Role::Prepare, &mut Vec::new()) {
        error!(error = %e, "prepare check failed");
    }
    reboot_transactional(targets, reboot).await;
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
) {
    let registry = WorkflowRegistry::default();

    // Resolve reboot before locking so a missing downgrader early-returns
    // without leaving the group locked.
    let Ok(reboot) = build_reboot_map(targets, &registry, Role::Downgrade) else {
        return;
    };

    if targets.update_lock().await.is_err() {
        return;
    }

    downgrade_body(targets, &registry, report, packages, reboot).await;
    targets.unlock().await;
}

/// The locked body of [`perform_downgrade`].
async fn downgrade_body(
    targets: &mut HostsGroup,
    registry: &WorkflowRegistry,
    report: &dyn SetRepo,
    packages: &[String],
    reboot: BTreeMap<String, String>,
) {
    targets.fanout_set_repo(RepoOp::Remove, report).await;

    // Run the list_command to discover each host's available downgrade
    // versions, then parse `name = version` lines, keeping the highest per pkg.
    let joined = packages.join(" ");
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
        targets.run(Command::PerHost(list_map)).await;
    }

    // hostname -> { package -> highest available version }
    let mut versions: HashMap<String, HashMap<String, String>> = HashMap::new();
    for target in targets.targets() {
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
            let vars: HashMap<&str, &str> = [("package", package.as_str()), ("version", ver)]
                .into_iter()
                .collect();
            if let Ok(rendered) = doer.render_command(&vars) {
                cmd.insert(hn.to_owned(), rendered);
            }
        }
        if !cmd.is_empty() {
            targets.run(Command::PerHost(cmd)).await;
            for e in run_checks(targets, registry, Role::Downgrade, &mut Vec::new()) {
                if !transactional_hosts.contains(e.host.as_deref().unwrap_or("")) {
                    error!(error = %e, "downgrade check failed");
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
        let joined_specs = specs.join(" ");
        let vars: HashMap<&str, &str> = [("package", joined_specs.as_str())].into_iter().collect();
        if let Ok(rendered) = doer.render_command(&vars) {
            combined.insert(hn.clone(), rendered);
        }
    }
    if !combined.is_empty() {
        targets.run(Command::PerHost(combined)).await;
        for e in run_checks(targets, registry, Role::Downgrade, &mut Vec::new()) {
            if transactional_hosts.contains(e.host.as_deref().unwrap_or("")) {
                error!(error = %e, "downgrade check failed");
            }
        }
    }

    reboot_transactional(targets, reboot).await;

    downgrade_verdict(targets).await;
}

/// Emits the post-downgrade "done" / "downgrade not completed" verdict.
///
/// Ports upstream `commands/downgrade.py:51-72`: re-query each host, then rotate
/// `before = after; after = current` per package. If any package's rotated
/// `before == after` (both known) the downgrade did not move that version, so the
/// whole run is reported as **not completed** (a warning) and the scan
/// short-circuits; otherwise it is **done** (info). Iterated in sorted hostname
/// order to keep the log deterministic.
async fn downgrade_verdict(targets: &mut HostsGroup) {
    // Query every host's versions concurrently (serial hosts one at a time)
    // via the shared fan-out, then run the pure verdict scan below.
    targets.query_versions().await;

    let mut completed = true;
    'hosts: for target in targets.targets_mut() {
        for pkg in target.packages_mut() {
            let after = pkg.after().cloned();
            pkg.set_before_version(after);
            let current = pkg.current().cloned();
            pkg.set_after_version(current);
            if let (Some(before), Some(after)) = (pkg.before(), pkg.after())
                && before == after
            {
                completed = false;
                break 'hosts;
            }
        }
    }

    if completed {
        tracing::info!("done");
    } else {
        warn!("downgrade not completed");
    }
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
        // flags: remove-repo prepare).
        perform_prepare(targets, report, packages, false, false, false).await;
    }

    targets.package_check(false).await;

    if let Err(e) = targets.update_lock().await {
        return Err(UpdateFailure::Check(UpdateError::reason_only(
            e.to_string(),
        )));
    }

    targets.fanout_set_repo(RepoOp::Add, report).await;

    let repa = repa_for(maintenance_id, review_id);
    let joined = packages.join(" ");
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

    if newpackage {
        perform_prepare(targets, report, packages, false, true, false).await;
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

        perform_prepare(
            &mut group,
            &report,
            &["pkg-a".to_owned(), "pkg-b".to_owned()],
            false,
            false,
            false,
        )
        .await;

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
        perform_prepare(
            &mut group,
            &NoopRepo,
            &["branding-upstream".to_owned(), "pkg-a".to_owned()],
            false,
            false,
            false,
        )
        .await;
        let cmds = handle.commands();
        let install = cmds
            .iter()
            .find(|c| c.contains("zypper -n in -y -l"))
            .unwrap();
        assert!(install.contains("pkg-a"));
        assert!(!install.contains("branding-upstream"));
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

        perform_downgrade(&mut group, &NoopRepo, &["pkg-a".to_owned()]).await;

        let cmds = handle.commands();
        assert!(
            cmds.iter()
                .any(|c| c.contains("pkg-a") && c.contains("1.0-1")),
            "expected downgrade to the resolved version: {cmds:?}"
        );
    }

    #[tokio::test]
    async fn downgrade_verdict_warns_when_version_did_not_move() {
        // The re-query returns pkg-a 1.0-1; seeding `after` to the same version
        // means the rotated before == after ⇒ "downgrade not completed".
        let (mut t, _h) = sles_target("h1", "pkg-a 1.0-1\n");
        let mut pkg = mtui_types::package::Package::new("pkg-a");
        pkg.set_after(Some("1.0-1")).unwrap();
        t.set_packages(vec![pkg]);
        let mut group = HostsGroup::new(vec![t], false);

        downgrade_verdict(&mut group).await;

        // After the rotation, `after` holds the re-queried `current`.
        let p = &group.get("h1").unwrap().packages()[0];
        assert_eq!(
            p.before(),
            p.after(),
            "unchanged version leaves before == after"
        );
    }

    #[tokio::test]
    async fn downgrade_verdict_done_when_version_moved() {
        // Re-query returns 0.9-1 but `after` was 1.0-1 ⇒ before != after ⇒ done.
        let (mut t, _h) = sles_target("h1", "pkg-a 0.9-1\n");
        let mut pkg = mtui_types::package::Package::new("pkg-a");
        pkg.set_after(Some("1.0-1")).unwrap();
        t.set_packages(vec![pkg]);
        let mut group = HostsGroup::new(vec![t], false);

        downgrade_verdict(&mut group).await;

        let p = &group.get("h1").unwrap().packages()[0];
        assert_ne!(p.before(), p.after(), "moved version ⇒ before != after");
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
        // exit 104 on the updater command ⇒ the update check flags "update stack
        // locked"; the flow must NOT issue a repo-remove (repos kept for retry).
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
        let (t, handle) = sles_target_with_exit("h1", "pkg-a = 1.0-1\n", 104);
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

        perform_downgrade(
            &mut group,
            &NoopRepo,
            &["pkg-a".to_owned(), "pkg-b".to_owned()],
        )
        .await;

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

        report
            .perform_prepare(
                &mut group,
                &["pkg-a".to_owned(), "pkg-b".to_owned()],
                false,
                false,
                false,
            )
            .await;

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

        report
            .perform_downgrade(&mut group, &["pkg-a".to_owned()])
            .await;

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
