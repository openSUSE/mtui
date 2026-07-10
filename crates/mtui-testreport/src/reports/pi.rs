//! The PI [`TestReport`] implementation ([`PiReport`]).
//!
//! Port of upstream `mtui.test_reports.pi_report.PITestReport`. It keys its
//! identity on the parsed [`RequestReviewID`] and derives its update-repo map by
//! delegating unconditionally to [`reporepoparse`](super::repoparse::reporepoparse)
//! — the simplest of the concrete reports, reusing the helper already ported for
//! [`SlReport`](super::sl::SlReport). PI has no git commit to verify, so
//! [`check_hash`](TestReport::check_hash) is a constant `(true, "", "")`
//! (upstream always returns `True, "", ""`).
//!
//! ## Scope (task nbv.12)
//!
//! Mirrors the `SlReport` boundaries:
//! * `set_repo` (the [`SetRepo`] impl driving [`RepoManager::run_zypper`]) is
//!   implemented here (task nbv.fly): add uses upstream's `-n ar -cfGkn` (same as
//!   SL), remove uses `-n rr`.
//! * `list_update_commands` renders per-host commands via `target.doer('updater')`
//!   upstream, but the `OperationGroup`/doer seam on `Target` is deferred (see the
//!   `TODO(Phase 4)` in `mtui-hosts::target::operation`). Until it is wired this
//!   is a documented no-op stub.
//! * `id()` returns `""` when no RRID is loaded (upstream `str(self.rrid)` would
//!   raise); this matches the graceful path chosen for `SlReport`.

use std::collections::HashMap;

use mtui_config::options::Config;
use mtui_hosts::{
    HostsGroup, InstallOperation, Operation, RepoOp, SetRepo, Target, UninstallOperation,
};
use mtui_types::{RequestReviewID, SystemProduct};
use tracing::debug;

use super::repoparse::reporepoparse;
use super::set_repo_with_add_flags;
use super::update_flow;
use crate::testreport::{HashCheck, TestReport, TestReportBase};

/// A [`TestReport`] for PI updates (upstream `PITestReport`).
pub struct PiReport {
    base: TestReportBase,
}

impl PiReport {
    /// Builds a [`PiReport`] from `config`.
    ///
    /// Upstream's `__init__` seeds the rating/realid envelope fields to empty and
    /// `repositories` to an empty set; [`TestReportBase::new`] already applies
    /// those defaults, so this simply wraps a fresh base.
    #[must_use]
    pub fn new(config: Config) -> Self {
        Self {
            base: TestReportBase::new(config),
        }
    }
}

#[async_trait::async_trait]
impl TestReport for PiReport {
    fn base(&self) -> &TestReportBase {
        &self.base
    }

    fn base_mut(&mut self) -> &mut TestReportBase {
        &mut self.base
    }

    fn id(&self) -> String {
        // Upstream `str(self.rrid)`. Empty when nothing is loaded.
        self.base
            .rrid
            .as_ref()
            .map(RequestReviewID::to_string)
            .unwrap_or_default()
    }

    fn parser(&self) -> HashMap<String, String> {
        // Upstream registers `{"hosts": ReducedMetadataParser, "json": JSONParser}`.
        // The skeleton trait models the table's *keys* as strings; the concrete
        // parser dispatch lives in the loader (a later task). Values are the
        // upstream parser names so callers can branch on them.
        HashMap::from([
            ("hosts".to_string(), "ReducedMetadataParser".to_string()),
            ("json".to_string(), "JSONParser".to_string()),
        ])
    }

    fn update_repos_parser(&self) -> HashMap<SystemProduct, String> {
        // Upstream `reporepoparse(self.repositories, self.products)` — no
        // maintenance-id branching (unlike SL).
        let repos: Vec<String> = self.base.repositories.iter().cloned().collect();
        reporepoparse(&repos, &self.base.products)
    }

    fn list_update_commands(&self, _targets: &HostsGroup) {
        // Upstream renders per-host `updater` commands for display; the bespoke
        // `perform_update` flow that runs them is implemented below. A standalone
        // read-only listing has no consumer yet (the `list`/`run` Wave-1 command
        // lands in mtui-rs-2d3.6), so this stays a no-op until then.
        debug!("list_update_commands: no listing consumer yet (see mtui-rs-2d3.6)");
    }

    // Shared `perform_*` flows (upstream defines these once on the base
    // `TestReport`; SL/PI/OBS behave identically). See `SlReport` for the
    // rationale behind the per-report delegation.
    async fn perform_install(&self, targets: &mut HostsGroup, packages: &[String]) {
        update_flow::add_op_history(targets, "install", None, packages).await;
        InstallOperation::new(packages.to_vec()).run(targets).await;
    }

    async fn perform_uninstall(&self, targets: &mut HostsGroup, packages: &[String]) {
        update_flow::add_op_history(targets, "uninstall", None, packages).await;
        UninstallOperation::new(packages.to_vec())
            .run(targets)
            .await;
    }

    async fn perform_prepare(
        &self,
        targets: &mut HostsGroup,
        packages: &[String],
        force: bool,
        testing: bool,
        installed_only: bool,
    ) {
        update_flow::perform_prepare(targets, self, packages, force, testing, installed_only).await;
    }

    async fn perform_downgrade(&self, targets: &mut HostsGroup, packages: &[String]) {
        let id = self.rrid().map(ToString::to_string);
        update_flow::add_op_history(targets, "downgrade", id.as_deref(), packages).await;
        update_flow::perform_downgrade(targets, self, packages).await;
    }

    async fn perform_update(
        &self,
        targets: &mut HostsGroup,
        noprepare: bool,
        newpackage: bool,
    ) -> Result<(), crate::update_workflow::UpdateError> {
        let id = self.rrid().map(ToString::to_string);
        let packages = self.get_package_list();
        update_flow::add_op_history(targets, "update", id.as_deref(), &packages).await;
        update_flow::perform_update_with_rollback(self, targets, noprepare, newpackage).await
    }

    fn as_set_repo(&self) -> Option<&dyn mtui_hosts::SetRepo> {
        Some(self)
    }

    async fn check_hash(&self) -> HashCheck {
        // Upstream PI always returns (True, "", "") — nothing to verify.
        HashCheck::Ok
    }
}

#[async_trait::async_trait]
impl SetRepo for PiReport {
    /// Ports `PITestReport.set_repo`: add uses `-n ar -cfGkn` (same as SL),
    /// remove uses `-n rr`, fanned out over [`TestReportBase::update_repos`].
    async fn set_repo(&self, target: &mut Target, operation: RepoOp) {
        set_repo_with_add_flags(&self.base, target, operation, "-n ar -cfGkn").await;
    }
}
