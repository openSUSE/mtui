//! The OBS [`TestReport`] implementation ([`ObsReport`]).
//!
//! Keys its identity on the parsed [`RequestReviewID`] and derives its
//! update-repo map by parsing the OBS/IBS checkout's `project.xml` via
//! [`obsrepoparse`], reading the checkout under
//! [`report_wd`](TestReportBase::report_wd). OBS is checked out with
//! `osc qam` / SVN (not Gitea), so there is no git commit to verify —
//! [`check_hash`](TestReport::check_hash) is the constant `(true, "", "")`.
//!
//! ## Scope (task nbv.11)
//!
//! Mirrors the `SlReport`/`PiReport` boundaries:
//! * `set_repo` (the [`SetRepo`] impl driving [`RepoManager::run_zypper`](mtui_hosts::RepoManager::run_zypper)) is
//!   implemented here (task nbv.fly): add uses the OBS-specific
//!   `-n ar -ckn` (note: no `fG`, unlike SL/PI), remove uses `-n rr`.
//! * `list_update_commands` would render per-host commands via
//!   `target.doer('updater')`, but the `OperationGroup`/doer seam on `Target`
//!   is deferred (see the `TODO(Phase 4)` in `mtui-hosts::target::operation`).
//!   Until it is wired this is a documented no-op stub.
//! * `_show_yourself_data` is not on the trait skeleton yet (same deferral as
//!   `SlReport`/`PiReport`).
//! * `id()` returns `""` when no RRID is loaded; this matches the graceful
//!   path chosen for the sibling reports.

use std::collections::HashMap;

use mtui_config::options::Config;
use mtui_hosts::{HostsGroup, RepoOp, SetRepo, Target};
use mtui_types::{RequestReviewID, SystemProduct};
use tracing::debug;

use super::repoparse::obsrepoparse;
use super::set_repo_with_add_flags;
use super::update_flow;
use crate::testreport::{HashCheck, TestReport, TestReportBase};

/// A [`TestReport`] for OBS/IBS updates.
pub struct ObsReport {
    base: TestReportBase,
}

impl ObsReport {
    /// Builds an [`ObsReport`] from `config`.
    ///
    /// [`TestReportBase::new`] already seeds the rating/realid envelope
    /// fields to empty, so this simply wraps a fresh base.
    #[must_use]
    pub fn new(config: Config) -> Self {
        Self {
            base: TestReportBase::new(config),
        }
    }
}

#[async_trait::async_trait]
impl TestReport for ObsReport {
    fn base(&self) -> &TestReportBase {
        &self.base
    }

    fn base_mut(&mut self) -> &mut TestReportBase {
        &mut self.base
    }

    fn id(&self) -> String {
        // Empty when nothing is loaded.
        self.base
            .rrid
            .as_ref()
            .map(RequestReviewID::to_string)
            .unwrap_or_default()
    }

    fn parser(&self) -> HashMap<String, String> {
        // The skeleton trait models the table's *keys* as strings; the concrete
        // parser dispatch lives in the loader (a later task). Values are the
        // parser names so callers can branch on them.
        HashMap::from([
            ("hosts".to_string(), "ReducedMetadataParser".to_string()),
            ("json".to_string(), "JSONParser".to_string()),
        ])
    }

    fn update_repos_parser(&self) -> HashMap<SystemProduct, String> {
        // Degrades to an empty map when no report is loaded (or the checkout
        // dir can't be resolved), matching the graceful style of the sibling
        // reports rather than panicking.
        match self.base.report_wd() {
            Ok(dir) => obsrepoparse(&self.base.repository, &dir),
            Err(e) => {
                debug!(error = %e, "update_repos_parser: no report working dir");
                HashMap::new()
            }
        }
    }

    fn list_update_commands(&self, _targets: &HostsGroup) {
        // This would render per-host `updater` commands for display; the
        // bespoke `perform_update` flow that runs them is implemented below.
        // A standalone read-only listing has no consumer yet (the `list`/`run`
        // Wave-1 command lands in mtui-rs-2d3.6), so this stays a no-op until
        // then.
        debug!("list_update_commands: no listing consumer yet (see mtui-rs-2d3.6)");
    }

    // Shared `perform_*` flows (SL/PI/OBS behave identically). See
    // `SlReport` for the rationale behind the per-report delegation.
    async fn perform_install(
        &self,
        targets: &mut HostsGroup,
        packages: &[String],
    ) -> Result<(), crate::update_workflow::UpdateError> {
        update_flow::perform_install(targets, packages).await
    }

    async fn perform_uninstall(
        &self,
        targets: &mut HostsGroup,
        packages: &[String],
    ) -> Result<(), crate::update_workflow::UpdateError> {
        update_flow::perform_uninstall(targets, packages).await
    }

    async fn perform_prepare(
        &self,
        targets: &mut HostsGroup,
        packages: &[String],
        force: bool,
        testing: bool,
        installed_only: bool,
    ) -> Result<(), crate::update_workflow::UpdateError> {
        update_flow::perform_prepare(targets, self, packages, force, testing, installed_only).await
    }

    async fn perform_downgrade(
        &self,
        targets: &mut HostsGroup,
        packages: &[String],
    ) -> Result<(), crate::update_workflow::UpdateError> {
        let id = self.rrid().map(ToString::to_string);
        update_flow::perform_downgrade(targets, self, packages, id.as_deref()).await
    }

    async fn perform_update(
        &self,
        targets: &mut HostsGroup,
        noprepare: bool,
        newpackage: bool,
        diagnostics: &mut Vec<crate::update_workflow::Diagnostic>,
    ) -> Result<(), crate::update_workflow::UpdateError> {
        update_flow::perform_update_with_rollback(self, targets, noprepare, newpackage, diagnostics)
            .await
    }

    fn as_set_repo(&self) -> Option<&dyn mtui_hosts::SetRepo> {
        Some(self)
    }

    async fn check_hash(&self) -> HashCheck {
        // Upstream OBS always returns (True, "", "") — OBS/IBS checkout is via
        // osc qam / SVN, so there is no git commit hash to verify.
        HashCheck::Ok
    }
}

#[async_trait::async_trait]
impl SetRepo for ObsReport {
    /// Adds a repo with OBS's `-n ar -ckn` (no `fG`,
    /// unlike SL/PI), removes with `-n rr`, fanned out over
    /// [`TestReportBase::update_repos`].
    async fn set_repo(&self, target: &mut Target, operation: RepoOp) {
        set_repo_with_add_flags(&self.base, target, operation, "-n ar -ckn").await;
    }
}
