//! [`HostsGroup`] — a composite over many [`Target`]s.
//!
//! ## Reference
//!
//! Ported from upstream `mtui/hosts/target/hostgroup.py` (`HostsGroup`, a
//! `UserDict[str, Target]`). Upstream's class is a god-object that also owns the
//! remote lock protocol, the reboot/reconnect lifecycle, package-version
//! querying, the full `perform_{install,uninstall,prepare,downgrade,update}`
//! update workflow, and a family of `report_*` methods.
//!
//! This module deliberately ports only the **container + fan-out** surface
//! (P2.5):
//!
//! * construction and [`select`](HostsGroup::select)ion of a host subset,
//! * [`names`](HostsGroup::names) / iteration,
//! * command fan-out via [`run`](HostsGroup::run) (delegating to
//!   [`super::actions::RunCommand`]),
//! * SFTP fan-out ([`sftp_put`](HostsGroup::sftp_put) /
//!   [`sftp_get`](HostsGroup::sftp_get) / [`sftp_remove`](HostsGroup::sftp_remove)).
//!
//! The remaining upstream responsibilities are owned by later tasks and are
//! **intentionally not stubbed here** (stubs calling not-yet-built `Target`
//! methods would be dead code and could tempt a crate cycle):
//!
//! * remote locks (`lock` / `unlock` / `pool_unlock` / `update_lock`) — **P2.6**,
//! * `reboot` / `_reboot` / reconnect — **P2.9**,
//! * `query_versions` and system/product parsing — **P2.8**,
//! * `perform_*` update workflow and `report_*` — **Phase 4** (needs the
//!   doer/check registries in `mtui-testreport`; adding them here would make
//!   `mtui-hosts` depend on `mtui-testreport` and break the acyclic graph).
//!
//! The internal map is a [`BTreeMap`] so `names()` / iteration are
//! deterministically ordered by hostname — upstream always iterates its dict via
//! `sorted()` for anything order-sensitive, so this matches observable
//! behaviour without adding an insertion-order dependency.

use std::collections::BTreeMap;
use std::path::Path;

use crate::error::{HostError, Result};

use super::Target;
use super::actions::{self, Command, RunCommand};

/// A composite over a group of [`Target`]s, keyed by hostname.
///
/// All hosts in a group are expected to be enabled; the lifetime of the object
/// should match the execution of a single user command (upstream note). See the
/// module docs for the ported vs. deferred surface.
pub struct HostsGroup {
    data: BTreeMap<String, Target>,
    /// Whether the surrounding session is interactive. Threaded through to the
    /// fan-out helpers as the (Phase 6) spinner/prompt seam; see
    /// [`actions`](super::actions).
    interactive: bool,
}

impl HostsGroup {
    /// Builds a group from `hosts`, keyed by [`Target::hostname`].
    ///
    /// `interactive` mirrors upstream: `true` for the REPL (spinner/prompt seam
    /// on), `false` for headless callers such as `mtui-mcp`.
    #[must_use]
    pub fn new(hosts: Vec<Target>, interactive: bool) -> Self {
        let data = hosts
            .into_iter()
            .map(|h| (h.hostname().to_owned(), h))
            .collect();
        Self { data, interactive }
    }

    /// Whether the surrounding session is interactive.
    #[must_use]
    pub const fn is_interactive(&self) -> bool {
        self.interactive
    }

    /// The number of hosts in the group.
    #[must_use]
    pub fn len(&self) -> usize {
        self.data.len()
    }

    /// Whether the group has no hosts.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    /// The hostnames in the group, sorted.
    #[must_use]
    pub fn names(&self) -> Vec<String> {
        self.data.keys().cloned().collect()
    }

    /// Whether `hostname` is a member of the group.
    #[must_use]
    pub fn contains(&self, hostname: &str) -> bool {
        self.data.contains_key(hostname)
    }

    /// A shared reference to a member target, if present.
    #[must_use]
    pub fn get(&self, hostname: &str) -> Option<&Target> {
        self.data.get(hostname)
    }

    /// A mutable reference to a member target, if present.
    pub fn get_mut(&mut self, hostname: &str) -> Option<&mut Target> {
        self.data.get_mut(hostname)
    }

    /// Iterates the member targets in sorted hostname order.
    pub fn targets(&self) -> impl Iterator<Item = &Target> {
        self.data.values()
    }

    /// Selects a subset of the group into a **new owned** group.
    ///
    /// Ported from upstream `HostsGroup.select`:
    ///
    /// * `hosts = None`, `enabled = false` → a clone of the whole group.
    /// * `hosts = None`, `enabled = true` → only non-disabled hosts.
    /// * `hosts = Some([..])` → exactly those hosts (and, when `enabled`, only
    ///   the non-disabled among them).
    ///
    /// An unknown hostname is a [`HostError::NotConnected`] (upstream's
    /// `HostIsNotConnectedError`).
    ///
    /// Selection **moves** the chosen targets out of `self` (a `Target` owns a
    /// live `Box<dyn Connection>` and is not `Clone`); `self` is consumed. This
    /// is the honest Rust deviation from upstream, which shares `Target`
    /// references across the parent and child dicts.
    pub fn select(self, hosts: Option<&[String]>, enabled: bool) -> Result<HostsGroup> {
        let interactive = self.interactive;
        let is_enabled = |t: &Target| t.state() != mtui_types::enums::TargetState::Disabled;

        let selected: Vec<Target> = match hosts {
            None => self
                .data
                .into_values()
                .filter(|t| !enabled || is_enabled(t))
                .collect(),
            Some(names) => {
                for name in names {
                    if !self.data.contains_key(name) {
                        return Err(HostError::NotConnected { host: name.clone() });
                    }
                }
                self.data
                    .into_iter()
                    .filter(|(hn, t)| names.contains(hn) && (!enabled || is_enabled(t)))
                    .map(|(_, t)| t)
                    .collect()
            }
        };

        Ok(HostsGroup::new(selected, interactive))
    }

    /// Runs a command across the group: parallel hosts concurrently, serial
    /// hosts one at a time.
    ///
    /// `cmd` accepts a single string (run on every host) or a per-host
    /// [`Command::PerHost`] map (hosts absent from the map are skipped). See
    /// [`RunCommand`].
    pub async fn run(&mut self, cmd: impl Into<Command>) {
        RunCommand::new(&mut self.data, cmd, self.interactive)
            .run()
            .await;
    }

    /// Uploads `local` to `remote` on every host in parallel.
    pub async fn sftp_put(&mut self, local: &Path, remote: &Path) {
        actions::sftp_put_all(&mut self.data, local, remote, self.interactive).await;
    }

    /// Downloads `remote` (per-host suffixed) into `local` from every host in
    /// parallel.
    pub async fn sftp_get(&mut self, remote: &str, local: &Path) {
        actions::sftp_get_all(&mut self.data, remote, local, self.interactive).await;
    }

    /// Deletes `path` on every host in parallel.
    pub async fn sftp_remove(&mut self, path: &Path) {
        actions::sftp_remove_all(&mut self.data, path, self.interactive).await;
    }
}

#[cfg(test)]
mod tests {
    use mtui_types::enums::{ExecutionMode, TargetState};
    use mtui_types::hostlog::CommandLog;

    use super::*;
    use crate::connection::MockConnection;

    fn echo(hostname: &str) -> MockConnection {
        MockConnection::new(hostname).with_default(CommandLog::new("", "ok", "", 0, 0))
    }

    fn tgt(hostname: &str, state: TargetState, mode: ExecutionMode) -> Target {
        Target::with_connection(hostname, state, mode, Box::new(echo(hostname)))
    }

    fn enabled(hostname: &str) -> Target {
        tgt(hostname, TargetState::Enabled, ExecutionMode::Parallel)
    }

    // --- construction / accessors ------------------------------------------

    #[test]
    fn new_keys_by_hostname_and_reports_names_sorted() {
        let g = HostsGroup::new(vec![enabled("h2"), enabled("h1")], true);
        assert_eq!(g.len(), 2);
        assert!(!g.is_empty());
        assert!(g.is_interactive());
        // BTreeMap orders names deterministically.
        assert_eq!(g.names(), vec!["h1".to_owned(), "h2".to_owned()]);
        assert!(g.contains("h1"));
        assert!(!g.contains("nope"));
        assert!(g.get("h1").is_some());
        assert!(g.get("nope").is_none());
    }

    #[test]
    fn empty_group() {
        let g = HostsGroup::new(vec![], false);
        assert!(g.is_empty());
        assert_eq!(g.len(), 0);
        assert!(!g.is_interactive());
        assert!(g.names().is_empty());
    }

    #[test]
    fn get_mut_and_targets_iter() {
        let mut g = HostsGroup::new(vec![enabled("h1")], true);
        assert!(g.get_mut("h1").is_some());
        assert!(g.get_mut("nope").is_none());
        assert_eq!(g.targets().count(), 1);
    }

    // --- select ------------------------------------------------------------

    #[test]
    fn select_none_not_enabled_returns_whole_group() {
        let g = HostsGroup::new(vec![enabled("h1"), enabled("h2")], true);
        let sel = g.select(None, false).unwrap();
        assert_eq!(sel.names(), vec!["h1".to_owned(), "h2".to_owned()]);
        assert!(sel.is_interactive());
    }

    #[test]
    fn select_none_enabled_drops_disabled() {
        let g = HostsGroup::new(
            vec![
                enabled("h1"),
                tgt("h2", TargetState::Disabled, ExecutionMode::Parallel),
            ],
            true,
        );
        let sel = g.select(None, true).unwrap();
        assert_eq!(sel.names(), vec!["h1".to_owned()]);
    }

    #[test]
    fn select_by_name_returns_subset() {
        let g = HostsGroup::new(vec![enabled("h1"), enabled("h2"), enabled("h3")], true);
        let sel = g
            .select(Some(&["h1".to_owned(), "h3".to_owned()]), false)
            .unwrap();
        assert_eq!(sel.names(), vec!["h1".to_owned(), "h3".to_owned()]);
    }

    #[test]
    fn select_by_name_with_enabled_filters_disabled() {
        let g = HostsGroup::new(
            vec![
                enabled("h1"),
                tgt("h2", TargetState::Disabled, ExecutionMode::Parallel),
            ],
            true,
        );
        let sel = g
            .select(Some(&["h1".to_owned(), "h2".to_owned()]), true)
            .unwrap();
        assert_eq!(sel.names(), vec!["h1".to_owned()]);
    }

    #[test]
    fn select_unknown_host_is_not_connected_error() {
        let g = HostsGroup::new(vec![enabled("h1")], true);
        // `HostsGroup` is not `Debug` (it owns `Box<dyn Connection>`), so match
        // the result explicitly instead of `unwrap_err`.
        match g.select(Some(&["ghost".to_owned()]), false) {
            Err(HostError::NotConnected { host }) => assert_eq!(host, "ghost"),
            other => panic!(
                "expected NotConnected error, got a group: {}",
                other.is_ok()
            ),
        }
    }

    // --- fan-out delegation ------------------------------------------------

    #[tokio::test]
    async fn run_dispatches_to_all_members() {
        let (m1, m2) = (echo("h1"), echo("h2"));
        let (h1, h2) = (m1.clone(), m2.clone());
        let mut g = HostsGroup::new(
            vec![
                Target::with_connection(
                    "h1",
                    TargetState::Enabled,
                    ExecutionMode::Parallel,
                    Box::new(m1),
                ),
                Target::with_connection(
                    "h2",
                    TargetState::Enabled,
                    ExecutionMode::Serial,
                    Box::new(m2),
                ),
            ],
            false,
        );

        g.run("hostname").await;

        assert_eq!(h1.commands(), vec!["hostname".to_owned()]);
        assert_eq!(h2.commands(), vec!["hostname".to_owned()]);
    }

    #[tokio::test]
    async fn sftp_helpers_fan_out() {
        use crate::connection::MockSftpOp;

        let m1 = echo("h1");
        let h1 = m1.clone();
        let mut g = HostsGroup::new(
            vec![Target::with_connection(
                "h1",
                TargetState::Enabled,
                ExecutionMode::Parallel,
                Box::new(m1),
            )],
            false,
        );

        g.sftp_put(Path::new("/l"), Path::new("/r")).await;
        g.sftp_get("/r", Path::new("/l")).await;
        g.sftp_remove(Path::new("/r")).await;

        let ops = h1.sftp_ops();
        assert!(matches!(ops[0], MockSftpOp::Put { .. }));
        assert!(matches!(ops[1], MockSftpOp::Get { .. }));
        assert!(matches!(ops[2], MockSftpOp::Remove(_)));
    }
}
