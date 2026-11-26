//! The SUSE Linux [`TestReport`] implementation ([`SlReport`]).
//!
//! Keys its identity on the parsed [`RequestReviewID`](mtui_types::RequestReviewID), derives its update-repo
//! map by dispatching among the [`repoparse`](super::repoparse) helpers, and
//! verifies its git commit hash against Gitea.

use std::collections::{BTreeSet, HashMap};

use mtui_config::options::Config;
use mtui_datasources::error::GiteaError;
use mtui_datasources::gitea::Gitea;
use mtui_hosts::{RepoOp, SetRepo, Target};
use mtui_types::SystemProduct;
use tracing::debug;

use super::repoparse::{gitrepoparse, reporepoparse};
use super::set_repo_with_add_flags;
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

    fn update_repos_parser(&self) -> HashMap<SystemProduct, String> {
        // The `repositories` short-circuit must stay first: it is populated
        // upstream of TeReGen and can start appearing for a git-served update
        // at any time, with no release-gated notice to mtui (#433). This
        // precedence is load-bearing, not a nicety.
        if !self.base.repositories.is_empty() {
            let repos: Vec<String> = self.base.repositories.iter().cloned().collect();
            return reporepoparse(&repos, &self.base.products);
        }
        gitrepoparse(&self.base.repository, &self.base.products)
    }

    fn as_set_repo(&self) -> Option<&dyn mtui_hosts::SetRepo> {
        Some(self)
    }

    async fn check_hash(&self) -> HashCheck {
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

    fn composition(&self) -> Option<&HashMap<SystemProduct, BTreeSet<String>>> {
        Some(&self.base.composed)
    }

    fn repositories(&self) -> Option<&HashMap<SystemProduct, String>> {
        Some(&self.base.update_repos)
    }
}
