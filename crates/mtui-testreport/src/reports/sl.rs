//! The SUSE Linux [`TestReport`] implementation ([`SlReport`]).
//!
//! Keys its identity on the parsed [`RequestReviewID`], derives its update-repo
//! map by dispatching among the [`repoparse`](super::repoparse) helpers, and
//! verifies its git commit hash against Gitea. The verification is bypassed for
//! an OBS-served update (no Gitea PR to compare against), keyed on the report's
//! own [`UpdateSource`] and **not** the RRID's maintenance id — issue #433: the
//! SL-Micro 6.0/6.1 cutover shares the `SLFO:1.1` id space between both
//! workflows.
//!
//! `list_update_commands` is a documented no-op stub: the doer seam
//! ([`PlanProvider::doer`](mtui_hosts::PlanProvider::doer)) is wired for
//! install/uninstall but has no listing consumer.

use std::collections::{BTreeSet, HashMap};

use mtui_config::options::Config;
use mtui_datasources::error::GiteaError;
use mtui_datasources::gitea::Gitea;
use mtui_hosts::{HostsGroup, RepoOp, SetRepo, Target};
use mtui_types::{RequestReviewID, SystemProduct, UpdateSource};
use tracing::debug;

use super::repoparse::{gitrepoparse, reporepoparse, slrepoparse};
use super::set_repo_with_add_flags;
use super::update_flow;
use crate::testreport::{HashCheck, TestReport, TestReportBase};

/// A [`TestReport`] for SUSE Linux updates.
pub struct SlReport {
    base: TestReportBase,
}

impl SlReport {
    /// Builds an [`SlReport`] from `config`.
    #[must_use]
    pub fn new(config: Config) -> Self {
        Self {
            base: TestReportBase::new(config),
        }
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
        // Empty when nothing is loaded.
        self.base
            .rrid
            .as_ref()
            .map(RequestReviewID::to_string)
            .unwrap_or_default()
    }

    fn parser(&self) -> HashMap<String, String> {
        // The trait models the table's *keys* as strings; the values are the
        // parser names, so callers can branch on them.
        HashMap::from([
            ("hosts".to_string(), "ReducedMetadataParser".to_string()),
            ("json".to_string(), "JSONParser".to_string()),
        ])
    }

    fn update_repos_parser(&self) -> HashMap<SystemProduct, String> {
        // The `repositories` short-circuit must stay first: it is populated
        // upstream of TeReGen and can start appearing for a git-served update
        // at any time, with no release-gated notice to mtui (#433). This
        // precedence is load-bearing, not a nicety.
        if !self.base.repositories.is_empty() {
            let repos: Vec<String> = self.base.repositories.iter().cloned().collect();
            return reporepoparse(&repos, &self.base.products);
        }
        if self.base.update_source == UpdateSource::Obs {
            return slrepoparse(&self.base.repository, &self.base.products);
        }
        gitrepoparse(&self.base.repository, &self.base.products)
    }

    fn list_update_commands(&self, _targets: &HostsGroup) {
        // `perform_update` below runs the per-host `updater` commands; the
        // read-only listing command only ever prints a placeholder.
        debug!("list_update_commands: no-op stub, not yet implemented");
    }

    // SL/PI/OBS behave identically here, but an object-safe `dyn TestReport`
    // cannot express a `where Self: SetRepo` default, so each report delegates
    // to the shared `update_flow` free functions.
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
        // An OBS-served update has no Gitea PR to compare against.
        if self.base.update_source == UpdateSource::Obs {
            return HashCheck::Ok;
        }

        let old = self.base.giteacohash.clone().unwrap_or_default();
        let giteaprapi = self.base.giteaprapi.clone().unwrap_or_default();
        let gitea = match Gitea::new(&self.base.config, &giteaprapi, None) {
            Ok(g) => g,
            // A missing token is a distinct, actionable failure; anything else
            // failing to build the client is a failed call.
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
    /// Adds a repo with `-n ar -cfGkn`, removes with
    /// `-n rr`, fanned out over [`TestReportBase::update_repos`].
    async fn set_repo(&self, target: &mut Target, operation: RepoOp) {
        set_repo_with_add_flags(&self.base, target, operation, "-n ar -cfGkn").await;
    }

    fn composition(&self) -> HashMap<SystemProduct, BTreeSet<String>> {
        self.base.composed.clone()
    }
}
