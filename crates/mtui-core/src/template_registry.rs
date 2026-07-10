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
//! exceptions. In Rust a [`Target`](mtui_hosts::Target) owns its live connection
//! and tears it down on `Drop`; dropping the removed report drops its
//! [`HostsGroup`](mtui_hosts::HostsGroup) and every `Target` within, so
//! `remove()` needs only to drop the entry. Pool-claim release (upstream
//! `release_claims`) is deferred to the report lifecycle task.

use indexmap::IndexMap;
use mtui_config::Config;
use mtui_hosts::get_arbiter;
use mtui_testreport::{NullReport, TestReport};

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

    /// Drops `rrid` from the registry, tearing down its host connections.
    ///
    /// If the removed template was active, the next remaining entry (insertion
    /// order) becomes active, or `None` when the registry empties. A no-op if
    /// `rrid` is absent (upstream raises `KeyError`; we degrade to a no-op since
    /// the only callers already gate on membership).
    pub fn remove(&mut self, rrid: &str) {
        // Dropping the entry drops its HostsGroup and every Target, which closes
        // the live connections on Drop (the Rust analogue of upstream's
        // best-effort `target.close()` loop).
        if self.entries.shift_remove(rrid).is_none() {
            return;
        }
        if self.active.as_deref() == Some(rrid) {
            self.active = self.entries.keys().next().cloned();
        }
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
