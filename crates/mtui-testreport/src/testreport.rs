//! The [`TestReport`] trait and its shared-state carrier [`TestReportBase`].
//!
//! Rust has no class inheritance, so the state every concrete report
//! (SL/PI/OBS/Null) shares lives in a plain [`TestReportBase`] they embed,
//! reached through the [`TestReport::base`]/[`TestReport::base_mut`] accessors
//! so trait-default and caller code need no downcast.
//!
//! Only the shared state and the abstract surface land here: the concrete
//! lifecycle (load/checkout/commit/export) is [`crate::lifecycle`], metadata
//! parsing [`crate::metadata_parsers`], per-report host-connect logic
//! [`crate::reports`].

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};

use mtui_config::options::Config;
use mtui_datasources::openqa::kernel::KernelOpenQA;
use mtui_datasources::oqa_search::results::OpenQAOverviewResult;
use mtui_datasources::qem_dashboard::dashboard_openqa::DashboardAutoOpenQA;
use mtui_hosts::{HostArbiter, HostsGroup, Owner, SetRepo};
use mtui_types::package::Package;
use mtui_types::{OpenQAResults, RequestReviewID, SystemProduct, UpdateSource, Workflow};

/// The concrete openQA state holder carried on a report.
///
/// Monomorphizes [`OpenQAResults`] to the QEM-dashboard "auto" result, the
/// per-instance "kernel" results and the `openqa_overview` payload; pinning
/// them here adds no new crate edge (`mtui-datasources` is already a dep).
pub type ReportOpenQA = OpenQAResults<DashboardAutoOpenQA, KernelOpenQA, OpenQAOverviewResult>;

use crate::checkout::TemplateIoError;
use crate::metadata_parsers::{JSONParser, ReducedMetadataParser, patchinfo_titles};

/// Shared state common to every [`TestReport`] implementation.
///
/// No `#[derive(Debug)]`: the embedded `mtui-hosts` collaborators
/// (`HostsGroup`, `HostArbiter`) do not implement it.
pub struct TestReportBase {
    /// The application configuration.
    pub(crate) config: Config,
    /// Per-report workflow mode. Defaults to [`Workflow::Manual`].
    pub workflow: Workflow,
    /// Path to the loaded testreport file, or `None` when nothing is loaded.
    pub path: Option<PathBuf>,
    /// Connected reference-host targets.
    pub targets: HostsGroup,
    /// `SystemProduct -> repository` map for the update repositories.
    ///
    /// Keyed on the flat [`SystemProduct`] `(name, version, arch)` tuple, as
    /// the `*repoparse` helpers build and
    /// [`RepoManager::run_zypper`](mtui_hosts) consumes; the refhost `Product`
    /// has no `arch`, so keying on it would be lossy.
    pub update_repos: HashMap<SystemProduct, String>,
    /// Known hostnames for this report.
    pub hostnames: HashSet<String>,
    /// When non-empty, newly connected hosts are locked with this comment
    /// (set while a PI assignment is active).
    pub lock_comment: String,
    /// Process-global host arbiter (RFC §5.7). A borrow of the singleton
    /// ([`get_arbiter`](mtui_hosts::get_arbiter)); `None` for
    /// directly-constructed reports, which fall back to the legacy
    /// remote-lock-only connect path.
    pub arbiter: Option<&'static HostArbiter>,
    /// Composite `(registry_id, RRID)` ownership key. `None` until wired by the
    /// template registry.
    pub owner: Option<Owner>,
    /// Hosts this report has claimed through the arbiter (for release).
    pub pool_claims: HashSet<String>,
    /// Per-slot ordered candidate hostnames captured during pool selection, so
    /// connect can fall back to a sibling host when the primary claim fails.
    pub slot_candidates: HashMap<String, Vec<String>>,
    /// Set when a load asked for autoconnect; the actual connect is deferred
    /// until after the host arbiter is wired.
    pub autoconnect_pending: bool,
    /// The reason a load failed, stashed on the [`NullReport`](crate::reports::NullReport)
    /// substituted by [`make_testreport`](crate::make_testreport) so the caller
    /// can surface *why* (svn checkout / gitea / hash / read failure) rather than
    /// a bare "could not load". `None` on a successfully loaded report.
    pub load_error: Option<String>,
    /// Set when this report **loaded successfully but its checked-out hash
    /// still differs from the Gitea PR** — the operator (interactively) or
    /// caller (`force_continue`, openSUSE/mtui#517) chose to proceed with
    /// stale content anyway. `None` on every other load. A REPL session
    /// already saw the equivalent `warn!` line on its own terminal; this
    /// field exists so a non-interactive caller (`mtui-mcp`), which never
    /// sees `tracing` output, can surface the same fact from the tool
    /// result instead of silently trusting content that may be out of date.
    pub stale_hash_warning: Option<String>,
    /// Bugzilla `id -> title` map.
    pub bugs: HashMap<String, String>,
    /// Jira `id -> title` map.
    pub jira: HashMap<String, String>,
    /// Test platform strings.
    pub testplatforms: Vec<String>,
    /// Product name strings parsed from the template.
    pub products: Vec<String>,
    /// Update category.
    pub category: String,
    /// Packager.
    pub packager: String,
    /// Reviewer.
    pub reviewer: String,
    /// The Slack message this update's review was requested on, if any.
    ///
    /// Written by `request_review` and read back on load so a later `approve`
    /// verifies the ack against the exact message rather than trusting that a
    /// review happened.
    pub slack_review: Option<SlackReviewMarker>,
    /// Update repository string.
    pub repository: String,
    /// Update repository URLs.
    pub repositories: HashSet<String>,
    /// `SystemProduct -> the package names this update composes for it`,
    /// indexed from the metadata envelope's `binaries` block.
    ///
    /// Empty when the report carries no `binaries` block, or when the block
    /// could not be indexed (`metadata_parsers::index_binaries` owns that rule).
    pub composed: HashMap<SystemProduct, BTreeSet<String>>,
    /// Nested package map: `product -> { package name -> version }`.
    ///
    /// A report routinely spans multiple products, each shipping its own set
    /// of packages and versions; [`TestReport::get_package_list`] flattens them.
    pub packages: HashMap<String, HashMap<String, String>>,
    /// Parsed Request Review ID, or `None` when unset/invalid.
    pub rrid: Option<RequestReviewID>,
    /// Update rating.
    pub rating: Option<String>,
    /// Raw request id from the metadata envelope (JSON key `id`).
    pub realid: Option<String>,
    /// Gitea pull-request reference (JSON key `gitea_pr`).
    pub giteapr: Option<String>,
    /// Gitea pull-request API URL (JSON key `gitea_pr_api`).
    pub giteaprapi: Option<String>,
    /// Gitea commit hash (JSON key `gitea_commit_hash`).
    pub giteacohash: Option<String>,
    /// Which update workflow mtui drives for this report, resolved once at
    /// load from [`giteacohash`](Self::giteacohash) by
    /// [`JSONParser`]. Defaults to
    /// [`UpdateSource::Obs`] until a report is loaded.
    pub update_source: UpdateSource,
    /// `hostname -> product-drift warning lines` from the last connect.
    pub product_warnings: HashMap<String, Vec<String>>,
    /// The report's openQA results.
    ///
    /// Empty until `reload_openqa` / `set_workflow` populate it; consumed by
    /// the exporters for openQA-enriched templates.
    pub openqa: ReportOpenQA,
}

impl TestReportBase {
    /// Builds the shared state with its default values.
    ///
    /// `targets` starts headless (`is_repl = false`);
    /// [`make_testreport`](crate::make_testreport) reconciles it to the session
    /// mode once via [`set_is_repl`](HostsGroup::set_is_repl) before handing the
    /// report over — the session is the single source of truth, and the flag is
    /// never mutated afterwards.
    #[must_use]
    pub fn new(config: Config) -> Self {
        Self {
            config,
            workflow: Workflow::Manual,
            path: None,
            targets: HostsGroup::new(Vec::new(), false),
            update_repos: HashMap::new(),
            hostnames: HashSet::new(),
            lock_comment: String::new(),
            arbiter: None,
            owner: None,
            pool_claims: HashSet::new(),
            slot_candidates: HashMap::new(),
            autoconnect_pending: false,
            load_error: None,
            stale_hash_warning: None,
            bugs: HashMap::new(),
            jira: HashMap::new(),
            testplatforms: Vec::new(),
            products: Vec::new(),
            category: String::new(),
            packager: String::new(),
            reviewer: String::new(),
            slack_review: None,
            repository: String::new(),
            repositories: HashSet::new(),
            composed: HashMap::new(),
            packages: HashMap::new(),
            rrid: None,
            rating: None,
            realid: None,
            giteapr: None,
            giteaprapi: None,
            giteacohash: None,
            update_source: UpdateSource::default(),
            product_warnings: HashMap::new(),
            openqa: ReportOpenQA::new(),
        }
    }

    /// The working directory of the loaded report checkout.
    ///
    /// The parent directory of [`path`](Self::path), created if absent. The OBS
    /// report feeds this to
    /// [`obsrepoparse`](crate::reports::repoparse::obsrepoparse), which reads
    /// `project.xml` from it.
    ///
    /// Returns [`std::io::ErrorKind::NotFound`] when no report is loaded, and
    /// propagates any directory-creation error, so callers can degrade
    /// explicitly rather than panic.
    pub fn report_wd(&self) -> std::io::Result<PathBuf> {
        let path = self
            .path
            .as_ref()
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "empty path"))?;
        let dir = path.parent().unwrap_or_else(|| std::path::Path::new(""));
        std::fs::create_dir_all(dir)?;
        Ok(dir.to_path_buf())
    }
}

/// Resolves seed packages from a `product -> { name -> version }` map for a
/// host's `base_version`.
///
/// Uses the sole `"standard"` sub-map when that is the only key (a report
/// shipping one product-agnostic set, e.g. SLFO metadata), else the one keyed
/// by `base_version`; a `base_version` starting `"12"` additionally merges the
/// `"12"` sub-map. Each `name -> version` becomes a [`Package`] with its
/// [`required`](Package::required) set; an unparseable version is skipped
/// (best-effort) rather than aborting the whole host.
///
/// Takes a *borrowed* map so the composition root (`mtui-core::session`) can
/// resolve from a snapshot cloned before a `targets_mut()` borrow, keeping the
/// connect future `Send`.
#[must_use]
pub fn packages_for_map(
    map: &HashMap<String, HashMap<String, String>>,
    base_version: &str,
) -> Vec<Package> {
    let mut selected: HashMap<&String, &String> = HashMap::new();

    if map.len() == 1 && map.contains_key("standard") {
        for (name, ver) in &map["standard"] {
            selected.insert(name, ver);
        }
    } else if let Some(per_product) = map.get(base_version) {
        for (name, ver) in per_product {
            selected.insert(name, ver);
        }
    }
    if base_version.starts_with("12")
        && let Some(sle12) = map.get("12")
    {
        for (name, ver) in sle12 {
            selected.insert(name, ver);
        }
    }

    let mut packages: Vec<Package> = Vec::with_capacity(selected.len());
    for (name, ver) in selected {
        let mut pkg = Package::new(name.clone());
        if pkg.set_required(Some(ver)).is_err() {
            tracing::warn!(
                package = %name, version = %ver,
                "unparseable required version in metadata; leaving package unseeded"
            );
        }
        packages.push(pkg);
    }
    packages.sort_by(|a, b| a.name.cmp(&b.name));
    packages
}

/// The abstract test-report surface.
///
/// Concrete reports embed a [`TestReportBase`] and expose it through
/// [`base`](Self::base) / [`base_mut`](Self::base_mut); the other required
/// methods are the abstract surface. Non-abstract lifecycle methods (`read`,
/// `release_pool_claims`, `perform_install`, …) are trait defaults below.
///
/// `#[async_trait]` because [`check_hash`](Self::check_hash) drives async I/O
/// for git-backed reports (`SLTestReport` awaits `Gitea::get_hash`).
#[async_trait::async_trait]
pub trait TestReport {
    /// Borrows the shared state.
    fn base(&self) -> &TestReportBase;

    /// Mutably borrows the shared state.
    fn base_mut(&mut self) -> &mut TestReportBase;

    /// The report ID. Empty for an unloaded report.
    fn id(&self) -> String;

    /// The metadata field parser table.
    ///
    /// Maps a template field name to its parsed value. The table models
    /// values as `String`; the null object leaves it empty.
    fn parser(&self) -> HashMap<String, String>;

    /// The update-repository parser table.
    ///
    /// Keyed on the flat [`SystemProduct`] to match the `*repoparse` helpers and
    /// [`TestReportBase::update_repos`].
    fn update_repos_parser(&self) -> HashMap<SystemProduct, String>;

    /// Reads and parses a checkout's test-report template into this report.
    ///
    /// `path` names the checkout's `log` file; `metadata.json` is read from the
    /// same directory. Two-parser pipeline: [`ReducedMetadataParser`] over the
    /// `log` lines (reference hosts + bug/jira titles), then [`JSONParser`] over
    /// the metadata envelope, then `patchinfo.xml` overlays real bug/jira titles
    /// onto the ids the envelope carried. On success
    /// [`path`](TestReportBase::path) is set and the update-repo map derived via
    /// [`update_repos_parser`](Self::update_repos_parser).
    ///
    /// Gitea-hash verification is deferred to
    /// [`make_testreport`](crate::make_testreport): `read` is sync while
    /// [`check_hash`](Self::check_hash) is async.
    ///
    /// # Errors
    ///
    /// * [`ReadError::Template`] when the `log` file cannot be read (missing →
    ///   `ENOENT`, which the checkout seam treats as "needs checkout").
    /// * [`ReadError::MetadataMissing`] when `metadata.json` is absent.
    /// * [`ReadError::MetadataInvalid`] when `metadata.json` is not valid JSON.
    fn read(&mut self, path: &Path) -> Result<(), ReadError> {
        let tpl = std::fs::read_to_string(path).map_err(|e| {
            // Carry the errno so the checkout seam can branch on ENOENT.
            ReadError::Template(TemplateIoError::from_io(&e))
        })?;

        let dir = path.parent().unwrap_or_else(|| Path::new(""));
        let metadata_path = dir.join("metadata.json");
        if !metadata_path.is_file() {
            return Err(ReadError::MetadataMissing);
        }
        let metadata = std::fs::read_to_string(&metadata_path)
            .map_err(|e| ReadError::Template(TemplateIoError::from_io(&e)))?;

        let base = self.base_mut();
        for line in tpl.lines() {
            ReducedMetadataParser::parse(base, line);
        }
        JSONParser::parse_str(base, &metadata).map_err(|_| ReadError::MetadataInvalid)?;

        // The envelope's id set stays authoritative: titles overlay, never add.
        let titles = patchinfo_titles(dir);
        for (iid, title) in titles {
            if let Some(slot) = base.bugs.get_mut(&iid) {
                *slot = title;
            } else if let Some(slot) = base.jira.get_mut(&iid) {
                *slot = title;
            }
        }

        self.base_mut().path = Some(path.to_path_buf());
        let repos = self.update_repos_parser();
        self.base_mut().update_repos = repos;
        Ok(())
    }

    /// Drops this report's arbiter ownership and removes its remote pool locks.
    ///
    /// Best-effort
    /// [`Target::pool_unlock`](mtui_hosts::Target::pool_unlock) with
    /// `force = false` (so a claim owned by another template is left alone) for
    /// every claimed host, then clears the in-process claim set and drops
    /// ownership via
    /// [`HostArbiter::release_owner`](mtui_hosts::HostArbiter::release_owner).
    ///
    /// Idempotent, and a no-op when pool selection was never used
    /// ([`arbiter`](TestReportBase::arbiter)/[`owner`](TestReportBase::owner) are
    /// then `None`). Called from the exit path (`quit`,
    /// `TemplateRegistry.release_claims`).
    async fn release_pool_claims(&mut self) {
        let base = self.base_mut();
        // Snapshot so the `pool_claims` borrow ends before the `&mut` target calls.
        let claims: Vec<String> = base.pool_claims.iter().cloned().collect();
        for host in claims {
            if let Some(target) = base.targets.get_mut(&host) {
                target.pool_unlock(false).await;
            }
        }
        base.pool_claims.clear();
        base.slot_candidates.clear();
        if let (Some(arbiter), Some(owner)) = (base.arbiter.as_ref(), base.owner.as_ref()) {
            arbiter.release_owner(owner);
        }
    }

    /// Releases one host's in-process arbiter claim and prunes it from the
    /// slot-candidate map.
    ///
    /// The per-host analogue of
    /// [`release_pool_claims`](Self::release_pool_claims), called from
    /// `remove_host`: there is no `unload` over MCP, so without it a
    /// disconnected refhost stays claimed in the process-global
    /// [`HostArbiter`] for the server's lifetime.
    /// [`Target::close`](mtui_hosts::Target::close) drops the remote
    /// operation/pool-lock files; this clears the in-process ownership those
    /// locks and the `--free` probe never see.
    ///
    /// Only `host` leaves each slot's candidate list — siblings stay available
    /// as backup-refhost fallbacks (RFC §5.7) — and a slot is pruned only once
    /// empty; the whole-report variant clears the map instead. Idempotent, and
    /// a no-op when pool selection was never used.
    fn release_pool_claim(&mut self, host: &str) {
        let base = self.base_mut();
        base.pool_claims.remove(host);
        base.slot_candidates.retain(|_slot, candidates| {
            candidates.retain(|c| c != host);
            !candidates.is_empty()
        });
        if let (Some(arbiter), Some(owner)) = (base.arbiter.as_ref(), base.owner.as_ref()) {
            arbiter.release(host, owner);
        }
    }

    /// Emits the per-host update commands for `targets`. The null object is a
    /// no-op.
    fn list_update_commands(&self, targets: &HostsGroup);

    /// The deduplicated list of every package named in the report metadata.
    ///
    /// Flattens the package **names** across all products of the nested
    /// [`packages`](TestReportBase::packages) map. Sorted for reproducible
    /// snapshots; no caller depends on the order.
    fn get_package_list(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .base()
            .packages
            .values()
            .flat_map(|per_product| per_product.keys().cloned())
            .collect();
        names.sort_unstable();
        names.dedup();
        names
    }

    /// The plain-text test-report log URL: `{reports_url}/{id}/log`.
    fn testreport_url(&self) -> String {
        format!("{}/{}/log", self.base().config.reports_url, self.id())
    }

    /// The "fancy" test-report log URL: `{fancy_reports_url}/{id}/log`.
    fn fancy_report_url(&self) -> String {
        format!("{}/{}/log", self.base().config.fancy_reports_url, self.id())
    }

    /// The Bugzilla `id -> title` and Jira `id -> title` maps.
    ///
    /// Sorted [`BTreeMap`](std::collections::BTreeMap)s so the display renders
    /// ids in a stable order; the `list_bugs` command renders the
    /// "No bugs…"/"No Jira…" sentinels for an empty map.
    fn bug_maps(
        &self,
    ) -> (
        std::collections::BTreeMap<String, String>,
        std::collections::BTreeMap<String, String>,
    ) {
        let base = self.base();
        (
            base.bugs
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
            base.jira
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
        )
    }

    /// The aligned `(label, value)` metadata rows for `list_metadata`.
    ///
    /// Rows with an empty value are dropped, and the whole set is sorted by
    /// label. The caller renders each surviving row as `{label:15}: {value}`.
    fn show_yourself_data(&self) -> Vec<(String, String)> {
        let base = self.base();
        let mut bug_ids: Vec<&String> = base.bugs.keys().collect();
        bug_ids.sort();
        let mut jira_ids: Vec<&String> = base.jira.keys().collect();
        jira_ids.sort();

        let build_checks = {
            let url = self.testreport_url();
            // Strips the trailing "log" and appends "build_checks".
            format!("{}build_checks", &url[..url.len().saturating_sub(3)])
        };

        let mut rows: Vec<(String, String)> = vec![
            ("Category".to_owned(), base.category.clone()),
            (
                "ReviewRequestID".to_owned(),
                base.rrid
                    .as_ref()
                    .map(ToString::to_string)
                    .unwrap_or_default(),
            ),
            ("Rating".to_owned(), base.rating.clone().unwrap_or_default()),
            (
                "Gitea PR".to_owned(),
                base.giteapr.clone().unwrap_or_default(),
            ),
            ("Reviewer".to_owned(), base.reviewer.clone()),
            (
                "Slack Review".to_owned(),
                base.slack_review
                    .as_ref()
                    .map(|m| format!("{} {}", m.channel, m.ts))
                    .unwrap_or_default(),
            ),
            ("Packager".to_owned(), base.packager.clone()),
            (
                "Bugs".to_owned(),
                bug_ids
                    .iter()
                    .map(|s| s.as_str())
                    .collect::<Vec<_>>()
                    .join(", "),
            ),
            (
                "Jira".to_owned(),
                jira_ids
                    .iter()
                    .map(|s| s.as_str())
                    .collect::<Vec<_>>()
                    .join(", "),
            ),
            ("Packages".to_owned(), self.get_package_list().join(" ")),
            ("Build checks".to_owned(), build_checks),
            ("Testreport".to_owned(), self.testreport_url()),
            ("Repository".to_owned(), base.repository.clone()),
        ];
        rows.extend(
            base.testplatforms
                .iter()
                .map(|x| ("Testplatform".to_owned(), x.clone())),
        );
        rows.extend(
            base.products
                .iter()
                .map(|x| ("Products".to_owned(), x.clone())),
        );

        rows.retain(|(_, value)| !value.is_empty());
        rows.sort();
        rows
    }

    /// Installs `packages` on every host in `targets`.
    ///
    /// Drives the [`InstallOperation`](mtui_hosts::InstallOperation) template
    /// through the group's [`OperationGroup`](mtui_hosts::OperationGroup) impl,
    /// which resolves each host's installer doer/check via the `PlanProvider`
    /// injected by `update_flow::perform_install` (the shared body behind this
    /// method).
    ///
    /// Returns `Err` when the install command failed on one or more hosts
    /// (non-zero exit or non-empty stderr after the fan-out). The null object's
    /// default is a no-op `Ok(())`.
    async fn perform_install(
        &self,
        _targets: &mut HostsGroup,
        _packages: &[String],
    ) -> Result<(), crate::update_workflow::UpdateError> {
        Ok(())
    }

    /// Uninstalls `packages` from every host in `targets`.
    ///
    /// Drives the [`UninstallOperation`](mtui_hosts::UninstallOperation)
    /// template; see [`perform_install`](Self::perform_install). Default no-op
    /// `Ok(())`; returns `Err` on a per-host command failure.
    async fn perform_uninstall(
        &self,
        _targets: &mut HostsGroup,
        _packages: &[String],
    ) -> Result<(), crate::update_workflow::UpdateError> {
        Ok(())
    }

    /// Prepares `packages` on every host.
    ///
    /// The bespoke (non-template) preparer flow, all under the operation lock:
    /// fan the issue repo add/remove out, install every package in a single
    /// transaction (per-package for `installed_only`), run the preparer check,
    /// reboot transactional hosts. `testing` selects the repo-`add` (testing) vs
    /// repo-`remove` path and the testing preparer variant; `force` toggles
    /// `--force-resolution`; `installed_only` only touches already-installed
    /// packages.
    ///
    /// Returns `Err` on a missing preparer, lock contention, a failed issue-repo
    /// fan-out, a per-host prepare-command failure, or a prepare check failure.
    /// The null object's default is a no-op `Ok(())`.
    async fn perform_prepare(
        &self,
        _targets: &mut HostsGroup,
        _packages: &[String],
        _force: bool,
        _testing: bool,
        _installed_only: bool,
    ) -> Result<(), crate::update_workflow::UpdateError> {
        Ok(())
    }

    /// Downgrades `packages` on every host.
    ///
    /// Under the operation lock: remove the issue repos, resolve each package's
    /// available downgrade version via the downgrader `list_command`, downgrade
    /// (per-package for non-transactional hosts, one transaction for
    /// transactional ones), run the check, reboot transactional hosts.
    ///
    /// Returns `Err` on a missing downgrader, lock contention, or a per-host
    /// downgrade check failure. The null object's default is a no-op `Ok(())`.
    async fn perform_downgrade(
        &self,
        _targets: &mut HostsGroup,
        _packages: &[String],
    ) -> Result<(), crate::update_workflow::UpdateError> {
        Ok(())
    }

    /// Updates the hosts with this report's maintenance update.
    ///
    /// The full bespoke update flow: optional prepare, pre/post package checks,
    /// repo add, `updater` command render (with the `$repa` RRID selector), the
    /// per-host update check with failure aggregation, transactional reboot, and
    /// the two-phase repo cleanup — remove on success, **keep** on failure for
    /// retry/diagnosis. `noprepare` skips the initial prepare; `newpackage`
    /// runs a testing prepare afterwards.
    ///
    /// Returns `Err` on a per-host `updater` check failure (after a best-effort
    /// downgrade rollback) or a hard missing-updater failure; non-fatal
    /// diagnostics from the check are appended to `diagnostics`. The null
    /// object's default is a no-op `Ok(())`.
    async fn perform_update(
        &self,
        _targets: &mut HostsGroup,
        _noprepare: bool,
        _newpackage: bool,
        _diagnostics: &mut Vec<crate::update_workflow::Diagnostic>,
    ) -> Result<(), crate::update_workflow::UpdateError> {
        Ok(())
    }

    /// Verifies the loaded template hash.
    ///
    /// Returns a [`HashCheck`] rather than raising, so the async load
    /// orchestrator ([`make_testreport`](crate::make_testreport)) can branch on
    /// it. The null object and the non-git reports (OBS/PI) report
    /// [`HashCheck::Ok`], having nothing to verify; async because git-backed
    /// reports compare against a hash fetched from Gitea.
    async fn check_hash(&self) -> HashCheck;

    /// The working directory for target artifacts.
    ///
    /// Defaults to joining `config.target_tempdir` with `paths`, matching the
    /// null object; concrete reports override to root under the loaded report.
    fn target_wd(&self, paths: &[&str]) -> PathBuf {
        let mut p = self.base().config.target_tempdir.clone();
        for part in paths {
            p.push(part);
        }
        p
    }

    /// Whether a real report is loaded. Defaults to `true`; the null object
    /// overrides to `false`.
    fn is_loaded(&self) -> bool {
        true
    }

    /// Exposes this report as a [`SetRepo`] when it can add/remove issue repos.
    ///
    /// `SetRepo` is a distinct object-safe trait a `dyn TestReport` cannot be
    /// downcast to, but `set_repo` needs a `&dyn SetRepo` for
    /// [`HostsGroup::fanout_set_repo`](mtui_hosts::HostsGroup). SL/PI/OBS
    /// override this to return `Some(self)`; the null report keeps the `None`
    /// default, which the command surfaces as "no update loaded".
    fn as_set_repo(&self) -> Option<&dyn SetRepo> {
        None
    }

    /// The report's parsed [`RequestReviewID`], if loaded.
    ///
    /// Reads [`TestReportBase::rrid`]; `None` for the null report.
    fn rrid(&self) -> Option<&RequestReviewID> {
        self.base().rrid.as_ref()
    }

    /// The report's workflow mode.
    fn workflow(&self) -> Workflow {
        self.base().workflow
    }

    /// The report's openQA state holder.
    ///
    /// Reads [`TestReportBase::openqa`]; empty for the null report.
    fn openqa(&self) -> &ReportOpenQA {
        &self.base().openqa
    }

    /// Mutably borrows the report's openQA state holder.
    ///
    /// The mutable counterpart of [`openqa`](Self::openqa); the
    /// `reload_openqa` / `set_workflow` commands populate it in place.
    fn openqa_mut(&mut self) -> &mut ReportOpenQA {
        &mut self.base_mut().openqa
    }

    /// The Gitea pull-request API URL, if any.
    fn giteaprapi(&self) -> Option<&str> {
        self.base().giteaprapi.as_deref()
    }

    /// The Gitea checkout hash recorded in the template, if any.
    fn giteacohash(&self) -> Option<&str> {
        self.base().giteacohash.as_deref()
    }

    /// Which update workflow mtui drives for this report.
    ///
    /// See [`UpdateSource`] for the precedence rule; resolved once at load by
    /// [`JSONParser`] from
    /// [`giteacohash`](Self::giteacohash).
    fn update_source(&self) -> UpdateSource {
        self.base().update_source
    }

    /// The openQA incident id used by the QEM Dashboard / oqa-search queries.
    ///
    /// Uses `rrid.maintenance_id` as the incident number. `None` for the null
    /// report (no RRID).
    fn incident_id(&self) -> Option<String> {
        self.base().rrid.as_ref().map(|r| r.maintenance_id.clone())
    }

    /// Records the reviewer in the loaded testreport template on disk.
    ///
    /// Replaces the `Test Plan Reviewer:` line with the trimmed `name`
    /// (normalising away older `Suggested …` phrasings), rewrites the file
    /// atomically, and updates [`reviewer`](TestReportBase::reviewer) only
    /// after the write succeeds.
    ///
    /// # Errors
    ///
    /// * [`ReviewerError::Empty`] when `name` is empty/whitespace.
    /// * [`ReviewerError::NoTemplate`] when no template is loaded (`path` unset).
    /// * [`ReviewerError::NoReviewerLine`] when the template has no
    ///   `Test Plan Reviewer:` line to replace.
    /// * [`ReviewerError::Io`] when reading or atomically rewriting the file fails.
    fn set_reviewer(&mut self, name: &str) -> Result<(), ReviewerError> {
        let name = name.trim().to_owned();
        if name.is_empty() {
            return Err(ReviewerError::Empty);
        }
        let path = self.base().path.clone().ok_or(ReviewerError::NoTemplate)?;

        let text = std::fs::read_to_string(&path).map_err(ReviewerError::Io)?;
        let re = reviewer_line_re();
        if !re.is_match(&text) {
            return Err(ReviewerError::NoReviewerLine);
        }
        let new_text = re
            .replace(&text, format!("Test Plan Reviewer: {name}").as_str())
            .into_owned();

        crate::support::atomic_write_file(new_text.as_bytes(), &path).map_err(ReviewerError::Io)?;
        self.base_mut().reviewer = name;
        Ok(())
    }

    /// Records the Slack message a review was requested on, in the loaded
    /// template on disk.
    ///
    /// Unlike [`set_reviewer`](TestReport::set_reviewer) the line does **not**
    /// pre-exist in a server-generated template, so this replaces an existing
    /// marker and otherwise *inserts* after the `Test Plan Reviewer:` line;
    /// replacing-only would fail on every first use, and overwriting rather
    /// than duplicating keeps a re-run of `request_review` pointed at the
    /// newest message. As in `set_reviewer`, the in-memory field is updated
    /// only after the write succeeds, so a caller treating the error as fatal
    /// is not left believing a marker was persisted.
    ///
    /// # Errors
    ///
    /// * [`SlackReviewError::NoTemplate`] when no template is loaded.
    /// * [`SlackReviewError::NoAnchor`] when the template has no
    ///   `Test Plan Reviewer:` line to insert after.
    /// * [`SlackReviewError::Io`] when reading or rewriting the file fails.
    fn set_slack_review(&mut self, marker: &SlackReviewMarker) -> Result<(), SlackReviewError> {
        let path = self
            .base()
            .path
            .clone()
            .ok_or(SlackReviewError::NoTemplate)?;

        let text = std::fs::read_to_string(&path).map_err(SlackReviewError::Io)?;
        let line = marker.to_line();
        let existing = slack_review_line_re();

        let new_text = if existing.is_match(&text) {
            // `replace` hits the first marker; the collapse below removes any others.
            let replaced = existing.replace(&text, line.as_str()).into_owned();
            collapse_extra_marker_lines(&replaced)
        } else {
            let anchor = reviewer_line_re();
            let Some(m) = anchor.find(&text) else {
                return Err(SlackReviewError::NoAnchor);
            };
            let mut out = String::with_capacity(text.len() + line.len() + 1);
            out.push_str(&text[..m.end()]);
            out.push('\n');
            out.push_str(&line);
            out.push_str(&text[m.end()..]);
            out
        };

        crate::support::atomic_write_file(new_text.as_bytes(), &path)
            .map_err(SlackReviewError::Io)?;
        self.base_mut().slack_review = Some(marker.clone());
        Ok(())
    }
}

/// Drop every `Slack Review:` line after the first.
///
/// A template that somehow accumulated several markers (a merge, a hand edit)
/// would otherwise leave the reader picking one arbitrarily; collapsing on
/// write makes the file agree with the first-wins read.
fn collapse_extra_marker_lines(text: &str) -> String {
    let mut seen = false;
    let mut out: Vec<&str> = Vec::new();
    for line in text.lines() {
        if line.starts_with("Slack Review:") {
            if seen {
                continue;
            }
            seen = true;
        }
        out.push(line);
    }
    let mut joined = out.join("\n");
    // `lines()` drops a trailing newline; keep the file's original ending.
    if text.ends_with('\n') {
        joined.push('\n');
    }
    joined
}

/// Matches the `Test Plan Reviewer:` (or legacy `Suggested Test Plan
/// Reviewer:`) metadata line.
///
/// [`TestReport::set_reviewer`] replaces it;
/// [`TestReport::set_slack_review`] anchors its insert after it.
fn reviewer_line_re() -> regex::Regex {
    regex::Regex::new(r"(?m)^(?:Suggested )?Test Plan Reviewer:.*$")
        .expect("static reviewer-line regex is valid")
}

/// Matches the `Slack Review:` marker line written by `request_review`.
fn slack_review_line_re() -> regex::Regex {
    regex::Regex::new(r"(?m)^Slack Review:.*$").expect("static slack-review regex is valid")
}

/// The Slack message a review request was posted to.
///
/// Both halves are Slack's own canonical identifiers as returned by
/// `chat.postMessage` — never the configured channel *name*, which does not
/// work for the reaction and reply reads that verify the ack.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlackReviewMarker {
    /// Canonical channel ID, e.g. `C0123456789`.
    pub channel: String,
    /// Message timestamp, which is also its ID within the channel.
    pub ts: String,
}

impl SlackReviewMarker {
    /// Render the marker as it appears in the template.
    #[must_use]
    fn to_line(&self) -> String {
        format!("Slack Review: {} {}", self.channel, self.ts)
    }

    /// Parse a `Slack Review: <channel> <ts>` line.
    ///
    /// Anything not exactly that shape is `None`: a hand-edited or truncated
    /// marker is treated as absent rather than pointing at no real message.
    #[must_use]
    pub(crate) fn parse_line(line: &str) -> Option<Self> {
        let rest = line.strip_prefix("Slack Review:")?;
        let mut parts = rest.split_whitespace();
        let channel = parts.next()?;
        let ts = parts.next()?;
        if parts.next().is_some() {
            return None;
        }
        Some(Self {
            channel: channel.to_owned(),
            ts: ts.to_owned(),
        })
    }
}

/// Failures from [`TestReport::set_slack_review`].
#[derive(Debug, thiserror::Error)]
pub enum SlackReviewError {
    /// No template is loaded, so there is nothing to write the marker into.
    #[error("Called while missing path")]
    NoTemplate,
    /// The template has no `Test Plan Reviewer:` line to anchor the marker to.
    ///
    /// The marker never pre-exists, so it must be inserted somewhere
    /// deterministic; guessing risks corrupting the template.
    #[error("no 'Test Plan Reviewer:' line found in template to anchor the Slack marker to")]
    NoAnchor,
    /// Reading or atomically rewriting the template file failed.
    #[error("failed to write the Slack review marker to template: {0}")]
    Io(#[source] std::io::Error),
}

/// The outcome of [`TestReport::check_hash`], so the load path
/// ([`make_testreport`](crate::make_testreport)) can branch explicitly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HashCheck {
    /// The template hash matches the Gitea PR head, or there is nothing to
    /// verify (null / OBS / PI reports, or the legacy `1.1` maintenance id).
    Ok,
    /// The template hash differs from the Gitea PR head — the template is
    /// stale.
    Mismatch {
        /// The hash recorded in the checked-out template (`giteacohash`).
        expected: String,
        /// The hash currently at the PR head, fetched from Gitea.
        actual: String,
    },
    /// No Gitea API token is configured.
    MissingToken,
    /// The Gitea API call failed; carries the underlying error text for
    /// logging.
    Failed(String),
}

/// Failure reading/parsing a checkout's template.
#[derive(Debug, thiserror::Error)]
pub enum ReadError {
    /// The template `log` file could not be read.
    ///
    /// Carries the [`TemplateIoError`] so the checkout seam can branch on
    /// [`is_not_found`](TemplateIoError::is_not_found) to decide whether to
    /// trigger a fresh checkout.
    #[error(transparent)]
    Template(#[from] TemplateIoError),
    /// The sibling `metadata.json` is absent.
    #[error("metadata.json is missing from the checkout")]
    MetadataMissing,
    /// The `metadata.json` is not valid JSON.
    #[error("metadata.json is not valid JSON")]
    MetadataInvalid,
}

/// Failure recording a reviewer into the loaded template.
#[derive(Debug, thiserror::Error)]
pub enum ReviewerError {
    /// The reviewer name was empty or whitespace-only.
    #[error("reviewer must be a non-empty string")]
    Empty,
    /// No template is loaded.
    #[error("Called while missing path")]
    NoTemplate,
    /// The template has no `Test Plan Reviewer:` line to replace.
    #[error("no 'Test Plan Reviewer:' line found in template")]
    NoReviewerLine,
    /// Reading or atomically rewriting the template file failed.
    #[error("failed to write reviewer to template: {0}")]
    Io(#[source] std::io::Error),
}

#[cfg(test)]
mod tests {
    use super::*;
    use mtui_config::options::Config;

    fn config() -> Config {
        Config::default()
    }

    #[test]
    fn base_defaults_are_stable() {
        let cfg = config();
        let base = TestReportBase::new(cfg);

        assert_eq!(base.workflow, Workflow::Manual);
        assert!(base.path.is_none());
        assert!(base.update_repos.is_empty());
        assert!(base.hostnames.is_empty());
        assert_eq!(base.lock_comment, "");
        assert!(base.arbiter.is_none());
        assert!(base.owner.is_none());
        assert!(base.pool_claims.is_empty());
        assert!(base.slot_candidates.is_empty());
        assert!(!base.autoconnect_pending);
        assert!(base.bugs.is_empty());
        assert!(base.jira.is_empty());
        assert!(base.testplatforms.is_empty());
        assert!(base.products.is_empty());
        assert_eq!(base.category, "");
        assert_eq!(base.packager, "");
        assert_eq!(base.reviewer, "");
        assert_eq!(base.repository, "");
        assert!(base.repositories.is_empty());
        assert!(base.packages.is_empty());
        assert!(base.rrid.is_none());
        assert!(base.rating.is_none());
        assert!(base.realid.is_none());
        assert!(base.giteapr.is_none());
        assert!(base.giteaprapi.is_none());
        assert!(base.giteacohash.is_none());
        assert_eq!(base.update_source, UpdateSource::Obs);
        assert!(base.product_warnings.is_empty());
    }

    #[test]
    fn report_wd_returns_report_parent_and_ensures_it_exists() {
        let tmp = tempfile::tempdir().unwrap();
        let wd = tmp.path().join("checkout");
        let mut base = TestReportBase::new(config());
        base.path = Some(wd.join("log"));

        let got = base.report_wd().expect("report_wd");
        assert_eq!(got, wd);
        assert!(wd.is_dir(), "report_wd must create the directory");
    }

    #[test]
    fn report_wd_errors_when_no_report_loaded() {
        let base = TestReportBase::new(config());
        let err = base.report_wd().expect_err("no path -> error");
        assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
    }

    fn base_with_packages(entries: &[(&str, &str, &str)]) -> TestReportBase {
        let mut base = TestReportBase::new(config());
        for (product, name, ver) in entries {
            base.packages
                .entry((*product).to_owned())
                .or_default()
                .insert((*name).to_owned(), (*ver).to_owned());
        }
        base
    }

    #[test]
    fn packages_for_selects_by_base_version_and_sets_required() {
        // Metadata keyed by "15-SP6", i.e. the parse_product version string.
        let base = base_with_packages(&[
            ("15-SP6", "hplip", "3.26.4-150600.4.12.1"),
            ("15-SP6", "hplip-devel", "3.26.4-150600.4.12.1"),
            (
                "15-SP5",
                "release-notes-sles",
                "15.5.20260709-150500.3.35.1",
            ),
        ]);
        let pkgs = packages_for_map(&base.packages, "15-SP6");
        let names: Vec<&str> = pkgs.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, vec!["hplip", "hplip-devel"]);
        for p in &pkgs {
            assert_eq!(
                p.required().map(ToString::to_string),
                Some("3.26.4-150600.4.12.1".to_owned()),
                "required must be set for {}",
                p.name
            );
        }
    }

    #[test]
    fn packages_for_standard_only_map_used_regardless_of_base_version() {
        let base = base_with_packages(&[("standard", "patch", "2.7.6-999999_stage.1.1")]);
        let pkgs = packages_for_map(&base.packages, "16.0");
        assert_eq!(pkgs.len(), 1);
        assert_eq!(pkgs[0].name, "patch");
        assert_eq!(
            pkgs[0].required().map(ToString::to_string),
            Some("2.7.6-999999_stage.1.1".to_owned())
        );
    }

    #[test]
    fn packages_for_merges_sle12_special_case() {
        let base = base_with_packages(&[("12-SP5", "bash", "5.0-1"), ("12", "glibc", "2.31-1")]);
        let pkgs = packages_for_map(&base.packages, "12-SP5");
        let names: Vec<&str> = pkgs.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, vec!["bash", "glibc"]);
    }

    #[test]
    fn packages_for_returns_empty_when_no_submap_matches() {
        let base = base_with_packages(&[("15-SP6", "hplip", "3.26.4-1")]);
        assert!(packages_for_map(&base.packages, "15-SP5").is_empty());
    }

    #[test]
    fn packages_for_skips_unparseable_version() {
        let base = base_with_packages(&[("15-SP6", "goodpkg", "1.0-1"), ("15-SP6", "badpkg", "")]);
        let pkgs = packages_for_map(&base.packages, "15-SP6");
        // `parse_opt` reads "" as None, so badpkg survives — just unseeded.
        let good = pkgs.iter().find(|p| p.name == "goodpkg").unwrap();
        assert!(good.required().is_some());
    }

    /// A minimal report with a fixed id, so the trait-default metadata helpers
    /// can be exercised directly.
    struct MetaReport {
        base: TestReportBase,
    }

    #[async_trait::async_trait]
    impl TestReport for MetaReport {
        fn base(&self) -> &TestReportBase {
            &self.base
        }
        fn base_mut(&mut self) -> &mut TestReportBase {
            &mut self.base
        }
        fn id(&self) -> String {
            "SUSE:Maintenance:1:1".to_owned()
        }
        fn parser(&self) -> HashMap<String, String> {
            HashMap::new()
        }
        fn update_repos_parser(&self) -> HashMap<SystemProduct, String> {
            HashMap::new()
        }
        fn list_update_commands(&self, _targets: &HostsGroup) {}
        async fn check_hash(&self) -> HashCheck {
            HashCheck::Ok
        }
    }

    fn meta_report() -> MetaReport {
        let mut base = TestReportBase::new(config());
        base.category = "recommended".to_owned();
        base.reviewer = "alice".to_owned();
        base.bugs.insert("1200000".to_owned(), "boom".to_owned());
        base.jira.insert("PED-1".to_owned(), "epic".to_owned());
        base.testplatforms.push("base=sles".to_owned());
        base.rrid = Some("SUSE:Maintenance:1:1".parse().unwrap());
        base.rating = Some("moderate".to_owned());
        base.giteapr = Some("42".to_owned());
        MetaReport { base }
    }

    #[test]
    fn report_urls_are_derived_from_id_and_config() {
        let r = meta_report();
        assert!(r.testreport_url().ends_with("/SUSE:Maintenance:1:1/log"));
        assert!(r.fancy_report_url().ends_with("/SUSE:Maintenance:1:1/log"));
    }

    #[test]
    fn bug_maps_returns_sorted_maps() {
        let r = meta_report();
        let (bugs, jira) = r.bug_maps();
        assert_eq!(bugs.get("1200000").map(String::as_str), Some("boom"));
        assert_eq!(jira.get("PED-1").map(String::as_str), Some("epic"));
    }

    #[test]
    fn show_yourself_data_drops_empty_and_sorts() {
        let r = meta_report();
        let rows = r.show_yourself_data();
        assert!(rows.iter().all(|(_, v)| !v.is_empty()));
        let labels: Vec<&str> = rows.iter().map(|(l, _)| l.as_str()).collect();
        let mut sorted = labels.clone();
        sorted.sort_unstable();
        assert_eq!(labels, sorted);
        // Populated fields surface; empty ones (Packager) do not.
        let has = |name: &str| rows.iter().any(|(l, _)| l == name);
        assert!(has("Category"));
        assert!(has("Reviewer"));
        assert!(has("Bugs"));
        assert!(has("Testplatform"));
        assert!(has("ReviewRequestID"));
        assert!(has("Rating"));
        assert!(has("Gitea PR"));
        assert!(!has("Packager"));
        let build = rows.iter().find(|(l, _)| l == "Build checks").unwrap();
        assert!(build.1.ends_with("build_checks"), "{}", build.1);
    }

    #[test]
    fn rrid_workflow_gitea_incident_accessors() {
        let empty = MetaReport {
            base: TestReportBase::new(config()),
        };
        assert!(empty.rrid().is_none());
        assert!(empty.incident_id().is_none());

        let mut base = TestReportBase::new(config());
        base.rrid = Some("SUSE:Maintenance:12345:67890".parse().unwrap());
        base.giteaprapi = Some("https://gitea/api/pr/1".to_owned());
        base.giteacohash = Some("deadbeef".to_owned());
        base.update_source = UpdateSource::Git;
        base.workflow = Workflow::Kernel;
        let r = MetaReport { base };
        assert_eq!(r.rrid().unwrap().maintenance_id, "12345");
        assert_eq!(r.incident_id().as_deref(), Some("12345"));
        assert_eq!(r.giteaprapi(), Some("https://gitea/api/pr/1"));
        assert_eq!(r.giteacohash(), Some("deadbeef"));
        assert_eq!(r.update_source(), UpdateSource::Git);
        assert_eq!(r.workflow(), Workflow::Kernel);
    }

    /// Build a report backed by a temp template containing `body`.
    fn report_with_template(dir: &tempfile::TempDir, body: &str) -> (MetaReport, PathBuf) {
        let path = dir.path().join("log");
        std::fs::write(&path, body).unwrap();
        let mut base = TestReportBase::new(config());
        base.path = Some(path.clone());
        (MetaReport { base }, path)
    }

    fn marker(channel: &str, ts: &str) -> SlackReviewMarker {
        SlackReviewMarker {
            channel: channel.to_owned(),
            ts: ts.to_owned(),
        }
    }

    #[test]
    fn set_slack_review_inserts_when_the_marker_is_absent() {
        // The marker never pre-exists, so a replace-only implementation (like
        // set_reviewer's) would fail on every first use.
        let dir = tempfile::tempdir().unwrap();
        let (mut r, path) = report_with_template(
            &dir,
            "Category: recommended\nTest Plan Reviewer: bob\nEnd\n",
        );

        r.set_slack_review(&marker("C123", "1700000000.000100"))
            .unwrap();

        let written = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            written,
            "Category: recommended\nTest Plan Reviewer: bob\nSlack Review: C123 1700000000.000100\nEnd\n"
        );
        assert_eq!(
            r.base().slack_review,
            Some(marker("C123", "1700000000.000100"))
        );
    }

    #[test]
    fn set_slack_review_replaces_an_existing_marker() {
        // Re-running request_review must re-point the gate, not leave two markers.
        let dir = tempfile::tempdir().unwrap();
        let (mut r, path) = report_with_template(
            &dir,
            "Test Plan Reviewer: bob\nSlack Review: COLD 1.0\nEnd\n",
        );

        r.set_slack_review(&marker("CNEW", "2.0")).unwrap();

        let written = std::fs::read_to_string(&path).unwrap();
        assert!(written.contains("Slack Review: CNEW 2.0"), "{written}");
        assert!(!written.contains("COLD"), "{written}");
        assert_eq!(written.matches("Slack Review:").count(), 1, "{written}");
    }

    #[test]
    fn set_slack_review_collapses_duplicate_markers() {
        // A hand-edited or merged template can carry several markers; the writer
        // must leave exactly the one the reader would have picked.
        let dir = tempfile::tempdir().unwrap();
        let (mut r, path) = report_with_template(
            &dir,
            "Test Plan Reviewer: bob\nSlack Review: C1 1.0\nmiddle\nSlack Review: C2 2.0\nEnd\n",
        );

        r.set_slack_review(&marker("CNEW", "3.0")).unwrap();

        let written = std::fs::read_to_string(&path).unwrap();
        assert_eq!(written.matches("Slack Review:").count(), 1, "{written}");
        assert!(written.contains("Slack Review: CNEW 3.0"), "{written}");
        // Unrelated content between the markers survives.
        assert!(written.contains("middle"), "{written}");
        assert!(
            written.ends_with('\n'),
            "trailing newline kept: {written:?}"
        );
    }

    #[test]
    fn set_slack_review_needs_an_anchor_and_a_template() {
        let dir = tempfile::tempdir().unwrap();

        // No reviewer line: refuse rather than guess where the marker goes.
        let (mut r, _) = report_with_template(&dir, "Category: recommended\nEnd\n");
        assert!(matches!(
            r.set_slack_review(&marker("C1", "1.0")).unwrap_err(),
            SlackReviewError::NoAnchor
        ));

        // No template loaded at all.
        let mut unloaded = MetaReport {
            base: TestReportBase::new(config()),
        };
        assert!(matches!(
            unloaded.set_slack_review(&marker("C1", "1.0")).unwrap_err(),
            SlackReviewError::NoTemplate
        ));
    }

    #[test]
    fn set_slack_review_leaves_memory_untouched_when_the_write_fails() {
        // Mirrors set_reviewer: a caller that aborts must not believe it persisted.
        let dir = tempfile::tempdir().unwrap();
        let mut base = TestReportBase::new(config());
        base.path = Some(dir.path().join("does/not/exist/log"));
        let mut r = MetaReport { base };

        assert!(r.set_slack_review(&marker("C1", "1.0")).is_err());
        assert_eq!(r.base().slack_review, None);
    }

    #[test]
    fn slack_marker_round_trips_through_its_line() {
        let m = marker("C0123456789", "1700000000.000100");
        assert_eq!(SlackReviewMarker::parse_line(&m.to_line()), Some(m));
    }

    #[test]
    fn slack_marker_rejects_malformed_lines() {
        // A truncated or over-long marker points at no real message.
        assert_eq!(SlackReviewMarker::parse_line("Slack Review: C1"), None);
        assert_eq!(SlackReviewMarker::parse_line("Slack Review:"), None);
        assert_eq!(
            SlackReviewMarker::parse_line("Slack Review: C1 1.0 extra"),
            None
        );
        assert_eq!(SlackReviewMarker::parse_line("Reviewer: bob"), None);
    }

    #[test]
    fn set_reviewer_rewrites_template_line_and_updates_memory() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("log");
        std::fs::write(
            &path,
            "Category: recommended\nTest Plan Reviewer: old\nEnd\n",
        )
        .unwrap();
        let mut base = TestReportBase::new(config());
        base.path = Some(path.clone());
        let mut r = MetaReport { base };

        r.set_reviewer("  bob  ").unwrap();
        assert_eq!(r.base().reviewer, "bob");
        let written = std::fs::read_to_string(&path).unwrap();
        assert!(written.contains("Test Plan Reviewer: bob"), "{written}");
        assert!(!written.contains("old"), "{written}");
    }

    #[test]
    fn set_reviewer_normalizes_legacy_suggested_line() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("log");
        std::fs::write(&path, "Suggested Test Plan Reviewer: \n").unwrap();
        let mut base = TestReportBase::new(config());
        base.path = Some(path.clone());
        let mut r = MetaReport { base };
        r.set_reviewer("carol").unwrap();
        let written = std::fs::read_to_string(&path).unwrap();
        assert_eq!(written.trim(), "Test Plan Reviewer: carol");
    }

    #[test]
    fn set_reviewer_rejects_empty_missing_path_and_missing_line() {
        // Empty name.
        assert!(matches!(
            MetaReport {
                base: TestReportBase::new(config())
            }
            .set_reviewer("   "),
            Err(ReviewerError::Empty)
        ));
        // No template path loaded.
        assert!(matches!(
            MetaReport {
                base: TestReportBase::new(config())
            }
            .set_reviewer("bob"),
            Err(ReviewerError::NoTemplate)
        ));
        // Path set but no reviewer line.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("log");
        std::fs::write(&path, "Category: recommended\n").unwrap();
        let mut base = TestReportBase::new(config());
        base.path = Some(path);
        assert!(matches!(
            MetaReport { base }.set_reviewer("bob"),
            Err(ReviewerError::NoReviewerLine)
        ));
    }

    #[tokio::test]
    async fn release_pool_claims_drops_arbiter_ownership_and_clears_claims() {
        let owner: Owner = ("reg-1".to_owned(), "SUSE:Maintenance:1:1".to_owned());
        // Leaked to get the `&'static` the field expects without touching the
        // shared process-global singleton.
        let arbiter: &'static HostArbiter = Box::leak(Box::new(HostArbiter::new()));
        assert!(arbiter.try_acquire("h1", &owner));
        assert!(arbiter.try_acquire("h2", &owner));

        let mut base = TestReportBase::new(config());
        base.arbiter = Some(arbiter);
        base.owner = Some(owner.clone());
        base.pool_claims.insert("h1".to_owned());
        base.pool_claims.insert("h2".to_owned());
        base.slot_candidates
            .insert("slot0".to_owned(), vec!["h1".to_owned(), "h2".to_owned()]);
        let mut r = MetaReport { base };

        r.release_pool_claims().await;

        assert!(r.base().pool_claims.is_empty());
        assert!(r.base().slot_candidates.is_empty());
        let arbiter = r.base().arbiter.as_ref().unwrap();
        assert!(arbiter.owner_of("h1").is_none());
        assert!(arbiter.owner_of("h2").is_none());
    }

    #[tokio::test]
    async fn release_pool_claims_is_a_noop_when_pooling_never_used() {
        // No arbiter / owner / claims: must not panic and stays empty.
        let mut r = MetaReport {
            base: TestReportBase::new(config()),
        };
        r.release_pool_claims().await;
        r.release_pool_claims().await; // idempotent second call
        assert!(r.base().pool_claims.is_empty());
        assert!(r.base().arbiter.is_none());
    }

    #[tokio::test]
    async fn release_pool_claim_frees_host_and_keeps_siblings() {
        let owner: Owner = ("reg-1".to_owned(), "SUSE:Maintenance:1:1".to_owned());
        let arbiter: &'static HostArbiter = Box::leak(Box::new(HostArbiter::new()));
        assert!(arbiter.try_acquire("h1", &owner));
        assert!(arbiter.try_acquire("h2", &owner));

        let mut base = TestReportBase::new(config());
        base.arbiter = Some(arbiter);
        base.owner = Some(owner.clone());
        base.pool_claims.insert("h1".to_owned());
        base.pool_claims.insert("h2".to_owned());
        // h1 primary, h2 backup sibling, in one slot.
        base.slot_candidates
            .insert("slot0".to_owned(), vec!["h1".to_owned(), "h2".to_owned()]);
        let mut r = MetaReport { base };

        r.release_pool_claim("h1");

        assert!(!r.base().pool_claims.contains("h1"));
        assert!(r.base().pool_claims.contains("h2"));
        // The freed host is re-acquirable by another owner; the sibling is not.
        let arbiter = r.base().arbiter.as_ref().unwrap();
        let other: Owner = ("reg-2".to_owned(), "SUSE:Maintenance:2:2".to_owned());
        assert!(arbiter.try_acquire("h1", &other));
        assert_eq!(arbiter.owner_of("h2"), Some(owner.clone()));
        assert_eq!(
            r.base().slot_candidates.get("slot0"),
            Some(&vec!["h2".to_owned()])
        );
    }

    #[tokio::test]
    async fn release_pool_claim_prunes_empty_slot() {
        let owner: Owner = ("reg-1".to_owned(), "SUSE:Maintenance:1:1".to_owned());
        let arbiter: &'static HostArbiter = Box::leak(Box::new(HostArbiter::new()));
        assert!(arbiter.try_acquire("only", &owner));

        let mut base = TestReportBase::new(config());
        base.arbiter = Some(arbiter);
        base.owner = Some(owner.clone());
        base.pool_claims.insert("only".to_owned());
        base.slot_candidates
            .insert("slot0".to_owned(), vec!["only".to_owned()]);
        let mut r = MetaReport { base };

        r.release_pool_claim("only");

        assert!(r.base().slot_candidates.is_empty());
        assert!(r.base().pool_claims.is_empty());
        assert!(
            r.base()
                .arbiter
                .as_ref()
                .unwrap()
                .owner_of("only")
                .is_none()
        );
    }

    #[tokio::test]
    async fn release_pool_claim_is_a_noop_when_pooling_never_used() {
        let mut r = MetaReport {
            base: TestReportBase::new(config()),
        };
        // No arbiter/owner/claims: must not panic, idempotent.
        r.release_pool_claim("ghost");
        r.release_pool_claim("ghost");
        assert!(r.base().pool_claims.is_empty());
        assert!(r.base().arbiter.is_none());
    }
}
