//! The SUSE Linux [`TestReport`] implementation ([`SlReport`]).
//!
//! Port of upstream `mtui.test_reports.sl_report.SLTestReport`. It keys its
//! identity on the parsed [`RequestReviewID`], derives its update-repo map by
//! dispatching among the [`repoparse`](super::repoparse) helpers, and verifies
//! its git commit hash against Gitea (bypassed for the legacy `1.1` maintenance
//! id, which is still served from IBS).
//!
//! ## Scope (task nbv.4)
//!
//! * `set_repo` (the `SetRepo` impl driving `RepoManager::run_zypper`) is **not**
//!   here: it lands in its dedicated dependent task (nbv.fly) together with the
//!   Target lock-wiring.
//! * `list_update_commands` renders per-host commands via `target.doer('updater')`
//!   upstream, but the `OperationGroup`/doer seam on `Target` is deferred (see the
//!   `TODO(Phase 4)` in `mtui-hosts::target::operation`). Until it is wired this
//!   is a documented no-op stub.

use std::collections::HashMap;

use mtui_config::options::Config;
use mtui_datasources::gitea::Gitea;
use mtui_hosts::{HostsGroup, InstallOperation, Operation, UninstallOperation};
use mtui_types::{RequestReviewID, SystemProduct};
use tracing::debug;

use super::repoparse::{gitrepoparse, reporepoparse, slrepoparse};
use crate::testreport::{TestReport, TestReportBase};

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
        // Deferred to the `updater` workflow (mtui-rs-9lf): upstream renders
        // per-host commands via `target.doer('updater')['command']
        // .safe_substitute(repa=..., packages=...)`. That role, its `$repa`
        // substitution, and `perform_update` are the bespoke non-template
        // update flow tracked in mtui-rs-9lf. The install/uninstall
        // `OperationGroup` seam (this task) does not cover it.
        debug!("list_update_commands: updater-role rendering deferred to mtui-rs-9lf");
    }

    async fn perform_install(&self, targets: &mut HostsGroup, packages: &[String]) {
        // Upstream `metadata.perform_install(targets, packages)` →
        // `targets.perform_install(packages)` → `InstallOperation(...).run()`.
        // The group resolves each host's installer doer/check through the
        // injected `PlanProvider`; a missing doer / unwired provider surfaces
        // inside the template as a logged early-return (no lock taken).
        InstallOperation::new(packages.to_vec()).run(targets).await;
    }

    async fn perform_uninstall(&self, targets: &mut HostsGroup, packages: &[String]) {
        // Upstream `metadata.perform_uninstall` → `targets.perform_uninstall`.
        UninstallOperation::new(packages.to_vec())
            .run(targets)
            .await;
    }

    async fn check_hash(&self) -> (bool, String, String) {
        // "1.1" is still served from IBS — no Gitea comparison.
        if self.maintenance_id() == Some("1.1") {
            return (true, String::new(), String::new());
        }

        let old = self.base.giteacohash.clone().unwrap_or_default();
        let giteaprapi = self.base.giteaprapi.clone().unwrap_or_default();
        let gitea = match Gitea::new(&self.base.config, &giteaprapi, None) {
            Ok(g) => g,
            Err(e) => {
                debug!(error = %e, "check_hash: could not build Gitea client");
                return (false, old, String::new());
            }
        };
        match gitea.get_hash().await {
            Ok(new) => (old == new, old, new),
            Err(e) => {
                debug!(error = %e, "check_hash: Gitea get_hash failed");
                (false, old, String::new())
            }
        }
    }
}
