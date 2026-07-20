//! The [`TestReport`] trait and its shared-state carrier [`TestReportBase`].
//!
//! This is the Phase 4.1 skeleton. Upstream mtui models test reports with an
//! abstract base class (`mtui.test_reports.testreport.TestReport`) whose
//! `__init__` populates a large block of shared state, plus a handful of
//! `@abstractmethod`s that each concrete report (SL/PI/OBS/Null) must supply.
//!
//! Rust has no class inheritance, so the shared state lives in a plain
//! [`TestReportBase`] struct that concrete reports embed, and the abstract
//! surface becomes the [`TestReport`] trait. The trait requires
//! [`TestReport::base`]/[`TestReport::base_mut`] accessors so trait-default and
//! caller code can reach the shared state without downcasting — the idiomatic
//! "composition over inheritance" pattern (see `AGENTS.md`: keep the trait thin,
//! inject collaborators).
//!
//! Only the shared state and the abstract-method surface land here. The
//! concrete lifecycle (load/checkout/commit/export), metadata parsing, pool
//! selection, and host-connect logic arrive in the later Phase 4 tasks that
//! depend on this skeleton.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use mtui_config::options::Config;
use mtui_datasources::openqa::kernel::KernelOpenQA;
use mtui_datasources::oqa_search::results::OpenQAOverviewResult;
use mtui_datasources::qem_dashboard::dashboard_openqa::DashboardAutoOpenQA;
use mtui_hosts::{HostArbiter, HostsGroup, Owner, SetRepo};
use mtui_types::package::Package;
use mtui_types::{OpenQAResults, RequestReviewID, SystemProduct, Workflow};

/// The concrete openQA state holder carried on a report.
///
/// Monomorphizes upstream `metadata.openqa` (`OpenQAResults`) to the concrete
/// connectors: the QEM-dashboard "auto" result, the per-instance "kernel"
/// results, and the `openqa_overview` payload. `mtui-testreport` already depends
/// on `mtui-datasources`, so pinning these types here adds no new crate edge.
pub type ReportOpenQA = OpenQAResults<DashboardAutoOpenQA, KernelOpenQA, OpenQAOverviewResult>;

use crate::checkout::TemplateIoError;
use crate::metadata_parsers::{JSONParser, ReducedMetadataParser, patchinfo_titles};

/// Shared state common to every [`TestReport`] implementation.
///
/// Ported field-for-field from upstream `TestReport.__init__` so that concrete
/// reports and the workflow engine observe the same defaults. Fields whose
/// behavior is only exercised by later Phase 4 tasks (pool selection, host
/// arbitration, connect) are carried here as pure state — no logic is wired to
/// them yet.
///
/// The [`openqa`](Self::openqa) holder carries the report's openQA state
/// ([`ReportOpenQA`]) — the QEM-dashboard "auto" result, the per-instance
/// "kernel" results, and the `openqa_overview` payload — populated by the
/// `reload_openqa` / `set_workflow` commands and consumed by the exporters.
///
/// Note: no `#[derive(Debug)]` — several embedded `mtui-hosts` collaborators
/// (`HostsGroup`, `HostArbiter`) do not implement `Debug`. A hand-written
/// summary impl can be added when a concrete need arises.
pub struct TestReportBase {
    /// The application configuration.
    pub config: Config,
    /// Per-report workflow mode (upstream replaced the global `config.auto` /
    /// `config.kernel` with this). Defaults to [`Workflow::Manual`].
    pub workflow: Workflow,
    /// Working directory for the report; defaults to `config.template_dir`.
    pub directory: PathBuf,
    /// Path to the loaded testreport file, or `None` when nothing is loaded.
    pub path: Option<PathBuf>,
    /// `hostname -> system` mapping.
    pub systems: HashMap<String, String>,
    /// Connected reference-host targets.
    pub targets: HostsGroup,
    /// `SystemProduct -> repository` map for the update repositories.
    ///
    /// Keyed on the flat [`SystemProduct`] `(name, version, arch)` tuple —
    /// upstream's `Product` `NamedTuple`. This is what the `*repoparse` helpers
    /// build and what [`RepoManager::run_zypper`](mtui_hosts) consumes; keying
    /// on the refhost `Product` (no `arch`) would be lossy and mismatch that
    /// consumer.
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
    /// Update repository string.
    pub repository: String,
    /// Update repository URLs (upstream `repositories`, a `frozenset[str]`).
    pub repositories: HashSet<String>,
    /// Nested package map: `product -> { package name -> version }`.
    ///
    /// A test report routinely spans multiple products, each shipping its own
    /// set of packages and versions (mirrors upstream `self.packages`, a
    /// `dict[str, dict[str, str]]`). Consumed by the future `get_package_list`
    /// which iterates products and flattens their package sets.
    pub packages: HashMap<String, HashMap<String, String>>,
    /// Parsed Request Review ID (upstream `rrid`), or `None` when unset/invalid.
    pub rrid: Option<RequestReviewID>,
    /// Update rating (upstream `rating`).
    pub rating: Option<String>,
    /// Raw request id from the metadata envelope (upstream `realid`, JSON `id`).
    pub realid: Option<String>,
    /// Gitea pull-request reference (upstream `giteapr`, JSON `gitea_pr`).
    pub giteapr: Option<String>,
    /// Gitea pull-request API URL (upstream `giteaprapi`, JSON `gitea_pr_api`).
    pub giteaprapi: Option<String>,
    /// Gitea commit hash (upstream `giteacohash`, JSON `gitea_commit_hash`).
    pub giteacohash: Option<String>,
    /// `hostname -> product-drift warning lines` from the last connect.
    pub product_warnings: HashMap<String, Vec<String>>,
    /// The report's openQA results (upstream `metadata.openqa`).
    ///
    /// Empty until `reload_openqa` / `set_workflow` populate it; consumed by the
    /// exporters for openQA-enriched templates.
    pub openqa: ReportOpenQA,
}

impl TestReportBase {
    /// Builds the shared state with upstream `TestReport.__init__` defaults.
    ///
    /// `directory` mirrors upstream by defaulting to `config.template_dir`. The
    /// targets [`HostsGroup`] starts headless (`is_repl = false`); the load site
    /// ([`make_testreport`](crate::make_testreport)) reconciles it to the
    /// session mode once, via [`set_is_repl`](Self::set_is_repl), before the
    /// report is handed to the session — the session is the single source of
    /// truth and the flag is never mutated afterwards.
    #[must_use]
    pub fn new(config: Config) -> Self {
        let directory = config.template_dir.clone();
        Self {
            config,
            workflow: Workflow::Manual,
            directory,
            path: None,
            systems: HashMap::new(),
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
            bugs: HashMap::new(),
            jira: HashMap::new(),
            testplatforms: Vec::new(),
            products: Vec::new(),
            category: String::new(),
            packager: String::new(),
            reviewer: String::new(),
            repository: String::new(),
            repositories: HashSet::new(),
            packages: HashMap::new(),
            rrid: None,
            rating: None,
            realid: None,
            giteapr: None,
            giteaprapi: None,
            giteacohash: None,
            product_warnings: HashMap::new(),
            openqa: ReportOpenQA::new(),
        }
    }

    /// The working directory of the loaded report checkout (upstream
    /// `report_wd`).
    ///
    /// Upstream returns `ensure_dir_exists(self.path.parent, *paths)`; the base
    /// case every current caller needs is the parent directory of the loaded
    /// report [`path`](Self::path), created if absent. The OBS report feeds this
    /// to [`obsrepoparse`](crate::reports::repoparse::obsrepoparse), which reads
    /// `project.xml` from it.
    ///
    /// Returns [`io::ErrorKind::NotFound`] when no report is loaded (upstream
    /// `assert self.path, "empty path"`), and propagates any directory-creation
    /// error, so callers can degrade explicitly rather than panic.
    ///
    /// The variadic `*paths` join upstream accepts is intentionally omitted: no
    /// current caller needs it. Extend when the load/checkout lifecycle task
    /// introduces one, rather than speculating on the shape now.
    pub fn report_wd(&self) -> std::io::Result<PathBuf> {
        let path = self
            .path
            .as_ref()
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "empty path"))?;
        let dir = path.parent().unwrap_or_else(|| std::path::Path::new(""));
        std::fs::create_dir_all(dir)?;
        Ok(dir.to_path_buf())
    }

    /// Resolves the [`Package`] list to seed onto a host whose base product
    /// version is `base_version`, each carrying its metadata-`required` version.
    ///
    /// Ports upstream `Target._parse_packages` (`mtui/hosts/target/target.py`),
    /// which selects the right product sub-map of
    /// [`packages`](Self::packages) (`product -> { name -> version }`):
    ///
    /// * if the map holds exactly the single key `"standard"`, use it (a report
    ///   that ships one product-agnostic set — e.g. SLFO metadata);
    /// * otherwise use the sub-map keyed by the host's `base_version` (the
    ///   `parse_product` string, e.g. `"15-SP6"`, which equals the metadata
    ///   product key);
    /// * additionally, when `base_version` starts with `"12"`, merge in the
    ///   `"12"` sub-map (upstream's SLE-12 special case).
    ///
    /// Each resolved `name -> version` becomes a [`Package`] with its
    /// [`required`](Package::required) set. An unparseable version is skipped
    /// (best-effort, mirroring upstream's tolerant setters), leaving that
    /// package unseeded rather than aborting the whole host. Returns an empty
    /// vec when no sub-map matches (upstream returns `{}`).
    #[must_use]
    pub fn packages_for(&self, base_version: &str) -> Vec<Package> {
        packages_for_map(&self.packages, base_version)
    }
}

/// The free-standing body of [`TestReportBase::packages_for`], operating on a
/// borrowed `product -> { name -> version }` map.
///
/// Factored out so the composition root (`mtui-core::session`) can resolve seed
/// packages from a *snapshot* of the metadata map (cloned before a
/// `targets_mut()` borrow, to keep the connect future `Send`) without needing a
/// live `&TestReportBase` across the connect `.await`.
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
/// Mirrors the `@abstractmethod`s of upstream `TestReport`. Concrete reports
/// embed a [`TestReportBase`] and expose it through [`base`](Self::base) /
/// [`base_mut`](Self::base_mut); the remaining required methods are the abstract
/// surface every report must supply. Non-abstract lifecycle methods
/// (`connect_target`, `export`, pool selection, …) are added by the later
/// Phase 4 tasks.
///
/// The trait is `#[async_trait]` because [`check_hash`](Self::check_hash) drives
/// async I/O for git-backed reports (`SLTestReport` awaits `Gitea::get_hash`).
#[async_trait::async_trait]
pub trait TestReport {
    /// Borrows the shared state.
    fn base(&self) -> &TestReportBase;

    /// Mutably borrows the shared state.
    fn base_mut(&mut self) -> &mut TestReportBase;

    /// The report ID (upstream `id` property). Empty for an unloaded report.
    fn id(&self) -> String;

    /// The metadata field parser table (upstream `_parser`).
    ///
    /// Maps a template field name to its parsed value. The concrete shape of
    /// parsed values is refined in the metadata-parser task (P4.2); the
    /// skeleton uses `String` values, which the null object leaves empty.
    fn parser(&self) -> HashMap<String, String>;

    /// The update-repository parser table (upstream `_update_repos_parser`).
    ///
    /// Keyed on the flat [`SystemProduct`] to match the `*repoparse` helpers and
    /// [`TestReportBase::update_repos`].
    fn update_repos_parser(&self) -> HashMap<SystemProduct, String>;

    /// Reads and parses a checkout's test-report template into this report
    /// (upstream `TestReport.read` → `_open_and_parse` + `_update_repos_parse`).
    ///
    /// `path` names the checkout's `log` file; `metadata.json` is read from the
    /// same directory. The two-parser pipeline mirrors upstream `_parse_json`:
    /// the `hosts` parser ([`ReducedMetadataParser`]) is fed the `log` lines
    /// (reference hosts + bug/jira titles), then the `json` parser
    /// ([`JSONParser`]) applies the metadata envelope, and finally
    /// `patchinfo.xml` overlays real bug/jira titles onto the ids the envelope
    /// carried. On success [`path`](TestReportBase::path) is set and the
    /// update-repo map is derived via [`update_repos_parser`](Self::update_repos_parser).
    ///
    /// The Gitea-hash verification upstream runs at the tail of `read`
    /// (`check_hash` → `InvalidGiteaHashError`) is deferred to
    /// [`make_testreport`](crate::make_testreport): `read` is sync while
    /// [`check_hash`](Self::check_hash) is async (it fetches the PR head from
    /// Gitea), so the check fires in the async load orchestrator right after a
    /// successful read, before the report is handed back. This method is the
    /// parse-and-populate slice only.
    ///
    /// # Errors
    ///
    /// * [`ReadError::Template`] when the `log` file cannot be read (missing →
    ///   `ENOENT`, which the checkout seam treats as "needs checkout").
    /// * [`ReadError::MetadataMissing`] when `metadata.json` is absent.
    /// * [`ReadError::MetadataInvalid`] when `metadata.json` is not valid JSON.
    fn read(&mut self, path: &Path) -> Result<(), ReadError> {
        // `_open_and_parse`: read the template `log` and the sibling metadata.
        let tpl = std::fs::read_to_string(path).map_err(|e| {
            // A missing/unreadable template must carry its errno so the checkout
            // seam can branch on ENOENT (upstream `TemplateIOError`).
            ReadError::Template(TemplateIoError::from_io(&e))
        })?;

        let dir = path.parent().unwrap_or_else(|| Path::new(""));
        let metadata_path = dir.join("metadata.json");
        if !metadata_path.is_file() {
            return Err(ReadError::MetadataMissing);
        }
        let metadata = std::fs::read_to_string(&metadata_path)
            .map_err(|e| ReadError::Template(TemplateIoError::from_io(&e)))?;

        // `_parse_json`: hosts parser over the log lines, then the JSON envelope.
        let base = self.base_mut();
        for line in tpl.lines() {
            ReducedMetadataParser::parse(base, line);
        }
        JSONParser::parse_str(base, &metadata).map_err(|_| ReadError::MetadataInvalid)?;

        // `_enrich_issue_titles`: overlay real bug/jira titles from patchinfo.xml
        // onto the ids the envelope already carried, leaving the id set
        // authoritative (no new ids introduced).
        let titles = patchinfo_titles(dir);
        for (iid, title) in titles {
            if let Some(slot) = base.bugs.get_mut(&iid) {
                *slot = title;
            } else if let Some(slot) = base.jira.get_mut(&iid) {
                *slot = title;
            }
        }

        // Upstream `read` resolves the path and then derives update repos.
        self.base_mut().path = Some(path.to_path_buf());
        let repos = self.update_repos_parser();
        self.base_mut().update_repos = repos;
        Ok(())
    }

    /// Drops this report's arbiter ownership and removes its remote pool locks.
    ///
    /// Ports upstream `TestReport.release_pool_claims`: for every host this
    /// report claimed through the arbiter, best-effort
    /// [`Target::pool_unlock`](mtui_hosts::Target::pool_unlock) the remote
    /// pool-claim lock (`force = false`, so a claim owned by another template is
    /// left alone), then clear the in-process claim set and drop the arbiter
    /// ownership via [`HostArbiter::release_owner`](mtui_hosts::HostArbiter::release_owner).
    ///
    /// Idempotent and safe when pool selection was never used
    /// ([`arbiter`](TestReportBase::arbiter)/[`owner`](TestReportBase::owner) are
    /// then `None`). Called from the exit path (upstream `quit` and
    /// `TemplateRegistry.release_claims`); the remote lock-wire format is
    /// untouched — release goes through the same `pool_unlock` primitive that
    /// created the claim.
    async fn release_pool_claims(&mut self) {
        let base = self.base_mut();
        // Snapshot claims so the borrow of `pool_claims` is released before the
        // mutable per-target `pool_unlock` calls.
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
    /// Ports upstream `TestReport.release_pool_claim`: the per-host analogue of
    /// [`release_pool_claims`](Self::release_pool_claims), called from
    /// `remove_host` so a disconnected refhost does not stay claimed in the
    /// process-global [`HostArbiter`](mtui_hosts::HostArbiter) for the rest of
    /// the server's lifetime (there is no `unload` over MCP, so the template
    /// stays loaded). [`Target::close`](mtui_hosts::Target::close) already drops
    /// the remote operation/pool-lock files; this clears the in-process
    /// ownership those locks and the `--free` probe never see.
    ///
    /// Only `host` is dropped from each slot's candidate list — siblings stay
    /// available as backup-refhost fallbacks (RFC §5.7); a slot is pruned only
    /// once it has no candidates left. (Contrast
    /// [`release_pool_claims`](Self::release_pool_claims), which clears the whole
    /// map because it tears the entire report down.)
    ///
    /// Idempotent and safe when pool selection was never used
    /// ([`arbiter`](TestReportBase::arbiter)/[`owner`](TestReportBase::owner) are
    /// then `None`).
    fn release_pool_claim(&mut self, host: &str) {
        let base = self.base_mut();
        base.pool_claims.remove(host);
        // Drop only this host from each slot; keep siblings as backups, and
        // prune a slot only once it is empty.
        base.slot_candidates.retain(|_slot, candidates| {
            candidates.retain(|c| c != host);
            !candidates.is_empty()
        });
        if let (Some(arbiter), Some(owner)) = (base.arbiter.as_ref(), base.owner.as_ref()) {
            arbiter.release(host, owner);
        }
    }

    /// Emits the per-host update commands for `targets` (upstream
    /// `list_update_commands`). The null object is a no-op.
    fn list_update_commands(&self, targets: &HostsGroup);

    /// The deduplicated list of every package named in the report metadata
    /// (upstream `get_package_list`).
    ///
    /// Iterates the nested [`packages`](TestReportBase::packages) map
    /// (`product -> { name -> version }`) and flattens the package **names**
    /// across all products, deduplicated. Upstream returns them in `set` order;
    /// the port sorts for determinism (the callers — `perform_update` /
    /// `perform_prepare` — only join the list into a command string, so order is
    /// not behaviourally significant, but a stable order keeps snapshots and
    /// tests reproducible).
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

    /// The plain-text test-report log URL (upstream `_testreport_url`):
    /// `{reports_url}/{id}/log`.
    fn testreport_url(&self) -> String {
        format!("{}/{}/log", self.base().config.reports_url, self.id())
    }

    /// The "fancy" test-report log URL (upstream `fancy_report_url`):
    /// `{fancy_reports_url}/{id}/log`.
    fn fancy_report_url(&self) -> String {
        format!("{}/{}/log", self.base().config.fancy_reports_url, self.id())
    }

    /// The Bugzilla `id -> title` and Jira `id -> title` maps (upstream
    /// `list_bugs`, which forwards `self.bugs`/`self.jira` to the display sink).
    ///
    /// Returned as sorted [`BTreeMap`](std::collections::BTreeMap)s so the
    /// display renders ids in a stable order (upstream sorts at render time).
    /// The `list_bugs` command feeds these to
    /// [`CommandPromptDisplay::list_bugs`](../../mtui_core/display); an empty
    /// map renders the upstream "No bugs…"/"No Jira…" sentinels.
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

    /// The aligned `(label, value)` metadata rows for `list_metadata` (upstream
    /// `_show_yourself_data` + `_aligned_write`).
    ///
    /// Rows with an empty value are dropped, and the whole set is sorted by
    /// label, matching upstream's `sorted(data)` + `if value:` filter. The
    /// caller renders each surviving row as `{label:15}: {value}`.
    fn show_yourself_data(&self) -> Vec<(String, String)> {
        let base = self.base();
        let mut systems: Vec<&String> = base.systems.keys().collect();
        systems.sort();
        let hosts = systems
            .iter()
            .map(|s| s.as_str())
            .collect::<Vec<_>>()
            .join(" ");

        let mut bug_ids: Vec<&String> = base.bugs.keys().collect();
        bug_ids.sort();
        let mut jira_ids: Vec<&String> = base.jira.keys().collect();
        jira_ids.sort();

        let build_checks = {
            let url = self.testreport_url();
            // Upstream `_testreport_url()[:-3] + "build_checks"` strips the
            // trailing "log" and appends "build_checks".
            format!("{}build_checks", &url[..url.len().saturating_sub(3)])
        };

        let mut rows: Vec<(String, String)> = vec![
            ("Category".to_owned(), base.category.clone()),
            ("Hosts".to_owned(), hosts),
            ("Reviewer".to_owned(), base.reviewer.clone()),
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

    /// Installs `packages` on every host in `targets` (upstream
    /// `metadata.perform_install` → `targets.perform_install`).
    ///
    /// Drives the [`InstallOperation`](mtui_hosts::InstallOperation) template
    /// through the group's [`OperationGroup`](mtui_hosts::OperationGroup) impl,
    /// which resolves each host's installer doer/check via the injected
    /// `PlanProvider` (wired at the composition root). The default is a no-op —
    /// the null report has nothing to install — so only reports backed by real
    /// doer tables override it.
    ///
    /// Returns `Err` when the install command failed on one or more hosts
    /// (non-zero exit or non-empty stderr after the fan-out), aggregated the
    /// same way [`perform_update`](Self::perform_update) reports failures. The
    /// null object's default is a no-op `Ok(())`.
    async fn perform_install(
        &self,
        _targets: &mut HostsGroup,
        _packages: &[String],
    ) -> Result<(), crate::update_workflow::UpdateError> {
        Ok(())
    }

    /// Uninstalls `packages` from every host in `targets` (upstream
    /// `metadata.perform_uninstall` → `targets.perform_uninstall`).
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

    /// Prepares `packages` on every host (upstream
    /// `HostsGroup.perform_prepare`).
    ///
    /// The bespoke (non-template) preparer flow: fan the issue repo add/remove
    /// out, install every package in a single transaction (or per-package for
    /// the `installed_only` variant), run the preparer check, and reboot
    /// transactional hosts — all under the operation lock. `testing` selects the
    /// repo-`add` (testing) vs repo-`remove` path and the testing preparer
    /// variant; `force` toggles `--force-resolution`; `installed_only` only
    /// touches already-installed packages. Default no-op (the null report has
    /// nothing to prepare); real reports override.
    ///
    /// Returns `Err` on a missing preparer, lock contention, a failed issue-repo
    /// fan-out, a per-host prepare-command failure, or a prepare check failure —
    /// aggregated like [`perform_update`](Self::perform_update). The null
    /// object's default is a no-op `Ok(())`.
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

    /// Downgrades `packages` on every host (upstream
    /// `HostsGroup.perform_downgrade`).
    ///
    /// Removes the issue repos, resolves each package's available downgrade
    /// version via the downgrader `list_command`, then downgrades — per-package
    /// for non-transactional hosts, combined into a single transaction for
    /// transactional hosts — runs the check, and reboots transactional hosts,
    /// under the operation lock. Default no-op; real reports override.
    ///
    /// Returns `Err` on a missing downgrader, lock contention, or a per-host
    /// downgrade check failure — aggregated like
    /// [`perform_update`](Self::perform_update). The null object's default is a
    /// no-op `Ok(())`.
    async fn perform_downgrade(
        &self,
        _targets: &mut HostsGroup,
        _packages: &[String],
    ) -> Result<(), crate::update_workflow::UpdateError> {
        Ok(())
    }

    /// Updates the hosts with this report's maintenance update (upstream
    /// `HostsGroup.perform_update`).
    ///
    /// The full bespoke update flow: optional prepare, pre/post package checks,
    /// repo add, `updater` command render (with the `$repa` RRID selector), the
    /// per-host update check with failure aggregation, transactional reboot, and
    /// the two-phase repo cleanup (remove on success, **keep** on failure for
    /// retry/diagnosis). `noprepare` skips the initial prepare; `newpackage`
    /// runs a testing prepare after the update. Default no-op; real reports
    /// override.
    ///
    /// Returns `Err` when the update did not apply: a per-host `updater` check
    /// failure (after a best-effort downgrade rollback) or a hard
    /// missing-updater failure. The null object's default is a no-op `Ok(())`.
    ///
    /// Recognised-but-non-fatal diagnostic sections from the update check
    /// (upstream `checks/update.py`'s two `print(...)` blocks) are appended to
    /// `diagnostics` for the command layer to render through the display.
    async fn perform_update(
        &self,
        _targets: &mut HostsGroup,
        _noprepare: bool,
        _newpackage: bool,
        _diagnostics: &mut Vec<crate::update_workflow::Diagnostic>,
    ) -> Result<(), crate::update_workflow::UpdateError> {
        Ok(())
    }

    /// Verifies the loaded template hash (upstream `check_hash`).
    ///
    /// Returns a [`HashCheck`] describing the outcome. The null object and the
    /// non-git reports (OBS/PI) report [`HashCheck::Ok`] since they have nothing
    /// to verify. Async because git-backed reports compare against a hash
    /// fetched from Gitea.
    ///
    /// Unlike upstream — which raises `MissingGiteaTokenError`,
    /// `FailedGiteaCallError`, or `InvalidGiteaHashError` from inside `read` —
    /// this returns the outcome so the async load orchestrator
    /// ([`make_testreport`](crate::make_testreport)) can branch on it (`read`
    /// is sync; `check_hash` is async).
    async fn check_hash(&self) -> HashCheck;

    /// The working directory for target artifacts (upstream `target_wd`).
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

    /// Whether a real report is loaded (upstream `__bool__`). Defaults to
    /// `true`; the null object overrides to `false`.
    fn is_loaded(&self) -> bool {
        true
    }

    /// Exposes this report as a [`SetRepo`] when it can add/remove issue repos.
    ///
    /// The `set_repo` command needs a `&dyn SetRepo` to fan the repo add/remove
    /// out over the group ([`HostsGroup::fanout_set_repo`](mtui_hosts::HostsGroup)),
    /// but `SetRepo` is a distinct object-safe trait a `dyn TestReport` cannot be
    /// downcast to. Reports that implement `SetRepo` (SL/PI/OBS) override this to
    /// return `Some(self)`; the null report (nothing to set) keeps the `None`
    /// default, which the command surfaces as "no update loaded".
    fn as_set_repo(&self) -> Option<&dyn SetRepo> {
        None
    }

    /// The report's parsed [`RequestReviewID`], if loaded (upstream
    /// `metadata.rrid`).
    ///
    /// Reads [`TestReportBase::rrid`]; `None` for the null report.
    fn rrid(&self) -> Option<&RequestReviewID> {
        self.base().rrid.as_ref()
    }

    /// The report's workflow mode (upstream `metadata.workflow`).
    fn workflow(&self) -> Workflow {
        self.base().workflow
    }

    /// The report's openQA state holder (upstream `metadata.openqa`).
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

    /// The Gitea pull-request API URL (upstream `metadata.giteaprapi`), if any.
    fn giteaprapi(&self) -> Option<&str> {
        self.base().giteaprapi.as_deref()
    }

    /// The Gitea checkout hash recorded in the template (upstream
    /// `metadata.giteacohash`), if any.
    fn giteacohash(&self) -> Option<&str> {
        self.base().giteacohash.as_deref()
    }

    /// The openQA incident id used by the QEM Dashboard / oqa-search queries.
    ///
    /// Ports upstream's use of `rrid.maintenance_id` as the incident number.
    /// `None` for the null report (no RRID).
    fn incident_id(&self) -> Option<String> {
        self.base().rrid.as_ref().map(|r| r.maintenance_id.clone())
    }

    /// Records the reviewer in the loaded testreport template on disk (upstream
    /// `set_reviewer`).
    ///
    /// Replaces the `Test Plan Reviewer:` metadata line with `name`, rewrites
    /// the template file atomically, and updates the in-memory
    /// [`reviewer`](TestReportBase::reviewer) only after the write succeeds
    /// (older `Suggested …` phrasings are normalised away). `name` is trimmed.
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
}

/// Matches the `Test Plan Reviewer:` (or legacy `Suggested Test Plan
/// Reviewer:`) metadata line, ported from upstream `_reviewer_line`.
///
/// Compiled on demand; only [`TestReport::set_reviewer`] uses it.
fn reviewer_line_re() -> regex::Regex {
    regex::Regex::new(r"(?m)^(?:Suggested )?Test Plan Reviewer:.*$")
        .expect("static reviewer-line regex is valid")
}

/// The outcome of [`TestReport::check_hash`] (upstream's `check_hash` plus the
/// exception family `_checkout` catches around it).
///
/// Upstream signals these four states by raising exceptions from inside `read`
/// (`MissingGiteaTokenError`, `FailedGiteaCallError`, `InvalidGiteaHashError`)
/// or returning a `(True, …)` tuple; the Rust load path branches on this enum
/// instead (see [`make_testreport`](crate::make_testreport)).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HashCheck {
    /// The template hash matches the Gitea PR head, or there is nothing to
    /// verify (null / OBS / PI reports, or the legacy `1.1` maintenance id).
    Ok,
    /// The template hash differs from the Gitea PR head — the template is stale
    /// (upstream `InvalidGiteaHashError`).
    Mismatch {
        /// The hash recorded in the checked-out template (`giteacohash`).
        expected: String,
        /// The hash currently at the PR head, fetched from Gitea.
        actual: String,
    },
    /// No Gitea API token is configured (upstream `MissingGiteaTokenError`).
    MissingToken,
    /// The Gitea API call failed (upstream `FailedGiteaCallError`); carries the
    /// underlying error text for logging.
    Failed(String),
}

/// Failure reading/parsing a checkout's template (upstream `TemplateIOError` /
/// `MetadataNotLoadedError` raised from `_open_and_parse`).
#[derive(Debug, thiserror::Error)]
pub enum ReadError {
    /// The template `log` file could not be read.
    ///
    /// Carries the [`TemplateIoError`] so the checkout seam can branch on
    /// [`is_not_found`](TemplateIoError::is_not_found) (upstream `e.errno !=
    /// ENOENT`) to decide whether a missing template triggers a fresh checkout.
    #[error(transparent)]
    Template(#[from] TemplateIoError),
    /// The sibling `metadata.json` is absent (upstream `MetadataNotLoadedError`).
    #[error("metadata.json is missing from the checkout")]
    MetadataMissing,
    /// The `metadata.json` is not valid JSON (upstream `JSONDecodeError` →
    /// `MetadataNotLoadedError`).
    #[error("metadata.json is not valid JSON")]
    MetadataInvalid,
}

/// Failure recording a reviewer into the loaded template (upstream raises
/// `ValueError` / `RuntimeError` / `TemplateFormatError`).
#[derive(Debug, thiserror::Error)]
pub enum ReviewerError {
    /// The reviewer name was empty or whitespace-only.
    #[error("reviewer must be a non-empty string")]
    Empty,
    /// No template is loaded (upstream "Called while missing path").
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
    fn base_defaults_match_upstream_init() {
        let cfg = config();
        let template_dir = cfg.template_dir.clone();
        let base = TestReportBase::new(cfg);

        assert_eq!(base.workflow, Workflow::Manual);
        assert_eq!(base.directory, template_dir);
        assert!(base.path.is_none());
        assert!(base.systems.is_empty());
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

    /// Builds a `TestReportBase` whose `packages` map has one product `key`
    /// carrying `entries` (`name -> version`).
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
        // The user's exact case: metadata keyed by "15-SP6" (== parse_product
        // version string), five hplip packages, host base version "15-SP6".
        let base = base_with_packages(&[
            ("15-SP6", "hplip", "3.26.4-150600.4.12.1"),
            ("15-SP6", "hplip-devel", "3.26.4-150600.4.12.1"),
            (
                "15-SP5",
                "release-notes-sles",
                "15.5.20260709-150500.3.35.1",
            ),
        ]);
        let pkgs = base.packages_for("15-SP6");
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
        // SLFO metadata ships a single "standard" product set; it applies to any
        // host base version.
        let base = base_with_packages(&[("standard", "patch", "2.7.6-999999_stage.1.1")]);
        let pkgs = base.packages_for("16.0");
        assert_eq!(pkgs.len(), 1);
        assert_eq!(pkgs[0].name, "patch");
        assert_eq!(
            pkgs[0].required().map(ToString::to_string),
            Some("2.7.6-999999_stage.1.1".to_owned())
        );
    }

    #[test]
    fn packages_for_merges_sle12_special_case() {
        // Upstream merges the "12" sub-map for any 12.x host on top of the
        // base-version sub-map.
        let base = base_with_packages(&[("12-SP5", "bash", "5.0-1"), ("12", "glibc", "2.31-1")]);
        let pkgs = base.packages_for("12-SP5");
        let names: Vec<&str> = pkgs.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, vec!["bash", "glibc"]);
    }

    #[test]
    fn packages_for_returns_empty_when_no_submap_matches() {
        let base = base_with_packages(&[("15-SP6", "hplip", "3.26.4-1")]);
        assert!(base.packages_for("15-SP5").is_empty());
    }

    #[test]
    fn packages_for_skips_unparseable_version() {
        // A garbage version leaves the package unseeded rather than aborting.
        let base = base_with_packages(&[("15-SP6", "goodpkg", "1.0-1"), ("15-SP6", "badpkg", "")]);
        let pkgs = base.packages_for("15-SP6");
        // Empty string clears required (parse_opt treats "" as None), so badpkg
        // is present but with no required version; goodpkg has one.
        let good = pkgs.iter().find(|p| p.name == "goodpkg").unwrap();
        assert!(good.required().is_some());
    }

    /// A minimal report over a [`TestReportBase`] with a fixed id, so the
    /// trait-default metadata helpers (`bug_maps`, `show_yourself_data`,
    /// `testreport_url`, `fancy_report_url`) can be exercised directly.
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
        base.systems.insert("h1".to_owned(), "SLES-15.5".to_owned());
        base.testplatforms.push("base=sles".to_owned());
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
        // Every emitted row has a non-empty value.
        assert!(rows.iter().all(|(_, v)| !v.is_empty()));
        // Sorted by label.
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
        assert!(!has("Packager"));
        // Build checks strips the trailing "log".
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
        base.workflow = Workflow::Kernel;
        let r = MetaReport { base };
        assert_eq!(r.rrid().unwrap().maintenance_id, "12345");
        assert_eq!(r.incident_id().as_deref(), Some("12345"));
        assert_eq!(r.giteaprapi(), Some("https://gitea/api/pr/1"));
        assert_eq!(r.giteacohash(), Some("deadbeef"));
        assert_eq!(r.workflow(), Workflow::Kernel);
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
        // Leak a test-local arbiter to obtain the `&'static` the field expects
        // without touching the shared process-global singleton.
        let arbiter: &'static HostArbiter = Box::leak(Box::new(HostArbiter::new()));
        // Claim two hosts through the arbiter for this owner.
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

        // In-process claim set and slot candidates are cleared.
        assert!(r.base().pool_claims.is_empty());
        assert!(r.base().slot_candidates.is_empty());
        // Arbiter ownership is dropped for every previously-held host.
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
        // One slot holds both as candidates (h1 primary, h2 backup sibling).
        base.slot_candidates
            .insert("slot0".to_owned(), vec!["h1".to_owned(), "h2".to_owned()]);
        let mut r = MetaReport { base };

        r.release_pool_claim("h1");

        // h1's in-process claim is dropped; h2 stays claimed.
        assert!(!r.base().pool_claims.contains("h1"));
        assert!(r.base().pool_claims.contains("h2"));
        // The freed host is re-acquirable by another owner.
        let arbiter = r.base().arbiter.as_ref().unwrap();
        let other: Owner = ("reg-2".to_owned(), "SUSE:Maintenance:2:2".to_owned());
        assert!(arbiter.try_acquire("h1", &other));
        // h2 is still owned by us (its sibling stays as backup).
        assert_eq!(arbiter.owner_of("h2"), Some(owner.clone()));
        // The slot survives (h2 still a candidate), with h1 pruned out.
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

        // The slot had no siblings left, so it is pruned entirely.
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
