//! The null-object [`TestReport`] implementation.
//!
//! Used when no test report is loaded. It is falsy, has an empty ID, no report
//! path and empty parser tables, roots its target working directory directly
//! under `config.target_tempdir`, and reports a trivially-valid hash.

use std::collections::HashMap;

use mtui_config::options::Config;
use mtui_hosts::HostsGroup;
use mtui_types::SystemProduct;

use crate::testreport::{HashCheck, TestReport, TestReportBase};

/// A null-object [`TestReport`], active when nothing is loaded.
pub struct NullReport {
    base: TestReportBase,
}

impl NullReport {
    /// Builds a [`NullReport`] from `config`.
    ///
    /// `path` stays unset so [`TestReportBase::report_wd`] errors rather than
    /// resolving somewhere a dispatch could act (#524).
    #[must_use]
    pub fn new(config: Config) -> Self {
        Self {
            base: TestReportBase::new(config),
        }
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

    async fn check_hash(&self) -> HashCheck {
        HashCheck::Ok
    }

    fn is_loaded(&self) -> bool {
        false
    }

    // `target_wd` uses the trait default (join under `config.target_tempdir`).
}
