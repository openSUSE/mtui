//! Shared test doubles for the mtui-core integration tests.
//!
//! Compiled independently into each integration-test binary, so helpers used by
//! only some binaries look "dead" to the others.
#![allow(dead_code)]

use std::collections::HashMap;

use mtui_config::Config;
use mtui_hosts::{HostsGroup, MockConnection, Target};
use mtui_testreport::{TestReport, TestReportBase};
use mtui_types::SystemProduct;
use mtui_types::enums::{ExecutionMode, TargetState};

/// A minimal [`TestReport`] double with a settable RRID and host group.
///
/// The Rust analogue of upstream's `MagicMock`-based `make_report`: it carries a
/// [`TestReportBase`] (so `base()`/`targets` work) and reports the RRID it was
/// built with. Everything else is a no-op / empty, which is all the registry and
/// fan-out engine exercise.
pub struct FakeReport {
    base: TestReportBase,
    rrid: String,
}

impl FakeReport {
    /// A report with the given RRID and no connected hosts.
    #[must_use]
    pub fn new(rrid: &str) -> Self {
        Self::with_hosts(rrid, &[])
    }

    /// A report with the given RRID and one `Target` per hostname (each backed
    /// by a [`MockConnection`], so `targets` is non-empty).
    #[must_use]
    pub fn with_hosts(rrid: &str, hosts: &[&str]) -> Self {
        let mut base = TestReportBase::new(Config::default());
        let targets: Vec<Target> = hosts
            .iter()
            .map(|h| {
                Target::with_connection(
                    *h,
                    TargetState::Enabled,
                    ExecutionMode::Serial,
                    Box::new(MockConnection::new(*h)),
                )
            })
            .collect();
        base.targets = HostsGroup::new(targets, false);
        Self {
            base,
            rrid: rrid.to_owned(),
        }
    }

    /// Boxes this report for insertion into a registry.
    #[must_use]
    pub fn boxed(self) -> Box<dyn TestReport + Send> {
        Box::new(self)
    }
}

#[async_trait::async_trait]
impl TestReport for FakeReport {
    fn base(&self) -> &TestReportBase {
        &self.base
    }
    fn base_mut(&mut self) -> &mut TestReportBase {
        &mut self.base
    }
    fn id(&self) -> String {
        self.rrid.clone()
    }
    fn parser(&self) -> HashMap<String, String> {
        HashMap::new()
    }
    fn update_repos_parser(&self) -> HashMap<SystemProduct, String> {
        HashMap::new()
    }
    fn list_update_commands(&self, _targets: &HostsGroup) {}
    async fn check_hash(&self) -> (bool, String, String) {
        (true, String::new(), String::new())
    }
}
