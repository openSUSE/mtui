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
//! The upstream responsibilities that reach into higher crates are routed
//! through object-safe seams so `mtui-hosts` never depends on `mtui-testreport`:
//! the `perform_*` update workflow runs via the injected
//! [`PlanProvider`] behind [`HostsGroup`]'s `impl OperationGroup`, and the repo
//! change fan-out ([`fanout_set_repo`](HostsGroup::fanout_set_repo)) drives the
//! object-safe [`SetRepo`] hook whose report impls live in `mtui-testreport`.
//! The pool-claim lock ([`pool_unlock`](HostsGroup::pool_unlock)),
//! `query_versions` / system-product parsing, and lock reporting
//! ([`report_locks`](HostsGroup::report_locks)) are all bound here.
//!
//! The internal map is a [`BTreeMap`] so `names()` / iteration are
//! deterministically ordered by hostname — upstream always iterates its dict via
//! `sorted()` for anything order-sensitive, so this matches observable
//! behaviour without adding an insertion-order dependency.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use crate::error::{HostError, Result};

use mtui_types::system::System;

use super::actions::{self, Command, RunCommand};
use super::operation::{HostCommandMap, HostPlan, OperationGroup, PlanProvider};
use super::repo_manager::{RepoOp, SetRepo};
use super::{LockRow, Target};

/// The per-host result of a group [`lock`](HostsGroup::lock) /
/// [`unlock`](HostsGroup::unlock) fan-out.
///
/// The lock fan-out stays best-effort (one contended host never aborts the
/// batch), but a command now needs to tell a **benign** outcome apart from a
/// **real** failure so it does not report contention as an error. This mirrors
/// the map-returning style of [`close`](HostsGroup::close): each host's outcome
/// is collected keyed by hostname.
///
/// * [`Acquired`](LockOutcome::Acquired) / [`Released`](LockOutcome::Released) —
///   the operation succeeded (or was a no-op: an unconnected host has no lock to
///   act on, treated as success so it is never reported as a failure).
/// * [`Contended`](LockOutcome::Contended) — benign: the lock is held by another
///   owner ([`HostError::TargetLocked`]); the fan-out skipped it, upstream's
///   `suppress(TargetLockedError)`. A caller should **not** count this as a
///   failure.
/// * [`Failed`](LockOutcome::Failed) — a real transport/SFTP error acquiring or
///   releasing the lock; the caller may name the host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LockOutcome {
    /// The operation lock was acquired (or was already ours / a no-op).
    Acquired,
    /// The operation lock was released (or there was nothing to release).
    Released,
    /// The lock is held by another owner — benign contention, not a failure.
    Contended,
    /// A real transport/SFTP error occurred; the string is the reason.
    Failed(String),
}

/// A composite over a group of [`Target`]s, keyed by hostname.
///
/// All hosts in a group are expected to be enabled; the lifetime of the object
/// should match the execution of a single user command (upstream note). See the
/// module docs for the seam layout that keeps `mtui-hosts` acyclic.
pub struct HostsGroup {
    data: BTreeMap<String, Target>,
    /// Whether the surrounding session is interactive. Threaded through to the
    /// fan-out helpers as the (Phase 6) spinner/prompt seam; see
    /// [`actions`](super::actions).
    is_repl: bool,
    /// The injected update-workflow doer/check resolver, or `None` before the
    /// composition root wires one in.
    ///
    /// Held as `mtui-hosts`' own [`PlanProvider`] (not the `mtui-testreport`
    /// registry directly) so the crate graph stays acyclic; `mtui-core::wiring`
    /// supplies the concrete adapter. When absent, [`OperationGroup::plans`]
    /// returns [`HostError::NoPlanProvider`] — the update workflow is unwired.
    plan_provider: Option<Arc<dyn PlanProvider>>,
    /// The session-level serialised [`Prompter`](crate::Prompter), or `None`
    /// under headless callers (`mtui-mcp`).
    ///
    /// Pushed down from the composition root (`mtui-core::Session`) via
    /// [`set_prompter`](Self::set_prompter): it installs the derived
    /// command-timeout prompt on every member [`Target`] via
    /// [`Target::set_timeout_prompt`]. `None` keeps a command timeout an
    /// immediate abort (upstream `prompter=None`).
    prompter: Option<crate::Prompter>,
    /// Maximum hosts to fan out to concurrently in the parallel batch. Pushed
    /// down from the composition root (`mtui-core::Session`) via
    /// [`set_max_parallel`](Self::set_max_parallel) from `[connection]
    /// max_parallel`. `0` (the default before wiring) means "unconfigured" and
    /// the fan-out primitive applies its own conservative default.
    max_parallel: usize,
}

impl HostsGroup {
    /// Builds a group from `hosts`, keyed by [`Target::hostname`].
    ///
    /// `interactive` mirrors upstream: `true` for the REPL (spinner/prompt seam
    /// on), `false` for headless callers such as `mtui-mcp`.
    #[must_use]
    pub fn new(hosts: Vec<Target>, is_repl: bool) -> Self {
        let data = hosts
            .into_iter()
            .map(|h| (h.hostname().to_owned(), h))
            .collect();
        Self {
            data,
            is_repl,
            plan_provider: None,
            prompter: None,
            max_parallel: 0,
        }
    }

    /// Sets the parallel-batch fan-out bound from `[connection] max_parallel`.
    ///
    /// Pushed down by the composition root (`mtui-core::Session`), mirroring
    /// [`set_prompter`](Self::set_prompter). `0` leaves the fan-out primitive's
    /// own conservative default in effect.
    pub fn set_max_parallel(&mut self, max_parallel: usize) {
        self.max_parallel = max_parallel;
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

    /// Whether the surrounding session is the interactive REPL (spinner / prompt
    /// seam on), as opposed to a headless caller (`mtui-mcp`).
    #[must_use]
    pub const fn is_repl(&self) -> bool {
        self.is_repl
    }

    /// Reconciles the group's session mode to `is_repl` at **load time**.
    ///
    /// The report's targets group is default-built headless
    /// ([`TestReportBase`](mtui_testreport docs)); the load site applies the real
    /// session mode once, before any host is added or fan-out runs, so the
    /// spinner/prompt seam matches the session. The session is the single source
    /// of truth; this is not a runtime toggle.
    pub fn set_is_repl(&mut self, is_repl: bool) {
        self.is_repl = is_repl;
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

    /// Mutably iterates the member targets in sorted hostname order.
    pub fn targets_mut(&mut self) -> impl Iterator<Item = &mut Target> {
        self.data.values_mut()
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
        let is_repl = self.is_repl;
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

        let mut group = HostsGroup::new(selected, is_repl);
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
        let is_repl = self.is_repl;
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

        let mut selected = HostsGroup::new(selected, is_repl);
        selected.plan_provider = provider.clone();
        let mut remainder = HostsGroup::new(remainder, is_repl);
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
    ///
    /// When the group already carries a [`Prompter`](crate::Prompter) (installed
    /// via [`set_prompter`](Self::set_prompter)), the incoming target inherits the
    /// derived command-timeout prompt, so a target moved in by
    /// [`merge`](Self::merge) (the split→run→restore round-trip) or added after
    /// the prompter was set still surfaces the interactive timeout prompt.
    pub fn add(&mut self, mut target: Target) {
        if let Some(prompter) = self.prompter.as_ref() {
            target.set_timeout_prompt(prompter.as_timeout_prompt());
        }
        self.data.insert(target.hostname().to_owned(), target);
    }

    /// Removes and returns the target named `hostname`, if present.
    ///
    /// Ports the container half of upstream `remove_host`/`HostsGroup.__delitem__`.
    /// This only detaches the [`Target`] from the group; it does **not**
    /// disconnect it — dropping a `Target` closes its transport but never runs
    /// [`Target::close`], so it does not release the remote operation/pool
    /// locks. The caller (`remove_host`) is responsible for
    /// [`close`](Target::close)ing the returned target to drop those locks.
    /// `None` when no such host is in the group.
    pub fn remove(&mut self, hostname: &str) -> Option<Target> {
        self.data.remove(hostname)
    }

    /// Runs a command across the group in parallel.
    ///
    /// `cmd` accepts a single string (run on every host) or a per-host
    /// [`Command::PerHost`] map (hosts absent from the map are skipped). See
    /// [`RunCommand`].
    pub async fn run(&mut self, cmd: impl Into<Command>) {
        let max_parallel = self.max_parallel;
        RunCommand::new(&mut self.data, cmd, self.is_repl)
            .with_max_parallel(max_parallel)
            .run()
            .await;
    }

    /// Uploads `local` to `remote` on every host in parallel.
    pub async fn sftp_put(&mut self, local: &Path, remote: &Path) {
        let max_parallel = self.max_parallel;
        actions::sftp_put_all(&mut self.data, local, remote, self.is_repl, max_parallel).await;
    }

    /// Downloads `remote` (per-host suffixed) into `local` from every host in
    /// parallel.
    pub async fn sftp_get(&mut self, remote: &str, local: &Path) {
        let max_parallel = self.max_parallel;
        actions::sftp_get_all(&mut self.data, remote, local, self.is_repl, max_parallel).await;
    }

    /// Locks every host in the group for `comment`, best-effort.
    ///
    /// Ports upstream `HostsGroup.lock`: each per-target [`Target::lock`] is
    /// attempted, and a [`HostError::TargetLocked`] from a foreign-owned host is
    /// suppressed (upstream wraps each call in `suppress(TargetLockedError)`) so
    /// one contended host never aborts the fan-out. Other transport errors are
    /// logged, not propagated.
    ///
    /// Returns each host's [`LockOutcome`] keyed by hostname (collected exactly
    /// like [`close`](Self::close), so the map is deterministic regardless of
    /// completion order) so a command can report which hosts a real failure hit
    /// while leaving benign [`Contended`](LockOutcome::Contended) hosts
    /// unreported. Existing callers that ignore the return value keep compiling.
    pub async fn lock(&mut self, comment: &str) -> BTreeMap<String, LockOutcome> {
        self.lock_where(comment, |_t| true).await
    }

    /// Locks only the named hosts for `comment`, best-effort, otherwise
    /// identical to [`lock`](Self::lock).
    ///
    /// The returned [`LockOutcome`] map contains an entry for every host in
    /// `names` that exists in the group (unknown names are silently skipped, like
    /// the whole-group fan-out). Callers that must serialize a remote transaction
    /// on a `-t` subset (e.g. `run`) lock exactly that subset instead of the
    /// whole fleet.
    pub async fn lock_selected(
        &mut self,
        comment: &str,
        names: &std::collections::BTreeSet<String>,
    ) -> BTreeMap<String, LockOutcome> {
        self.lock_where(comment, |t| names.contains(t.hostname()))
            .await
    }

    /// Shared implementation for [`lock`](Self::lock) /
    /// [`lock_selected`](Self::lock_selected): the only difference is the
    /// `select` predicate handed to [`run_fanout`](super::actions::run_fanout).
    async fn lock_where<S>(&mut self, comment: &str, select: S) -> BTreeMap<String, LockOutcome>
    where
        S: Fn(&Target) -> bool + Send + Sync,
    {
        use std::sync::Mutex;

        let (is_repl, max_parallel) = (self.is_repl, self.max_parallel);
        let collected: Mutex<BTreeMap<String, LockOutcome>> = Mutex::new(BTreeMap::new());
        actions::run_fanout(
            &mut self.data,
            is_repl,
            max_parallel,
            Some("lock"),
            select,
            |t| {
                let comment = comment.to_owned();
                let collected = &collected;
                Box::pin(async move {
                    let outcome = match t.lock(&comment).await {
                        Ok(()) => LockOutcome::Acquired,
                        Err(HostError::TargetLocked(msg)) => {
                            tracing::debug!(host = %t.hostname(), %msg, "lock: held by another owner, skipping");
                            LockOutcome::Contended
                        }
                        Err(e) => {
                            tracing::warn!(host = %t.hostname(), error = %e, "lock failed");
                            LockOutcome::Failed(e.to_string())
                        }
                    };
                    collected.lock().unwrap().insert(t.hostname().to_owned(), outcome);
                }) as actions::BoxTargetFut<'_>
            },
        )
        .await;
        collected.into_inner().unwrap()
    }

    /// Releases every host's operation lock, best-effort.
    ///
    /// Ports upstream `HostsGroup.unlock`: delegates to the per-target
    /// [`Target::unlock`] (which already suppresses [`HostError::TargetLocked`]
    /// for a foreign lock), so a contended host never aborts the fan-out.
    ///
    /// Returns each host's [`LockOutcome`] keyed by hostname (like
    /// [`lock`](Self::lock)): [`Released`](LockOutcome::Released) on success,
    /// [`Contended`](LockOutcome::Contended) for a benign foreign-owned lock, and
    /// [`Failed`](LockOutcome::Failed) for a real transport error. Existing
    /// callers that ignore the return value keep compiling.
    pub async fn unlock(&mut self) -> BTreeMap<String, LockOutcome> {
        self.unlock_where(|_t| true).await
    }

    /// Releases the operation lock on only the named hosts, best-effort,
    /// otherwise identical to [`unlock`](Self::unlock).
    ///
    /// Used to roll back a partial [`lock_selected`](Self::lock_selected): a
    /// caller that aborts a subset operation unlocks exactly the hosts it
    /// acquired this call, leaving every other host untouched.
    pub async fn unlock_selected(
        &mut self,
        names: &std::collections::BTreeSet<String>,
    ) -> BTreeMap<String, LockOutcome> {
        self.unlock_where(|t| names.contains(t.hostname())).await
    }

    /// Shared implementation for [`unlock`](Self::unlock) /
    /// [`unlock_selected`](Self::unlock_selected).
    async fn unlock_where<S>(&mut self, select: S) -> BTreeMap<String, LockOutcome>
    where
        S: Fn(&Target) -> bool + Send + Sync,
    {
        use std::sync::Mutex;

        let (is_repl, max_parallel) = (self.is_repl, self.max_parallel);
        let collected: Mutex<BTreeMap<String, LockOutcome>> = Mutex::new(BTreeMap::new());
        actions::run_fanout(
            &mut self.data,
            is_repl,
            max_parallel,
            Some("unlock"),
            select,
            |t| {
                let collected = &collected;
                Box::pin(async move {
                    let outcome = match t.unlock_reporting(false).await {
                        Ok(()) => LockOutcome::Released,
                        Err(HostError::TargetLocked(_)) => LockOutcome::Contended,
                        Err(e) => LockOutcome::Failed(e.to_string()),
                    };
                    collected
                        .lock()
                        .unwrap()
                        .insert(t.hostname().to_owned(), outcome);
                }) as actions::BoxTargetFut<'_>
            },
        )
        .await;
        collected.into_inner().unwrap()
    }

    /// Releases every host's pool claim, best-effort.
    ///
    /// Ports upstream `HostsGroup.pool_unlock`: delegates to the per-target
    /// [`Target::pool_unlock`] (which suppresses [`HostError::TargetLocked`] for
    /// a claim owned by another template), so one contended host never aborts
    /// the fan-out. `force` removes claims owned by other templates too.
    pub async fn pool_unlock(&mut self, force: bool) {
        let (is_repl, max_parallel) = (self.is_repl, self.max_parallel);
        actions::run_fanout(
            &mut self.data,
            is_repl,
            max_parallel,
            Some("pool_unlock"),
            |_t| true,
            |t| Box::pin(async move { t.pool_unlock(force).await }) as actions::BoxTargetFut<'_>,
        )
        .await;
    }

    /// Disconnects every host, optionally rebooting or powering them off.
    ///
    /// Ports upstream `quit`'s per-host `Target.close(action)` fan-out:
    /// delegates to [`Target::close`], which best-effort unlocks the operation
    /// and pool locks, then reboots (`Some("reboot")`), powers off
    /// (`Some("poweroff")` → shell `halt`), or simply closes (`None`). Used on
    /// session exit; unlike [`reboot`](Self::reboot) it never reconnects.
    ///
    /// Fans out concurrently across the group via
    /// [`run_fanout`](super::actions::run_fanout), mirroring upstream's
    /// concurrent close. The overall wait budget is applied by the caller. A
    /// no-op when the group is empty.
    ///
    /// Returns each host's teardown outcome keyed by hostname so `quit` can name
    /// a host that failed to disconnect (upstream `quit` logs
    /// `failed to disconnect from <host>: <err>` per future). Collected exactly
    /// like [`report_locks`](Self::report_locks): each host's
    /// [`Target::close`] result is inserted into a shared map inside the fan-out,
    /// so the map is deterministic regardless of completion order.
    pub async fn close(&mut self, action: Option<&str>) -> BTreeMap<String, Result<()>> {
        use std::sync::Mutex;

        let (is_repl, max_parallel) = (self.is_repl, self.max_parallel);
        let action = action.map(str::to_owned);
        let collected: Mutex<BTreeMap<String, Result<()>>> = Mutex::new(BTreeMap::new());
        actions::run_fanout(
            &mut self.data,
            is_repl,
            max_parallel,
            Some("close"),
            |_t| true,
            |t| {
                let action = action.clone();
                let collected = &collected;
                Box::pin(async move {
                    let hostname = t.hostname().to_owned();
                    let outcome = t.close(action.as_deref()).await;
                    collected.lock().unwrap().insert(hostname, outcome);
                }) as actions::BoxTargetFut<'_>
            },
        )
        .await;
        collected.into_inner().unwrap()
    }

    /// Reports the lock state of every host in the group to `sink`.
    ///
    /// Ports upstream `HostsGroup.report_locks`: for each host in sorted name
    /// order (the [`BTreeMap`] iteration order matches upstream's
    /// `sorted(self.data.keys())`), resolve the operation lock — or the
    /// pool-claim lock when `pool` is `true` — via
    /// [`Target::lock_status`](Target::lock_status), then forward
    /// `(hostname, system, &row)` through the per-target
    /// [`Reporter::locks`](crate::Reporter::locks) sink. `sink` is `FnMut` so it
    /// is invoked once per host. Async because each host's lock is read over
    /// SFTP; best-effort per host (a read failure resolves to the unlocked row
    /// rather than aborting the fan-out).
    pub async fn report_locks<F>(&mut self, mut sink: F, pool: bool)
    where
        F: FnMut(&str, &System, &LockRow),
    {
        use std::sync::Mutex;

        // Phase 1 (I/O): resolve every host's lock row concurrently via the
        // shared fan-out. Each host's `(system, row)` is collected keyed by
        // hostname, so the drain below is deterministically sorted regardless
        // of completion order.
        let collected: Mutex<BTreeMap<String, (System, LockRow)>> = Mutex::new(BTreeMap::new());
        let (is_repl, max_parallel) = (self.is_repl, self.max_parallel);
        actions::run_fanout(
            &mut self.data,
            is_repl,
            max_parallel,
            Some("report_locks"),
            |_t| true,
            |t| {
                let collected = &collected;
                Box::pin(async move {
                    let row = t.lock_status(pool).await;
                    collected
                        .lock()
                        .unwrap()
                        .insert(t.hostname().to_owned(), (t.system().clone(), row));
                }) as actions::BoxTargetFut<'_>
            },
        )
        .await;

        // Phase 2 (pure): forward each host to the sink in sorted hostname order
        // (matching upstream's `sorted(self.data.keys())`).
        for (hostname, (system, row)) in collected.into_inner().unwrap() {
            sink(&hostname, &system, &row);
        }
    }

    /// Installs the session-level serialised [`Prompter`](crate::Prompter) on the
    /// group and every member host.
    ///
    /// The single push-down point for interactive prompting: it stores the
    /// prompter on the group and installs the derived command-timeout prompt on
    /// each member [`Target`] via [`Target::set_timeout_prompt`], so a target
    /// connected *after* this call (and one already connected via the builder)
    /// both surface the timeout prompt. The composition root
    /// (`mtui-core::Session`) calls this when a prompter is present; headless
    /// callers (`mtui-mcp`) never do, leaving a command timeout an immediate
    /// abort.
    pub fn set_prompter(&mut self, prompter: crate::Prompter) {
        let timeout_prompt = prompter.as_timeout_prompt();
        for target in self.data.values_mut() {
            target.set_timeout_prompt(timeout_prompt.clone());
        }
        self.prompter = Some(prompter);
    }

    /// Fans a repository add/remove out across every host in the group.
    ///
    /// Ports upstream `HostsGroup._fanout_set_repo`, which runs
    /// `t.repo_manager.set(operation, testreport)` on every host. `report` is the
    /// object-safe [`SetRepo`] hook the composition root supplies (the concrete
    /// `SlReport`/`ObsReport`/… `set_repo` impl in `mtui-testreport`), so the
    /// group never depends on the report crate.
    ///
    /// Fans out concurrently via [`run_fanout`](super::actions::run_fanout)
    /// (upstream's `run_parallel`): every host runs its repo change in
    /// parallel. The per-host `last*` state is left in place so a caller can
    /// inspect `lasterr()` after the fan-out (upstream's prepare
    /// abort-on-`lasterr`).
    pub async fn fanout_set_repo(&mut self, operation: RepoOp, report: &dyn SetRepo) {
        let (is_repl, max_parallel) = (self.is_repl, self.max_parallel);
        actions::run_fanout(
            &mut self.data,
            is_repl,
            max_parallel,
            Some("set_repo"),
            |_t| true,
            |t| {
                Box::pin(async move { t.repo_manager().set(operation, report).await })
                    as actions::BoxTargetFut<'_>
            },
        )
        .await;
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
    /// Appends a history entry to every member target's remote history file.
    ///
    /// Ports upstream `HostsGroup.add_history`: fans [`Target::add_history`] out
    /// across the group (enabled hosts only, best-effort per host).
    pub async fn add_history(&mut self, fields: &[String]) {
        let (is_repl, max_parallel) = (self.is_repl, self.max_parallel);
        actions::run_fanout(
            &mut self.data,
            is_repl,
            max_parallel,
            Some("add_history"),
            |_t| true,
            |t| {
                let fields = fields.to_vec();
                Box::pin(async move { t.add_history(&fields).await }) as actions::BoxTargetFut<'_>
            },
        )
        .await;
    }

    /// Queries every host's installed package versions concurrently.
    ///
    /// The I/O phase shared by [`package_check`](Self::package_check) and the
    /// downgrade verdict: fans [`Target::query_versions`] out through the shared
    /// [`run_fanout`](super::actions::run_fanout) primitive in parallel, so the
    /// pure per-package bookkeeping that follows never blocks on I/O.
    pub async fn query_versions(&mut self) {
        let (is_repl, max_parallel) = (self.is_repl, self.max_parallel);
        actions::run_fanout(
            &mut self.data,
            is_repl,
            max_parallel,
            Some("query_versions"),
            |_t| true,
            |t| Box::pin(async move { t.query_versions().await }) as actions::BoxTargetFut<'_>,
        )
        .await;
    }

    pub async fn package_check(&mut self, post: bool) {
        // Phase 1 (I/O): query every host's installed versions concurrently,
        // through the shared fan-out primitive.
        self.query_versions().await;

        // Phase 2 (pure): fold each host's queried versions into its packages'
        // before/after fields and emit the update-sanity warnings. No I/O, so
        // this runs sequentially over the group (order-independent).
        for target in self.data.values_mut() {
            let hostname = target.hostname().to_owned();

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
    /// **Fails closed**: success (`Ok`) means *every* host is verifiably locked
    /// by this process. Any per-host failure marks the group contended — a
    /// foreign lock, a `TargetLocked` contention on acquire, or an
    /// SFTP/transport error reading the lock state or writing the lockfile.
    /// A host whose state we cannot even read is treated as "not ours".
    ///
    /// # Errors
    ///
    /// Returns [`HostError::Update`] when one or more hosts were locked by
    /// another owner or could not be locked/read.
    pub async fn update_lock(&mut self) -> Result<()> {
        use std::sync::atomic::{AtomicBool, Ordering};

        // Probe-and-acquire concurrently across the group via the shared
        // fan-out. Each host's probe+acquire is self-contained and
        // order-independent; a foreign-locked host flips the shared `skipped`
        // flag. The per-host lock wire semantics are unchanged — only the
        // fan-out is now concurrent (Contract preserved).
        let skipped = AtomicBool::new(false);
        let (is_repl, max_parallel) = (self.is_repl, self.max_parallel);
        actions::run_fanout(
            &mut self.data,
            is_repl,
            max_parallel,
            Some("update_lock"),
            |_t| true,
            |target| {
                let skipped = &skipped;
                Box::pin(async move {
                    // Load the lock (is_locked) before reading ownership;
                    // is_mine requires a prior load and is order-sensitive. A
                    // read failure (fail-closed load) means the host's state is
                    // unknown — treat it as a skip rather than assuming free.
                    let locked = match target.is_locked().await {
                        Ok(v) => v,
                        Err(e) => {
                            skipped.store(true, Ordering::SeqCst);
                            tracing::warn!(host = %target.hostname(), error = %e, "update_lock: lock state unreadable; skipping");
                            return;
                        }
                    };
                    let foreign = locked
                        && target
                            .lock_mut()
                            .is_some_and(|l| !l.is_mine().unwrap_or(false));
                    if foreign {
                        skipped.store(true, Ordering::SeqCst);
                        let hostname = target.hostname().to_owned();
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
                        // Any failure to acquire — contention or an SFTP/transport
                        // error — means we do NOT own this host, so the whole
                        // group must abort. Flip `skipped` on every error, not
                        // just `TargetLocked`.
                        match target.lock("").await {
                            Ok(()) => {}
                            Err(HostError::TargetLocked(msg)) => {
                                skipped.store(true, Ordering::SeqCst);
                                tracing::debug!(host = %target.hostname(), %msg, "update_lock: held by another owner");
                            }
                            Err(e) => {
                                skipped.store(true, Ordering::SeqCst);
                                tracing::warn!(host = %target.hostname(), error = %e, "update_lock: lock failed");
                            }
                        }
                    }
                }) as actions::BoxTargetFut<'_>
            },
        )
        .await;

        if skipped.into_inner() {
            // Release the locks we did take, best-effort (concurrently), then
            // signal the abort.
            let _ = self.unlock().await;
            return Err(HostError::Update("Hosts locked".to_owned()));
        }
        Ok(())
    }

    /// Reboots only the named hosts and reconnects each, verifying the reboot.
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
    ///
    /// Returns each host's reboot outcome keyed by hostname, mirroring
    /// [`close`](Self::close): `Ok(())` when the host rebooted (boot id changed,
    /// or could not be confirmed) and reconnected; `Err(HostError)` when the
    /// reconnect failed or the boot id was **unchanged** (the host did not
    /// actually reboot). A reconnect failure takes precedence over a later
    /// boot-id verdict for the same host.
    ///
    /// The boot-id snapshot, reboot, reconnect, and verify phases all restrict to
    /// hosts in `names`; the post-reboot relock re-applies only to that subset.
    /// The returned outcome map contains an entry for every named host that
    /// exists in the group (unknown names are silently skipped, like the
    /// whole-group fan-out). Honours the command's `-t/--target` selection so an
    /// operator never reboots hosts they did not ask for.
    pub async fn reboot_selected(
        &mut self,
        command: &str,
        relock_comment: &str,
        names: &std::collections::BTreeSet<String>,
    ) -> BTreeMap<String, Result<()>> {
        self.reboot_where(command, relock_comment, |t| names.contains(t.hostname()))
            .await
    }

    /// Shared implementation behind [`reboot_selected`](Self::reboot_selected):
    /// the `select` predicate is handed to each
    /// [`run_fanout`](super::actions::run_fanout)
    /// phase (and to the relock).
    async fn reboot_where<S>(
        &mut self,
        command: &str,
        relock_comment: &str,
        select: S,
    ) -> BTreeMap<String, Result<()>>
    where
        S: Fn(&Target) -> bool + Send + Sync,
    {
        use std::sync::Mutex;

        if self.data.is_empty() {
            tracing::info!("No hosts to reboot");
            return BTreeMap::new();
        }
        let names: Vec<String> = self
            .data
            .values()
            .filter(|t| select(t))
            .map(|t| t.hostname().to_owned())
            .collect();
        if names.is_empty() {
            tracing::info!("No selected hosts to reboot");
            return BTreeMap::new();
        }
        tracing::info!(hosts = %names.join(", "), "Rebooting");
        let (is_repl, max_parallel) = (self.is_repl, self.max_parallel);

        // Phase 1: record boot ids before rebooting (concurrently), so we can
        // confirm a fresh boot afterwards.
        let old_boot_ids: Mutex<BTreeMap<String, String>> = Mutex::new(BTreeMap::new());
        actions::run_fanout(
            &mut self.data,
            is_repl,
            max_parallel,
            Some("boot_id"),
            &select,
            |t| {
                let old_boot_ids = &old_boot_ids;
                Box::pin(async move {
                    let id = t.boot_id().await;
                    old_boot_ids
                        .lock()
                        .unwrap()
                        .insert(t.hostname().to_owned(), id);
                }) as actions::BoxTargetFut<'_>
            },
        )
        .await;
        let old_boot_ids = old_boot_ids.into_inner().unwrap();

        // Phase 2: fire the reboot on every host (it drops the connection).
        actions::run_fanout(
            &mut self.data,
            is_repl,
            max_parallel,
            Some("reboot"),
            &select,
            |t| {
                let command = command.to_owned();
                Box::pin(async move { t.reboot(&command).await }) as actions::BoxTargetFut<'_>
            },
        )
        .await;

        // Per-host outcome, collected across the reconnect + verify phases so the
        // returned map is deterministic regardless of completion order (as in
        // `close`). A reconnect failure is recorded first and takes precedence;
        // the verify phase only downgrades a host that reconnected cleanly.
        let outcomes: Mutex<BTreeMap<String, Result<()>>> = Mutex::new(BTreeMap::new());

        // Phase 3: reconnect every host (concurrently) with retry + backoff.
        actions::run_fanout(
            &mut self.data,
            is_repl,
            max_parallel,
            Some("reconnect"),
            &select,
            |t| {
                let outcomes = &outcomes;
                Box::pin(async move {
                    let hostname = t.hostname().to_owned();
                    let retry = t.reboot_retries();
                    let outcome = match t.reconnect(retry, true).await {
                        Ok(()) => {
                            tracing::info!(host = %hostname, "is back up");
                            Ok(())
                        }
                        Err(e) => {
                            tracing::error!(host = %hostname, error = %e, "reconnect after reboot failed");
                            Err(e)
                        }
                    };
                    outcomes.lock().unwrap().insert(hostname, outcome);
                }) as actions::BoxTargetFut<'_>
            },
        )
        .await;

        // Phase 4: verify each host's boot id changed (concurrently). Only a host
        // that reconnected cleanly can be downgraded here — a reconnect failure
        // already recorded above is the more fundamental error and is preserved.
        actions::run_fanout(
            &mut self.data,
            is_repl,
            max_parallel,
            Some("verify_reboot"),
            &select,
            |t| {
                let old = old_boot_ids.get(t.hostname()).cloned().unwrap_or_default();
                let outcomes = &outcomes;
                Box::pin(async move {
                    let hostname = t.hostname().to_owned();
                    let new_boot_id = t.boot_id().await;
                    if let Err(reason) = Self::verify_boot_id(&hostname, &old, &new_boot_id) {
                        let mut map = outcomes.lock().unwrap();
                        // Preserve a reconnect failure; only mark a cleanly
                        // reconnected host as failed for an unchanged boot id.
                        if !matches!(map.get(&hostname), Some(Err(_))) {
                            map.insert(hostname, Err(HostError::Update(reason)));
                        }
                    }
                }) as actions::BoxTargetFut<'_>
            },
        )
        .await;

        if !relock_comment.is_empty() {
            tracing::info!("Re-applying lock after reboot");
            // Re-lock only the hosts that were rebooted (a reboot clears
            // `/var/lock`); a targeted reboot must not touch unselected hosts.
            let relock: std::collections::BTreeSet<String> = names.iter().cloned().collect();
            let _ = self.lock_selected(relock_comment, &relock).await;
        }

        outcomes.into_inner().unwrap()
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
        let (is_repl, max_parallel) = (self.is_repl, self.max_parallel);

        // Fire the reboot on every named host first (it drops the connection),
        // then reconnect each once it is back up — both phases fan out
        // concurrently via the shared primitive. `should_run` restricts the
        // fan-out to the named (transactional) hosts.
        actions::run_fanout(
            &mut self.data,
            is_repl,
            max_parallel,
            Some("reboot"),
            |t| reboot.contains_key(t.hostname()),
            |t| {
                let command = reboot.get(t.hostname()).cloned().unwrap_or_default();
                Box::pin(async move { t.reboot(&command).await }) as actions::BoxTargetFut<'_>
            },
        )
        .await;
        actions::run_fanout(
            &mut self.data,
            is_repl,
            max_parallel,
            Some("reconnect"),
            |t| reboot.contains_key(t.hostname()),
            |t| {
                Box::pin(async move {
                    let retry = t.reboot_retries();
                    if let Err(e) = t.reconnect(retry, true).await {
                        tracing::error!(host = %t.hostname(), error = %e, "reconnect after reboot failed");
                    } else {
                        tracing::info!(host = %t.hostname(), "is back up");
                    }
                }) as actions::BoxTargetFut<'_>
            },
        )
        .await;
    }

    /// Decides whether `hostname`'s boot id confirms a reboot, logging as before.
    ///
    /// Ports upstream `HostsGroup._verify_reboot`, factored to a pure
    /// (I/O-free) comparison so the boot-id read can fan out concurrently in
    /// [`reboot`](Self::reboot) and this just decides on the two values.
    /// `/proc/sys/kernel/random/boot_id` is regenerated on every boot, so an
    /// unchanged value means the host did not actually reboot.
    ///
    /// Returns the per-host verdict for the [`reboot`](Self::reboot) outcome map:
    ///
    /// * an **unchanged** non-empty id is a definite failure (`Err`) — the host
    ///   did not reboot (still logged at ERROR);
    /// * a **missing** (empty) old or new id is *not* a failure (`Ok`) — the
    ///   read could not confirm the reboot either way (still logged at WARN),
    ///   so it does not mark the host failed;
    /// * a **changed** id is success (`Ok`).
    fn verify_boot_id(
        hostname: &str,
        old_boot_id: &str,
        new_boot_id: &str,
    ) -> std::result::Result<(), String> {
        if old_boot_id.is_empty() || new_boot_id.is_empty() {
            tracing::warn!(host = %hostname, "could not read boot id to confirm the reboot");
            Ok(())
        } else if old_boot_id == new_boot_id {
            tracing::error!(
                host = %hostname, boot_id = %new_boot_id,
                "boot id unchanged after reboot -- the host may not have rebooted"
            );
            Err("boot id unchanged after reboot -- the host may not have rebooted".to_owned())
        } else {
            Ok(())
        }
    }
}

/// Drives the install/uninstall [`Operation`](super::operation::Operation)
/// template against this group.
///
/// This is the concrete binding of the object-safe [`OperationGroup`] seam
/// declared in [`operation`](super::operation): it resolves each target's per-role
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
        let _ = HostsGroup::unlock(self).await;
    }
}

#[cfg(test)]
mod tests {
    use mtui_types::enums::TargetState;
    use mtui_types::hostlog::CommandLog;

    use super::*;
    use crate::connection::MockConnection;
    use crate::target::TARGET_LOCK_PATH;

    fn echo(hostname: &str) -> MockConnection {
        MockConnection::new(hostname).with_default(CommandLog::new("", "ok", "", 0, 0))
    }

    fn tgt(hostname: &str, state: TargetState) -> Target {
        Target::with_connection(hostname, state, Box::new(echo(hostname)))
    }

    fn enabled(hostname: &str) -> Target {
        tgt(hostname, TargetState::Enabled)
    }

    // --- construction / accessors ------------------------------------------

    #[test]
    fn new_keys_by_hostname_and_reports_names_sorted() {
        let g = HostsGroup::new(vec![enabled("h2"), enabled("h1")], true);
        assert_eq!(g.len(), 2);
        assert!(!g.is_empty());
        assert!(g.is_repl());
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
        assert!(!g.is_repl());
        assert!(g.names().is_empty());
    }

    #[test]
    fn get_mut_and_targets_iter() {
        let mut g = HostsGroup::new(vec![enabled("h1")], true);
        assert!(g.get_mut("h1").is_some());
        assert!(g.get_mut("nope").is_none());
        assert_eq!(g.targets().count(), 1);
    }

    #[tokio::test]
    async fn run_honors_max_parallel_and_still_runs_every_host() {
        // With a small configured bound, a many-host command must still reach
        // every host (correctness under the bound). The peak-concurrency cap
        // itself is asserted deterministically on the `run_parallel` primitive
        // in `actions::tests`; here we prove the group forwards the bound and
        // does not drop hosts.
        let mocks: Vec<MockConnection> = (0..12).map(|i| echo(&format!("h{i:02}"))).collect();
        let handles: Vec<MockConnection> = mocks.to_vec();
        let targets: Vec<Target> = mocks
            .into_iter()
            .enumerate()
            .map(|(i, m)| {
                Target::with_connection(format!("h{i:02}"), TargetState::Enabled, Box::new(m))
            })
            .collect();
        let mut g = HostsGroup::new(targets, false);
        g.set_max_parallel(3);
        g.run("uptime").await;
        for (i, h) in handles.iter().enumerate() {
            assert_eq!(
                h.commands(),
                vec!["uptime".to_owned()],
                "h{i:02} should have run once"
            );
        }
    }

    // --- add / remove ------------------------------------------------------

    #[test]
    fn add_inserts_keyed_by_hostname() {
        let mut g = HostsGroup::new(vec![enabled("h1")], true);
        g.add(enabled("h2"));
        assert_eq!(g.names(), vec!["h1".to_owned(), "h2".to_owned()]);
        assert!(g.contains("h2"));
    }

    /// A no-op [`Prompter`] whose reader returns Enter without touching stdin.
    fn noop_prompter() -> crate::Prompter {
        crate::Prompter::new(std::sync::Arc::new(|_t: String| {
            Box::pin(async move { Ok(String::new()) })
                as std::pin::Pin<
                    Box<dyn std::future::Future<Output = std::io::Result<String>> + Send>,
                >
        }))
    }

    #[test]
    fn set_prompter_installs_timeout_prompt_on_all_members() {
        let mut g = HostsGroup::new(vec![enabled("h1"), enabled("h2")], true);
        assert!(!g.get("h1").unwrap().has_timeout_prompt());

        g.set_prompter(noop_prompter());

        assert!(g.get("h1").unwrap().has_timeout_prompt());
        assert!(g.get("h2").unwrap().has_timeout_prompt());
    }

    #[test]
    fn add_after_set_prompter_inherits_timeout_prompt() {
        let mut g = HostsGroup::new(vec![enabled("h1")], true);
        g.set_prompter(noop_prompter());
        // A host added (or merged) after the prompter was set still gets it, so a
        // split→run→restore round-trip never loses the interactive timeout prompt.
        g.add(enabled("h2"));
        assert!(g.get("h2").unwrap().has_timeout_prompt());
    }

    #[test]
    fn merge_carries_prompter_to_incoming_hosts() {
        let mut g = HostsGroup::new(vec![enabled("h1")], true);
        g.set_prompter(noop_prompter());
        let other = HostsGroup::new(vec![enabled("h2")], true);
        g.merge(other);
        assert!(g.get("h2").unwrap().has_timeout_prompt());
    }

    #[test]
    fn add_same_hostname_replaces_last_writer_wins() {
        let mut g = HostsGroup::new(vec![enabled("h1")], true);
        // A second target with the same hostname but disabled replaces the first.
        g.add(tgt("h1", TargetState::Disabled));
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
        assert!(sel.is_repl());
    }

    #[test]
    fn select_none_enabled_drops_disabled() {
        let g = HostsGroup::new(vec![enabled("h1"), tgt("h2", TargetState::Disabled)], true);
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
        let g = HostsGroup::new(vec![enabled("h1"), tgt("h2", TargetState::Disabled)], true);
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
        assert!(sel.is_repl());
        assert!(rem.is_repl());
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
        let g = HostsGroup::new(vec![enabled("h1"), tgt("h2", TargetState::Disabled)], true);
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
        let other = HostsGroup::new(vec![tgt("h1", TargetState::Disabled)], true);
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
                Target::with_connection("h1", TargetState::Enabled, Box::new(m1)),
                Target::with_connection("h2", TargetState::Enabled, Box::new(m2)),
            ],
            false,
        );

        g.run("hostname").await;

        assert_eq!(h1.commands(), vec!["hostname".to_owned()]);
        assert_eq!(h2.commands(), vec!["hostname".to_owned()]);
    }

    #[tokio::test]
    async fn close_fans_out_action_to_all_members() {
        let (m1, m2) = (echo("h1"), echo("h2"));
        let (h1, h2) = (m1.clone(), m2.clone());
        let mut g = HostsGroup::new(
            vec![
                Target::with_connection("h1", TargetState::Enabled, Box::new(m1)),
                Target::with_connection("h2", TargetState::Enabled, Box::new(m2)),
            ],
            false,
        );

        let outcomes = g.close(Some("poweroff")).await;

        // Every member reports a successful teardown, keyed by hostname.
        assert_eq!(outcomes.len(), 2);
        assert!(outcomes["h1"].is_ok());
        assert!(outcomes["h2"].is_ok());
        // Both hosts received the mapped `halt` command and were closed.
        assert_eq!(h1.fired_commands(), vec!["halt".to_owned()]);
        assert_eq!(h2.fired_commands(), vec!["halt".to_owned()]);
        assert!(h1.is_closed());
        assert!(h2.is_closed());
    }

    #[tokio::test]
    async fn close_surfaces_per_host_failure() {
        // One member's connection fails to close; the other closes cleanly. The
        // returned map names the failing host with an `Err` and the healthy one
        // with `Ok`, so `quit` can name the failure.
        let ok = echo("h1");
        let bad = echo("h2").with_failing_close();
        let mut g = HostsGroup::new(
            vec![
                Target::with_connection("h1", TargetState::Enabled, Box::new(ok)),
                Target::with_connection("h2", TargetState::Enabled, Box::new(bad)),
            ],
            false,
        );

        let outcomes = g.close(None).await;

        assert!(outcomes["h1"].is_ok());
        assert!(
            matches!(&outcomes["h2"], Err(HostError::Connect { host, .. }) if host == "h2"),
            "failing host is named in the outcome map"
        );
    }

    #[tokio::test]
    async fn close_empty_group_is_noop() {
        let mut g = HostsGroup::new(vec![], false);
        // Must not panic on an empty group; no per-host outcomes.
        assert!(g.close(Some("reboot")).await.is_empty());
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
                Box::new(m1),
            )],
            false,
        );

        g.sftp_put(Path::new("/l"), Path::new("/r")).await;
        g.sftp_get("/r", Path::new("/l")).await;

        let ops = h1.sftp_ops();
        assert!(matches!(ops[0], MockSftpOp::Put { .. }));
        assert!(matches!(ops[1], MockSftpOp::Get { .. }));
    }

    // --- reboot lifecycle (P2.9) -------------------------------------------

    /// A mock that answers the boot-id probe with a *fixed* `boot_id` (same value
    /// on every read) and everything else with "ok". Because the pre- and
    /// post-reboot reads are identical, this models a host that did **not**
    /// reboot — the verify path records a failure.
    fn reboot_mock(hostname: &str, boot_id: &str) -> MockConnection {
        MockConnection::new(hostname)
            .with_default(CommandLog::new("", "ok", "", 0, 0))
            .with_response(
                "cat /proc/sys/kernel/random/boot_id",
                CommandLog::new("cat /proc/sys/kernel/random/boot_id", boot_id, "", 0, 0),
            )
    }

    /// A mock whose boot-id probe returns a *fresh* value on every read, modelling
    /// a host that actually rebooted (pre- and post-reboot ids differ) so the
    /// verify path records success.
    fn rebooted_mock(hostname: &str) -> MockConnection {
        MockConnection::new(hostname)
            .with_default(CommandLog::new("", "ok", "", 0, 0))
            .with_changing_boot_id()
    }

    #[tokio::test]
    async fn reboot_fires_reconnects_and_verifies_all_hosts() {
        let (m1, m2) = (rebooted_mock("h1"), rebooted_mock("h2"));
        let (h1, h2) = (m1.clone(), m2.clone());
        let mut g = HostsGroup::new(
            vec![
                Target::with_connection("h1", TargetState::Enabled, Box::new(m1)),
                Target::with_connection("h2", TargetState::Enabled, Box::new(m2)),
            ],
            false,
        );

        let all: std::collections::BTreeSet<String> =
            ["h1".to_owned(), "h2".to_owned()].into_iter().collect();
        let outcomes = g.reboot_selected("systemctl reboot", "", &all).await;

        // Each host was sent the reboot fire-and-forget and reconnected once.
        assert_eq!(h1.fired_commands(), vec!["systemctl reboot".to_owned()]);
        assert_eq!(h2.fired_commands(), vec!["systemctl reboot".to_owned()]);
        assert_eq!(h1.reconnect_count(), 1);
        assert_eq!(h2.reconnect_count(), 1);
        // Distinct boot ids on both reads ⇒ both hosts rebooted successfully.
        assert_eq!(outcomes.len(), 2);
        assert!(outcomes["h1"].is_ok());
        assert!(outcomes["h2"].is_ok());
        // The reboot lifecycle grants the boot-aware backoff budget (the
        // target's config default is `reboot_retries = 10`), not the fast-path
        // `(0, false)` used by run/shell/sftp.
        assert_eq!(h1.last_reconnect_args(), Some((10, true)));
        assert_eq!(h2.last_reconnect_args(), Some((10, true)));
    }

    #[tokio::test]
    async fn reboot_selected_touches_only_named_hosts() {
        // Regression for mtui-rs-issz: a `-t`-scoped reboot must fire the reboot,
        // reconnect, and verify only on the named subset; unselected hosts see no
        // reboot command, no reconnect, and produce no outcome entry.
        let (m1, m2) = (rebooted_mock("h1"), rebooted_mock("h2"));
        let (h1, h2) = (m1.clone(), m2.clone());
        let mut g = HostsGroup::new(
            vec![
                Target::with_connection("h1", TargetState::Enabled, Box::new(m1)),
                Target::with_connection("h2", TargetState::Enabled, Box::new(m2)),
            ],
            false,
        );

        let only_h1: std::collections::BTreeSet<String> = ["h1".to_owned()].into_iter().collect();
        let outcomes = g.reboot_selected("systemctl reboot", "", &only_h1).await;

        // h1 was rebooted and reconnected; h2 was left entirely untouched.
        assert_eq!(h1.fired_commands(), vec!["systemctl reboot".to_owned()]);
        assert_eq!(h1.reconnect_count(), 1);
        assert!(h2.fired_commands().is_empty());
        assert_eq!(h2.reconnect_count(), 0);
        assert_eq!(outcomes.len(), 1);
        assert!(outcomes["h1"].is_ok());
        assert!(!outcomes.contains_key("h2"));
    }

    #[tokio::test]
    async fn reboot_relocks_when_comment_given() {
        let m1 = reboot_mock("h1", "id-1");
        let h1 = m1.clone();
        let mut g = HostsGroup::new(
            vec![Target::with_connection(
                "h1",
                TargetState::Enabled,
                Box::new(m1),
            )],
            false,
        );

        let all: std::collections::BTreeSet<String> = ["h1".to_owned()].into_iter().collect();
        g.reboot_selected("systemctl reboot", "PI testing", &all)
            .await;

        // The relock wrote a lock file: the host now reports locked.
        assert!(g.get_mut("h1").unwrap().is_locked().await.unwrap());
        assert_eq!(h1.reconnect_count(), 1);
    }

    #[tokio::test]
    async fn reboot_empty_group_is_noop() {
        let mut g = HostsGroup::new(vec![], false);
        // Must not panic; nothing to reboot, no per-host outcomes.
        let none: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        assert!(
            g.reboot_selected("systemctl reboot", "relock", &none)
                .await
                .is_empty()
        );
        assert!(g.is_empty());
    }

    #[tokio::test]
    async fn reboot_verify_unchanged_boot_id_records_failure() {
        // Same boot id on both reads models a host that did NOT reboot; the
        // verify path logs an error and records a per-host failure while the
        // group call still completes.
        let m1 = reboot_mock("h1", "same-id");
        let h1 = m1.clone();
        let mut g = HostsGroup::new(
            vec![Target::with_connection(
                "h1",
                TargetState::Enabled,
                Box::new(m1),
            )],
            false,
        );
        let all: std::collections::BTreeSet<String> = ["h1".to_owned()].into_iter().collect();
        let outcomes = g.reboot_selected("systemctl reboot", "", &all).await;
        assert_eq!(h1.reconnect_count(), 1);
        // Reconnect succeeded but the boot id was unchanged -> recorded failure.
        assert!(
            matches!(&outcomes["h1"], Err(HostError::Update(msg)) if msg.contains("boot id unchanged")),
            "unchanged boot id must be recorded as a failure, got {:?}",
            outcomes["h1"]
        );
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
                Box::new(m1),
            )],
            false,
        );
        let all: std::collections::BTreeSet<String> = ["h1".to_owned()].into_iter().collect();
        let outcomes = g.reboot_selected("systemctl reboot", "", &all).await;
        assert_eq!(h1.reconnect_count(), 1);
        assert_eq!(h1.fired_commands(), vec!["systemctl reboot".to_owned()]);
        // An empty (unreadable) boot id could not confirm the reboot either way,
        // so it is NOT recorded as a failure (only warned).
        assert!(
            outcomes["h1"].is_ok(),
            "an unconfirmable boot id must not be a failure, got {:?}",
            outcomes["h1"]
        );
    }

    #[tokio::test]
    async fn reboot_records_reconnect_failure() {
        let m1 = reboot_mock("h1", "id-1");
        // Recreate with a failing reconnect.
        let m1 = m1.failing_reconnect();
        let h1 = m1.clone();
        let mut g = HostsGroup::new(
            vec![Target::with_connection(
                "h1",
                TargetState::Enabled,
                Box::new(m1),
            )],
            false,
        );
        let all: std::collections::BTreeSet<String> = ["h1".to_owned()].into_iter().collect();
        let outcomes = g.reboot_selected("systemctl reboot", "", &all).await;
        assert_eq!(h1.reconnect_count(), 1);
        // A reconnect failure is recorded as the host's failure.
        assert!(
            matches!(&outcomes["h1"], Err(HostError::ReconnectFailed { host }) if host == "h1"),
            "reconnect failure must be recorded, got {:?}",
            outcomes["h1"]
        );
    }

    // --- _reboot (transactional subset, via OperationGroup) -----------------

    #[tokio::test]
    async fn operation_reboot_fires_and_reconnects_named_hosts() {
        let (m1, m2) = (reboot_mock("h1", "id-1"), reboot_mock("h2", "id-2"));
        let (h1, h2) = (m1.clone(), m2.clone());
        let mut g = HostsGroup::new(
            vec![
                Target::with_connection("h1", TargetState::Enabled, Box::new(m1)),
                Target::with_connection("h2", TargetState::Enabled, Box::new(m2)),
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
        // The transactional reboot path also grants the boot-aware budget.
        assert_eq!(h1.last_reconnect_args(), Some((10, true)));
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
                Target::with_connection("h2", TargetState::Enabled, Box::new(foreign)),
            ],
            false,
        );

        let err = g.update_lock().await.expect_err("foreign lock -> abort");
        assert!(matches!(err, HostError::Update(_)));
        // h1's lock was released during the abort.
        assert!(!g.get_mut("h1").unwrap().is_locked().await.unwrap());
    }

    #[tokio::test]
    async fn update_lock_errors_and_releases_on_non_contention_lock_failure() {
        // h2 is free but its atomic lock create fails with a non-contention
        // SFTP error: update_lock must still abort the whole group (we do not
        // own h2) and release the lock h1 took. Fail-closed group ownership.
        let failing = MockConnection::new("h2")
            .with_default(CommandLog::new("", "ok", "", 0, 0))
            .with_exclusive_write_error(TARGET_LOCK_PATH);
        let mut g = HostsGroup::new(
            vec![
                enabled("h1"),
                Target::with_connection("h2", TargetState::Enabled, Box::new(failing)),
            ],
            false,
        );

        let err = g
            .update_lock()
            .await
            .expect_err("non-contention lock failure -> abort");
        assert!(matches!(err, HostError::Update(_)));
        assert!(!g.get_mut("h1").unwrap().is_locked().await.unwrap());
    }

    #[tokio::test]
    async fn lock_and_unlock_fan_out_over_group() {
        let mut g = HostsGroup::new(vec![enabled("h1"), enabled("h2")], false);
        let locked = g.lock("session").await;
        assert_eq!(locked["h1"], LockOutcome::Acquired);
        assert_eq!(locked["h2"], LockOutcome::Acquired);
        assert!(g.get_mut("h1").unwrap().is_locked().await.unwrap());
        assert!(g.get_mut("h2").unwrap().is_locked().await.unwrap());

        let unlocked = g.unlock().await;
        assert_eq!(unlocked["h1"], LockOutcome::Released);
        assert_eq!(unlocked["h2"], LockOutcome::Released);
        assert!(!g.get_mut("h1").unwrap().is_locked().await.unwrap());
        assert!(!g.get_mut("h2").unwrap().is_locked().await.unwrap());
    }

    #[tokio::test]
    async fn lock_reports_benign_contention_distinct_from_failure() {
        // h1 is free (Acquired); h2 carries a foreign lock (benign Contended);
        // h3's atomic lock create fails with a non-contention SFTP error (real
        // Failed). The command can tell contention apart from a true failure.
        let foreign = MockConnection::new("h2")
            .with_default(CommandLog::new("", "ok", "", 0, 0))
            .with_file(TARGET_LOCK_PATH, b"1700000000:alice:4242:busy".to_vec());
        let failing = MockConnection::new("h3")
            .with_default(CommandLog::new("", "ok", "", 0, 0))
            .with_exclusive_write_error(TARGET_LOCK_PATH);
        let mut g = HostsGroup::new(
            vec![
                enabled("h1"),
                Target::with_connection("h2", TargetState::Enabled, Box::new(foreign)),
                Target::with_connection("h3", TargetState::Enabled, Box::new(failing)),
            ],
            false,
        );

        let outcomes = g.lock("session").await;
        assert_eq!(outcomes["h1"], LockOutcome::Acquired);
        assert_eq!(outcomes["h2"], LockOutcome::Contended);
        assert!(
            matches!(&outcomes["h3"], LockOutcome::Failed(_)),
            "a non-contention lock error must be Failed, got {:?}",
            outcomes["h3"]
        );
    }

    #[tokio::test]
    async fn lock_selected_touches_only_named_hosts() {
        // h1 selected + free (Acquired); h2 selected + foreign-locked
        // (Contended); h3 NOT selected -> absent from the map and left unlocked.
        let foreign = MockConnection::new("h2")
            .with_default(CommandLog::new("", "ok", "", 0, 0))
            .with_file(TARGET_LOCK_PATH, b"1700000000:alice:4242:busy".to_vec());
        let mut g = HostsGroup::new(
            vec![
                enabled("h1"),
                Target::with_connection("h2", TargetState::Enabled, Box::new(foreign)),
                enabled("h3"),
            ],
            false,
        );

        let names: std::collections::BTreeSet<String> =
            ["h1".to_owned(), "h2".to_owned()].into_iter().collect();
        let outcomes = g.lock_selected("session", &names).await;

        assert_eq!(outcomes["h1"], LockOutcome::Acquired);
        assert_eq!(outcomes["h2"], LockOutcome::Contended);
        assert!(
            !outcomes.contains_key("h3"),
            "unselected host must be absent from the outcome map: {outcomes:?}"
        );
        // The unselected host was never locked.
        assert!(!g.get_mut("h3").unwrap().is_locked().await.unwrap());

        // Rolling back the acquired subset releases h1 without touching h3.
        let released = g.unlock_selected(&names).await;
        assert_eq!(released["h1"], LockOutcome::Released);
        assert!(!released.contains_key("h3"));
        assert!(!g.get_mut("h1").unwrap().is_locked().await.unwrap());
    }

    #[tokio::test]
    async fn unlock_reports_benign_contention_distinct_from_release() {
        // h1 is ours (locked below) -> Released; h2 is foreign -> benign
        // Contended (not a failure).
        let foreign = MockConnection::new("h2")
            .with_default(CommandLog::new("", "ok", "", 0, 0))
            .with_file(TARGET_LOCK_PATH, b"1700000000:alice:4242:busy".to_vec());
        let mut g = HostsGroup::new(
            vec![
                enabled("h1"),
                Target::with_connection("h2", TargetState::Enabled, Box::new(foreign)),
            ],
            false,
        );
        let _ = g.lock("session").await; // locks h1; h2 stays foreign-locked

        let outcomes = g.unlock().await;
        assert_eq!(outcomes["h1"], LockOutcome::Released);
        assert_eq!(outcomes["h2"], LockOutcome::Contended);
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
                Target::with_connection("h1", TargetState::Enabled, Box::new(c1)),
                Target::with_connection("h2", TargetState::Enabled, Box::new(c2)),
            ],
            false,
        );
        // Stamp the group's RRID so both claims are recognised as ours.
        for t in g.data.values_mut() {
            t.set_rrid("SUSE:Maintenance:1:2");
        }
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
                Target::with_connection("h1", TargetState::Enabled, Box::new(c1)),
                Target::with_connection("h2", TargetState::Enabled, Box::new(c2)),
            ],
            false,
        );
        for t in g.data.values_mut() {
            t.set_rrid("SUSE:Maintenance:1:2");
        }
        g.pool_unlock(false).await;
        assert!(h1.file_contents(POOL_LOCK_PATH).is_none());
        // h2's foreign claim is left in place (the failure was suppressed).
        assert!(h2.file_contents(POOL_LOCK_PATH).is_some());
    }

    // --- report_locks (list_locks fan-out) ----------------------------------

    #[tokio::test]
    async fn report_locks_forwards_each_host_in_sorted_order() {
        // h2 is foreign-locked, h1 is free: the sink sees both, sorted by name.
        let c2 = echo("h2").with_file(TARGET_LOCK_PATH, b"1700000000:alice:4242:busy".to_vec());
        let mut g = HostsGroup::new(
            vec![
                Target::with_connection("h2", TargetState::Enabled, Box::new(c2)),
                enabled("h1"),
            ],
            false,
        );
        let mut rows: Vec<(String, LockRow)> = Vec::new();
        g.report_locks(
            |host, _system, row| rows.push((host.to_owned(), row.clone())),
            false,
        )
        .await;
        assert_eq!(rows.len(), 2);
        // BTreeMap → sorted: h1 first (free), then h2 (foreign-locked).
        assert_eq!(rows[0].0, "h1");
        assert!(!rows[0].1.is_locked);
        assert_eq!(rows[1].0, "h2");
        assert!(rows[1].1.is_locked);
        assert!(!rows[1].1.is_mine);
        assert_eq!(rows[1].1.locked_by, "alice");
        assert_eq!(rows[1].1.comment, "busy");
    }

    #[tokio::test]
    async fn report_locks_pool_variant_reads_pool_claims() {
        use crate::target::POOL_LOCK_PATH;
        let c1 = echo("h1").with_file(
            POOL_LOCK_PATH,
            b"1700000000:bob:99:mtui pool SUSE:Maintenance:9:9 [bob]".to_vec(),
        );
        let mut g = HostsGroup::new(
            vec![Target::with_connection(
                "h1",
                TargetState::Enabled,
                Box::new(c1),
            )],
            false,
        );
        let mut rows: Vec<(String, LockRow)> = Vec::new();
        g.report_locks(
            |host, _system, row| rows.push((host.to_owned(), row.clone())),
            true,
        )
        .await;
        assert_eq!(rows.len(), 1);
        assert!(rows[0].1.is_locked);
        assert_eq!(rows[0].1.locked_by, "bob");
        // The pool path surfaces the parsed RRID in the detail slot.
        assert_eq!(rows[0].1.comment, "SUSE:Maintenance:9:9");
    }

    #[tokio::test]
    async fn report_locks_empty_group_calls_sink_zero_times() {
        let mut g = HostsGroup::new(vec![], false);
        let mut n = 0;
        g.report_locks(|_, _, _| n += 1, false).await;
        assert_eq!(n, 0);
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
                Target::with_connection("h2", TargetState::Enabled, Box::new(foreign)),
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
        Target::with_connection(hostname, TargetState::Enabled, Box::new(conn))
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
