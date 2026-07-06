//! The null-object [`TestReport`] implementation.
//!
//! Port of upstream `mtui.test_reports.null_report.NullTestReport`: used when no
//! test report is loaded. It is falsy, has an empty ID and empty parser tables,
//! roots its target working directory directly under `config.target_tempdir`,
//! and reports a trivially-valid hash.

use std::collections::HashMap;
use std::path::PathBuf;

use mtui_config::options::Config;
use mtui_hosts::HostsGroup;
use mtui_types::SystemProduct;

use crate::testreport::{TestReport, TestReportBase};

/// A null-object [`TestReport`], active when nothing is loaded.
pub struct NullReport {
    base: TestReportBase,
}

impl NullReport {
    /// Builds a [`NullReport`] from `config`.
    ///
    /// Mirrors upstream, which sets `self.path = Path.cwd() / "None"`. When the
    /// current directory cannot be determined the path falls back to a bare
    /// `None` component, matching the upstream intent (a placeholder path that
    /// never resolves to a real template).
    #[must_use]
    pub fn new(config: Config) -> Self {
        let mut base = TestReportBase::new(config);
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        base.path = Some(cwd.join("None"));
        Self { base }
    }
}

#[async_trait::async_trait]
impl TestReport for NullReport {
    fn base(&self) -> &TestReportBase {
        &self.base
    }

    fn base_mut(&mut self) -> &mut TestReportBase {
        &mut self.base
    }

    fn id(&self) -> String {
        String::new()
    }

    fn parser(&self) -> HashMap<String, String> {
        HashMap::new()
    }

    fn update_repos_parser(&self) -> HashMap<SystemProduct, String> {
        HashMap::new()
    }

    fn list_update_commands(&self, _targets: &HostsGroup) {
        // Null object: does nothing.
    }

    async fn check_hash(&self) -> (bool, String, String) {
        (true, String::new(), String::new())
    }

    fn is_loaded(&self) -> bool {
        false
    }

    // `target_wd` uses the trait default (join under `config.target_tempdir`),
    // which is exactly upstream `NullTestReport.target_wd`.
}
