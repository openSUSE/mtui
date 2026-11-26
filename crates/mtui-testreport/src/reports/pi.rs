//! The PI [`TestReport`] implementation ([`PiReport`]).
//!
//! Keys its identity on the parsed [`RequestReviewID`](mtui_types::RequestReviewID) and derives its
//! update-repo map by delegating unconditionally to [`reporepoparse`] — the
//! simplest of the concrete reports, reusing the same helper as
//! [`SlReport`](super::sl::SlReport). PI has no git commit to verify, so the
//! default [`check_hash`](TestReport::check_hash) applies.
//!
//! `set_repo` adds with `-n ar -cfGkn`, same as SL.

use std::collections::{BTreeSet, HashMap};

use mtui_config::options::Config;
use mtui_hosts::{HostsGroup, RepoOp, SetRepo, Target};
use mtui_types::SystemProduct;
use tracing::debug;

use super::repoparse::reporepoparse;
use super::set_repo_with_add_flags;
use crate::testreport::{TestReport, TestReportBase};

/// A [`TestReport`] for PI updates.
pub struct PiReport {
    base: TestReportBase,
}

impl PiReport {
    /// Builds a [`PiReport`] from `config`.
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

    fn update_repos_parser(&self) -> HashMap<SystemProduct, String> {
        // No maintenance-id branching (unlike SL).
        let repos: Vec<String> = self.base.repositories.iter().cloned().collect();
        reporepoparse(&repos, &self.base.products)
    }

    fn as_set_repo(&self) -> Option<&dyn mtui_hosts::SetRepo> {
        Some(self)
    }

    fn list_update_commands(&self, _targets: &HostsGroup) {
        // `perform_update` below runs the per-host `updater` commands; the
        // read-only listing command only ever prints a placeholder.
        debug!("list_update_commands: no-op stub, not yet implemented");
    }
}

#[async_trait::async_trait]
impl SetRepo for PiReport {
    /// Adds a repo with `-n ar -cfGkn` (same as SL),
    /// removes with `-n rr`, fanned out over [`TestReportBase::update_repos`].
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
