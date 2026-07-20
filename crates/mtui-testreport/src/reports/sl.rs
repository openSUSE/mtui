//! The SUSE Linux [`TestReport`] implementation ([`SlReport`]).
//!
//! Port of upstream `mtui.test_reports.sl_report.SLTestReport`. It keys its
//! identity on the parsed [`RequestReviewID`], derives its update-repo map by
//! dispatching among the [`repoparse`](super::repoparse) helpers, and verifies
//! its git commit hash against Gitea (bypassed for the legacy `1.1` maintenance
//! id, which is still served from IBS).
//!
//! ## Scope
//!
//! * `set_repo` (the [`SetRepo`] impl driving [`RepoManager::run_zypper`]) is
//!   implemented here (task nbv.fly): add uses upstream's `-n ar -cfGkn`, remove
//!   uses `-n rr`, both fanned out over [`TestReportBase::update_repos`].
//! * `list_update_commands` renders per-host commands via `target.doer('updater')`
//!   upstream, but the `OperationGroup`/doer seam on `Target` is deferred (see the
//!   `TODO(Phase 4)` in `mtui-hosts::target::operation`). Until it is wired this
//!   is a documented no-op stub.

use std::collections::HashMap;

use mtui_config::options::Config;
use mtui_datasources::error::GiteaError;
use mtui_datasources::gitea::Gitea;
use mtui_hosts::{
    HostsGroup, InstallOperation, Operation, RepoOp, SetRepo, Target, UninstallOperation,
};
use mtui_types::{RequestReviewID, SystemProduct};
use tracing::debug;

use super::repoparse::{gitrepoparse, reporepoparse, slrepoparse};
use super::set_repo_with_add_flags;
use super::update_flow;
use crate::testreport::{HashCheck, TestReport, TestReportBase};

/// A [`TestReport`] for SUSE Linux updates (upstream `SLTestReport`).
pub struct SlReport {
    base: TestReportBase,
}

impl SlReport {
    /// Builds an [`SlReport`] from `config`.
    ///
    /// Upstream's `__init__` seeds the git/rating envelope fields to empty and
    /// `repositories` to an empty set; [`TestReportBase::new`] already applies
    /// those defaults, so this simply wraps a fresh base.
    #[must_use]
    pub fn new(config: Config) -> Self {
        Self {
            base: TestReportBase::new(config),
        }
    }

    /// The maintenance id of the loaded RRID, or `None` when no RRID is set.
    fn maintenance_id(&self) -> Option<&str> {
        self.base.rrid.as_ref().map(|r| r.maintenance_id.as_str())
    }
}

#[async_trait::async_trait]
impl TestReport for SlReport {
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
        // Upstream dispatch order:
        //   repositories set        -> reporepoparse(repositories, products)
        //   maintenance_id == "1.1" -> slrepoparse(repository, products)
        //   otherwise               -> gitrepoparse(repository, products)
        if !self.base.repositories.is_empty() {
            let repos: Vec<String> = self.base.repositories.iter().cloned().collect();
            return reporepoparse(&repos, &self.base.products);
        }
        if self.maintenance_id() == Some("1.1") {
            return slrepoparse(&self.base.repository, &self.base.products);
        }
        gitrepoparse(&self.base.repository, &self.base.products)
    }

    fn list_update_commands(&self, _targets: &HostsGroup) {
        // Upstream renders per-host `updater` commands for display via
        // `target.doer('updater')['command'].safe_substitute(...)`. The bespoke
        // `perform_update` flow that actually runs them is implemented below; a
        // standalone read-only *listing* has no consumer yet (the `list`/`run`
        // Wave-1 command lands in mtui-rs-2d3.6), so this stays a no-op until
        // that command needs it.
        debug!("list_update_commands: no listing consumer yet (see mtui-rs-2d3.6)");
    }

    // Upstream defines these five `perform_*` flows once on the base
    // `TestReport`; every report (SL/PI/OBS) inherits identical behaviour and
    // differs only in `set_repo` / `list_update_commands`. Rust's object-safe
    // `dyn TestReport` cannot express a `where Self: SetRepo` default, so each
    // `SetRepo` report delegates to the shared `update_flow` free functions
    // below (thin, identical across the three reports).
    async fn perform_install(
        &self,
        targets: &mut HostsGroup,
        packages: &[String],
    ) -> Result<(), crate::update_workflow::UpdateError> {
        update_flow::add_op_history(targets, "install", None, packages).await;
        InstallOperation::new(packages.to_vec()).run(targets).await;
        update_flow::install_verdict("install", targets)
    }

    async fn perform_uninstall(
        &self,
        targets: &mut HostsGroup,
        packages: &[String],
    ) -> Result<(), crate::update_workflow::UpdateError> {
        update_flow::add_op_history(targets, "uninstall", None, packages).await;
        UninstallOperation::new(packages.to_vec())
            .run(targets)
            .await;
        update_flow::install_verdict("uninstall", targets)
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
        update_flow::add_op_history(targets, "downgrade", id.as_deref(), packages).await;
        update_flow::perform_downgrade(targets, self, packages).await
    }

    async fn perform_update(
        &self,
        targets: &mut HostsGroup,
        noprepare: bool,
        newpackage: bool,
        diagnostics: &mut Vec<crate::update_workflow::Diagnostic>,
    ) -> Result<(), crate::update_workflow::UpdateError> {
        let id = self.rrid().map(ToString::to_string);
        let packages = self.get_package_list();
        update_flow::add_op_history(targets, "update", id.as_deref(), &packages).await;
        update_flow::perform_update_with_rollback(self, targets, noprepare, newpackage, diagnostics)
            .await
    }

    fn as_set_repo(&self) -> Option<&dyn mtui_hosts::SetRepo> {
        Some(self)
    }

    async fn check_hash(&self) -> HashCheck {
        // "1.1" is still served from IBS — no Gitea comparison.
        if self.maintenance_id() == Some("1.1") {
            return HashCheck::Ok;
        }

        let old = self.base.giteacohash.clone().unwrap_or_default();
        let giteaprapi = self.base.giteaprapi.clone().unwrap_or_default();
        let gitea = match Gitea::new(&self.base.config, &giteaprapi, None) {
            Ok(g) => g,
            // A missing token is a distinct, actionable failure upstream
            // surfaces as `MissingGiteaTokenError`; anything else building the
            // client is a failed call.
            Err(GiteaError::MissingToken) => return HashCheck::MissingToken,
            Err(e) => {
                debug!(error = %e, "check_hash: could not build Gitea client");
                return HashCheck::Failed(e.to_string());
            }
        };
        match gitea.get_hash().await {
            Ok(new) if old == new => HashCheck::Ok,
            Ok(new) => HashCheck::Mismatch {
                expected: old,
                actual: new,
            },
            Err(e) => {
                debug!(error = %e, "check_hash: Gitea get_hash failed");
                HashCheck::Failed(e.to_string())
            }
        }
    }
}

#[async_trait::async_trait]
impl SetRepo for SlReport {
    /// Ports `SLTestReport.set_repo`: add uses `-n ar -cfGkn`, remove uses
    /// `-n rr`, fanned out over [`TestReportBase::update_repos`].
    async fn set_repo(&self, target: &mut Target, operation: RepoOp) {
        set_repo_with_add_flags(&self.base, target, operation, "-n ar -cfGkn").await;
    }
}
