//! Concrete [`TestReport`](crate::TestReport) implementations.
//!
//! Ships the null object ([`NullReport`]), the SUSE Linux report ([`SlReport`]),
//! the PI report ([`PiReport`]), and the OBS report ([`ObsReport`]). The
//! [`repoparse`] helpers derive each report's update-repo map.

pub mod null;
pub mod obs;
pub mod pi;
pub mod repoparse;
pub mod sl;

pub use null::NullReport;
pub use obs::ObsReport;
pub use pi::PiReport;
pub use sl::SlReport;

use std::collections::BTreeMap;

use mtui_hosts::{RepoOp, Target};
use mtui_types::{RequestReviewID, SystemProduct};

use crate::testreport::TestReportBase;

/// Shared `set_repo` body for every concrete report.
///
/// Ports the three near-identical `set_repo` methods upstream (`sl_report.py`,
/// `pi_report.py`, `obs_report.py`): they differ *only* in the `zypper ar` flag
/// string (`add_flags`), so this helper takes it as a parameter and otherwise
/// forwards `base.update_repos` + `base.rrid` into
/// [`RepoManager::run_zypper`](mtui_hosts::RepoManager::run_zypper).
///
/// `run_zypper` wants a [`BTreeMap`] (deterministic order); the report stores an
/// unordered [`HashMap`](std::collections::HashMap), so we convert here. When no
/// RRID is loaded the report is not usable for a repo change (upstream assumes a
/// loaded report), so we log and return without touching the host.
async fn set_repo_with_add_flags(
    base: &TestReportBase,
    target: &mut Target,
    operation: RepoOp,
    add_flags: &str,
) {
    let Some(rrid) = base.rrid.as_ref() else {
        tracing::debug!("set_repo: no RRID loaded; nothing to (un)register");
        return;
    };
    let repos: BTreeMap<SystemProduct, String> = base
        .update_repos
        .iter()
        .map(|(p, url)| (p.clone(), url.clone()))
        .collect();
    let cmd = match operation {
        RepoOp::Add => add_flags,
        RepoOp::Remove => "-n rr",
    };
    let rrid: &RequestReviewID = rrid;
    target.repo_manager().run_zypper(cmd, &repos, rrid).await;
}
