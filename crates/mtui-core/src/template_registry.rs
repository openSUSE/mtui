//! The collection of loaded templates plus an active pointer.
//!
//! [`TestReport`]s keyed by RRID, each an individually lockable [`ReportEntry`]
//! a dispatch acquires via [`handle`](TemplateRegistry::handle). The stable
//! per-instance [`id`](TemplateRegistry::id) seeds the `(registry.id, RRID)`
//! owner key host arbitration uses.
//!
//! ## Teardown
//!
//! Dropping a removed report closes every transport but cannot release its
//! *async* ownership (the arbiter claim, the remote pool-claim and operation
//! locks), and a wedged host must not hang removal. So
//! [`remove`](TemplateRegistry::remove) and the same-RRID replacement in
//! `add_or_replace` are **async** and run the REPL `quit`'s bounded teardown
//! before the entry is dropped and active repointed.

use std::sync::Arc;
use std::time::Duration;

use indexmap::IndexMap;
use mtui_config::Config;
use mtui_hosts::get_arbiter;
use mtui_testreport::TestReport;
use tokio::sync::Mutex;

use crate::session::HOST_CLOSE_TIMEOUT;

/// A loaded report behind its own lock.
///
/// A dispatch takes the [`OwnedMutexGuard`](tokio::sync::OwnedMutexGuard) of
/// exactly the entry it acts on (via [`TemplateRegistry::handle`]), so it need
/// not borrow the whole registry.
pub type ReportEntry = Arc<Mutex<Box<dyn TestReport + Send + Sync>>>;

/// What a `list_templates` row could be read from a loaded entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TemplateRow {
    /// Connected-host count and workflow label.
    Read(usize, &'static str),
    /// Held by another dispatch, so neither could be read.
    Busy,
}

/// The per-report close budget; test-overridable.
#[cfg(not(test))]
fn remove_close_timeout() -> Duration {
    HOST_CLOSE_TIMEOUT
}
#[cfg(test)]
fn remove_close_timeout() -> Duration {
    tests::close_timeout_override()
}

/// Per-host teardown outcomes from removing (or replacing) a report.
///
/// Best-effort diagnostics to log; removal always completes regardless.
#[derive(Debug, Default)]
pub struct RemoveReport {
    /// Hosts that failed to disconnect, as `(hostname, error)` pairs.
    pub(crate) failed: Vec<(String, String)>,
    /// Hosts still disconnecting when the close budget expired.
    pub(crate) stragglers: Vec<String>,
    /// What the session left on each closed host, read after the close.
    pub(crate) leftovers: std::collections::BTreeMap<String, mtui_hosts::HostLeftovers>,
}

/// Holds the loaded templates and tracks the active one.
pub struct TemplateRegistry {
    /// Loaded reports keyed by RRID, in insertion (fan-out) order.
    entries: IndexMap<String, ReportEntry>,
    /// The active RRID, or `None` when nothing is loaded.
    active: Option<String>,
    /// Stable per-registry identity; the owner-key seed for host arbitration.
    id: String,
}

impl TemplateRegistry {
    /// Builds an empty registry.
    #[must_use]
    pub fn new(_config: Config) -> Self {
        Self {
            entries: IndexMap::new(),
            active: None,
            id: uuid::Uuid::new_v4().simple().to_string(),
        }
    }

    /// Stable per-registry identity (the arbitration owner-key seed).
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Inserts (or replaces) `report` keyed by its RRID.
    ///
    /// The first template added becomes active; re-adding an existing RRID
    /// replaces the stored report but leaves the active pointer alone. An empty
    /// RRID (the [`NullReport`](mtui_testreport::NullReport) sentinel) is ignored.
    pub fn add(&mut self, mut report: Box<dyn TestReport + Send + Sync>) {
        let rrid = report.id();
        if rrid.is_empty() {
            return;
        }
        // Both set: autoconnect takes the pool-selection path (one host per slot).
        {
            let base = report.base_mut();
            base.arbiter = Some(get_arbiter());
            base.owner = Some((self.id.clone(), rrid.clone()));
        }
        let is_new = !self.entries.contains_key(&rrid);
        self.entries
            .insert(rrid.clone(), Arc::new(Mutex::new(report)));
        if is_new && self.active.is_none() {
            self.active = Some(rrid);
        }
    }

    /// Inserts `report`, first tearing down any report already loaded under the
    /// same RRID.
    ///
    /// The *old* report's async ownership (arbiter claim, remote locks) and
    /// hosts are released before the new one is stored, or both leak. A new RRID
    /// is a plain [`add`](Self::add). Returns the old report's per-host teardown
    /// failures (empty for a new insert).
    pub(crate) async fn add_or_replace(
        &mut self,
        report: Box<dyn TestReport + Send + Sync>,
    ) -> RemoveReport {
        let rrid = report.id();
        if rrid.is_empty() {
            return RemoveReport::default();
        }
        let removed = if self.entries.contains_key(&rrid) {
            self.teardown(&rrid).await
        } else {
            RemoveReport::default()
        };
        self.add(report);
        removed
    }

    /// Releases `rrid`'s async ownership, closes its hosts, then drops it.
    ///
    /// The bounded teardown the REPL `quit` runs:
    /// [`release_pool_claims`](mtui_testreport::TestReport::release_pool_claims)
    /// then [`HostsGroup::close`](mtui_hosts::HostsGroup)`(None)`, each under
    /// `remove_close_timeout`. Only then is the entry dropped and, if active, the
    /// pointer repointed, so a reader never sees a half-torn-down active report.
    /// A no-op if `rrid` is absent. Returns per-host failures and stragglers.
    pub async fn remove(&mut self, rrid: &str) -> RemoveReport {
        if !self.entries.contains_key(rrid) {
            return RemoveReport::default();
        }
        self.teardown(rrid).await
    }

    /// Shared body of [`remove`](Self::remove) and
    /// [`add_or_replace`](Self::add_or_replace); assumes `rrid` is loaded.
    async fn teardown(&mut self, rrid: &str) -> RemoveReport {
        let mut result = RemoveReport::default();
        let timeout = remove_close_timeout();
        if let Some(entry) = self.entries.get(rrid).cloned() {
            let mut report = entry.lock().await;
            // Before disconnecting; a no-op without pooling.
            for host in report.release_pool_claims(timeout).await {
                tracing::warn!("still releasing the pool claim on {host}");
            }
            // So a straggler can still be named per host.
            let hosts = report.base_mut().targets.names();
            let close = report.base_mut().targets.close(None);
            match tokio::time::timeout(timeout, close).await {
                Ok(outcomes) => {
                    for (host, outcome) in outcomes {
                        if let Err(e) = outcome {
                            result.failed.push((host, e.to_string()));
                        }
                    }
                }
                Err(_) => result.stragglers = hosts,
            }
            result.leftovers = report.base().targets.leftovers();
        }
        self.entries.shift_remove(rrid);
        if self.active.as_deref() == Some(rrid) {
            self.active = self.entries.keys().next().cloned();
        }
        result
    }

    /// Returns a cloned handle (lockable [`ReportEntry`]) for `rrid`, or `None`
    /// if absent.
    ///
    /// The caller `.lock()`s it to read or mutate the report.
    #[must_use]
    pub fn handle(&self, rrid: &str) -> Option<ReportEntry> {
        self.entries.get(rrid).map(Arc::clone)
    }

    /// The cloned handle for the active report, or `None` when nothing is loaded.
    #[must_use]
    pub fn active_handle(&self) -> Option<ReportEntry> {
        self.active.as_deref().and_then(|rrid| self.handle(rrid))
    }

    /// The active RRID, or `None` when nothing is loaded.
    #[must_use]
    pub fn active_rrid(&self) -> Option<&str> {
        self.active.as_deref()
    }

    /// Makes `rrid` the active template. Returns `false` if `rrid` is not
    /// loaded.
    pub fn set_active(&mut self, rrid: &str) -> bool {
        if self.entries.contains_key(rrid) {
            self.active = Some(rrid.to_owned());
            true
        } else {
            false
        }
    }

    /// Clears the active pointer (the empty-session state).
    pub(crate) fn set_active_none(&mut self) {
        self.active = None;
    }

    /// Every loaded RRID in insertion order (for completion).
    #[must_use]
    pub fn rrids(&self) -> Vec<String> {
        self.entries.keys().cloned().collect()
    }

    /// Whether at least one (non-null) template is loaded.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Number of loaded templates.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Membership test by RRID.
    #[must_use]
    pub fn contains(&self, rrid: &str) -> bool {
        self.entries.contains_key(rrid)
    }

    /// Builds a cheap per-call view of this registry that **shares** the same
    /// per-entry report locks.
    ///
    /// The entry map and active pointer are cloned, but each entry is an
    /// `Arc<Mutex<..>>`, so a command acting through a snapshot mutates content
    /// the canonical registry sees. Used by
    /// [`Session::fork_for_call`](crate::Session::fork_for_call); the
    /// [`id`](Self::id) is preserved.
    #[must_use]
    pub(crate) fn snapshot(&self) -> Self {
        Self {
            entries: self
                .entries
                .iter()
                .map(|(k, v)| (k.clone(), Arc::clone(v)))
                .collect(),
            active: self.active.clone(),
            id: self.id.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use mtui_hosts::{MockConnection, Owner, Target};
    use mtui_types::hostlog::CommandLog;

    use super::*;
    use crate::commands::testkit::{fake_report, fake_report_from_base, target_with};

    /// Test-only override for [`remove_close_timeout`], in milliseconds;
    /// `u64::MAX` means the production [`HOST_CLOSE_TIMEOUT`]. Serialised by
    /// [`CLOSE_TIMEOUT_LOCK`] so a shrunk budget never leaks into a concurrent
    /// test (the whole suite shares one process).
    static CLOSE_TIMEOUT_MS: AtomicU64 = AtomicU64::new(u64::MAX);
    static CLOSE_TIMEOUT_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    pub(super) fn close_timeout_override() -> Duration {
        match CLOSE_TIMEOUT_MS.load(Ordering::SeqCst) {
            u64::MAX => HOST_CLOSE_TIMEOUT,
            ms => Duration::from_millis(ms),
        }
    }

    fn registry() -> TemplateRegistry {
        TemplateRegistry::new(Config::default())
    }

    fn healthy(host: &str) -> Target {
        target_with(host, |c| {
            c.with_default(CommandLog::new("", "ok", "", 0, 0))
        })
    }

    #[tokio::test]
    async fn remove_absent_is_a_noop() {
        let mut reg = registry();
        let removed = reg.remove("SUSE:Maintenance:9:9").await;
        assert!(removed.failed.is_empty());
        assert!(removed.stragglers.is_empty());
        assert!(reg.is_empty());
    }

    #[tokio::test]
    async fn remove_drops_entry_and_repoints_active() {
        let mut reg = registry();
        reg.add(fake_report("SUSE:Maintenance:1:1", &["h1"], "ok"));
        reg.add(fake_report("SUSE:Maintenance:2:2", &["h2"], "ok"));
        assert_eq!(reg.active_rrid(), Some("SUSE:Maintenance:1:1"));

        // Removing the active template promotes the survivor.
        let removed = reg.remove("SUSE:Maintenance:1:1").await;
        assert!(removed.failed.is_empty());
        assert!(!reg.contains("SUSE:Maintenance:1:1"));
        assert_eq!(reg.active_rrid(), Some("SUSE:Maintenance:2:2"));

        // Removing the last empties the registry and clears active.
        reg.remove("SUSE:Maintenance:2:2").await;
        assert!(reg.is_empty());
        assert_eq!(reg.active_rrid(), None);
    }

    /// Claims `hosts` through the process-global arbiter under the owner key
    /// `add` assigned (`(registry.id, rrid)`) and records them as the report's
    /// pool claims, returning the owner so callers can assert release. Callers
    /// must pass hostnames unique per test: the arbiter is shared across the
    /// consolidated test binary.
    fn claim_hosts(reg: &mut TemplateRegistry, rrid: &str, hosts: &[&str]) -> Owner {
        let owner: Owner = (reg.id().to_owned(), rrid.to_owned());
        let arbiter = mtui_hosts::get_arbiter();
        let entry = reg.handle(rrid).expect("report loaded");
        let mut report = entry.try_lock().expect("entry uncontended in test");
        for h in hosts {
            assert!(arbiter.try_acquire(h, &owner), "host {h} claimable");
            report.base_mut().pool_claims.insert((*h).to_owned());
        }
        owner
    }

    #[tokio::test]
    async fn remove_releases_arbiter_ownership_and_pool_claims() {
        let mut reg = registry();
        reg.add(fake_report("SUSE:Maintenance:1:1", &["h1", "h2"], "ok"));
        claim_hosts(
            &mut reg,
            "SUSE:Maintenance:1:1",
            &["arb-remove-h1", "arb-remove-h2"],
        );
        let arbiter = mtui_hosts::get_arbiter();

        reg.remove("SUSE:Maintenance:1:1").await;

        assert!(arbiter.owner_of("arb-remove-h1").is_none());
        assert!(arbiter.owner_of("arb-remove-h2").is_none());
        assert!(!reg.contains("SUSE:Maintenance:1:1"));
    }

    #[tokio::test]
    async fn remove_names_host_that_fails_to_disconnect_but_still_removes() {
        let mut reg = registry();
        let mut base = mtui_testreport::TestReportBase::new(Config::default());
        base.rrid = "SUSE:Maintenance:1:1".parse().ok();
        base.targets = mtui_hosts::HostsGroup::new(
            vec![
                healthy("good"),
                target_with("bad", MockConnection::with_failing_close),
            ],
            false,
        );
        reg.add(fake_report_from_base(base));

        let removed = reg.remove("SUSE:Maintenance:1:1").await;
        assert!(!reg.contains("SUSE:Maintenance:1:1"), "removal completes");
        assert_eq!(removed.failed.len(), 1, "the failing host is named");
        assert_eq!(removed.failed[0].0, "bad");
        assert!(removed.stragglers.is_empty());
    }

    #[tokio::test]
    async fn remove_returns_promptly_when_a_host_straggles() {
        let _guard = CLOSE_TIMEOUT_LOCK.lock().await;
        CLOSE_TIMEOUT_MS.store(50, Ordering::SeqCst);

        let gate = std::sync::Arc::new(tokio::sync::Notify::new());
        let mut base = mtui_testreport::TestReportBase::new(Config::default());
        base.rrid = "SUSE:Maintenance:1:1".parse().ok();
        base.targets = mtui_hosts::HostsGroup::new(
            vec![target_with("wedged", {
                let gate = std::sync::Arc::clone(&gate);
                move |c| c.with_blocking_close(gate)
            })],
            false,
        );
        let mut reg = registry();
        reg.add(fake_report_from_base(base));

        let start = std::time::Instant::now();
        let removed =
            tokio::time::timeout(Duration::from_secs(5), reg.remove("SUSE:Maintenance:1:1"))
                .await
                .expect("remove must return despite the wedged host");
        assert!(start.elapsed() < Duration::from_secs(5));
        assert!(!reg.contains("SUSE:Maintenance:1:1"), "removal completes");
        assert_eq!(removed.stragglers, vec!["wedged".to_owned()]);

        gate.notify_waiters();
        CLOSE_TIMEOUT_MS.store(u64::MAX, Ordering::SeqCst);
    }

    #[tokio::test]
    async fn add_or_replace_tears_down_old_report_first() {
        let mut reg = registry();
        reg.add(fake_report("SUSE:Maintenance:1:1", &["h1", "h2"], "ok"));
        claim_hosts(
            &mut reg,
            "SUSE:Maintenance:1:1",
            &["arb-replace-h1", "arb-replace-h2"],
        );
        let arbiter = mtui_hosts::get_arbiter();

        // Re-loading the same RRID releases the old claims before storing.
        let removed = reg
            .add_or_replace(fake_report("SUSE:Maintenance:1:1", &["h3"], "ok"))
            .await;
        assert!(removed.failed.is_empty());
        assert!(
            arbiter.owner_of("arb-replace-h1").is_none(),
            "old claims released"
        );
        assert!(arbiter.owner_of("arb-replace-h2").is_none());
        assert_eq!(reg.len(), 1);
        assert_eq!(reg.active_rrid(), Some("SUSE:Maintenance:1:1"));
    }

    #[tokio::test]
    async fn add_or_replace_new_rrid_is_a_plain_insert() {
        let mut reg = registry();
        let removed = reg
            .add_or_replace(fake_report("SUSE:Maintenance:1:1", &["h1"], "ok"))
            .await;
        assert!(removed.failed.is_empty());
        assert!(removed.stragglers.is_empty());
        assert!(reg.contains("SUSE:Maintenance:1:1"));
    }

    #[tokio::test]
    async fn add_or_replace_ignores_null_sentinel() {
        let mut reg = registry();
        let removed = reg.add_or_replace(fake_report("", &["h1"], "ok")).await;
        assert!(removed.failed.is_empty());
        assert!(reg.is_empty());
    }

    /// `snapshot` clones the registry structure while *sharing* each entry's
    /// `Arc<Mutex<..>>` (same report lock) and preserving the stable id + active
    /// pointer — so a per-call fork acts on the same reports.
    #[test]
    fn snapshot_shares_entries_and_preserves_id_and_active() {
        let mut reg = registry();
        reg.add(fake_report("SUSE:Maintenance:1:1", &["h1"], "ok"));
        reg.add(fake_report("SUSE:Maintenance:2:2", &["h2"], "ok"));
        reg.set_active("SUSE:Maintenance:2:2");

        let snap = reg.snapshot();
        assert_eq!(snap.id(), reg.id(), "stable id preserved");
        assert_eq!(snap.active_rrid(), Some("SUSE:Maintenance:2:2"));
        assert_eq!(snap.rrids(), reg.rrids());
        // The entry handles are the *same* Arc (shared report lock).
        let a = reg.handle("SUSE:Maintenance:1:1").expect("orig entry");
        let b = snap.handle("SUSE:Maintenance:1:1").expect("snap entry");
        assert!(Arc::ptr_eq(&a, &b), "snapshot shares the entry Arc");
    }
}
