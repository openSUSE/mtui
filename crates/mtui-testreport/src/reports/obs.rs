//! The OBS [`TestReport`] implementation ([`ObsReport`]).
//!
//! Keys its identity on the parsed [`RequestReviewID`](mtui_types::RequestReviewID) and derives its
//! update-repo map from the report document's `install.targets[]`
//! ([`obs_update_repos`](crate::ingest::obs_update_repos)). OBS is not
//! Gitea-backed, so there is no git commit to verify and the default
//! [`check_hash`](TestReport::check_hash) applies.
//!
//! `set_repo` adds with the OBS-specific `-n ar -ckn` — no `fG`, unlike SL/PI.

use std::collections::{BTreeSet, HashMap};

use mtui_config::options::Config;
use mtui_hosts::{RepoOp, SetRepo, Target};
use mtui_types::SystemProduct;
use tracing::debug;

use super::set_repo_with_add_flags;
use crate::testreport::{TestReport, TestReportBase};

/// A [`TestReport`] for OBS/IBS updates.
pub struct ObsReport {
    base: TestReportBase,
}

impl ObsReport {
    /// Builds an [`ObsReport`] from `config`.
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

    fn update_repos_parser(&self) -> HashMap<SystemProduct, String> {
        match self.base.document.as_ref() {
            Some(doc) => crate::ingest::obs_update_repos(doc),
            None => {
                debug!("update_repos_parser: no report document loaded");
                HashMap::new()
            }
        }
    }

    fn as_set_repo(&self) -> Option<&dyn mtui_hosts::SetRepo> {
        Some(self)
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

    fn composition(&self) -> Option<&HashMap<SystemProduct, BTreeSet<String>>> {
        Some(&self.base.composed)
    }

    fn repositories(&self) -> Option<&HashMap<SystemProduct, String>> {
        Some(&self.base.update_repos)
    }
}
