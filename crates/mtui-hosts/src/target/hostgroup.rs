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
//! This module ports the **container + fan-out** surface (P2.5) and the
//! **operation-lock + reboot lifecycle** (P2.9):
//!
//! * construction and [`select`](HostsGroup::select)ion of a host subset,
//! * [`names`](HostsGroup::names) / iteration,
//! * command fan-out via [`run`](HostsGroup::run) (delegating to
//!   [`super::actions::RunCommand`]),
//! * SFTP fan-out ([`sftp_put`](HostsGroup::sftp_put) /
//!   [`sftp_get`](HostsGroup::sftp_get) / [`sftp_remove`](HostsGroup::sftp_remove)),
//! * the operation-lock fan-out ([`lock`](HostsGroup::lock) /
//!   [`unlock`](HostsGroup::unlock) / [`update_lock`](HostsGroup::update_lock)),
//!   over the per-[`Target`] [`TargetLock`](super::TargetLock),
//! * the reboot/reconnect lifecycle ([`reboot`](HostsGroup::reboot) with boot-id
//!   verification and optional relock, plus the transactional-only `_reboot`
//!   path driven through the [`OperationGroup`] seam).
//!
//! The remaining upstream responsibilities are owned by later tasks and are
//! **intentionally not stubbed here** (stubs calling not-yet-built `Target`
//! methods would be dead code and could tempt a crate cycle):
//!
//! * the pool-claim lock (`pool_unlock`) — pool-claim wiring,
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
use std::sync::Arc;

use crate::error::{HostError, Result};

use super::Target;
use super::actions::{self, Command, RunCommand};
use super::operation::{HostCommandMap, HostPlan, LastOutput, OperationGroup, PlanProvider};

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
    /// The injected update-workflow doer/check resolver, or `None` before the
    /// composition root wires one in.
    ///
    /// Held as `mtui-hosts`' own [`PlanProvider`] (not the `mtui-testreport`
    /// registry directly) so the crate graph stays acyclic; `mtui-core::wiring`
    /// supplies the concrete adapter. When absent, [`OperationGroup::plans`]
    /// returns [`HostError::NoPlanProvider`] — the update workflow is unwired.
    plan_provider: Option<Arc<dyn PlanProvider>>,
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
        Self {
            data,
            interactive,
            plan_provider: None,
        }
    }

    /// Injects the update-workflow [`PlanProvider`] (builder-style), enabling the
    /// [`OperationGroup`] surface (install / uninstall).
    ///
    /// Called by the composition root (`mtui-core::wiring`) with the concrete
    /// adapter over the `mtui-testreport` doer/check registries. Without it,
    /// [`OperationGroup::plans`] returns [`HostError::NoPlanProvider`].
    #[must_use]
    pub fn with_plan_provider(mut self, provider: Arc<dyn PlanProvider>) -> Self {
        self.plan_provider = Some(provider);
        self
    }

    /// Sets (or replaces) the injected [`PlanProvider`] in place.
    pub fn set_plan_provider(&mut self, provider: Arc<dyn PlanProvider>) {
        self.plan_provider = Some(provider);
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
        let provider = self.plan_provider.clone();
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

        let mut group = HostsGroup::new(selected, interactive);
        group.plan_provider = provider;
        Ok(group)
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

    /// Locks every host in the group for `comment`, best-effort.
    ///
    /// Ports upstream `HostsGroup.lock`: each per-target [`Target::lock`] is
    /// attempted, and a [`HostError::TargetLocked`] from a foreign-owned host is
    /// suppressed (upstream wraps each call in `suppress(TargetLockedError)`) so
    /// one contended host never aborts the fan-out. Other transport errors are
    /// logged, not propagated.
    pub async fn lock(&mut self, comment: &str) {
        for target in self.data.values_mut() {
            match target.lock(comment).await {
                Ok(()) => {}
                Err(HostError::TargetLocked(msg)) => {
                    tracing::debug!(host = %target.hostname(), %msg, "lock: held by another owner, skipping");
                }
                Err(e) => {
                    tracing::warn!(host = %target.hostname(), error = %e, "lock failed");
                }
            }
        }
    }

    /// Releases every host's operation lock, best-effort.
    ///
    /// Ports upstream `HostsGroup.unlock`: delegates to the per-target
    /// [`Target::unlock`] (which already suppresses [`HostError::TargetLocked`]
    /// for a foreign lock), so a contended host never aborts the fan-out.
    pub async fn unlock(&mut self) {
        for target in self.data.values_mut() {
            target.unlock(false).await;
        }
    }

    /// Acquires the shared operation lock across every host in the group.
    ///
    /// Ports upstream `HostsGroup.update_lock`: for each host, if it is already
    /// locked by another owner, log a warning (with the lock's timestamp, owner
    /// and any comment) and mark the group as partially contended; otherwise
    /// take the lock. If any host was skipped, release the locks we did take
    /// (best-effort) and return [`HostError::Update`] so the caller aborts — the
    /// group is not fully owned by this process.
    ///
    /// # Errors
    ///
    /// Returns [`HostError::Update`] when one or more hosts were locked by
    /// another owner.
    pub async fn update_lock(&mut self) -> Result<()> {
        let names: Vec<String> = self.data.keys().cloned().collect();
        let mut skipped = false;
        for hostname in &names {
            let Some(target) = self.data.get_mut(hostname) else {
                continue;
            };
            // Load the lock (is_locked) before reading ownership; is_mine
            // requires a prior load and is order-sensitive.
            let locked = target.is_locked().await.unwrap_or(false);
            let foreign = locked
                && target
                    .lock_mut()
                    .is_some_and(|l| !l.is_mine().unwrap_or(false));
            if foreign {
                skipped = true;
                let lock = target.lock_mut().expect("foreign implies a built lock");
                let time = lock.time().await.unwrap_or_default();
                let by = lock.locked_by().await.unwrap_or_default();
                let comment = lock.comment().await.unwrap_or_default();
                tracing::warn!(
                    host = %hostname, since = %time, by = %by,
                    "host is locked; skipping"
                );
                if !comment.is_empty() {
                    tracing::info!(host = %hostname, %by, %comment, "lock comment");
                }
            } else {
                match target.lock("").await {
                    Ok(()) => {}
                    Err(HostError::TargetLocked(msg)) => {
                        tracing::debug!(host = %hostname, %msg, "update_lock: held by another owner");
                    }
                    Err(e) => {
                        tracing::warn!(host = %hostname, error = %e, "update_lock: lock failed");
                    }
                }
            }
        }

        if skipped {
            // Release the locks we did take, best-effort, then signal the abort.
            for target in self.data.values_mut() {
                target.unlock(false).await;
            }
            return Err(HostError::Update("Hosts locked".to_owned()));
        }
        Ok(())
    }

    /// Reboots every host in the group and reconnects, verifying the reboot.
    ///
    /// Ports upstream `HostsGroup.reboot`:
    ///
    /// * capture each host's [`boot_id`](Target::boot_id) *before* rebooting,
    /// * dispatch `command` fire-and-forget on every host (the reboot drops the
    ///   connection),
    /// * reconnect each host (sorted) with the connection's retry + backoff,
    /// * verify each host's boot id changed (see
    ///   [`verify_reboot`](Self::verify_reboot)),
    /// * if `relock_comment` is non-empty, re-apply the lock across the group —
    ///   a reboot clears `/var/lock` (tmpfs), so an active lock (e.g. a Product
    ///   Increment testing lock) must be re-asserted to survive.
    ///
    /// Works for both transactional and non-transactional hosts. A no-op when
    /// the group is empty.
    pub async fn reboot(&mut self, command: &str, relock_comment: &str) {
        if self.data.is_empty() {
            tracing::info!("No hosts to reboot");
            return;
        }
        let names: Vec<String> = self.data.keys().cloned().collect();
        tracing::info!(hosts = %names.join(", "), "Rebooting");

        // Record boot ids before rebooting so we can confirm a fresh boot after.
        let mut old_boot_ids: BTreeMap<String, String> = BTreeMap::new();
        for name in &names {
            if let Some(t) = self.data.get_mut(name) {
                old_boot_ids.insert(name.clone(), t.boot_id().await);
            }
        }

        for t in self.data.values_mut() {
            t.reboot(command).await;
        }
        for name in &names {
            if let Some(t) = self.data.get_mut(name) {
                if let Err(e) = t.reconnect().await {
                    tracing::error!(host = %name, error = %e, "reconnect after reboot failed");
                } else {
                    tracing::info!(host = %name, "is back up");
                }
            }
        }

        for name in &names {
            let old = old_boot_ids.get(name).cloned().unwrap_or_default();
            self.verify_reboot(name, &old).await;
        }

        if !relock_comment.is_empty() {
            tracing::info!("Re-applying lock after reboot");
            self.lock(relock_comment).await;
        }
    }

    /// Reboots the *transactional* hosts named in `reboot` and reconnects each.
    ///
    /// Ports upstream `HostsGroup._reboot`: transactional hosts contribute a
    /// per-host reboot command from the operation's doer. Each is dispatched
    /// fire-and-forget, then reconnected (sorted) with the connection's retry +
    /// backoff. Unlike [`reboot`](Self::reboot) this path takes no boot-id
    /// snapshot / verification (upstream's `_reboot` does not), and is a no-op
    /// when the map is empty.
    async fn reboot_transactional(&mut self, reboot: &BTreeMap<String, String>) {
        if reboot.is_empty() {
            return;
        }
        let mut names: Vec<&String> = reboot.keys().collect();
        names.sort();
        tracing::info!(
            hosts = %names.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", "),
            "Rebooting transactional hosts"
        );
        // Fire the reboot on every named host first (it drops the connection),
        // then reconnect each once it is back up.
        for (hostname, command) in reboot {
            if let Some(t) = self.data.get_mut(hostname) {
                t.reboot(command).await;
            }
        }
        for hostname in names {
            if let Some(t) = self.data.get_mut(hostname) {
                if let Err(e) = t.reconnect().await {
                    tracing::error!(host = %hostname, error = %e, "reconnect after reboot failed");
                } else {
                    tracing::info!(host = %hostname, "is back up");
                }
            }
        }
    }

    /// Logs an error if `hostname`'s boot id did not change after a reboot.
    ///
    /// Ports upstream `HostsGroup._verify_reboot`.
    /// `/proc/sys/kernel/random/boot_id` is regenerated on every boot, so an
    /// unchanged value means the host did not actually reboot. A missing (empty)
    /// old or new id is a warning (could not confirm); an unchanged non-empty id
    /// is an error.
    async fn verify_reboot(&mut self, hostname: &str, old_boot_id: &str) {
        let new_boot_id = match self.data.get_mut(hostname) {
            Some(t) => t.boot_id().await,
            None => return,
        };
        if old_boot_id.is_empty() || new_boot_id.is_empty() {
            tracing::warn!(host = %hostname, "could not read boot id to confirm the reboot");
        } else if old_boot_id == new_boot_id {
            tracing::error!(
                host = %hostname, boot_id = %new_boot_id,
                "boot id unchanged after reboot -- the host may not have rebooted"
            );
        }
    }
}

/// Drives the install/uninstall [`Operation`](super::operation::Operation)
/// template against this group.
///
/// This is the composition-root binding deferred by the `TODO` in
/// [`operation`](super::operation): it resolves each target's per-role
/// [`Doer`](super::operation::Doer) / [`Check`](super::operation::Check) through
/// the injected [`PlanProvider`], and delegates command/reboot fan-out to
/// [`HostsGroup::run`] via a [`Command::PerHost`] map.
///
/// ## Lock + reboot lifecycle
///
/// `update_lock` / `unlock` / `reboot` delegate to the inherent
/// [`HostsGroup`] methods, which fan the per-host operation lock
/// (`/var/lock/mtui.lock`) and the reboot/reconnect lifecycle out across the
/// group over each [`Target`]'s connect-time
/// [`TargetLock`](super::TargetLock). `update_lock` returns
/// [`HostError::Update`] if any host is locked by another owner (after
/// releasing the locks it took), which the template surfaces to abort the run.
#[async_trait::async_trait]
impl OperationGroup for HostsGroup {
    fn plans(&mut self, role: &str) -> std::result::Result<Vec<HostPlan>, HostError> {
        let provider = self
            .plan_provider
            .as_ref()
            .ok_or(HostError::NoPlanProvider)?
            .clone();

        let mut plans = Vec::with_capacity(self.data.len());
        for target in self.data.values() {
            // Upstream keys the registry lookup on
            // `(self.system.get_release(), self.transactional)`. An unknown /
            // unparsed system has no release, which upstream surfaces as the
            // role's Missing*Error (no doer for an empty key) — reproduce that.
            let release = target.system().get_release().map_err(|_| match role {
                "uninstaller" => HostError::MissingUninstaller {
                    release: String::new(),
                },
                "updater" => HostError::MissingUpdater {
                    release: String::new(),
                },
                "preparer" => HostError::MissingPreparer {
                    release: String::new(),
                },
                "downgrader" => HostError::MissingDowngrader {
                    release: String::new(),
                },
                _ => HostError::MissingInstaller {
                    release: String::new(),
                },
            })?;
            let transactional = target.transactional();
            let doer = provider.doer(role, &release, transactional)?;
            let check = provider.check(role, &release, transactional);
            plans.push(HostPlan {
                hostname: target.hostname().to_owned(),
                transactional,
                doer,
                check,
            });
        }
        Ok(plans)
    }

    async fn update_lock(&mut self) -> Result<()> {
        HostsGroup::update_lock(self).await
    }

    async fn run(&mut self, commands: HostCommandMap) {
        let map: BTreeMap<String, String> = commands.into_iter().collect();
        HostsGroup::run(self, Command::PerHost(map)).await;
    }

    async fn reboot(&mut self, reboot: HostCommandMap) {
        // Only transactional hosts contribute reboot entries; upstream's
        // `_reboot` fires the reboot fire-and-forget on each, then reconnects
        // each (sorted) with the connection's retry + backoff.
        let map: BTreeMap<String, String> = reboot.into_iter().collect();
        HostsGroup::reboot_transactional(self, &map).await;
    }

    async fn unlock(&mut self) {
        HostsGroup::unlock(self).await;
    }

    fn last_output(&self, hostname: &str) -> LastOutput {
        match self.data.get(hostname) {
            Some(t) => LastOutput {
                lastout: t.lastout().to_owned(),
                lastin: t.lastin().to_owned(),
                lasterr: t.lasterr().to_owned(),
                lastexit: t.lastexit(),
            },
            None => LastOutput::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use mtui_types::enums::{ExecutionMode, TargetState};
    use mtui_types::hostlog::CommandLog;

    use super::*;
    use crate::connection::MockConnection;
    use crate::target::TARGET_LOCK_PATH;

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

    // --- reboot lifecycle (P2.9) -------------------------------------------

    /// A mock that answers the boot-id probe with `boot_id` and everything else
    /// with "ok", so a group reboot can capture/verify a boot id.
    fn reboot_mock(hostname: &str, boot_id: &str) -> MockConnection {
        MockConnection::new(hostname)
            .with_default(CommandLog::new("", "ok", "", 0, 0))
            .with_response(
                "cat /proc/sys/kernel/random/boot_id",
                CommandLog::new("cat /proc/sys/kernel/random/boot_id", boot_id, "", 0, 0),
            )
    }

    #[tokio::test]
    async fn reboot_fires_reconnects_and_verifies_all_hosts() {
        let (m1, m2) = (reboot_mock("h1", "id-1"), reboot_mock("h2", "id-2"));
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
                    ExecutionMode::Parallel,
                    Box::new(m2),
                ),
            ],
            false,
        );

        g.reboot("systemctl reboot", "").await;

        // Each host was sent the reboot fire-and-forget and reconnected once.
        assert_eq!(h1.fired_commands(), vec!["systemctl reboot".to_owned()]);
        assert_eq!(h2.fired_commands(), vec!["systemctl reboot".to_owned()]);
        assert_eq!(h1.reconnect_count(), 1);
        assert_eq!(h2.reconnect_count(), 1);
    }

    #[tokio::test]
    async fn reboot_relocks_when_comment_given() {
        let m1 = reboot_mock("h1", "id-1");
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

        g.reboot("systemctl reboot", "PI testing").await;

        // The relock wrote a lock file: the host now reports locked.
        assert!(g.get_mut("h1").unwrap().is_locked().await.unwrap());
        assert_eq!(h1.reconnect_count(), 1);
    }

    #[tokio::test]
    async fn reboot_empty_group_is_noop() {
        let mut g = HostsGroup::new(vec![], false);
        // Must not panic; nothing to reboot.
        g.reboot("systemctl reboot", "relock").await;
        assert!(g.is_empty());
    }

    #[tokio::test]
    async fn reboot_verify_unchanged_boot_id_does_not_panic() {
        // Same boot id on both reads models a host that did NOT reboot; the
        // verify path logs an error but the group call still completes.
        let m1 = reboot_mock("h1", "same-id");
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
        g.reboot("systemctl reboot", "").await;
        assert_eq!(h1.reconnect_count(), 1);
    }

    #[tokio::test]
    async fn reboot_verify_missing_boot_id_does_not_panic() {
        // boot-id probe times out -> empty id -> "could not confirm" warning
        // branch; the reboot still fires and reconnects.
        let m1 = MockConnection::new("h1")
            .with_default(CommandLog::new("", "ok", "", 0, 0))
            .with_timeout("cat /proc/sys/kernel/random/boot_id");
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
        g.reboot("systemctl reboot", "").await;
        assert_eq!(h1.reconnect_count(), 1);
        assert_eq!(h1.fired_commands(), vec!["systemctl reboot".to_owned()]);
    }

    #[tokio::test]
    async fn reboot_reports_reconnect_failure_without_panicking() {
        let m1 = reboot_mock("h1", "id-1");
        // Recreate with a failing reconnect.
        let m1 = m1.failing_reconnect();
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
        g.reboot("systemctl reboot", "").await;
        assert_eq!(h1.reconnect_count(), 1);
    }

    // --- _reboot (transactional subset, via OperationGroup) -----------------

    #[tokio::test]
    async fn operation_reboot_fires_and_reconnects_named_hosts() {
        let (m1, m2) = (reboot_mock("h1", "id-1"), reboot_mock("h2", "id-2"));
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
                    ExecutionMode::Parallel,
                    Box::new(m2),
                ),
            ],
            false,
        );

        // Only h1 is transactional -> only it appears in the reboot map.
        let map: HostCommandMap = vec![("h1".to_owned(), "transactional-update reboot".to_owned())];
        OperationGroup::reboot(&mut g, map).await;

        assert_eq!(
            h1.fired_commands(),
            vec!["transactional-update reboot".to_owned()]
        );
        assert_eq!(h1.reconnect_count(), 1);
        // h2 was not in the map: untouched.
        assert!(h2.fired_commands().is_empty());
        assert_eq!(h2.reconnect_count(), 0);
    }

    #[tokio::test]
    async fn operation_reboot_empty_map_is_noop() {
        let m1 = reboot_mock("h1", "id-1");
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
        OperationGroup::reboot(&mut g, Vec::new()).await;
        assert!(h1.fired_commands().is_empty());
        assert_eq!(h1.reconnect_count(), 0);
    }

    // --- update_lock / lock / unlock fan-out (P2.9) -------------------------

    #[tokio::test]
    async fn update_lock_locks_all_free_hosts() {
        let mut g = HostsGroup::new(vec![enabled("h1"), enabled("h2")], false);
        g.update_lock().await.expect("all free -> ok");
        // Both hosts now report locked.
        assert!(g.get_mut("h1").unwrap().is_locked().await.unwrap());
        assert!(g.get_mut("h2").unwrap().is_locked().await.unwrap());
    }

    #[tokio::test]
    async fn update_lock_errors_and_releases_when_a_host_is_foreign_locked() {
        // h2 carries a foreign lock (different user+pid) -> skipped; the whole
        // group aborts and the lock h1 took is released.
        let foreign = MockConnection::new("h2")
            .with_default(CommandLog::new("", "ok", "", 0, 0))
            .with_file(TARGET_LOCK_PATH, b"1700000000:alice:4242:busy".to_vec());
        let mut g = HostsGroup::new(
            vec![
                enabled("h1"),
                Target::with_connection(
                    "h2",
                    TargetState::Enabled,
                    ExecutionMode::Parallel,
                    Box::new(foreign),
                ),
            ],
            false,
        );

        let err = g.update_lock().await.expect_err("foreign lock -> abort");
        assert!(matches!(err, HostError::Update(_)));
        // h1's lock was released during the abort.
        assert!(!g.get_mut("h1").unwrap().is_locked().await.unwrap());
    }

    #[tokio::test]
    async fn lock_and_unlock_fan_out_over_group() {
        let mut g = HostsGroup::new(vec![enabled("h1"), enabled("h2")], false);
        g.lock("session").await;
        assert!(g.get_mut("h1").unwrap().is_locked().await.unwrap());
        assert!(g.get_mut("h2").unwrap().is_locked().await.unwrap());

        g.unlock().await;
        assert!(!g.get_mut("h1").unwrap().is_locked().await.unwrap());
        assert!(!g.get_mut("h2").unwrap().is_locked().await.unwrap());
    }

    #[tokio::test]
    async fn unlock_suppresses_foreign_lock_and_continues() {
        // h1 is ours (locked below), h2 is foreign: unlock must skip h2 without
        // aborting and still release h1.
        let foreign = MockConnection::new("h2")
            .with_default(CommandLog::new("", "ok", "", 0, 0))
            .with_file(TARGET_LOCK_PATH, b"1700000000:alice:4242:busy".to_vec());
        let mut g = HostsGroup::new(
            vec![
                enabled("h1"),
                Target::with_connection(
                    "h2",
                    TargetState::Enabled,
                    ExecutionMode::Parallel,
                    Box::new(foreign),
                ),
            ],
            false,
        );
        g.lock("session").await; // locks h1; h2 stays foreign-locked
        g.unlock().await;
        assert!(!g.get_mut("h1").unwrap().is_locked().await.unwrap());
        // h2's foreign lock is still present (unlock suppressed the failure).
        assert!(g.get_mut("h2").unwrap().is_locked().await.unwrap());
    }
}
