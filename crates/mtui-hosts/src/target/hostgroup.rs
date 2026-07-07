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
//! * construction and [`select`](HostsGroup::select)ion of a host subset (plus
//!   the non-consuming [`select_split`](HostsGroup::select_split) +
//!   [`merge`](HostsGroup::merge) pair that lets a `-t` subset operation preserve
//!   the unselected hosts, standing in for upstream's shared-reference dict),
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
use super::repo_manager::{RepoOp, SetRepo};

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

    /// Splits the group into the `-t` selection and the unselected remainder,
    /// preserving both halves.
    ///
    /// This is the non-consuming counterpart to [`select`](Self::select) used by
    /// the `perform_*` / `set_repo` drivers: because a `Target` owns a live
    /// connection and cannot be shared, a plain `select` moves the subset out and
    /// **drops** the rest. `select_split` instead partitions the group in one pass
    /// so the caller can run the operation over the selected subset and then
    /// [`merge`](Self::merge) the remainder back — the Rust stand-in for
    /// upstream's shared-reference parent dict, where the unselected hosts always
    /// survive the operation.
    ///
    /// Selection semantics match [`select`](Self::select):
    ///
    /// * `hosts = None` → every host (or, when `enabled`, only the non-disabled)
    ///   is *selected*; the remainder holds whatever the `enabled` filter drops.
    /// * `hosts = Some([..])` → exactly those hosts are selected (and, when
    ///   `enabled`, only the non-disabled among them); every other host — plus any
    ///   named-but-disabled host filtered out by `enabled` — lands in the
    ///   remainder.
    ///
    /// An unknown hostname is a [`HostError::NotConnected`], as in
    /// [`select`](Self::select). Both returned groups inherit `interactive` and
    /// the injected [`PlanProvider`].
    ///
    /// # Errors
    ///
    /// [`HostError::NotConnected`] when a named host is not a member of the group.
    pub fn select_split(
        self,
        hosts: Option<&[String]>,
        enabled: bool,
    ) -> Result<(HostsGroup, HostsGroup)> {
        let interactive = self.interactive;
        let provider = self.plan_provider.clone();
        let is_enabled = |t: &Target| t.state() != mtui_types::enums::TargetState::Disabled;

        if let Some(names) = hosts {
            for name in names {
                if !self.data.contains_key(name) {
                    return Err(HostError::NotConnected { host: name.clone() });
                }
            }
        }

        let mut selected: Vec<Target> = Vec::new();
        let mut remainder: Vec<Target> = Vec::new();
        for (hn, t) in self.data {
            let named = hosts.is_none_or(|names| names.contains(&hn));
            if named && (!enabled || is_enabled(&t)) {
                selected.push(t);
            } else {
                remainder.push(t);
            }
        }

        let mut selected = HostsGroup::new(selected, interactive);
        selected.plan_provider = provider.clone();
        let mut remainder = HostsGroup::new(remainder, interactive);
        remainder.plan_provider = provider;
        Ok((selected, remainder))
    }

    /// Folds every target of `other` into `self`, keyed by hostname.
    ///
    /// The group-extend counterpart to [`select_split`](Self::select_split): after
    /// a subset operation the driver merges the untouched remainder back so the
    /// live report regains its unselected hosts. Delegates to [`add`](Self::add),
    /// so a hostname present in both is last-writer-wins (the `other` target
    /// replaces `self`'s). Callers pass disjoint halves, so no collision occurs in
    /// practice.
    pub fn merge(&mut self, other: HostsGroup) {
        for t in other.data.into_values() {
            self.add(t);
        }
    }

    /// Adds (or replaces) a target in the group, keyed by its hostname.
    ///
    /// Ports the container half of upstream `add_host`/`HostsGroup.__setitem__`:
    /// a target with a hostname already present replaces the existing entry
    /// (upstream's dict assignment is last-writer-wins). The connection-building
    /// and refhosts-from-testplatform autoconnect stay in the `add_host` command
    /// / composition root; this is purely the container mutation.
    pub fn add(&mut self, target: Target) {
        self.data.insert(target.hostname().to_owned(), target);
    }

    /// Removes and returns the target named `hostname`, if present.
    ///
    /// Ports the container half of upstream `remove_host`/`HostsGroup.__delitem__`
    /// (upstream disconnects the target and purges its log before dropping the
    /// dict entry; here dropping the returned [`Target`] closes its owned
    /// connection). `None` when no such host is in the group.
    pub fn remove(&mut self, hostname: &str) -> Option<Target> {
        self.data.remove(hostname)
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

    /// Releases every host's pool claim, best-effort.
    ///
    /// Ports upstream `HostsGroup.pool_unlock`: delegates to the per-target
    /// [`Target::pool_unlock`] (which suppresses [`HostError::TargetLocked`] for
    /// a claim owned by another template), so one contended host never aborts
    /// the fan-out. `force` removes claims owned by other templates too.
    pub async fn pool_unlock(&mut self, force: bool) {
        for target in self.data.values_mut() {
            target.pool_unlock(force).await;
        }
    }

    /// Stamps the owning template's RRID onto every host in the group.
    ///
    /// The single push-down point for pool-claim ownership identity: the report
    /// layer calls this after attaching its targets so each [`Target`]'s
    /// [`PoolLock`](crate::PoolLock) adopts the RRID (upstream builds each
    /// `Target` with `_rrid` directly; here the group is built before the owning
    /// report's RRID is known, so it is pushed down).
    pub fn set_rrid(&mut self, rrid: impl Into<String>) {
        let rrid = rrid.into();
        for target in self.data.values_mut() {
            target.set_rrid(rrid.clone());
        }
    }

    /// Fans a repository add/remove out across every host in the group.
    ///
    /// Ports upstream `HostsGroup._fanout_set_repo`, which runs
    /// `t.repo_manager.set(operation, testreport)` on every host. `report` is the
    /// object-safe [`SetRepo`] hook the composition root supplies (the concrete
    /// `SlReport`/`ObsReport`/… `set_repo` impl in `mtui-testreport`), so the
    /// group never depends on the report crate.
    ///
    /// Upstream fans these out with `run_parallel`; because [`SetRepo::set_repo`]
    /// takes `&mut Target` and repo add/remove is order-independent, this drives
    /// them sequentially — the observable per-host `zypper` effect is identical.
    /// The per-host `last*` state is left in place so a caller can inspect
    /// `lasterr()` after the fan-out (upstream's prepare abort-on-`lasterr`).
    pub async fn fanout_set_repo(&mut self, operation: RepoOp, report: &dyn SetRepo) {
        for target in self.data.values_mut() {
            target.repo_manager().set(operation, report).await;
        }
    }

    /// Queries every host's tracked package versions and logs update-sanity
    /// warnings.
    ///
    /// Ports the `package_check` closure nested in upstream
    /// `HostsGroup.perform_update`. Each host's [`Target::query_versions`] runs
    /// first, populating each package's `current` version; then, per package:
    ///
    /// * **pre** (`post == false`): record `current` as the package's `before`
    ///   version.
    /// * **post** (`post == true`): record `current` as the package's `after`
    ///   version (leaving `before` as captured on the pre-pass).
    ///
    /// and emit the four upstream warnings:
    ///
    /// * *too recent* — pre-update installed version is already `>=` the required
    ///   version,
    /// * *not updated* — `after == before` on the post-pass,
    /// * *below required* — post-update version is `<` the required version,
    /// * *missing* — the package is not installed (`before` is `None`), collected
    ///   and logged once per host.
    ///
    /// The packages must already be [`seeded`](Target::set_packages) with their
    /// `required` versions.
    pub async fn package_check(&mut self, post: bool) {
        for target in self.data.values_mut() {
            let hostname = target.hostname().to_owned();
            target.query_versions().await;

            let mut not_installed: Vec<String> = Vec::new();
            for pkg in target.packages_mut() {
                let required = pkg.required().cloned();
                let current = pkg.current().cloned();
                if post {
                    pkg.set_after_version(current.clone());
                } else {
                    pkg.set_before_version(current.clone());
                }
                let before = pkg.before().cloned();
                let after = if post { pkg.after().cloned() } else { None };

                match &before {
                    None => not_installed.push(pkg.name.clone()),
                    Some(before_v) => {
                        if let Some(req) = &required
                            && before_v >= req
                        {
                            tracing::warn!(
                                host = %hostname, package = %pkg.name,
                                installed = %before_v, required = %req,
                                "package is too recent"
                            );
                        }
                    }
                }

                if let (Some(a), Some(b)) = (&after, &before)
                    && a == b
                {
                    tracing::warn!(
                        host = %hostname, package = %pkg.name, version = %a,
                        "package was not updated"
                    );
                }
                if let (Some(a), Some(req)) = (&after, &required)
                    && a < req
                {
                    tracing::warn!(
                        host = %hostname, package = %pkg.name,
                        installed = %a, required = %req,
                        "package does not match required version"
                    );
                }
            }

            if !not_installed.is_empty() {
                tracing::warn!(
                    host = %hostname, packages = %not_installed.join(", "),
                    "these packages are missing"
                );
            }
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

    // --- add / remove ------------------------------------------------------

    #[test]
    fn add_inserts_keyed_by_hostname() {
        let mut g = HostsGroup::new(vec![enabled("h1")], true);
        g.add(enabled("h2"));
        assert_eq!(g.names(), vec!["h1".to_owned(), "h2".to_owned()]);
        assert!(g.contains("h2"));
    }

    #[test]
    fn add_same_hostname_replaces_last_writer_wins() {
        let mut g = HostsGroup::new(vec![enabled("h1")], true);
        // A second target with the same hostname but disabled replaces the first.
        g.add(tgt("h1", TargetState::Disabled, ExecutionMode::Parallel));
        assert_eq!(g.len(), 1);
        assert_eq!(g.get("h1").unwrap().state(), TargetState::Disabled);
    }

    #[test]
    fn remove_returns_target_and_drops_entry() {
        let mut g = HostsGroup::new(vec![enabled("h1"), enabled("h2")], true);
        let removed = g.remove("h1");
        assert!(removed.is_some());
        assert_eq!(removed.unwrap().hostname(), "h1");
        assert!(!g.contains("h1"));
        assert_eq!(g.names(), vec!["h2".to_owned()]);
    }

    #[test]
    fn remove_missing_is_none() {
        let mut g = HostsGroup::new(vec![enabled("h1")], true);
        assert!(g.remove("nope").is_none());
        assert_eq!(g.len(), 1);
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

    // --- select_split / merge ----------------------------------------------

    #[test]
    fn select_split_by_name_returns_subset_and_remainder() {
        let g = HostsGroup::new(vec![enabled("h1"), enabled("h2"), enabled("h3")], true);
        let (sel, rem) = g.select_split(Some(&["h1".to_owned()]), true).unwrap();
        assert_eq!(sel.names(), vec!["h1".to_owned()]);
        assert_eq!(rem.names(), vec!["h2".to_owned(), "h3".to_owned()]);
        // Both halves inherit `interactive`.
        assert!(sel.is_interactive());
        assert!(rem.is_interactive());
    }

    #[test]
    fn select_split_none_selects_all_empty_remainder() {
        let g = HostsGroup::new(vec![enabled("h1"), enabled("h2")], true);
        let (sel, rem) = g.select_split(None, true).unwrap();
        assert_eq!(sel.names(), vec!["h1".to_owned(), "h2".to_owned()]);
        assert!(rem.is_empty());
    }

    #[test]
    fn select_split_named_disabled_lands_in_remainder() {
        // A named host filtered out by `enabled` is preserved in the remainder,
        // not dropped — this is the whole point of the split.
        let g = HostsGroup::new(
            vec![
                enabled("h1"),
                tgt("h2", TargetState::Disabled, ExecutionMode::Parallel),
            ],
            true,
        );
        let (sel, rem) = g
            .select_split(Some(&["h1".to_owned(), "h2".to_owned()]), true)
            .unwrap();
        assert_eq!(sel.names(), vec!["h1".to_owned()]);
        assert_eq!(rem.names(), vec!["h2".to_owned()]);
    }

    #[test]
    fn select_split_unknown_host_is_not_connected_error() {
        let g = HostsGroup::new(vec![enabled("h1")], true);
        match g.select_split(Some(&["ghost".to_owned()]), false) {
            Err(HostError::NotConnected { host }) => assert_eq!(host, "ghost"),
            other => panic!("expected NotConnected error, got ok: {}", other.is_ok()),
        }
    }

    #[test]
    fn merge_folds_other_into_self() {
        let mut g = HostsGroup::new(vec![enabled("h1")], true);
        let other = HostsGroup::new(vec![enabled("h2"), enabled("h3")], true);
        g.merge(other);
        assert_eq!(
            g.names(),
            vec!["h1".to_owned(), "h2".to_owned(), "h3".to_owned()]
        );
    }

    #[test]
    fn merge_collision_is_last_writer_wins() {
        let mut g = HostsGroup::new(vec![enabled("h1")], true);
        // `other`'s h1 is disabled and must replace `self`'s enabled h1.
        let other = HostsGroup::new(
            vec![tgt("h1", TargetState::Disabled, ExecutionMode::Parallel)],
            true,
        );
        g.merge(other);
        assert_eq!(g.len(), 1);
        assert_eq!(g.get("h1").unwrap().state(), TargetState::Disabled);
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
    async fn pool_unlock_fans_out_over_group() {
        use crate::target::POOL_LOCK_PATH;
        // Two hosts each carry our pool claim; pool_unlock removes both. The
        // claim's user must match the target's identity (config `session_user`,
        // which defaults to $USER), so stamp it dynamically.
        let me = mtui_config::Config::default().session_user;
        let mine = format!("1700000000:{me}:1:mtui pool SUSE:Maintenance:1:2 [me]").into_bytes();
        let c1 = echo("h1").with_file(POOL_LOCK_PATH, mine.clone());
        let c2 = echo("h2").with_file(POOL_LOCK_PATH, mine);
        let (h1, h2) = (c1.clone(), c2.clone());
        let mut g = HostsGroup::new(
            vec![
                Target::with_connection(
                    "h1",
                    TargetState::Enabled,
                    ExecutionMode::Parallel,
                    Box::new(c1),
                ),
                Target::with_connection(
                    "h2",
                    TargetState::Enabled,
                    ExecutionMode::Parallel,
                    Box::new(c2),
                ),
            ],
            false,
        );
        // Stamp the group's RRID so both claims are recognised as ours.
        g.set_rrid("SUSE:Maintenance:1:2");
        g.pool_unlock(false).await;
        assert!(h1.file_contents(POOL_LOCK_PATH).is_none());
        assert!(h2.file_contents(POOL_LOCK_PATH).is_none());
    }

    #[tokio::test]
    async fn pool_unlock_suppresses_foreign_claim_and_continues() {
        use crate::target::POOL_LOCK_PATH;
        // h1 is ours, h2 is a foreign template's claim: pool_unlock skips h2
        // without aborting and still removes h1's.
        let me = mtui_config::Config::default().session_user;
        let mine = format!("1700000000:{me}:1:mtui pool SUSE:Maintenance:1:2 [me]").into_bytes();
        let foreign = b"1700000000:alice:4242:mtui pool SUSE:Maintenance:9:9 [alice]".to_vec();
        let c1 = echo("h1").with_file(POOL_LOCK_PATH, mine);
        let c2 = echo("h2").with_file(POOL_LOCK_PATH, foreign);
        let (h1, h2) = (c1.clone(), c2.clone());
        let mut g = HostsGroup::new(
            vec![
                Target::with_connection(
                    "h1",
                    TargetState::Enabled,
                    ExecutionMode::Parallel,
                    Box::new(c1),
                ),
                Target::with_connection(
                    "h2",
                    TargetState::Enabled,
                    ExecutionMode::Parallel,
                    Box::new(c2),
                ),
            ],
            false,
        );
        g.set_rrid("SUSE:Maintenance:1:2");
        g.pool_unlock(false).await;
        assert!(h1.file_contents(POOL_LOCK_PATH).is_none());
        // h2's foreign claim is left in place (the failure was suppressed).
        assert!(h2.file_contents(POOL_LOCK_PATH).is_some());
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

    // --- fanout_set_repo ---------------------------------------------------

    /// A [`SetRepo`] test double recording `(hostname, op)` per invocation.
    #[derive(Clone, Default)]
    struct RecordingSetRepo {
        seen: Arc<std::sync::Mutex<Vec<(String, RepoOp)>>>,
    }

    #[async_trait::async_trait]
    impl SetRepo for RecordingSetRepo {
        async fn set_repo(&self, target: &mut Target, operation: RepoOp) {
            self.seen
                .lock()
                .unwrap()
                .push((target.hostname().to_owned(), operation));
        }
    }

    #[tokio::test]
    async fn fanout_set_repo_visits_every_host_with_the_operation() {
        let mut g = HostsGroup::new(vec![enabled("h1"), enabled("h2")], false);
        let report = RecordingSetRepo::default();
        g.fanout_set_repo(RepoOp::Add, &report).await;
        let seen = report.seen.lock().unwrap().clone();
        assert_eq!(
            seen,
            vec![
                ("h1".to_owned(), RepoOp::Add),
                ("h2".to_owned(), RepoOp::Add),
            ]
        );
    }

    // --- package_check -----------------------------------------------------

    fn host_with_rpm_output(hostname: &str, stdout: &str) -> Target {
        let conn =
            MockConnection::new(hostname).with_default(CommandLog::new("", stdout, "", 0, 0));
        Target::with_connection(
            hostname,
            TargetState::Enabled,
            ExecutionMode::Parallel,
            Box::new(conn),
        )
    }

    #[tokio::test]
    async fn package_check_records_before_on_pre_and_after_on_post() {
        use mtui_types::package::Package;
        use mtui_types::rpmver::RPMVersion;

        let mut pkg = Package::new("bash");
        pkg.set_required(Some("5.2-1")).unwrap();
        let mut t = host_with_rpm_output("h1", "bash 5.1-1\n");
        t.set_packages(vec![pkg]);
        let mut g = HostsGroup::new(vec![t], false);

        // Pre-pass records the installed version as `before`.
        g.package_check(false).await;
        let before = g.get("h1").unwrap().packages()[0].before().cloned();
        assert_eq!(before, Some(RPMVersion::parse("5.1-1").unwrap()));
        assert!(g.get("h1").unwrap().packages()[0].after().is_none());

        // Post-pass records `after` (same scripted output ⇒ equals `before`,
        // the "package was not updated" state), leaving `before` intact.
        g.package_check(true).await;
        let p = &g.get("h1").unwrap().packages()[0];
        assert_eq!(p.after(), Some(&RPMVersion::parse("5.1-1").unwrap()));
        assert_eq!(p.before(), p.after());
    }

    #[tokio::test]
    async fn package_check_marks_missing_package_before_as_none() {
        use mtui_types::package::Package;

        let mut pkg = Package::new("foo");
        pkg.set_required(Some("1.0-1")).unwrap();
        let mut t = host_with_rpm_output("h1", "package foo is not installed\n");
        t.set_packages(vec![pkg]);
        let mut g = HostsGroup::new(vec![t], false);

        g.package_check(false).await;
        assert!(g.get("h1").unwrap().packages()[0].before().is_none());
    }
}
