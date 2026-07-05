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
use std::path::PathBuf;

use mtui_config::options::Config;
use mtui_hosts::{HostArbiter, HostsGroup, Owner};
use mtui_types::{Product, Workflow};

/// Shared state common to every [`TestReport`] implementation.
///
/// Ported field-for-field from upstream `TestReport.__init__` so that concrete
/// reports and the workflow engine observe the same defaults. Fields whose
/// behavior is only exercised by later Phase 4 tasks (pool selection, host
/// arbitration, connect) are carried here as pure state — no logic is wired to
/// them yet.
///
/// The upstream `openqa: OpenQAResults` field is intentionally **not** included
/// yet: [`mtui_types::OpenQAResults`] is generic over concrete `OpenQAResult`
/// implementations that do not exist until the exporter tasks (P4.7). Adding it
/// now would force inventing result types ahead of their scope; it lands with
/// those tasks.
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
    /// `Product -> repository` map for the update repositories.
    pub update_repos: HashMap<Product, String>,
    /// Known hostnames for this report.
    pub hostnames: HashSet<String>,
    /// When non-empty, newly connected hosts are locked with this comment
    /// (set while a PI assignment is active).
    pub lock_comment: String,
    /// Process-global host arbiter (RFC §5.7). `None` for directly-constructed
    /// reports, which fall back to the legacy remote-lock-only connect path.
    pub arbiter: Option<HostArbiter>,
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
    /// Package `name -> version` map.
    pub packages: HashMap<String, String>,
    /// `hostname -> product-drift warning lines` from the last connect.
    pub product_warnings: HashMap<String, Vec<String>>,
}

impl TestReportBase {
    /// Builds the shared state with upstream `TestReport.__init__` defaults.
    ///
    /// `directory` mirrors upstream by defaulting to `config.template_dir`.
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
            bugs: HashMap::new(),
            jira: HashMap::new(),
            testplatforms: Vec::new(),
            products: Vec::new(),
            category: String::new(),
            packager: String::new(),
            reviewer: String::new(),
            repository: String::new(),
            packages: HashMap::new(),
            product_warnings: HashMap::new(),
        }
    }
}

/// The abstract test-report surface.
///
/// Mirrors the `@abstractmethod`s of upstream `TestReport`. Concrete reports
/// embed a [`TestReportBase`] and expose it through [`base`](Self::base) /
/// [`base_mut`](Self::base_mut); the remaining required methods are the abstract
/// surface every report must supply. Non-abstract lifecycle methods
/// (`connect_target`, `export`, pool selection, …) are added by the later
/// Phase 4 tasks.
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
    fn update_repos_parser(&self) -> HashMap<Product, String>;

    /// Emits the per-host update commands for `targets` (upstream
    /// `list_update_commands`). The null object is a no-op.
    fn list_update_commands(&self, targets: &HostsGroup);

    /// Verifies the loaded template hash (upstream `check_hash`).
    ///
    /// Returns `(ok, expected, actual)`. The null object reports `(true, "",
    /// "")` since it has nothing to verify.
    fn check_hash(&self) -> (bool, String, String);

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
        assert!(base.packages.is_empty());
        assert!(base.product_warnings.is_empty());
    }
}
