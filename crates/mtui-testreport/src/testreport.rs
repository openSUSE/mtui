//! The [`TestReport`] trait and its shared-state carrier [`TestReportBase`].
//!
//! The state every concrete report (SL/PI/OBS/Null) shares lives in a plain
//! [`TestReportBase`] they embed, reached through [`TestReport::base`] /
//! [`TestReport::base_mut`] so trait-default and caller code need no downcast.
//!
//! The concrete lifecycle (load/refresh/commit/export) is [`crate::lifecycle`],
//! document ingest [`crate::ingest`], per-report host-connect logic
//! [`crate::reports`].

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use mtui_config::options::Config;
use mtui_datasources::openqa::kernel::KernelOpenQA;
use mtui_datasources::oqa_search::results::OpenQAOverviewResult;
use mtui_datasources::qem_dashboard::dashboard_openqa::DashboardAutoOpenQA;
use mtui_hosts::{HostArbiter, HostsGroup, Owner, SetRepo};
use mtui_types::package::Package;
use mtui_types::{OpenQAResults, RequestReviewID, System, SystemProduct, UpdateSource, Workflow};

use crate::reports::update_flow;

/// The concrete openQA state holder carried on a report.
///
/// Monomorphizes [`OpenQAResults`] to the QEM-dashboard "auto" result, the
/// per-instance "kernel" results and the `openqa_overview` payload.
pub type ReportOpenQA = OpenQAResults<DashboardAutoOpenQA, KernelOpenQA, OpenQAOverviewResult>;

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
    /// [`RepoManager::run_zypper`](mtui_hosts) consumes.
    pub update_repos: HashMap<SystemProduct, String>,
    /// Known hostnames for this report.
    pub hostnames: HashSet<String>,
    /// When non-empty, newly connected hosts are locked with this comment
    /// (set while a PI assignment is active).
    pub lock_comment: String,
    /// Process-global host arbiter: a borrow of the singleton
    /// ([`get_arbiter`](mtui_hosts::get_arbiter)); `None` for
    /// directly-constructed reports, which use the remote-lock-only connect path.
    pub arbiter: Option<&'static HostArbiter>,
    /// Composite `(registry_id, RRID)` ownership key. `None` until wired by the
    /// template registry.
    pub owner: Option<Owner>,
    /// Hosts this report has claimed through the arbiter (for release).
    pub pool_claims: HashSet<String>,
    /// Per-slot ordered candidate hostnames captured during pool selection, so
    /// connect can fall back to a sibling host when the primary claim fails.
    pub slot_candidates: HashMap<String, Vec<String>>,
    /// The [`slot_candidates`](Self::slot_candidates) key each pool host was
    /// drawn for, so a host that matches two slots stays with the one it serves.
    pub pool_slots: HashMap<String, String>,
    /// Set when a load asked for autoconnect; the actual connect is deferred
    /// until after the host arbiter is wired.
    pub autoconnect_pending: bool,
    /// The reason a load failed, stashed on the [`NullReport`](crate::reports::NullReport)
    /// substituted by [`make_testreport`](crate::make_testreport) so the caller
    /// can surface *why* (teregen / gitea / hash failure). `None` on a
    /// successfully loaded report.
    pub load_error: Option<String>,
    /// Set when this report **loaded successfully but its checked-out hash
    /// still differs from the Gitea PR** and the operator or caller
    /// (`force_continue`) proceeded with stale content anyway. A non-interactive
    /// caller (`mtui-mcp`) never sees the `warn!` line, so it surfaces this from
    /// the tool result instead.
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
    /// verifies the ack against the exact message.
    pub slack_review: Option<SlackReviewMarker>,
    /// Update repository string.
    pub repository: String,
    /// Update repository URLs.
    pub repositories: HashSet<String>,
    /// `SystemProduct -> the package names this update composes for it`,
    /// indexed from the document's `install.targets[].binaries`.
    ///
    /// Empty when the document composes nothing (`ingest::should_compose`
    /// owns that rule).
    pub composed: HashMap<SystemProduct, BTreeSet<String>>,
    /// `target version -> { package name -> version }`, or `"standard"` for
    /// SLFO/PI. Products sharing a version share an entry, so hosts are seeded
    /// from [`host_packages`](Self::host_packages) instead.
    pub packages: HashMap<String, HashMap<String, String>>,
    /// What [`packages_for_host`] seeds each connected host with.
    pub host_packages: Arc<HostPackages>,
    /// Parsed Request Review ID, or `None` when unset/invalid.
    pub rrid: Option<RequestReviewID>,
    /// Update rating.
    pub rating: Option<String>,
    /// Raw request id from the report document (`id`).
    pub realid: Option<String>,
    /// Gitea pull-request reference (JSON key `gitea_pr`).
    pub giteapr: Option<String>,
    /// Gitea pull-request API URL (JSON key `gitea_pr_api`).
    pub giteaprapi: Option<String>,
    /// Gitea commit hash (JSON key `gitea_commit_hash`).
    pub giteacohash: Option<String>,
    /// Which update workflow mtui drives for this report, resolved once at
    /// load from the document's `workflow`. Defaults to
    /// [`UpdateSource::Obs`] until a report is loaded.
    pub update_source: UpdateSource,
    /// `hostname -> product-drift warning lines` from the last connect.
    pub product_warnings: HashMap<String, Vec<String>>,
    /// The report's openQA results.
    ///
    /// Empty until `reload_openqa` / `set_workflow` populate it; consumed by
    /// the exporters for openQA-enriched templates.
    pub openqa: ReportOpenQA,
    /// The v2 document this report was loaded from; `None` only on a
    /// `NullReport`/unloaded report.
    pub document: Option<mtui_types::report_document::ReportDocument>,
    /// The [`document`](Self::document)'s response `ETag`, when the server
    /// sent one. `None` alongside `document`.
    pub document_etag: Option<String>,
    /// How teregen's live report schema differs from the one this build ships
    /// (the first differing pointer), or `None` when it matches or could not be
    /// checked. While set, every write to teregen is refused.
    pub schema_drift: Option<String>,
    /// `true` while [`document`](Self::document) holds authored edits that no
    /// `commit` has uploaded yet.
    pub document_dirty: bool,
}

impl TestReportBase {
    /// Builds the shared state with its default values.
    ///
    /// `targets` starts headless (`is_repl = false`);
    /// [`make_testreport`](crate::make_testreport) reconciles it to the session
    /// mode once via [`set_is_repl`](HostsGroup::set_is_repl); the flag is never
    /// mutated afterwards.
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
            pool_slots: HashMap::new(),
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
            host_packages: Arc::default(),
            rrid: None,
            rating: None,
            realid: None,
            giteapr: None,
            giteaprapi: None,
            giteacohash: None,
            update_source: UpdateSource::default(),
            product_warnings: HashMap::new(),
            openqa: ReportOpenQA::new(),
            document: None,
            document_etag: None,
            document_dirty: false,
            schema_drift: None,
        }
    }

    /// Records that `export` authored the pointers in `touched` onto the
    /// document; an empty slice leaves the flag as it was.
    pub fn mark_document_authored(&mut self, touched: &[&str]) {
        if !touched.is_empty() {
            self.document_dirty = true;
        }
    }

    /// The working directory of the loaded report: the parent directory of
    /// [`path`](Self::path), created if absent, for install logs and other
    /// artifacts.
    ///
    /// Returns [`std::io::ErrorKind::NotFound`] when no report is loaded, and
    /// propagates any directory-creation error.
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

/// The packages connected hosts are seeded with, chosen at ingest by
/// document kind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostPackages {
    /// SLFO/PI: one product-agnostic set every host tracks.
    Shared(HashMap<String, String>),
    /// Per normalized product, keyed like [`TestReportBase::update_repos`].
    PerProduct(HashMap<SystemProduct, HashMap<String, String>>),
}

impl Default for HostPackages {
    /// Seeds nothing: a report not built from a document tracks nothing.
    fn default() -> Self {
        Self::PerProduct(HashMap::new())
    }
}

/// A host's seed packages. Where two of its products name one package, the
/// higher version wins: zypper installs the newest its repositories offer.
#[must_use]
pub fn packages_for_host(packages: &HostPackages, system: &System) -> Vec<Package> {
    let mut selected = std::collections::BTreeMap::<&str, &str>::new();
    match packages {
        HostPackages::Shared(shared) => {
            selected.extend(
                shared
                    .iter()
                    .map(|(name, ver)| (name.as_str(), ver.as_str())),
            );
        }
        HostPackages::PerProduct(by_product) => {
            for product in system.flatten() {
                for (name, ver) in by_product.get(&product).into_iter().flatten() {
                    if selected
                        .get(name.as_str())
                        .is_none_or(|kept| is_higher_version(ver, kept))
                    {
                        selected.insert(name, ver);
                    }
                }
            }
        }
    }
    selected
        .into_iter()
        .map(|(name, ver)| required_package(name, ver))
        .collect()
}

/// Whether `candidate` is a higher RPM version than `kept`; an unparseable
/// candidate never is.
pub(crate) fn is_higher_version(candidate: &str, kept: &str) -> bool {
    mtui_types::RPMVersion::parse(candidate).ok() > mtui_types::RPMVersion::parse(kept).ok()
}

/// A [`Package`] requiring `ver`; an unparseable version is logged and left
/// unset (best-effort) rather than aborting the whole host.
fn required_package(name: &str, ver: &str) -> Package {
    let mut pkg = Package::new(name);
    if pkg.set_required(Some(ver)).is_err() {
        tracing::warn!(
            package = %name, version = %ver,
            "unparseable required version in metadata; leaving package unseeded"
        );
    }
    pkg
}

/// The abstract test-report surface.
///
/// Concrete reports embed a [`TestReportBase`] and expose it through
/// [`base`](Self::base) / [`base_mut`](Self::base_mut); the other required
/// methods are the abstract surface, the rest trait defaults.
///
/// `#[async_trait]` because [`check_hash`](Self::check_hash) drives async I/O
/// for git-backed reports.
#[async_trait::async_trait]
pub trait TestReport {
    /// Borrows the shared state.
    fn base(&self) -> &TestReportBase;

    /// Mutably borrows the shared state.
    fn base_mut(&mut self) -> &mut TestReportBase;

    /// The report ID. Empty for an unloaded report.
    fn id(&self) -> String {
        self.rrid().map(ToString::to_string).unwrap_or_default()
    }

    /// The update-repository parser table, keyed like
    /// [`TestReportBase::update_repos`].
    fn update_repos_parser(&self) -> HashMap<SystemProduct, String>;

    /// Drops this report's arbiter ownership and removes its remote pool locks.
    ///
    /// Best-effort
    /// [`HostsGroup::pool_unlock_collecting`](mtui_hosts::HostsGroup::pool_unlock_collecting)
    /// with `force = false` (so a claim owned by another template is left alone) for
    /// every claimed host, then clears the in-process claim set and drops
    /// ownership via
    /// [`HostArbiter::release_owner`](mtui_hosts::HostArbiter::release_owner).
    ///
    /// The remote releases fan out concurrently under `budget`, so a host that
    /// stops answering costs at most `budget` and never holds up the others.
    /// The in-process state is cleared regardless. Returns the claimed hosts
    /// whose release had not finished when the budget ran out, sorted.
    ///
    /// Idempotent, and a no-op when pool selection was never used.
    async fn release_pool_claims(&mut self, budget: std::time::Duration) -> Vec<String> {
        let base = self.base_mut();
        let claims: BTreeSet<String> = base
            .pool_claims
            .iter()
            .filter(|host| base.targets.contains(host))
            .cloned()
            .collect();
        let collected = std::sync::Mutex::new(std::collections::BTreeMap::new());
        let _ = tokio::time::timeout(
            budget,
            base.targets
                .pool_unlock_collecting(false, &claims, &collected),
        )
        .await;
        let finished = collected.into_inner().unwrap_or_else(|e| e.into_inner());
        let unfinished = claims
            .into_iter()
            .filter(|host| !finished.contains_key(host))
            .collect();
        base.pool_claims.clear();
        base.slot_candidates.clear();
        base.pool_slots.clear();
        if let (Some(arbiter), Some(owner)) = (base.arbiter.as_ref(), base.owner.as_ref()) {
            arbiter.release_owner(owner);
        }
        unfinished
    }

    /// Releases one host's in-process arbiter claim and prunes it from the
    /// slot-candidate map.
    ///
    /// The per-host analogue of
    /// [`release_pool_claims`](Self::release_pool_claims), called from
    /// `remove_host`: without it a disconnected refhost stays claimed in the
    /// process-global [`HostArbiter`] for the server's lifetime.
    /// [`Target::close`](mtui_hosts::Target::close) drops the remote lock files;
    /// this clears the in-process ownership.
    ///
    /// Only `host` leaves each slot's candidate list (siblings stay available as
    /// fallbacks) and a slot is pruned only once empty. Idempotent, and a no-op
    /// when pool selection was never used.
    fn release_pool_claim(&mut self, host: &str) {
        let base = self.base_mut();
        base.pool_claims.remove(host);
        base.pool_slots.remove(host);
        base.slot_candidates.retain(|_slot, candidates| {
            candidates.retain(|c| c != host);
            !candidates.is_empty()
        });
        if let (Some(arbiter), Some(owner)) = (base.arbiter.as_ref(), base.owner.as_ref()) {
            arbiter.release(host, owner);
        }
    }

    /// The deduplicated list of every package named in the report metadata.
    ///
    /// Flattens the package **names** across all products of the nested
    /// [`packages`](TestReportBase::packages) map, sorted for reproducible
    /// snapshots.
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
    /// ids in a stable order.
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
    /// Rows with an empty value are dropped, and the set is sorted by label.
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
    /// Drives the install [`Operation`](mtui_hosts::Operation) template, which
    /// resolves each host's installer doer/check via the `PlanProvider` injected
    /// by `update_flow::perform_install`.
    ///
    /// Returns `Err` when the install command failed on one or more hosts
    /// (non-zero exit or non-empty stderr after the fan-out). Holds lost in
    /// the reboot gap are degradation, not failure: each is pushed onto
    /// `diagnostics`. A report with no [`as_set_repo`](Self::as_set_repo) (the
    /// null object) is a no-op `Ok(())`; so is every `perform_*` default below.
    async fn perform_install(
        &self,
        targets: &mut HostsGroup,
        packages: &[String],
        diagnostics: &mut Vec<crate::update_workflow::Diagnostic>,
    ) -> Result<(), crate::update_workflow::UpdateError> {
        if self.as_set_repo().is_none() {
            return Ok(());
        }
        update_flow::perform_install(targets, packages, diagnostics).await
    }

    /// Uninstalls `packages` from every host in `targets`.
    ///
    /// Drives the uninstall [`Operation`](mtui_hosts::Operation) template; see
    /// [`perform_install`](Self::perform_install).
    async fn perform_uninstall(
        &self,
        targets: &mut HostsGroup,
        packages: &[String],
        diagnostics: &mut Vec<crate::update_workflow::Diagnostic>,
    ) -> Result<(), crate::update_workflow::UpdateError> {
        if self.as_set_repo().is_none() {
            return Ok(());
        }
        update_flow::perform_uninstall(targets, packages, diagnostics).await
    }

    /// Prepares `packages` on every host.
    ///
    /// The bespoke (non-template) preparer flow, all under the operation lock:
    /// fan the issue repo add/remove out, install every package in a single
    /// transaction, run the preparer check, reboot transactional hosts.
    /// `testing` selects the repo-`add` vs repo-`remove` path and the testing
    /// preparer variant; `force` toggles `--force-resolution`; `installed_only`
    /// only touches already-installed packages.
    ///
    /// Returns `Err` on a missing preparer, lock contention, a failed issue-repo
    /// fan-out, a per-host prepare-command failure, or a prepare check failure.
    async fn perform_prepare(
        &self,
        targets: &mut HostsGroup,
        packages: &[String],
        force: bool,
        testing: bool,
        installed_only: bool,
        diagnostics: &mut Vec<crate::update_workflow::Diagnostic>,
    ) -> Result<(), crate::update_workflow::UpdateError> {
        let Some(report) = self.as_set_repo() else {
            return Ok(());
        };
        update_flow::perform_prepare(
            targets,
            report,
            packages,
            force,
            testing,
            installed_only,
            diagnostics,
        )
        .await
    }

    /// Downgrades `packages` on every host.
    ///
    /// Under the operation lock: remove the issue repos, resolve each package's
    /// available downgrade version, downgrade (per-package for non-transactional
    /// hosts, one transaction for transactional ones), run the check, reboot
    /// transactional hosts.
    ///
    /// Returns `Err` on a missing downgrader, lock contention, or a per-host
    /// downgrade check failure.
    async fn perform_downgrade(
        &self,
        targets: &mut HostsGroup,
        packages: &[String],
        diagnostics: &mut Vec<crate::update_workflow::Diagnostic>,
    ) -> Result<(), crate::update_workflow::UpdateError> {
        let Some(report) = self.as_set_repo() else {
            return Ok(());
        };
        let id = self.rrid().map(ToString::to_string);
        update_flow::perform_downgrade(targets, report, packages, id.as_deref(), diagnostics).await
    }

    /// Updates the hosts with this report's maintenance update.
    ///
    /// The full bespoke update flow: optional prepare, package checks, repo add,
    /// the per-host update check, transactional reboot, and the two-phase repo
    /// cleanup — remove on success, **keep** on failure for retry/diagnosis.
    /// `noprepare` skips the initial prepare; `newpackage` runs a testing
    /// prepare afterwards.
    ///
    /// Returns `Err` on a per-host `updater` check failure (after a best-effort
    /// downgrade rollback) or a hard missing-updater failure; non-fatal
    /// diagnostics from the check are appended to `diagnostics`.
    async fn perform_update(
        &self,
        targets: &mut HostsGroup,
        noprepare: bool,
        newpackage: bool,
        diagnostics: &mut Vec<crate::update_workflow::Diagnostic>,
    ) -> Result<(), crate::update_workflow::UpdateError> {
        let Some(report) = self.as_set_repo() else {
            return Ok(());
        };
        let packages = self.get_package_list();
        update_flow::perform_update_with_rollback(
            report,
            self.rrid(),
            &packages,
            targets,
            noprepare,
            newpackage,
            diagnostics,
        )
        .await
    }

    /// Verifies the loaded template hash.
    ///
    /// Returns a [`HashCheck`] rather than raising, so
    /// [`make_testreport`](crate::make_testreport) can branch on it. The default,
    /// for the null object and the non-git reports (OBS/PI), is
    /// [`HashCheck::Ok`]; git-backed reports compare against a hash from Gitea.
    async fn check_hash(&self) -> HashCheck {
        HashCheck::Ok
    }

    /// The working directory for target artifacts.
    ///
    /// Defaults to joining `config.target_tempdir` with `paths`; concrete
    /// reports override to root under the loaded report.
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
    /// A `dyn TestReport` cannot be downcast to the object-safe `SetRepo`, but
    /// [`HostsGroup::fanout_set_repo`](mtui_hosts::HostsGroup) needs one.
    /// SL/PI/OBS return `Some(self)`; the null report keeps `None`, which makes
    /// every `perform_*` default a no-op.
    fn as_set_repo(&self) -> Option<&dyn SetRepo> {
        None
    }

    /// The report's parsed [`RequestReviewID`]; `None` for the null report.
    fn rrid(&self) -> Option<&RequestReviewID> {
        self.base().rrid.as_ref()
    }

    /// The report's workflow mode.
    fn workflow(&self) -> Workflow {
        self.base().workflow
    }

    /// The report's openQA state holder; empty for the null report.
    fn openqa(&self) -> &ReportOpenQA {
        &self.base().openqa
    }

    /// Mutably borrows the report's openQA state holder.
    ///
    /// The mutable counterpart of [`openqa`](Self::openqa).
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
    /// See [`UpdateSource`].
    fn update_source(&self) -> UpdateSource {
        self.base().update_source
    }

    /// The openQA incident id used by the QEM Dashboard / oqa-search queries.
    ///
    /// Uses `rrid.maintenance_id`; `None` for the null report.
    fn incident_id(&self) -> Option<String> {
        self.base().rrid.as_ref().map(|r| r.maintenance_id.clone())
    }
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

/// The outcome of [`TestReport::check_hash`], so the load path
/// ([`make_testreport`](crate::make_testreport)) can branch explicitly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HashCheck {
    /// The template hash matches the Gitea PR head, or there is nothing to
    /// verify (null / OBS / PI reports).
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

#[cfg(test)]
mod tests {
    use super::*;
    use mtui_config::options::Config;
    use std::time::Duration;

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
        assert_eq!(
            *base.host_packages,
            HostPackages::PerProduct(HashMap::new())
        );
        assert!(base.rrid.is_none());
        assert!(base.rating.is_none());
        assert!(base.realid.is_none());
        assert!(base.giteapr.is_none());
        assert!(base.giteaprapi.is_none());
        assert!(base.giteacohash.is_none());
        assert_eq!(base.update_source, UpdateSource::Obs);
        assert!(base.product_warnings.is_empty());
        // Unset until a document is loaded.
        assert!(base.document.is_none());
        assert!(base.document_etag.is_none());
        assert!(!base.document_dirty);
    }

    #[test]
    fn mark_document_authored_ignores_an_empty_touch_list() {
        let mut base = TestReportBase::new(config());
        base.mark_document_authored(&[]);
        assert!(!base.document_dirty);
    }

    #[test]
    fn mark_document_authored_sets_the_flag_when_something_was_touched() {
        let mut base = TestReportBase::new(config());
        base.mark_document_authored(&["/testing/openqa"]);
        assert!(base.document_dirty);
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

    // --- packages_for_host ---

    /// `RPMVersion::parse` rejects only an empty string, which never outranks a
    /// version and is always outranked by one.
    #[test]
    fn is_higher_version_ranks_any_version_above_an_unparseable_one() {
        assert!(is_higher_version("1.0-1", ""));
        assert!(!is_higher_version("", "1.0-1"));
    }

    type TargetSpec<'a> = (&'a str, &'a str, &'a str, &'a [(&'a str, &'a str)]);

    /// The package maps of a document ingested from `raw`.
    fn ingested(raw: &str) -> TestReportBase {
        let doc: mtui_types::report_document::ReportDocument = raw.parse().expect("fixture parses");
        let mut base = TestReportBase::new(config());
        crate::ingest::apply_document(&mut base, &doc);
        base
    }

    /// `MAINTENANCE_OBS` with its targets replaced by `(product, version, arch,
    /// [(name, version-release)])` rows, then ingested.
    fn ingested_with_targets(targets: &[TargetSpec<'_>]) -> TestReportBase {
        let mut doc: mtui_types::report_document::ReportDocument =
            mtui_types::fixtures::MAINTENANCE_OBS.parse().unwrap();
        let template = doc.install.targets[0].clone();
        doc.install.targets = targets
            .iter()
            .map(|(product, version, arch, binaries)| {
                let mut t = template.clone();
                t.product = (*product).to_owned();
                t.version = (*version).to_owned();
                t.arch = (*arch).to_owned();
                t.binaries = binaries
                    .iter()
                    .map(|(name, ver)| ((*name).to_owned(), format!("{ver}.{arch}")))
                    .collect();
                t
            })
            .collect();
        let mut base = TestReportBase::new(config());
        crate::ingest::apply_document(&mut base, &doc);
        base
    }

    fn host(base: (&str, &str, &str), addons: &[(&str, &str, &str)]) -> System {
        System::new(
            SystemProduct::new(base.0, base.1, base.2),
            addons
                .iter()
                .map(|(n, v, a)| SystemProduct::new(*n, *v, *a))
                .collect(),
            false,
        )
    }

    /// `(name, required)` of what `packages_for_host` seeds `system` with.
    fn seeded(base: &TestReportBase, system: &System) -> Vec<(String, Option<String>)> {
        packages_for_host(&base.host_packages, system)
            .iter()
            .map(|p| (p.name.clone(), p.required().map(ToString::to_string)))
            .collect()
    }

    fn want(rows: &[(&str, &str)]) -> Vec<(String, Option<String>)> {
        rows.iter()
            .map(|(n, v)| ((*n).to_owned(), Some((*v).to_owned())))
            .collect()
    }

    /// A report whose only target is an LTSS one still seeds the plain-SLES
    /// base host that carries the LTSS addon.
    #[test]
    fn packages_for_host_seeds_a_base_host_from_its_ltss_addon() {
        let base = ingested(mtui_types::fixtures::MAINTENANCE_OBS);
        let system = host(
            ("SLES", "15-SP6", "s390x"),
            &[("SLES-LTSS", "15-SP6", "s390x")],
        );
        assert_eq!(
            seeded(&base, &system),
            want(&[
                ("smc-tools", "1.8.8-150600.3.9.1"),
                ("smc-tools-completion", "1.8.8-150600.3.9.1"),
            ])
        );
    }

    /// A TERADATA host gets only the TERADATA target's packages, not those of
    /// another product that shares its base version.
    #[test]
    fn packages_for_host_seeds_only_the_matching_products_targets() {
        let base = ingested_with_targets(&[
            (
                "SLE-Product-SLES",
                "15-SP4-TERADATA",
                "x86_64",
                &[("stunnel", "5.62-150200.3.19.1")],
            ),
            (
                "SLE-Product-SLES_SAP",
                "15-SP4",
                "x86_64",
                &[("smc-tools", "1.8.8-150600.3.9.1")],
            ),
        ]);
        let system = host(("SLES_TERADATA", "15-SP4", "x86_64"), &[]);
        assert_eq!(
            seeded(&base, &system),
            want(&[("stunnel", "5.62-150200.3.19.1")])
        );
    }

    #[test]
    fn packages_for_host_skips_a_target_of_another_arch() {
        let base = ingested_with_targets(&[
            (
                "SLE-Product-SLES",
                "15-SP6-LTSS",
                "s390x",
                &[
                    ("smc-tools", "1.8.8-150600.3.9.1"),
                    ("smc-tools-completion", "1.8.8-150600.3.9.1"),
                ],
            ),
            (
                "SLE-Product-SLES",
                "15-SP6-LTSS",
                "x86_64",
                &[("smc-tools", "1.8.8-150600.3.9.1")],
            ),
        ]);
        let system = host(
            ("SLES", "15-SP6", "x86_64"),
            &[("SLES-LTSS", "15-SP6", "x86_64")],
        );
        assert_eq!(
            seeded(&base, &system),
            want(&[("smc-tools", "1.8.8-150600.3.9.1")])
        );
    }

    /// Each package keeps the higher required version of the two products
    /// naming it, whichever of them sorts first.
    #[test]
    fn packages_for_host_keeps_the_higher_version_two_products_name() {
        let base = ingested_with_targets(&[
            (
                "SLE-Product-SLES",
                "15-SP6",
                "s390x",
                &[
                    ("smc-tools", "1.8.8-150600.3.6.1"),
                    ("smc-tools-completion", "1.8.8-150600.3.9.1"),
                ],
            ),
            (
                "SLE-Product-SLES",
                "15-SP6-LTSS",
                "s390x",
                &[
                    ("smc-tools", "1.8.8-150600.3.9.1"),
                    ("smc-tools-completion", "1.8.8-150600.3.6.1"),
                ],
            ),
        ]);
        let system = host(
            ("SLES", "15-SP6", "s390x"),
            &[("SLES-LTSS", "15-SP6", "s390x")],
        );
        assert_eq!(
            seeded(&base, &system),
            want(&[
                ("smc-tools", "1.8.8-150600.3.9.1"),
                ("smc-tools-completion", "1.8.8-150600.3.9.1"),
            ])
        );
    }

    /// A host none of whose products a target names tracks nothing, even when
    /// its base version matches a target's: the update adds no repository there.
    #[test]
    fn packages_for_host_seeds_nothing_on_a_host_matching_no_product() {
        let base = ingested(mtui_types::fixtures::MAXIMAL);
        let system = host(("SLES_SAP", "15-SP6", "x86_64"), &[]);
        assert_eq!(seeded(&base, &system), want(&[]));
    }

    /// A SLE 12 module target (`SLE-Module-Public-Cloud 12`) seeds a 12-SP5 host
    /// through the module's own `.prod` version, `12`; a 12-SP5 host without the
    /// module tracks nothing.
    #[test]
    fn packages_for_host_seeds_a_sle12_host_only_through_its_module() {
        let base = ingested(mtui_types::fixtures::MAINTENANCE_OPENQA_L3);
        let with_module = host(
            ("SLES", "12-SP5", "x86_64"),
            &[("sle-module-public-cloud", "12", "x86_64")],
        );
        assert_eq!(
            seeded(&base, &with_module),
            want(&[("google-guest-agent", "20260903.01-1.73.1")])
        );
        let without = host(("SLES", "12-SP5", "x86_64"), &[]);
        assert_eq!(seeded(&base, &without), want(&[]));
    }

    #[test]
    fn packages_for_host_seeds_the_shared_set_for_a_product_increment() {
        let base = ingested(mtui_types::fixtures::PI);
        assert!(matches!(*base.host_packages, HostPackages::Shared(_)));
        let system = host(("SLES", "16.0", "x86_64"), &[]);
        assert_eq!(seeded(&base, &system), want(&[("examplepkg", "1.0-1.1")]));
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
        fn update_repos_parser(&self) -> HashMap<SystemProduct, String> {
            HashMap::new()
        }
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
        base.pool_slots.insert("h1".to_owned(), "slot0".to_owned());
        let mut r = MetaReport { base };

        r.release_pool_claims(Duration::from_secs(5)).await;

        assert!(r.base().pool_claims.is_empty());
        assert!(r.base().slot_candidates.is_empty());
        assert!(r.base().pool_slots.is_empty());
        let arbiter = r.base().arbiter.as_ref().unwrap();
        assert!(arbiter.owner_of("h1").is_none());
        assert!(arbiter.owner_of("h2").is_none());
    }

    #[tokio::test]
    async fn release_pool_claims_clears_state_and_names_only_the_hung_host_on_timeout() {
        use mtui_hosts::{MockConnection, POOL_LOCK_PATH, Target};
        use mtui_types::enums::TargetState;

        let rrid = "SUSE:Maintenance:1:1";
        let owner: Owner = ("reg-1".to_owned(), rrid.to_owned());
        let arbiter: &'static HostArbiter = Box::leak(Box::new(HostArbiter::new()));
        for host in ["hung", "healthy", "elsewhere"] {
            assert!(arbiter.try_acquire(host, &owner));
        }

        let me = config().session_user;
        let claim = format!("1700000000:{me}:1:mtui pool {rrid} [{rrid}]");
        let healthy = MockConnection::new("healthy").with_file(POOL_LOCK_PATH, claim);
        let hung = MockConnection::new("hung").with_sftp_session_delay(Duration::from_secs(3600));
        let mut healthy_target =
            Target::with_connection("healthy", TargetState::Enabled, Box::new(healthy.clone()));
        healthy_target.set_rrid(rrid);
        let hung_target = Target::with_connection("hung", TargetState::Enabled, Box::new(hung));

        let mut base = TestReportBase::new(config());
        base.arbiter = Some(arbiter);
        base.owner = Some(owner);
        base.targets = HostsGroup::new(vec![hung_target, healthy_target], false);
        for host in ["hung", "healthy", "elsewhere"] {
            base.pool_claims.insert(host.to_owned());
        }
        base.slot_candidates
            .insert("slot0".to_owned(), vec!["hung".to_owned()]);
        let mut r = MetaReport { base };

        let unfinished = tokio::time::timeout(
            Duration::from_secs(5),
            r.release_pool_claims(Duration::from_millis(100)),
        )
        .await
        .expect("the release must end at its budget");

        // "elsewhere" is claimed but not in the group: never named.
        assert_eq!(unfinished, vec!["hung".to_owned()]);
        assert!(
            healthy.file_contents(POOL_LOCK_PATH).is_none(),
            "healthy: the release itself removed the claim"
        );
        assert!(r.base().pool_claims.is_empty());
        assert!(r.base().slot_candidates.is_empty());
        for host in ["hung", "healthy", "elsewhere"] {
            assert!(arbiter.owner_of(host).is_none(), "{host}: owner released");
        }
    }

    #[tokio::test]
    async fn release_pool_claims_is_a_noop_when_pooling_never_used() {
        // No arbiter / owner / claims: must not panic and stays empty.
        let mut r = MetaReport {
            base: TestReportBase::new(config()),
        };
        r.release_pool_claims(Duration::from_secs(5)).await;
        r.release_pool_claims(Duration::from_secs(5)).await; // idempotent second call
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
        base.pool_slots.insert("h1".to_owned(), "slot0".to_owned());
        base.pool_slots.insert("h2".to_owned(), "slot1".to_owned());
        let mut r = MetaReport { base };

        r.release_pool_claim("h1");

        assert_eq!(
            r.base().pool_slots,
            HashMap::from([("h2".to_owned(), "slot1".to_owned())])
        );
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
