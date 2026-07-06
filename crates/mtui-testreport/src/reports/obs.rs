//! The OBS [`TestReport`] implementation ([`ObsReport`]).
//!
//! Port of upstream `mtui.test_reports.obs_report.OBSTestReport`. It keys its
//! identity on the parsed [`RequestReviewID`] and derives its update-repo map by
//! parsing the OBS/IBS checkout's `project.xml` via
//! [`obsrepoparse`](super::repoparse::obsrepoparse), reading the checkout under
//! [`report_wd`](TestReportBase::report_wd). OBS is checked out with
//! `osc qam` / SVN (not Gitea), so there is no git commit to verify —
//! [`check_hash`](TestReport::check_hash) is the constant `(true, "", "")`
//! (upstream always returns `True, "", ""`).
//!
//! ## Scope (task nbv.11)
//!
//! Mirrors the `SlReport`/`PiReport` boundaries:
//! * `set_repo` (the `SetRepo` impl driving `RepoManager::run_zypper`) is **not**
//!   here: it lands in its dedicated dependent task (nbv.fly) together with the
//!   Target lock-wiring.
//! * `list_update_commands` renders per-host commands via `target.doer('updater')`
//!   upstream, but the `OperationGroup`/doer seam on `Target` is deferred (see the
//!   `TODO(Phase 4)` in `mtui-hosts::target::operation`). Until it is wired this
//!   is a documented no-op stub.
//! * `_show_yourself_data` is not on the trait skeleton yet (same deferral as
//!   `SlReport`/`PiReport`).
//! * `id()` returns `""` when no RRID is loaded (upstream `str(self.rrid)` would
//!   raise); this matches the graceful path chosen for the sibling reports.

use std::collections::HashMap;

use mtui_config::options::Config;
use mtui_hosts::HostsGroup;
use mtui_types::{RequestReviewID, SystemProduct};
use tracing::debug;

use super::repoparse::obsrepoparse;
use crate::testreport::{TestReport, TestReportBase};

/// A [`TestReport`] for OBS/IBS updates (upstream `OBSTestReport`).
pub struct ObsReport {
    base: TestReportBase,
}

impl ObsReport {
    /// Builds an [`ObsReport`] from `config`.
    ///
    /// Upstream's `__init__` seeds the rating/realid envelope fields to empty;
    /// [`TestReportBase::new`] already applies those defaults, so this simply
    /// wraps a fresh base.
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
        // Upstream `obsrepoparse(self.repository, self.report_wd())`. Upstream
        // asserts `self.path`; we degrade to an empty map when no report is
        // loaded (or the checkout dir can't be resolved), matching the graceful
        // style of the sibling reports rather than panicking.
        match self.base.report_wd() {
            Ok(dir) => obsrepoparse(&self.base.repository, &dir),
            Err(e) => {
                debug!(error = %e, "update_repos_parser: no report working dir");
                HashMap::new()
            }
        }
    }

    fn list_update_commands(&self, _targets: &HostsGroup) {
        // Deferred: upstream renders per-host commands via
        // `target.doer('updater')['command'].safe_substitute(repa=..., packages=...)`.
        // The `OperationGroup`/doer accessor on `Target` is not wired yet
        // (see `TODO(Phase 4)` in `mtui-hosts::target::operation`). The real
        // rendering lands with that seam.
        debug!(
            "list_update_commands: doer rendering deferred until the OperationGroup seam is wired"
        );
    }

    async fn check_hash(&self) -> (bool, String, String) {
        // Upstream OBS always returns (True, "", "") — OBS/IBS checkout is via
        // osc qam / SVN, so there is no git commit hash to verify.
        (true, String::new(), String::new())
    }
}
