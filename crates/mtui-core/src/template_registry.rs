//! The collection of loaded templates plus an active pointer.
//!
//! Port of upstream `mtui.template_registry.TemplateRegistry`. It replaces the
//! historical scalar `metadata` / `targets` state with a keyed collection of
//! [`TestReport`] instances and an "active" pointer, keyed by RRID
//! (`report.id()`). A single [`NullReport`] is held as the fallback so
//! [`active`](TemplateRegistry::active) never returns `None` and is never
//! inserted into the keyed collection (its id is the empty string).
//!
//! A stable per-instance [`id`](TemplateRegistry::id) is established here; it is
//! the owner-key seed the host-arbitration work keys on as `(registry.id, RRID)`
//! (RFC §5.7). One registry per REPL process, one per MCP session.
//!
//! ## Rust deviation: teardown
//!
//! Upstream `remove()` calls `target.close()` on each host, suppressing
//! exceptions. Dropping a removed report drops its
//! [`HostsGroup`](mtui_hosts::HostsGroup) and every `Target`, which closes the
//! transport on `Drop` — but that cannot release the report's *async* ownership:
//! the in-process arbiter claim, the remote pool-claim lock
//! (`/var/lock/mtui-pool.lock`), and the remote operation lock
//! (`/var/lock/mtui.lock`) all need an `await`, and a wedged host must not hang
//! removal. So [`remove`](TemplateRegistry::remove) and the same-RRID
//! replacement path in [`add_or_replace`](TemplateRegistry::add_or_replace) are
//! **async** and run the same bounded teardown as the REPL `quit` command
//! (release pool claims, then `targets.close(None)` under a per-report budget)
//! before the entry is dropped and the active pointer is repointed.

use std::time::Duration;

use indexmap::IndexMap;
use mtui_config::Config;
use mtui_hosts::get_arbiter;
use mtui_testreport::{NullReport, TestReport};

/// Wall-clock budget for one removed report's host-close fan-out (upstream
/// `DISCONNECT_TIMEOUT_SECONDS = 45.0` / `quit`'s `CLOSE_TIMEOUT`); removal must
/// still complete if a host hangs during teardown.
const REMOVE_CLOSE_TIMEOUT: Duration = Duration::from_secs(45);

/// Resolves the per-report close budget. Overridable in tests (via
/// [`tests::set_close_timeout`]) so the wedged-host path can be exercised without
/// waiting the full 45s; always [`REMOVE_CLOSE_TIMEOUT`] in production.
#[cfg(not(test))]
fn remove_close_timeout() -> Duration {
    REMOVE_CLOSE_TIMEOUT
}
#[cfg(test)]
fn remove_close_timeout() -> Duration {
    tests::close_timeout_override()
}

/// Per-host teardown outcomes from removing (or replacing) a report.
///
/// Best-effort diagnostics for the caller to log; removal always completes
/// regardless of what is reported here (mirroring `quit`'s per-host logging).
#[derive(Debug, Default)]
pub struct RemoveReport {
    /// Hosts that failed to disconnect, as `(hostname, error)` pairs (upstream
    /// `failed to disconnect from <host>: <err>`).
    pub failed: Vec<(String, String)>,
    /// Hosts still disconnecting when the close budget expired (upstream
    /// `still disconnecting from <host> after <secs> seconds`).
    pub stragglers: Vec<String>,
}

/// Holds the loaded templates and tracks the active one.
pub struct TemplateRegistry {
    /// Loaded reports keyed by RRID, in insertion order (for fan-out order).
    entries: IndexMap<String, Box<dyn TestReport + Send + Sync>>,
    /// The active RRID, or `None` when nothing is loaded.
    active: Option<String>,
    /// The null-object fallback returned by [`active`](Self::active) when empty.
    null: Box<dyn TestReport + Send + Sync>,
    /// Stable per-registry identity; the owner-key seed for host arbitration.
    id: String,
}

impl TemplateRegistry {
    /// Builds an empty registry with a fresh [`NullReport`] fallback.
    ///
    /// `config` is retained for parity with the runtime and future arbiter
    /// wiring; the null report is built from it, matching upstream's
    /// `null_factory`.
    #[must_use]
    pub fn new(config: Config) -> Self {
        Self::with_null(Box::new(NullReport::new(config)))
    }

    /// Builds an empty registry with an explicit null-object fallback.
    ///
    /// Test seam mirroring upstream's injectable `null_factory`.
    #[must_use]
    pub fn with_null(null: Box<dyn TestReport + Send + Sync>) -> Self {
        Self {
            entries: IndexMap::new(),
            active: None,
            null,
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
    /// replaces the stored report but leaves the active pointer alone. A report
    /// with an empty RRID is the failed-load sentinel ([`NullReport`]) and is
    /// silently ignored so it never becomes a phantom entry that breaks fan-out.
    pub fn add(&mut self, mut report: Box<dyn TestReport + Send + Sync>) {
        let rrid = report.id();
        if rrid.is_empty() {
            return;
        }
        // Wire the process-global host arbiter and this report's `(registry_id,
        // RRID)` owner key before it is stored, mirroring upstream
        // `TemplateRegistry.add` (`report._arbiter = self.arbiter`, `_owner =
        // (self.id, rrid)`). With both set, `refhosts_from_tp`/autoconnect take
        // the pool-selection path (one host per slot) instead of connecting
        // every candidate.
        {
            let base = report.base_mut();
            base.arbiter = Some(get_arbiter());
            base.owner = Some((self.id.clone(), rrid.clone()));
        }
        let is_new = !self.entries.contains_key(&rrid);
        self.entries.insert(rrid.clone(), report);
        if is_new && self.active.is_none() {
            self.active = Some(rrid);
        }
    }

    /// Inserts `report`, first tearing down any report already loaded under the
    /// same RRID.
    ///
    /// The same-RRID replacement path (upstream `load` overwriting a template):
    /// re-adding an existing RRID must release the *old* report's async ownership
    /// (arbiter claim + remote pool/operation locks) and close its hosts before
    /// the new report is stored, otherwise the replaced report leaks its locks
    /// and connections. A brand-new RRID is a plain [`add`](Self::add) with no
    /// teardown. Returns any per-host teardown failures observed while removing
    /// the old report (empty for a new insert); the caller may log them
    /// best-effort. The empty-RRID null sentinel is ignored exactly as by
    /// [`add`](Self::add).
    pub async fn add_or_replace(
        &mut self,
        report: Box<dyn TestReport + Send + Sync>,
    ) -> RemoveReport {
        let rrid = report.id();
        if rrid.is_empty() {
            return RemoveReport::default();
        }
        // Tear the previous occupant down (releasing its claims/locks/connections)
        // before it is dropped by the insert below.
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
    /// The bounded teardown the REPL `quit` runs, applied to a single removed
    /// report: [`release_pool_claims`](mtui_testreport::TestReport::release_pool_claims)
    /// (in-process arbiter ownership + remote pool-claim lock) then
    /// [`HostsGroup::close`](mtui_hosts::HostsGroup)`(None)` (per-host operation
    /// lock + graceful disconnect) under [`remove_close_timeout`]. Only after the
    /// teardown returns is the entry dropped and — if it was active — the active
    /// pointer repointed to the next remaining entry (insertion order), so a
    /// reader never observes a half-torn-down active report. A no-op if `rrid` is
    /// absent (upstream raises `KeyError`; the callers already gate on
    /// membership). Returns per-host teardown failures and any straggler names
    /// for best-effort logging.
    pub async fn remove(&mut self, rrid: &str) -> RemoveReport {
        if !self.entries.contains_key(rrid) {
            return RemoveReport::default();
        }
        self.teardown(rrid).await
    }

    /// Shared teardown body for [`remove`](Self::remove) and
    /// [`add_or_replace`](Self::add_or_replace). Assumes `rrid` is loaded.
    async fn teardown(&mut self, rrid: &str) -> RemoveReport {
        let mut result = RemoveReport::default();
        let timeout = remove_close_timeout();
        if let Some(report) = self.entries.get_mut(rrid) {
            // Release arbiter ownership + remote pool locks before disconnecting
            // (best-effort; a no-op without pooling).
            report.release_pool_claims().await;
            // Snapshot hostnames so a straggler (the whole close exceeding the
            // budget) can still be named per host.
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
        }
        // Ownership released: now drop the entry and repoint active.
        self.entries.shift_remove(rrid);
        if self.active.as_deref() == Some(rrid) {
            self.active = self.entries.keys().next().cloned();
        }
        result
    }

    /// Returns the loaded report for `rrid`, or `None` if absent.
    #[must_use]
    pub fn get(&self, rrid: &str) -> Option<&(dyn TestReport + Send + Sync)> {
        self.entries.get(rrid).map(|b| &**b)
    }

    /// Mutably returns the loaded report for `rrid`, or `None` if absent.
    pub fn get_mut(&mut self, rrid: &str) -> Option<&mut Box<dyn TestReport + Send + Sync>> {
        self.entries.get_mut(rrid)
    }

    /// The active report, or the [`NullReport`] fallback when nothing is loaded.
    #[must_use]
    pub fn active(&self) -> &(dyn TestReport + Send + Sync) {
        match &self.active {
            Some(rrid) => &*self.entries[rrid],
            None => &*self.null,
        }
    }

    /// Mutably borrows the active report, or the null fallback.
    pub fn active_mut(&mut self) -> &mut Box<dyn TestReport + Send + Sync> {
        match &self.active {
            Some(rrid) => &mut self.entries[rrid],
            None => &mut self.null,
        }
    }

    /// The active RRID, or `None` when nothing is loaded.
    #[must_use]
    pub fn active_rrid(&self) -> Option<&str> {
        self.active.as_deref()
    }

    /// Makes `rrid` the active template. Returns `false` if `rrid` is not loaded
    /// (upstream raises `KeyError`).
    pub fn set_active(&mut self, rrid: &str) -> bool {
        if self.entries.contains_key(rrid) {
            self.active = Some(rrid.to_owned());
            true
        } else {
            false
        }
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
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use mtui_hosts::{MockConnection, Owner, Target};
    use mtui_types::enums::{ExecutionMode, TargetState};
    use mtui_types::hostlog::CommandLog;

    use super::*;
    use crate::commands::testkit::{fake_report, fake_report_from_base};

    /// Test-only override for [`remove_close_timeout`], in milliseconds.
    /// `u64::MAX` means "use the production [`REMOVE_CLOSE_TIMEOUT`]". Serialised
    /// by [`CLOSE_TIMEOUT_LOCK`] so a shrunk budget never leaks into a concurrent
    /// test (the whole integration suite shares one process).
    static CLOSE_TIMEOUT_MS: AtomicU64 = AtomicU64::new(u64::MAX);
    static CLOSE_TIMEOUT_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    pub(super) fn close_timeout_override() -> Duration {
        match CLOSE_TIMEOUT_MS.load(Ordering::SeqCst) {
            u64::MAX => REMOVE_CLOSE_TIMEOUT,
            ms => Duration::from_millis(ms),
        }
    }

    fn registry() -> TemplateRegistry {
        TemplateRegistry::new(Config::default())
    }

    /// Builds a target whose mock connection is scripted with `build`.
    fn target_with(host: &str, build: impl FnOnce(MockConnection) -> MockConnection) -> Target {
        let conn = build(MockConnection::new(host));
        Target::with_connection(
            host,
            TargetState::Enabled,
            ExecutionMode::Parallel,
            Box::new(conn),
        )
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

    /// Claims `hosts` for the report loaded under `rrid` through the process-global
    /// arbiter, matching the owner key `add` assigned (`(registry.id, rrid)`), and
    /// records them as the report's in-process pool claims. Returns the owner so
    /// callers can assert release afterwards. Uses unique hostnames per test so the
    /// process-global arbiter (shared across the consolidated test binary) does not
    /// cross-contaminate.
    fn claim_hosts(reg: &mut TemplateRegistry, rrid: &str, hosts: &[&str]) -> Owner {
        let owner: Owner = (reg.id().to_owned(), rrid.to_owned());
        let arbiter = mtui_hosts::get_arbiter();
        let report = reg.get_mut(rrid).expect("report loaded");
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
        // Claim two hosts through the process-global arbiter under the owner key
        // `add` assigned, then remove and assert release. Unique hostnames.
        claim_hosts(
            &mut reg,
            "SUSE:Maintenance:1:1",
            &["arb-remove-h1", "arb-remove-h2"],
        );
        let arbiter = mtui_hosts::get_arbiter();

        reg.remove("SUSE:Maintenance:1:1").await;

        // Arbiter ownership is dropped for every previously-claimed host.
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

        // Re-load the same RRID: the old report's claims are released before the
        // new report is stored.
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
}
