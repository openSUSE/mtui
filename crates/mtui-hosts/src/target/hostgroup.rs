//! [`HostsGroup`] — a composite over many [`Target`]s.
//!
//! Owns the container + fan-out surface (construction, [`select`](HostsGroup::select),
//! the non-consuming [`select_split`](HostsGroup::select_split) +
//! [`merge`](HostsGroup::merge) pair that keeps unselected hosts through a `-t` subset
//! operation, command and SFTP fan-out), the operation-lock fan-out
//! ([`lock`](HostsGroup::lock) / [`unlock`](HostsGroup::unlock) /
//! [`update_lock`](HostsGroup::update_lock)), and the reboot/reconnect lifecycle
//! ([`reboot`](HostsGroup::reboot) with boot-id verification and optional relock).
//!
//! Code that reaches into higher crates goes through object-safe seams so
//! `mtui-hosts` never depends on `mtui-testreport`: the injected [`PlanProvider`]
//! behind `impl OperationGroup`, and the [`SetRepo`] hook driven by
//! [`fanout_set_repo`](HostsGroup::fanout_set_repo).
//!
//! The internal map is a [`BTreeMap`], so `names()` and iteration are sorted by
//! hostname.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use crate::connection::SftpRead;
use crate::error::{HostError, LockRefusal, Result};

use mtui_types::package::VersionCheck;
use mtui_types::system::System;

use super::actions::{self, BoxTargetFut, Command, boxed};
use super::operation::{
    HostCommandMap, HostPlan, OperationGroup, PlanProvider, RebootFailure, RebootFailureCause,
};
use super::repo_manager::{RepoOp, SetRepo};
use super::{LockKind, LockOwner, LockRow, OpLockHolders, Target};

/// Which hosts a group fan-out covers: `None` is the whole group, `Some` only
/// the named ones. A name not in the group is silently skipped.
pub type Scope<'a> = Option<&'a BTreeSet<String>>;

fn in_scope(scope: Scope<'_>) -> impl Fn(&Target) -> bool + Copy + '_ {
    move |t| scope.is_none_or(|names| names.contains(t.hostname()))
}

/// The per-host result of a group [`lock`](HostsGroup::lock) /
/// [`unlock`](HostsGroup::unlock) fan-out, keyed by hostname.
///
/// The fan-out stays best-effort (one contended host never aborts the batch), but
/// callers must tell benign contention from a real failure.
///
/// * [`Acquired`](LockOutcome::Acquired) / [`Released`](LockOutcome::Released) —
///   succeeded, or a no-op.
/// * [`Contended`](LockOutcome::Contended) — benign: held by another owner
///   ([`HostError::TargetLocked`]); not a failure.
/// * [`Failed`](LockOutcome::Failed) — a real transport/SFTP error, or an invalid
///   lock file ([`HostError::InvalidLockFile`]).
/// * [`Skipped`](LockOutcome::Skipped) — nothing was attempted on the wire (e.g. an
///   unconnected host); the verb never ran, so a caller must not report success.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LockOutcome {
    /// The operation lock was acquired (or was already ours / a no-op).
    Acquired,
    /// The operation lock was released (or there was nothing to release).
    Released,
    /// The lock is held by another owner — benign contention, not a failure.
    /// Carries the owner already loaded, so the report can name them without another
    /// round-trip.
    Contended(LockOwner),
    /// A real transport/SFTP error occurred; the string is the reason.
    Failed(String),
    /// Nothing was attempted on the wire — the string is a display-ready
    /// reason (e.g. `"not connected"`).
    Skipped(String),
}

/// Which `/var/lock` hold a reboot wiped and could not re-take.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LostHoldKind {
    /// The operation/zypper lock (`/var/lock/mtui.lock`).
    OperationLock,
    /// The pool-claim lock (`/var/lock/mtui-pool.lock`).
    PoolClaim,
}

impl LostHoldKind {
    /// The operator-facing noun for [`LostHold::describe`].
    #[must_use]
    fn as_str(self) -> &'static str {
        match self {
            Self::OperationLock => "operation lock",
            Self::PoolClaim => "pool claim",
        }
    }
}

impl From<LostHoldKind> for LockKind {
    fn from(kind: LostHoldKind) -> Self {
        match kind {
            LostHoldKind::OperationLock => Self::Op,
            LostHoldKind::PoolClaim => Self::Pool,
        }
    }
}

/// Whether the reboot proved the hold gone or only failed to re-take it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LostHoldProof {
    /// Another owner claimed the hold in the gap. The hold is gone.
    Proven,
    /// The re-take failed with a transport error or found a corrupt line. No wipe is
    /// proven, so the record is kept: dropping it would make a later plain `unlock`
    /// skip the host and strand the corrupt file.
    Unproven,
}

/// A hold this session recorded before a reboot that could not be re-taken
/// afterwards: another owner claimed the host in the gap, or the re-take
/// failed and the host may be unlocked.
///
/// The reboot outcome stays `Ok`; the hold is reported through the display layer
/// instead. A proven loss drops its in-memory record, so a later `unlock` stops
/// claiming it; an unproven one keeps it, since the hold may still be ours and a
/// corrupt file must stay visible to the next release attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LostHold {
    /// The host the hold was lost on.
    pub host: String,
    /// Which hold was lost.
    pub kind: LostHoldKind,
    /// The comment the lost hold carried.
    pub comment: String,
    /// Why the hold is reported: the other owner's lock line for a proven loss;
    /// `{error}; the host may be unlocked` for an unproven one.
    pub reason: String,
    /// Whether the loss is proven or not; a proven loss drops its record.
    pub proof: LostHoldProof,
}

impl LostHold {
    /// The operator-facing one-liner. A proven loss reads "reboot cleared …"; an
    /// unproven one reads "re-take … failed" (the host was reachable but its lock line
    /// could not be read, so no wipe is claimed).
    #[must_use]
    pub fn describe(&self) -> String {
        if self.proof == LostHoldProof::Unproven {
            if self.comment.is_empty() && self.kind == LostHoldKind::OperationLock {
                return format!(
                    "{}: re-take of the operation lock failed; {}",
                    self.host, self.reason
                );
            }
            return format!(
                "{}: re-take of the {} ({}) failed; {}",
                self.host,
                self.kind.as_str(),
                self.comment,
                self.reason
            );
        }
        if self.comment.is_empty() && self.kind == LostHoldKind::OperationLock {
            return format!(
                "{}: reboot cleared the operation lock; {}",
                self.host, self.reason
            );
        }
        format!(
            "{}: reboot cleared the {} ({}); {}",
            self.host,
            self.kind.as_str(),
            self.comment,
            self.reason
        )
    }
}

/// Records a hold the reboot wiped and could not be re-taken, warning once. A
/// proven loss also drops the target's in-memory record.
fn record_hold_loss(
    target: &mut Target,
    lost: &std::sync::Mutex<Vec<LostHold>>,
    kind: LostHoldKind,
    comment: String,
    reason: String,
    proof: LostHoldProof,
) {
    let loss = LostHold {
        host: target.hostname().to_owned(),
        kind,
        comment,
        reason,
        proof,
    };
    if proof == LostHoldProof::Proven {
        tracing::warn!(loss = %loss.describe(), "hold lost across the reboot");
        target.forget_hold(kind.into());
    } else {
        tracing::warn!(loss = %loss.describe(), "hold may be lost across the reboot");
    }
    lost.lock().unwrap().push(loss);
}

/// What a reboot fan-out took with it: the per-host outcomes plus the holds
/// the reboot wiped that could not be re-taken.
#[derive(Debug, Default)]
pub struct RebootReport {
    /// Each rebooted host's outcome, keyed by hostname.
    pub outcomes: BTreeMap<String, Result<()>>,
    /// One entry per hold the reboot wiped that could not be re-taken (record dropped
    /// if another owner claimed it, kept if the re-take failed or hit a corrupt line).
    pub lost_holds: Vec<LostHold>,
}

impl RebootReport {
    /// The per-host failures, in [`RebootFailure`] form for the update
    /// workflow's rollback routing. A host whose outcome is `Ok` — including
    /// one that lost a hold — contributes nothing.
    #[must_use]
    pub fn failures(&self) -> Vec<RebootFailure> {
        self.outcomes
            .iter()
            .filter_map(|(host, outcome)| {
                let e = outcome.as_ref().err()?;
                Some(RebootFailure {
                    host: host.clone(),
                    cause: reboot_failure_cause(e),
                    reason: e.to_string(),
                })
            })
            .collect()
    }
}

/// The `/var/lock` holds one host recorded before a reboot, keyed by
/// hostname for the post-reboot re-apply.
#[derive(Debug, Default, Clone)]
struct HeldHolds {
    /// The operation-lock comment this session stamped, if it holds one.
    op: Option<String>,
    /// The pool-claim comment this session stamped, if it holds one.
    pool: Option<String>,
}

/// A composite over a group of [`Target`]s, keyed by hostname, scoped to a single
/// user command. See the module docs for the seam layout.
pub struct HostsGroup {
    data: BTreeMap<String, Target>,
    /// Whether the session is interactive: the spinner/prompt seam for the fan-out
    /// helpers.
    is_repl: bool,
    /// The injected update-workflow doer/check resolver. Held as `mtui-hosts`' own
    /// [`PlanProvider`] to keep the crate graph acyclic; injected via
    /// [`set_plan_provider`](Self::set_plan_provider). When absent,
    /// [`OperationGroup::plans`] returns [`HostError::NoPlanProvider`].
    plan_provider: Option<Arc<dyn PlanProvider>>,
    /// The session-level serialised [`Prompter`](crate::Prompter), or `None` under
    /// headless callers (`mtui-mcp`). Pushed down via
    /// [`set_prompter`](Self::set_prompter), which installs the derived command-timeout
    /// prompt on every member; `None` keeps a timeout an immediate abort.
    prompter: Option<crate::Prompter>,
    /// Maximum hosts fanned out concurrently, pushed down from `[connection]
    /// max_parallel`. `0` means unconfigured and the fan-out applies its own default.
    max_parallel: usize,
    /// Cooperative cancellation seam, pushed down via
    /// [`set_cancel_token`](Self::set_cancel_token). Only explicit call-site checkpoints
    /// consult it; a parallel fan-out is never interrupted part-way.
    cancel: CancellationToken,
    /// The registry that lets members tell a sibling group's operation lock from their
    /// own; `None` leaves each member on its own.
    op_lock_holders: Option<OpLockHolders>,
}

impl HostsGroup {
    /// Builds a group from `hosts`, keyed by [`Target::hostname`].
    ///
    /// `is_repl`: `true` for the REPL (spinner/prompt seam on), `false` for headless
    /// callers such as `mtui-mcp`.
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
            cancel: CancellationToken::new(),
            op_lock_holders: None,
        }
    }

    /// Installs the registry shared with the process's other groups on every
    /// member, and on every target added later.
    pub fn set_op_lock_holders(&mut self, holders: OpLockHolders) {
        for t in self.data.values_mut() {
            t.set_op_lock_holders(holders.clone());
        }
        self.op_lock_holders = Some(holders);
    }

    /// Installs the cooperative cancellation token for this group.
    ///
    /// Pushed down from `mtui-core::Session` on every activation. Only explicit
    /// call-site checkpoints consult it (see [`cancel_requested`](Self::cancel_requested)).
    pub fn set_cancel_token(&mut self, cancel: CancellationToken) {
        self.cancel = cancel;
    }

    /// `true` once cooperative cancellation has been requested for this group.
    ///
    /// Polled by the long serial flows in `mtui-testreport` at their own boundaries.
    /// The parallel fan-out deliberately does **not** consult it: a host dropped
    /// mid-batch would be indistinguishable from one that ran and succeeded.
    #[must_use]
    pub fn cancel_requested(&self) -> bool {
        self.cancel.is_cancelled()
    }

    /// Swaps in a fresh (never-cancelled) token and returns the previous one.
    ///
    /// For recovery work that must run *because* the flow is stopping (the rollback
    /// downgrade), whose own per-package checkpoint would otherwise abort it at package
    /// 0. Callers restore the token afterwards.
    #[must_use = "restore the returned token once the recovery is done"]
    pub fn suspend_cancellation(&mut self) -> CancellationToken {
        std::mem::replace(&mut self.cancel, CancellationToken::new())
    }

    /// A clone of the group's cancellation token (clones share state).
    #[must_use]
    pub fn cancel_token(&self) -> CancellationToken {
        self.cancel.clone()
    }

    /// Sets the parallel-batch fan-out bound from the `[connection]`
    /// `max_parallel` config key.
    ///
    /// Pushed down by the report lifecycle. `0` leaves the fan-out primitive's default
    /// in effect.
    pub fn set_max_parallel(&mut self, max_parallel: usize) {
        self.max_parallel = max_parallel;
    }

    /// Injects the update-workflow [`PlanProvider`] (builder-style), enabling the
    /// [`OperationGroup`] surface (install / uninstall).
    ///
    /// For tests that own the group by value; production injects through
    /// [`set_plan_provider`](Self::set_plan_provider).
    #[must_use]
    pub fn with_plan_provider(mut self, provider: Arc<dyn PlanProvider>) -> Self {
        self.plan_provider = Some(provider);
        self
    }

    /// Injects the update-workflow [`PlanProvider`] in place.
    ///
    /// Called by `mtui-testreport`'s `perform_install` / `perform_uninstall`
    /// immediately before driving the [`Operation`](super::operation::Operation)
    /// template. Injecting at the point of use rather than at construction is
    /// deliberate: a `HostsGroup` is built in many places, and wiring any one can omit
    /// would silently disable `install`. Without a provider,
    /// [`OperationGroup::plans`] returns [`HostError::NoPlanProvider`].
    pub fn set_plan_provider(&mut self, provider: Arc<dyn PlanProvider>) {
        self.plan_provider = Some(provider);
    }

    /// Whether the surrounding session is the interactive REPL (spinner / prompt
    /// seam on), as opposed to a headless caller (`mtui-mcp`).
    #[must_use]
    pub const fn is_repl(&self) -> bool {
        self.is_repl
    }

    /// Reconciles the group's session mode to `is_repl` at **load time**, once, before
    /// any host is added or fan-out runs; the session is the single source of truth, not
    /// a runtime toggle.
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
    /// * `hosts = None`, `enabled = false` → a clone of the whole group.
    /// * `hosts = None`, `enabled = true` → only non-disabled hosts.
    /// * `hosts = Some([..])` → exactly those hosts (and, when `enabled`, only
    ///   the non-disabled among them).
    ///
    /// An unknown hostname is a [`HostError::NotConnected`].
    ///
    /// Selection **moves** the chosen targets out of `self` (a `Target` owns a
    /// live `Box<dyn Connection>` and is not `Clone`); `self` is consumed.
    pub fn select(self, hosts: Option<&[String]>, enabled: bool) -> Result<HostsGroup> {
        self.select_split(hosts, enabled)
            .map(|(selected, _)| selected)
    }

    /// A group over `targets` carrying this group's pushed-down session state:
    /// session mode, plan provider, `max_parallel`, cancellation token,
    /// prompter and operation-lock registry.
    fn sibling_group(&self, targets: Vec<Target>) -> HostsGroup {
        let mut group = HostsGroup::new(targets, self.is_repl);
        group.plan_provider = self.plan_provider.clone();
        group.max_parallel = self.max_parallel;
        group.cancel = self.cancel.clone();
        group.prompter = self.prompter.clone();
        group.op_lock_holders = self.op_lock_holders.clone();
        group
    }

    /// Splits the group into the `-t` selection and the unselected remainder,
    /// preserving both halves.
    ///
    /// The non-consuming counterpart to [`select`](Self::select), which **drops** the
    /// rest because a `Target` owns a live connection: the caller runs the operation
    /// over the selected subset, then [`merge`](Self::merge)s the remainder back.
    ///
    /// Selection semantics match `select`: `hosts = None` selects every host (or, when
    /// `enabled`, only non-disabled ones); `hosts = Some([..])` selects exactly those
    /// (again filtered by `enabled`). Everything else lands in the remainder. Both
    /// groups inherit `interactive`, the [`PlanProvider`], the `max_parallel` bound, the
    /// cancellation token, and the [`Prompter`](crate::Prompter).
    ///
    /// # Errors
    ///
    /// [`HostError::NotConnected`] when a named host is not a member of the group.
    pub fn select_split(
        mut self,
        hosts: Option<&[String]>,
        enabled: bool,
    ) -> Result<(HostsGroup, HostsGroup)> {
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
        for (hn, t) in std::mem::take(&mut self.data) {
            let named = hosts.is_none_or(|names| names.contains(&hn));
            if named && (!enabled || is_enabled(&t)) {
                selected.push(t);
            } else {
                remainder.push(t);
            }
        }

        Ok((self.sibling_group(selected), self.sibling_group(remainder)))
    }

    /// Folds every target of `other` into `self`, keyed by hostname, via
    /// [`add`](Self::add) (last-writer-wins on a collision; callers pass disjoint
    /// halves).
    pub fn merge(&mut self, other: HostsGroup) {
        for t in other.data.into_values() {
            self.add(t);
        }
    }

    /// Adds (or replaces) a target in the group, keyed by hostname (last-writer-wins).
    /// Connection building and autoconnect stay in the `add_host` command.
    ///
    /// When the group carries a [`Prompter`](crate::Prompter) (see
    /// [`set_prompter`](Self::set_prompter)), the incoming target inherits its
    /// command-timeout prompt, so targets moved in by [`merge`](Self::merge) or added
    /// later still surface it.
    pub fn add(&mut self, mut target: Target) {
        if let Some(prompter) = self.prompter.as_ref() {
            target.set_timeout_prompt(prompter.as_timeout_prompt());
        }
        if let Some(holders) = self.op_lock_holders.as_ref() {
            target.set_op_lock_holders(holders.clone());
        }
        self.data.insert(target.hostname().to_owned(), target);
    }

    /// Removes and returns the target named `hostname`, if present.
    ///
    /// Only detaches it: dropping a `Target` never runs [`Target::close`], so the caller
    /// (`remove_host`) must [`close`](Target::close) the returned target to release its
    /// remote locks.
    pub fn remove(&mut self, hostname: &str) -> Option<Target> {
        self.data.remove(hostname)
    }

    /// Runs a command across the group in parallel.
    ///
    /// `cmd` accepts a single string (run on every host) or a per-host
    /// [`Command::PerHost`] map (hosts absent from the map are skipped).
    pub async fn run(&mut self, cmd: impl Into<Command>) {
        actions::run_command(&mut self.data, cmd, self.is_repl, self.max_parallel).await;
    }

    /// Drives `op` over every target `select` admits, in parallel, under a
    /// spinner labelled `desc`.
    async fn fanout<'t, S, F>(&'t mut self, desc: &str, select: S, op: F)
    where
        S: FnMut(&Target) -> bool,
        F: Fn(&'t mut Target) -> BoxTargetFut<'t>,
    {
        actions::run_fanout(
            &mut self.data,
            self.is_repl,
            self.max_parallel,
            Some(desc),
            select,
            op,
        )
        .await;
    }

    /// [`fanout`](Self::fanout) where `op` yields a per-host value, inserted
    /// into the caller-owned `sink` as each host finishes; `None` leaves the
    /// host out. The caller owns the map when it applies a wall-clock budget:
    /// a returned map would be dropped with the abandoned future.
    async fn collect_fanout_into<'t, T, H, S, F>(
        &'t mut self,
        desc: &str,
        select: S,
        sink: H,
        op: F,
    ) where
        T: Send + 't,
        H: std::ops::Deref<Target = std::sync::Mutex<BTreeMap<String, T>>> + Clone + Send + 't,
        S: FnMut(&Target) -> bool,
        F: Fn(&'t mut Target) -> BoxTargetFut<'t, Option<T>>,
    {
        self.fanout(desc, select, |t| {
            let sink = sink.clone();
            let host = t.hostname().to_owned();
            let outcome = op(t);
            boxed(async move {
                if let Some(value) = outcome.await {
                    sink.lock().unwrap().insert(host, value);
                }
            })
        })
        .await;
    }

    /// [`collect_fanout_into`](Self::collect_fanout_into) into a map owned by
    /// this call, keyed by hostname so the result is deterministic regardless
    /// of completion order.
    async fn collect_fanout<'t, T, S, F>(
        &'t mut self,
        desc: &str,
        select: S,
        op: F,
    ) -> BTreeMap<String, T>
    where
        T: Send + 't,
        S: FnMut(&Target) -> bool,
        F: Fn(&'t mut Target) -> BoxTargetFut<'t, Option<T>>,
    {
        let sink = Arc::new(std::sync::Mutex::new(BTreeMap::new()));
        self.collect_fanout_into(desc, select, Arc::clone(&sink), op)
            .await;
        std::mem::take(&mut *sink.lock().unwrap())
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

    /// Reads `remote` into memory from every enabled host in parallel, each retaining
    /// at most `max_bytes` (`0` = unlimited) and reporting its file size and whether the
    /// content was cut. `scope` narrows the fan-out (see [`Scope`]).
    ///
    /// Returns one entry per *attempted* host keyed by hostname; [`Disabled`] hosts are
    /// absent. Unknown names are silently skipped; a caller that must refuse them checks
    /// [`names`](Self::names) first (the MCP `get` tool does).
    ///
    /// [`Disabled`]: super::TargetState::Disabled
    pub async fn sftp_read_capped(
        &mut self,
        remote: &Path,
        max_bytes: usize,
        scope: Scope<'_>,
    ) -> BTreeMap<String, std::result::Result<SftpRead, String>> {
        self.collect_fanout("FileDownload", in_scope(scope), |t| {
            let remote = remote.to_path_buf();
            // `None` = not attempted (disabled): the host stays out of the map.
            boxed(async move { t.sftp_read_capped(&remote, max_bytes).await })
        })
        .await
    }

    /// Uploads already-decoded bytes to `remote` on every enabled host in parallel,
    /// sharing one allocation across the fan-out.
    ///
    /// The in-band counterpart of [`sftp_put`](Self::sftp_put): the payload comes from
    /// the caller (the MCP `put` tool). Returns one entry per attempted host like
    /// [`sftp_read_capped`](Self::sftp_read_capped); `scope` narrows the fan-out (see
    /// [`Scope`]).
    pub async fn sftp_put_bytes(
        &mut self,
        data: &[u8],
        remote: &Path,
        scope: Scope<'_>,
    ) -> BTreeMap<String, std::result::Result<(), String>> {
        // One allocation shared across hosts; the caller has already size-capped the payload.
        let shared: Arc<[u8]> = Arc::from(data);
        self.collect_fanout("FileUpload", in_scope(scope), |t| {
            let shared = Arc::clone(&shared);
            let remote = remote.to_path_buf();
            boxed(async move { t.sftp_put_bytes(&shared, &remote).await })
        })
        .await
    }

    /// Locks every host in `scope` for `comment`, best-effort: a
    /// [`HostError::TargetLocked`] from a foreign-owned host is suppressed so one
    /// contended host never aborts the fan-out, and other transport errors are logged,
    /// not propagated.
    ///
    /// Returns each host's [`LockOutcome`] keyed by hostname, so a command can report real
    /// failures while leaving benign [`Contended`](LockOutcome::Contended) hosts
    /// unreported. Unknown names in `scope` are silently skipped; callers that serialize
    /// a remote transaction on a `-t` subset lock exactly that subset.
    pub async fn lock(&mut self, comment: &str, scope: Scope<'_>) -> BTreeMap<String, LockOutcome> {
        self.collect_fanout("lock", in_scope(scope), |t| {
            let comment = comment.to_owned();
            boxed(async move {
                Some(if !t.has_lock(LockKind::Op) {
                    LockOutcome::Skipped("not connected".to_owned())
                } else {
                    match t.lock(&comment).await {
                        Ok(()) => LockOutcome::Acquired,
                        Err(HostError::TargetLocked(msg)) => {
                            tracing::debug!(host = %t.hostname(), %msg, "lock: held by another owner, skipping");
                            LockOutcome::Contended(t.loaded_owner(LockKind::Op))
                        }
                        Err(e) => {
                            tracing::warn!(host = %t.hostname(), error = %e, "lock failed");
                            LockOutcome::Failed(e.to_string())
                        }
                    }
                })
            })
        })
        .await
    }

    /// Releases every host's operation lock in `scope`, best-effort, via the per-target
    /// [`Target::unlock`] (which suppresses [`HostError::TargetLocked`] for a foreign
    /// lock). Returns each host's [`LockOutcome`] keyed by hostname.
    ///
    /// A scoped unlock rolls back a partial scoped [`lock`](Self::lock): an aborted
    /// subset operation unlocks exactly the hosts it acquired.
    pub async fn unlock(&mut self, scope: Scope<'_>) -> BTreeMap<String, LockOutcome> {
        self.unlock_collecting(in_scope(scope)).await
    }

    /// Releases the named hosts' operation lock **including one owned by
    /// another user or session**, best-effort, otherwise identical to
    /// [`unlock`](Self::unlock).
    ///
    /// The fan-out behind `unlock --force`: a foreign lock is removed rather than
    /// reported as [`Contended`](LockOutcome::Contended).
    ///
    /// Outcomes land in `collected` as each host finishes, because the caller applies
    /// the wall-clock budget and a returned map would be dropped with the abandoned
    /// future.
    pub async fn unlock_force(
        &mut self,
        names: &BTreeSet<String>,
        collected: &std::sync::Mutex<BTreeMap<String, LockOutcome>>,
    ) {
        self.unlock_where(|t| names.contains(t.hostname()), true, collected)
            .await;
    }

    /// Releases only the operation locks **this group's own targets took**,
    /// best-effort, otherwise identical to [`unlock`](Self::unlock).
    ///
    /// For a caller cleaning up after an aborted operation of its own (the MCP
    /// `job_cancel` force-abort path). [`unlock`](Self::unlock) removes every lock the
    /// *process* owns, and wire ownership is per-PID, so on a refhost shared between two
    /// loaded templates or MCP sessions it would sweep up a sibling's live hold. Scoping
    /// on [`Target::holds_unmarked_operation_lock`] separates those and skips
    /// comment-marked exclusive reservations (the PI assignment lock, an operator's
    /// `lock <text>`).
    ///
    /// A host this group never locked is absent from the returned map, so `Released`
    /// means a hold really was dropped.
    ///
    /// **Residual**: two groups in one process that both locked the same host, or an
    /// operator's bare `lock` on a group that later ran an operation, are
    /// indistinguishable.
    pub async fn unlock_held(&mut self) -> BTreeMap<String, LockOutcome> {
        self.unlock_collecting(Target::holds_unmarked_operation_lock)
            .await
    }

    /// Releases the named hosts' operation lock **only where this group's own
    /// [`Target`] object took it** ([`Target::holds_operation_lock`]),
    /// best-effort, otherwise identical to [`unlock`](Self::unlock).
    ///
    /// Backs the `unlock` command's plain (no `--force`) path: a sibling [`HostsGroup`]
    /// sharing this refhost survives, which the wire's PID-based `is_mine` cannot
    /// guarantee inside one process. A comment-marked hold counts too (unlike
    /// [`unlock_held`](Self::unlock_held)), so `lock -c "reservation"` stays releasable
    /// by a plain `unlock`.
    ///
    /// A host with no built lock reports [`Skipped`](LockOutcome::Skipped)`("not
    /// connected")`; one connected but not held by this instance reports `Skipped` with a
    /// `list_locks`/`--force` pointer, never attempting the release.
    ///
    /// The in-memory hold record survives a reboot (only `/var/lock` is wiped) and both
    /// reboot paths re-apply it on reconnect, so the lock stays releasable by a plain
    /// `unlock`; a hold that could not be re-taken is reported.
    pub async fn unlock_taken(
        &mut self,
        names: &std::collections::BTreeSet<String>,
    ) -> BTreeMap<String, LockOutcome> {
        self.collect_fanout(
            "unlock",
            |t| names.contains(t.hostname()),
            |t| {
                boxed(async move {
                    Some(if !t.has_lock(LockKind::Op) {
                        LockOutcome::Skipped("not connected".to_owned())
                    } else if !t.holds_operation_lock() {
                        LockOutcome::Skipped(match t.sibling_operation_holder() {
                            Some(label) => format!(
                                "held by {}; release it there",
                                super::sibling_holder_name(&label)
                            ),
                            None => {
                                "not locked by this session; check list_locks, or unlock --force"
                                    .to_owned()
                            }
                        })
                    } else {
                        release_outcome(t.unlock_reporting(LockKind::Op, false).await, || {
                            t.loaded_owner(LockKind::Op)
                        })
                    })
                })
            },
        )
        .await
    }

    /// [`unlock_where`](Self::unlock_where) with `force = false` and a local
    /// map, for the callers that just want the outcomes back.
    async fn unlock_collecting<S>(&mut self, select: S) -> BTreeMap<String, LockOutcome>
    where
        S: Fn(&Target) -> bool + Send + Sync,
    {
        let collected = std::sync::Mutex::new(BTreeMap::new());
        self.unlock_where(select, false, &collected).await;
        collected.into_inner().unwrap()
    }

    /// Shared implementation for [`unlock`](Self::unlock) /
    /// [`unlock_force`](Self::unlock_force). `force` also removes a foreign-owned lock.
    ///
    /// Without `force`, a comment-marked hold of this session's own (an operator's
    /// `lock -c` reservation) is [`Skipped`](LockOutcome::Skipped): only the explicit
    /// [`unlock_taken`](Self::unlock_taken) may drop a reservation.
    async fn unlock_where<S>(
        &mut self,
        select: S,
        force: bool,
        collected: &std::sync::Mutex<BTreeMap<String, LockOutcome>>,
    ) where
        S: Fn(&Target) -> bool + Send + Sync,
    {
        self.collect_fanout_into("unlock", select, collected, |t| {
            boxed(async move {
                Some(if !t.has_lock(LockKind::Op) {
                    LockOutcome::Skipped("not connected".to_owned())
                } else if !force && t.holds_operation_lock() && !t.holds_unmarked_operation_lock() {
                    LockOutcome::Skipped("reservation kept".to_owned())
                } else {
                    release_outcome(t.unlock_reporting(LockKind::Op, force).await, || {
                        t.loaded_owner(LockKind::Op)
                    })
                })
            })
        })
        .await;
    }

    /// Releases the named hosts' pool claim into the caller-owned `collected`,
    /// best-effort. A claim owned by another template lands as
    /// [`Contended`](LockOutcome::Contended); `force` removes those too. The map is
    /// caller-owned for the same reason as [`unlock_force`](Self::unlock_force).
    pub async fn pool_unlock_collecting(
        &mut self,
        force: bool,
        names: &BTreeSet<String>,
        collected: &std::sync::Mutex<BTreeMap<String, LockOutcome>>,
    ) {
        self.collect_fanout_into(
            "pool_unlock",
            |t| names.contains(t.hostname()),
            collected,
            |t| {
                boxed(async move {
                    Some(if !t.has_lock(LockKind::Pool) {
                        LockOutcome::Skipped("not connected".to_owned())
                    } else {
                        release_outcome(t.unlock_reporting(LockKind::Pool, force).await, || {
                            t.loaded_owner(LockKind::Pool)
                        })
                    })
                })
            },
        )
        .await;
    }

    /// Disconnects every host, optionally rebooting or powering them off.
    ///
    /// Delegates to [`Target::close`], which best-effort unlocks the operation and pool
    /// locks, then reboots (`Some("reboot")`), powers off (`Some("poweroff")` → shell
    /// `halt`), or closes (`None`). Unlike [`reboot`](Self::reboot) it never
    /// reconnects; the caller applies the overall wait budget.
    ///
    /// Returns each host's teardown outcome keyed by hostname, so `quit` can name a host
    /// that failed to disconnect.
    pub async fn close(&mut self, action: Option<&str>) -> BTreeMap<String, Result<()>> {
        let collected = std::sync::Mutex::new(BTreeMap::new());
        self.close_collecting(action, &collected).await;
        collected.into_inner().unwrap()
    }

    /// As [`close`](Self::close), but outcomes land in `collected` as each host finishes,
    /// for a caller that applies a wall-clock budget (see
    /// [`unlock_force`](Self::unlock_force)).
    pub async fn close_collecting(
        &mut self,
        action: Option<&str>,
        collected: &std::sync::Mutex<BTreeMap<String, Result<()>>>,
    ) {
        self.collect_fanout_into(
            "close",
            |_t| true,
            collected,
            |t| boxed(async move { Some(t.close(action).await) }),
        )
        .await;
    }

    /// Reports the lock state of every host to `sink`, in sorted hostname order: the
    /// operation lock, or the pool-claim lock when `pool` is `true`, via
    /// [`Target::lock_status`](Target::lock_status). Best-effort per host: a read failure
    /// resolves to the unlocked row.
    pub async fn report_locks<F>(&mut self, mut sink: F, pool: bool)
    where
        F: FnMut(&str, &System, &LockRow),
    {
        // Phase 1 (I/O): resolve every host's lock row concurrently; the
        // drain below is sorted by hostname regardless of completion order.
        let collected = self
            .collect_fanout(
                "report_locks",
                |_t| true,
                |t| {
                    boxed(async move {
                        let row = t.lock_status(pool).await;
                        Some((t.system().clone(), row))
                    })
                },
            )
            .await;

        // Phase 2 (pure): forward each host to the sink in sorted hostname order.
        for (hostname, (system, row)) in collected {
            sink(&hostname, &system, &row);
        }
    }

    /// Installs the session-level serialised [`Prompter`](crate::Prompter) on the group
    /// and every member host, so targets connected before or after this call surface the
    /// command-timeout prompt. Called by `mtui-core::Session` when a prompter is present;
    /// headless callers (`mtui-mcp`) never do, leaving a timeout an immediate abort.
    pub fn set_prompter(&mut self, prompter: crate::Prompter) {
        let timeout_prompt = prompter.as_timeout_prompt();
        for target in self.data.values_mut() {
            target.set_timeout_prompt(timeout_prompt.clone());
        }
        self.prompter = Some(prompter);
    }

    /// Fans a repository add/remove out across every host in `scope`, in parallel.
    ///
    /// `report` is the object-safe [`SetRepo`] hook the caller passes (the concrete
    /// `set_repo` impl in `mtui-testreport`), so the group never depends on the report
    /// crate. Per-host `last*` state is left in place so a caller can inspect `lasterr()`
    /// (prepare aborts on it). A scope serves a flow that has already excluded a host:
    /// reconfiguring its repositories would break that exclusion.
    pub async fn fanout_set_repo(
        &mut self,
        operation: RepoOp,
        report: &dyn SetRepo,
        scope: Scope<'_>,
    ) {
        self.fanout("set_repo", in_scope(scope), |t| {
            boxed(async move { t.repo_manager().set(operation, report).await })
        })
        .await;
    }

    /// Appends a history entry to the remote history file of every member target in
    /// `scope` (enabled hosts only, best-effort).
    ///
    /// A flow whose work reached only some hosts scopes the row: `prepare` drops hosts it
    /// did not process, and a `:prepare:` row for them would put a false entry in
    /// `/var/log/mtui.log`, an interop contract other tools read. Names not in the group
    /// are ignored.
    pub async fn add_history(&mut self, fields: &[String], scope: Scope<'_>) {
        self.fanout("add_history", in_scope(scope), |t| {
            let fields = fields.to_vec();
            boxed(async move { t.add_history(&fields).await })
        })
        .await;
    }

    /// Queries the installed package versions of every host in `scope` concurrently:
    /// the I/O phase shared by [`package_check`](Self::package_check) and the downgrade
    /// verdict. Names not in the group are ignored.
    pub async fn query_versions(&mut self, scope: Scope<'_>) {
        self.fanout("query_versions", in_scope(scope), |t| {
            boxed(async move { t.query_versions().await })
        })
        .await;
    }

    /// Queries every host's tracked package versions and logs update-sanity warnings.
    ///
    /// The `package_check` step of the update workflow, over the hosts in `scope` (a
    /// host outside it never earns a warning for work it never received). After
    /// [`Target::query_versions`] populates each package's `current` version,
    /// `post == false` records it as `before` and `post == true` as `after`. Warnings:
    ///
    /// * *too recent* — pre-update installed version is already `>=` the required version,
    /// * *not updated* — `after == before` on the post-pass,
    /// * *below required* — post-update version is `<` the required version,
    /// * *missing* — before check is [`VersionCheck::NotInstalled`], logged once per host,
    /// * *unchecked* — before check is [`VersionCheck::NotChecked`] (the query never
    ///   answered), logged separately since it needs a different operator action.
    ///
    /// The packages must already be [`seeded`](Target::set_packages) with their
    /// `required` versions.
    pub async fn package_check(&mut self, post: bool, scope: Scope<'_>) {
        let select = in_scope(scope);
        // I/O: query the selected hosts' installed versions concurrently.
        self.query_versions(scope).await;

        // Pure: fold queried versions into before/after and emit the warnings.
        for target in self.data.values_mut().filter(|t| select(t)) {
            let hostname = target.hostname().to_owned();

            let mut not_installed: Vec<String> = Vec::new();
            let mut unchecked: Vec<String> = Vec::new();
            for pkg in target.packages_mut() {
                let required = pkg.required().cloned();
                let current_check = pkg.current_check().clone();
                if post {
                    pkg.set_after_check(current_check);
                } else {
                    pkg.set_before_check(current_check);
                }
                let before = pkg.before().cloned();
                let after = if post { pkg.after().cloned() } else { None };

                match pkg.before_check() {
                    VersionCheck::NotInstalled => not_installed.push(pkg.name.clone()),
                    VersionCheck::NotChecked => unchecked.push(pkg.name.clone()),
                    VersionCheck::Installed(before_v) => {
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
            if !unchecked.is_empty() {
                tracing::warn!(
                    host = %hostname, packages = %unchecked.join(", "),
                    "the version query never answered for these packages"
                );
            }
        }
    }

    /// Acquires the shared operation lock across every host in `scope`.
    ///
    /// **Fails closed**: `Ok` means *every* host is verifiably locked by this process.
    /// Any per-host failure (a foreign lock, `TargetLocked` contention on acquire, or an
    /// SFTP/transport error reading or writing the lock) marks the group contended; a
    /// host whose state cannot be read is treated as "not ours". On contention the locks
    /// we did take are released (best-effort, over the same scope) and
    /// [`HostError::HostsLocked`] is returned.
    ///
    /// A scope serves a flow that has already excluded a host: the fail-closed rule
    /// would otherwise let one excluded host abort the operation for every eligible
    /// peer. Names not in the group are ignored.
    ///
    /// # Errors
    ///
    /// Returns [`HostError::HostsLocked`] when one or more hosts were locked by
    /// another owner or could not be locked/read.
    pub async fn update_lock(&mut self, scope: Scope<'_>) -> Result<()> {
        use std::sync::Mutex;

        let select = in_scope(scope);

        // Probe-and-acquire concurrently; a skipped host pushes its reason onto the shared
        // `reasons` list.
        let reasons: Mutex<Vec<LockRefusal>> = Mutex::new(Vec::new());
        self.fanout("update_lock", select, |target| {
            let reasons = &reasons;
            boxed(async move {
                let host = target.hostname().to_owned();
                let refusal = |reason: String| LockRefusal {
                    host: host.clone(),
                    reason,
                };
                // A read failure means the host's state is unknown: a skip, never an assumption
                // that it is free.
                let refused = match target.foreign_operation_lock().await {
                    Err(e) => {
                        tracing::warn!(host = %host, error = %e, "update_lock: lock state unreadable; skipping");
                        Some(refusal(format!("lock state unreadable: {e}")))
                    }
                    Ok(Some(LockOwner { by, since, .. })) => {
                        tracing::warn!(host = %host, since = %since, by = %by, "host is locked; skipping");
                        Some(refusal(format!("held by {by} since {since}")))
                    }
                    // Contention or an SFTP/transport error means we do NOT own this host, so the
                    // group must abort.
                    Ok(None) => match target.lock("").await {
                        Ok(()) => None,
                        Err(HostError::TargetLocked(msg)) => {
                            tracing::debug!(host = %host, %msg, "update_lock: held by another owner");
                            Some(refusal(msg))
                        }
                        Err(e) => {
                            tracing::warn!(host = %host, error = %e, "update_lock: lock failed");
                            Some(refusal(e.to_string()))
                        }
                    },
                };
                if let Some(refusal) = refused {
                    reasons.lock().expect("reasons mutex").push(refusal);
                }
            })
        })
        .await;

        let mut reasons = reasons.into_inner().expect("reasons mutex");
        if !reasons.is_empty() {
            // Release the locks we did take, then abort. Sort first: the concurrent fan-out
            // collects in nondeterministic order.
            reasons.sort_by_cached_key(ToString::to_string);
            let _ = self.unlock_collecting(select).await;
            return Err(HostError::HostsLocked(reasons));
        }
        Ok(())
    }

    /// Reboots only the named hosts and reconnects each, verifying the reboot.
    ///
    /// Captures each host's `boot_id`, dispatches `command` fire-and-forget (the reboot
    /// drops the connection), reconnects each host (sorted) with retry + backoff, and
    /// verifies the boot id changed (`verify_reboot`). A non-empty `relock_comment`
    /// re-applies the lock afterwards, since a reboot clears `/var/lock` (tmpfs). Works
    /// for transactional and non-transactional hosts; a no-op for an empty group. All
    /// phases and the relock restrict to `names`, honouring `-t/--target`, so an
    /// operator never reboots hosts they did not ask for.
    ///
    /// Returns a [`RebootReport`] with an outcome per named host that exists in the group
    /// (unknown names are skipped), keyed by hostname: `Ok(())` when the host rebooted
    /// (boot id changed, or could not be confirmed) and reconnected; `Err(HostError)`
    /// when the reconnect failed or the boot id was **unchanged**. A reconnect failure
    /// takes precedence over a later boot-id verdict. The report also carries the holds
    /// the reboot wiped that could not be re-taken.
    pub async fn reboot_selected(
        &mut self,
        command: &str,
        relock_comment: &str,
        names: &std::collections::BTreeSet<String>,
    ) -> RebootReport {
        self.reboot_where(command, relock_comment, |t| names.contains(t.hostname()))
            .await
    }

    /// Snapshots this session's `/var/lock` holds for the selected hosts, before the
    /// relock overwrites the in-memory record, so the reboot paths can re-apply what
    /// the reboot wiped.
    fn snapshot_holds<S>(&self, select: &S) -> BTreeMap<String, HeldHolds>
    where
        S: Fn(&Target) -> bool,
    {
        self.data
            .values()
            .filter(|t| select(t))
            .map(|t| {
                (
                    t.hostname().to_owned(),
                    HeldHolds {
                        op: t.held_comment(LockKind::Op),
                        pool: t.held_comment(LockKind::Pool),
                    },
                )
            })
            .collect()
    }

    /// Re-applies the holds a reboot wiped (`/var/lock` is tmpfs): for every selected
    /// host whose reconnect succeeded, re-take the operation lock and pool claim
    /// recorded in `held`.
    ///
    /// Re-taking is idempotent and never overwrites someone else's lock, so the gate is
    /// the reconnect, not the verified reboot. A hold proven gone (taken by someone else
    /// in the gap) is collected into `lost` and its record dropped; it never fails the
    /// run. A re-take that fails with a transport error or finds a corrupt line is
    /// collected too, but its record is kept: the hold may still be ours, and dropping a
    /// corrupt line would strand it under a later plain `unlock`'s skip. Hosts that
    /// failed to reconnect, or held nothing, are untouched.
    async fn reapply_holds<S>(
        &mut self,
        select: &S,
        reconnected: &std::sync::Mutex<BTreeSet<String>>,
        held: &BTreeMap<String, HeldHolds>,
        lost: &std::sync::Mutex<Vec<LostHold>>,
    ) where
        S: Fn(&Target) -> bool + Send + Sync,
    {
        self.fanout("relock", select, |t| {
            let (held, lost, reconnected) = (held, lost, reconnected);
            boxed(async move {
                if !reconnected.lock().unwrap().contains(t.hostname()) {
                    return;
                }
                let Some(HeldHolds { op, pool }) = held.get(t.hostname()) else {
                    return;
                };
                if let Some(comment) = op {
                    match t.lock(comment).await {
                        Ok(()) => {}
                        Err(HostError::TargetLocked(reason)) => {
                            record_hold_loss(
                                t,
                                lost,
                                LostHoldKind::OperationLock,
                                comment.clone(),
                                reason,
                                LostHoldProof::Proven,
                            );
                        }
                        Err(e) => {
                            record_hold_loss(
                                t,
                                lost,
                                LostHoldKind::OperationLock,
                                comment.clone(),
                                format!("{e}; the host may be unlocked"),
                                LostHoldProof::Unproven,
                            );
                        }
                    }
                }
                if let Some(comment) = pool {
                    match t.pool_claim(comment).await {
                        Ok(true) => {}
                        Ok(false) => {
                            let owner = t.loaded_owner(LockKind::Pool);
                            record_hold_loss(
                                t,
                                lost,
                                LostHoldKind::PoolClaim,
                                comment.clone(),
                                if owner.by.is_empty() {
                                    "pool claim held by another template".to_owned()
                                } else {
                                    format!("pool claim held by {} since {}", owner.by, owner.since)
                                },
                                LostHoldProof::Proven,
                            );
                        }
                        Err(e) => {
                            record_hold_loss(
                                t,
                                lost,
                                LostHoldKind::PoolClaim,
                                comment.clone(),
                                format!("{e}; the host may be unlocked"),
                                LostHoldProof::Unproven,
                            );
                        }
                    }
                }
            })
        })
        .await;
    }

    /// Shared implementation behind [`reboot_selected`](Self::reboot_selected):
    /// the `select` predicate is handed to every phase (and to the relock).
    async fn reboot_where<S>(
        &mut self,
        command: &str,
        relock_comment: &str,
        select: S,
    ) -> RebootReport
    where
        S: Fn(&Target) -> bool + Send + Sync,
    {
        if self.data.is_empty() {
            tracing::info!("No hosts to reboot");
            return RebootReport::default();
        }
        let names: Vec<String> = self
            .data
            .values()
            .filter(|t| select(t))
            .map(|t| t.hostname().to_owned())
            .collect();
        if names.is_empty() {
            tracing::info!("No selected hosts to reboot");
            return RebootReport::default();
        }
        tracing::info!(hosts = %names.join(", "), "Rebooting");
        self.reboot_phases(select, |_host| command.to_owned(), relock_comment)
            .await
    }

    /// Reboots the *transactional* hosts named in `reboot` and reconnects each.
    ///
    /// Each host contributes a reboot command from the operation's doer, dispatched
    /// fire-and-forget, then reconnected (sorted) with retry + backoff and the same
    /// boot-id verification as [`reboot`](Self::reboot). A no-op when the map is empty.
    ///
    /// Returns each named host's outcome plus the holds the reboot wiped that could not
    /// be re-taken. `Ok(())` means "nothing contradicted it", not proof: an unreadable
    /// boot id (see `verify_boot_id`) leaves the host unverified and passing, logged at
    /// WARN, because failing open beats reverting a fleet on a probe that timed out.
    ///
    /// Three distinct failures are recorded, and the *cause* matters to the
    /// caller because only two of them leave a repairable host:
    ///
    /// * [`RebootNotDispatched`](HostError::RebootNotDispatched) — the command
    ///   never reached the host. It is still up, on the old snapshot.
    /// * [`RebootDidNotHappen`](HostError::RebootDidNotHappen) — it answered
    ///   with an unchanged boot id, so it never went down. Still up, new
    ///   snapshot inert.
    /// * [`ReconnectFailed`](HostError::ReconnectFailed) — it went away and did
    ///   not come back. Unreachable.
    ///
    /// Earliest phase wins: a host that could not be sent the reboot is
    /// reported as such rather than as whatever the later phases make of it,
    /// since those verdicts are consequences of the first failure. The one
    /// exception is a failed reconnect, which outranks a dispatch failure.
    async fn reboot_transactional(&mut self, reboot: &BTreeMap<String, String>) -> RebootReport {
        if reboot.is_empty() {
            return RebootReport::default();
        }
        // Every phase shares this predicate and skips a **disabled** host: it was
        // excluded from the command fan-out, so it staged no snapshot, and rebooting it
        // restarts a machine the operator took out of the run. Skipping it in only some
        // phases would make the boot-id probes read the same id twice and report a host
        // that "never rebooted", which `update` routes a rollback on.
        let selected = |t: &Target| {
            reboot.contains_key(t.hostname())
                && t.state() != mtui_types::enums::TargetState::Disabled
        };
        let mut names: Vec<&String> = reboot.keys().collect();
        names.sort();
        tracing::info!(
            hosts = %names.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", "),
            "Rebooting transactional hosts"
        );
        self.reboot_phases(
            selected,
            |host| reboot.get(host).cloned().unwrap_or_default(),
            "",
        )
        .await
    }

    /// The reboot lifecycle both paths share: boot-id snapshot, dispatch, reconnect,
    /// boot-id verification, optional relock, hold re-apply.
    ///
    /// `command_for` yields each selected host's reboot command; a non-empty
    /// `relock_comment` re-locks the selected hosts that held no operation lock. Outcomes
    /// are collected per phase so the map is deterministic. A host that could not be sent
    /// the reboot is reported as such unless its reconnect also failed, which outranks
    /// it; a verify verdict never replaces an earlier failure.
    async fn reboot_phases<S, C>(
        &mut self,
        select: S,
        command_for: C,
        relock_comment: &str,
    ) -> RebootReport
    where
        S: Fn(&Target) -> bool + Send + Sync,
        C: Fn(&str) -> String,
    {
        use std::sync::Mutex;

        // Snapshot this session's holds before the reboot wipes `/var/lock` and before
        // the relock overwrites the in-memory record.
        let held = self.snapshot_holds(&select);

        // Phase 1: record boot ids before rebooting (concurrently), so phase 4
        // can tell a real reboot from one that was accepted and did nothing.
        let old_boot_ids = self
            .collect_fanout("boot_id", &select, |t| {
                boxed(async move { Some(t.boot_id().await) })
            })
            .await;

        let outcomes: Mutex<BTreeMap<String, Result<()>>> = Mutex::new(BTreeMap::new());

        // Fire the reboot on every host. A failed dispatch leaves the session live, so
        // the reconnect would succeed trivially and report a reboot never received.
        self.fanout("reboot", &select, |t| {
            let command = command_for(t.hostname());
            let outcomes = &outcomes;
            boxed(async move {
                if let Err(e) = t.reboot(&command).await {
                    outcomes
                        .lock()
                        .unwrap()
                        .insert(t.hostname().to_owned(), Err(e));
                }
            })
        })
        .await;

        // Reconnect every host concurrently with retry + backoff. The re-apply gate
        // reads this set, not the verified outcomes: a host that came back has a wiped
        // `/var/lock` regardless, and re-taking is idempotent.
        let reconnected: Mutex<BTreeSet<String>> = Mutex::new(BTreeSet::new());
        self.fanout("reconnect", &select, |t| {
            let outcomes = &outcomes;
            let reconnected = &reconnected;
            boxed(async move {
                let hostname = t.hostname().to_owned();
                let retry = t.reboot_retries();
                let outcome = match t.reconnect(retry, true).await {
                    Ok(()) => {
                        tracing::info!(host = %hostname, "is back up");
                        reconnected.lock().unwrap().insert(hostname.clone());
                        Ok(())
                    }
                    Err(e) => {
                        tracing::error!(host = %hostname, error = %e, "reconnect after reboot failed");
                        Err(e)
                    }
                };
                let mut map = outcomes.lock().unwrap();
                // Precedence is deliberately not plain earliest-wins: a failed reconnect is the
                // only direct evidence of reachability, which decides whether the caller runs a
                // destructive rollback, so it outranks the dispatch diagnosis (a link dead before
                // dispatch yields a `Transport` error there that would claim the host is still
                // up). A *successful* reconnect must not overwrite a dispatch failure: it
                // succeeded only because the session was never torn down.
                match (&outcome, map.get(&hostname)) {
                    (Err(_), _) | (Ok(()), None) => {
                        map.insert(hostname, outcome);
                    }
                    (Ok(()), Some(_)) => {}
                }
            })
        })
        .await;

        // Verify each host's boot id changed. Only a host that got here cleanly is
        // downgraded; an earlier failure is more fundamental and is preserved.
        self.fanout("verify_reboot", &select, |t| {
            let old = old_boot_ids.get(t.hostname()).cloned().unwrap_or_default();
            let outcomes = &outcomes;
            boxed(async move {
                let hostname = t.hostname().to_owned();
                let new_boot_id = t.boot_id().await;
                if Self::verify_boot_id(&hostname, &old, &new_boot_id).is_err() {
                    let mut map = outcomes.lock().unwrap();
                    if !matches!(map.get(&hostname), Some(Err(_))) {
                        map.insert(
                            hostname.clone(),
                            Err(HostError::RebootDidNotHappen { host: hostname }),
                        );
                    }
                }
            })
        })
        .await;

        if !relock_comment.is_empty() {
            tracing::info!("Re-applying lock after reboot");
            // Re-lock only the rebooted hosts (a reboot clears `/var/lock`). Hosts already
            // holding an operation lock are re-taken with loss reporting below, so stamping
            // them here would be an extra SFTP write and a transient wrong stamp.
            let relock: BTreeSet<String> = held
                .iter()
                .filter(|(_, holds)| holds.op.is_none())
                .map(|(host, _)| host.clone())
                .collect();
            if !relock.is_empty() {
                let _ = self.lock(relock_comment, Some(&relock)).await;
            }
        }

        // Re-apply the holds the reboot wiped for every host whose reconnect succeeded;
        // a hold that could not be re-taken lands in `lost_holds` and never fails the
        // run.
        let lost: Mutex<Vec<LostHold>> = Mutex::new(Vec::new());
        self.reapply_holds(&select, &reconnected, &held, &lost)
            .await;

        RebootReport {
            outcomes: outcomes.into_inner().unwrap(),
            lost_holds: lost.into_inner().unwrap(),
        }
    }

    /// Decides whether `hostname`'s boot id confirms a reboot, from the two values
    /// (I/O-free, so the reads fan out concurrently). `/proc/sys/kernel/random/boot_id`
    /// is regenerated on every boot.
    ///
    /// An **unchanged** non-empty id is a definite failure (`Err`, logged at ERROR); a
    /// **missing** (empty) old or new id is not a failure (`Ok`, logged at WARN) since
    /// the reboot could not be confirmed either way; a **changed** id is success.
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

/// Folds a release result into a [`LockOutcome`]; a foreign owner is benign
/// contention, named from the load the failed release already did.
fn release_outcome(result: Result<()>, owner: impl FnOnce() -> LockOwner) -> LockOutcome {
    match result {
        Ok(()) => LockOutcome::Released,
        Err(HostError::TargetLocked(_)) => LockOutcome::Contended(owner()),
        Err(e) => LockOutcome::Failed(e.to_string()),
    }
}

/// Classifies a reboot failure for the [`OperationGroup`] seam.
///
/// An unrecognised error is deliberately [`Unreachable`](RebootFailureCause::Unreachable),
/// the arm that *suppresses* the automatic rollback: that downgrade is too
/// destructive to trigger on an unclassified error.
fn reboot_failure_cause(err: &HostError) -> RebootFailureCause {
    match err {
        HostError::RebootNotDispatched { .. } => RebootFailureCause::NotDispatched,
        HostError::RebootDidNotHappen { .. } => RebootFailureCause::NotRebooted,
        _ => RebootFailureCause::Unreachable,
    }
}

/// Drives the install/uninstall [`Operation`](super::operation::Operation) template
/// against this group: the concrete binding of the [`OperationGroup`] seam.
///
/// It resolves each target's [`Doer`](super::operation::Doer) /
/// [`Check`](super::operation::Check) through the injected [`PlanProvider`] and
/// delegates command/reboot fan-out to [`HostsGroup::run`] via a
/// [`Command::PerHost`] map. `update_lock` / `unlock` / `reboot` delegate to the
/// inherent [`HostsGroup`] methods; `update_lock` returns
/// [`HostError::HostsLocked`] if any host is locked by another owner (after releasing
/// what it took), which aborts the run.
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
            // A host without a `Target::workflow_key` (unknown system or undecided root mode)
            // surfaces as the role's Missing*Error.
            let (release, transactional) = target.workflow_key().ok_or_else(|| {
                HostError::missing_for(role, target.system().get_release().unwrap_or_default())
            })?;
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
        HostsGroup::update_lock(self, None).await
    }

    async fn run(&mut self, commands: HostCommandMap) {
        let map: BTreeMap<String, String> = commands.into_iter().collect();
        HostsGroup::run(self, Command::PerHost(map)).await;
    }

    async fn add_history(&mut self, fields: &[String]) {
        HostsGroup::add_history(self, fields, None).await;
    }

    fn last_output(&self, hostname: &str) -> Option<super::operation::HostOutput> {
        let target = self.data.get(hostname)?;
        Some(super::operation::HostOutput {
            stdout: target.lastout().to_owned(),
            stdin: target.lastin().to_owned(),
            stderr: target.lasterr().to_owned(),
            exitcode: target.lastexit().map_or(0, i32::from),
        })
    }

    async fn reboot(&mut self, reboot: HostCommandMap) -> RebootReport {
        // Only transactional hosts contribute reboot entries.
        let map: BTreeMap<String, String> = reboot.into_iter().collect();
        HostsGroup::reboot_transactional(self, &map).await
    }

    async fn unlock(&mut self) -> BTreeMap<String, LockOutcome> {
        HostsGroup::unlock(self, None).await
    }
}

#[cfg(test)]
mod tests {
    use mtui_types::enums::TargetState;
    use mtui_types::hostlog::CommandLog;

    use super::*;
    use crate::connection::Connection;
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

    /// The outcome the `1700000000:alice:4242:busy` lock fixture must produce:
    /// contention carries the owner out of the load the failure already did
    /// (#521), so a caller can name them.
    fn contended_by_alice() -> LockOutcome {
        LockOutcome::Contended(LockOwner {
            by: "alice".to_owned(),
            since: "Tuesday, 14.11.2023 22:13 UTC".to_owned(),
            ..Default::default()
        })
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
    fn select_split_carries_max_parallel_and_cancel_and_prompter() {
        // Regression: `select_split` used to copy only `plan_provider`, so a
        // `-t` subset silently reverted `max_parallel` to the fan-out default
        // and would have dropped the cancellation token with it.
        let mut g = HostsGroup::new(vec![enabled("h1"), enabled("h2")], true);
        g.set_max_parallel(7);
        let token = g.cancel_token();
        g.set_prompter(noop_prompter());

        let (sel, rem) = g
            .select_split(Some(&["h1".to_owned()]), true)
            .expect("split succeeds");

        assert_eq!(sel.max_parallel, 7, "selected half keeps the bound");
        assert_eq!(rem.max_parallel, 7, "remainder keeps the bound");
        // Both halves share the ORIGINAL token, so a cancel reaches them.
        assert!(!sel.cancel_requested());
        token.cancel();
        assert!(sel.cancel_requested(), "selected half observes the cancel");
        assert!(rem.cancel_requested(), "remainder observes the cancel");
        // Both halves, not just one: asserting only `sel` would not catch a fix
        // that carried the prompter to `selected` but dropped it from `remainder`.
        assert!(sel.prompter.is_some(), "selected half keeps the prompter");
        assert!(rem.prompter.is_some(), "remainder keeps the prompter");
    }

    #[test]
    fn select_carries_max_parallel_and_cancel_and_prompter() {
        let mut g = HostsGroup::new(vec![enabled("h1"), enabled("h2")], true);
        g.set_max_parallel(3);
        let token = g.cancel_token();
        g.set_prompter(noop_prompter());

        let sel = g.select(Some(&["h1".to_owned()]), true).expect("select");

        assert_eq!(sel.max_parallel, 3);
        token.cancel();
        assert!(sel.cancel_requested());
        assert!(sel.prompter.is_some(), "selected group keeps the prompter");
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

    /// The history fan-out is never gated on cancellation.
    ///
    /// The flows write their `/var/log/mtui.log` row *after* the work landed on
    /// the host, which for a cancelled operation means after the token was
    /// cancelled — an `update` stopped at its pre-dispatch gate still owes a row
    /// for the packages its prepare installed (#407). A cancel check here (or in
    /// `run_fanout`, whose own contract forbids one) would suppress the record
    /// exactly when an operator needs it: after a `job_cancel`.
    #[tokio::test]
    async fn add_history_still_writes_after_the_token_is_cancelled() {
        let conn = echo("h1");
        let handle = conn.clone();
        let mut g = HostsGroup::new(
            vec![Target::with_connection(
                "h1",
                TargetState::Enabled,
                Box::new(conn),
            )],
            false,
        );
        g.cancel_token().cancel();
        assert!(g.cancel_requested(), "the group observes the cancel");

        g.add_history(&["prepare".to_owned(), "pkg-a".to_owned()], None)
            .await;

        let written = String::from_utf8(
            handle
                .file_contents("/var/log/mtui.log")
                .expect("a cancelled group still records its history row"),
        )
        .unwrap();
        assert!(
            written.ends_with(":prepare:pkg-a\n"),
            "history line: {written:?}"
        );
    }

    /// `add_history_for` writes to the named hosts and to no others.
    ///
    /// The distinction the group-wide [`HostsGroup::add_history`] cannot make:
    /// `prepare` builds a per-host command map, so a host it never dispatched
    /// to must not be handed a row claiming an install (#407). Both directions
    /// are asserted — a predicate stuck at `true` fails on h2, one stuck at
    /// `false` fails on h1.
    #[tokio::test]
    async fn add_history_for_writes_only_to_the_named_hosts() {
        let (c1, c2) = (echo("h1"), echo("h2"));
        let (h1, h2) = (c1.clone(), c2.clone());
        let mut g = HostsGroup::new(
            vec![
                Target::with_connection("h1", TargetState::Enabled, Box::new(c1)),
                Target::with_connection("h2", TargetState::Enabled, Box::new(c2)),
            ],
            false,
        );

        let only_h1: BTreeSet<String> = ["h1".to_owned()].into_iter().collect();
        g.add_history(&["prepare".to_owned(), "pkg-a".to_owned()], Some(&only_h1))
            .await;

        let written = String::from_utf8(
            h1.file_contents("/var/log/mtui.log")
                .expect("the named host gets its row"),
        )
        .unwrap();
        assert!(
            written.ends_with(":prepare:pkg-a\n"),
            "history line: {written:?}"
        );
        assert!(
            h2.file_contents("/var/log/mtui.log").is_none(),
            "an unnamed host must not be written to: {:?}",
            h2.file_contents("/var/log/mtui.log")
                .map(|b| String::from_utf8_lossy(&b).into_owned())
        );
    }

    // --- sftp_read / sftp_put_bytes map fan-outs (#434) --------------------

    /// Per-host DISTINCT content: identical fixtures could pass with the
    /// fan-out returning one host's bytes for every key.
    #[tokio::test]
    async fn sftp_read_capped_fans_out_with_distinct_content() {
        let m1 = MockConnection::new("h1").with_file("/remote/f", b"alpha".to_vec());
        let m2 = MockConnection::new("h2").with_file("/remote/f", b"beta".to_vec());
        let mut g = HostsGroup::new(
            vec![
                Target::with_connection("h1", TargetState::Enabled, Box::new(m1)),
                Target::with_connection("h2", TargetState::Enabled, Box::new(m2)),
            ],
            false,
        );

        let got = g.sftp_read_capped(Path::new("/remote/f"), 0, None).await;

        assert_eq!(got.len(), 2);
        assert_eq!(got["h1"].as_ref().unwrap().bytes, b"alpha");
        assert_eq!(got["h2"].as_ref().unwrap().bytes, b"beta");
    }

    #[tokio::test]
    async fn sftp_read_capped_mixed_outcomes() {
        let ok = MockConnection::new("h1").with_file("/remote/f", b"alpha".to_vec());
        // h2: no seeded file -> the mock serves SftpNotFound.
        let bad = MockConnection::new("h2");
        let off = MockConnection::new("h3").with_file("/remote/f", b"gamma".to_vec());
        let mut g = HostsGroup::new(
            vec![
                Target::with_connection("h1", TargetState::Enabled, Box::new(ok)),
                Target::with_connection("h2", TargetState::Enabled, Box::new(bad)),
                Target::with_connection("h3", TargetState::Disabled, Box::new(off)),
            ],
            false,
        );

        let got = g.sftp_read_capped(Path::new("/remote/f"), 0, None).await;

        assert_eq!(got["h1"].as_ref().unwrap().bytes, b"alpha");
        assert!(got["h2"].is_err(), "h2 must carry its failure: {got:?}");
        assert!(
            !got.contains_key("h3"),
            "disabled host must be absent: {got:?}"
        );
    }

    #[tokio::test]
    async fn sftp_read_capped_scoped_only_touches_named() {
        let m1 = MockConnection::new("h1").with_file("/remote/f", b"alpha".to_vec());
        let m2 = MockConnection::new("h2").with_file("/remote/f", b"beta".to_vec());
        let h2 = m2.clone();
        let mut g = HostsGroup::new(
            vec![
                Target::with_connection("h1", TargetState::Enabled, Box::new(m1)),
                Target::with_connection("h2", TargetState::Enabled, Box::new(m2)),
            ],
            false,
        );

        let names: std::collections::BTreeSet<String> = ["h1".to_owned()].into();
        let got = g
            .sftp_read_capped(Path::new("/remote/f"), 0, Some(&names))
            .await;

        assert_eq!(got.len(), 1);
        assert_eq!(got["h1"].as_ref().unwrap().bytes, b"alpha");
        assert!(
            h2.sftp_ops().is_empty(),
            "unselected host must not be touched"
        );
    }

    #[tokio::test]
    async fn sftp_read_capped_gives_each_attempted_host_its_own_budget() {
        let m1 = MockConnection::new("h1").with_file("/remote/f", b"aaaaaaaaaa".to_vec());
        let m2 = MockConnection::new("h2").with_file("/remote/f", b"bbbbbb".to_vec());
        let off = MockConnection::new("h3").with_file("/remote/f", b"cccccccc".to_vec());
        let h3 = off.clone();
        let mut g = HostsGroup::new(
            vec![
                Target::with_connection("h1", TargetState::Enabled, Box::new(m1)),
                Target::with_connection("h2", TargetState::Enabled, Box::new(m2)),
                Target::with_connection("h3", TargetState::Disabled, Box::new(off)),
            ],
            false,
        );

        let got = g.sftp_read_capped(Path::new("/remote/f"), 4, None).await;

        assert_eq!(got.len(), 2, "disabled host is absent: {got:?}");
        assert_eq!(
            got["h1"],
            Ok(SftpRead {
                bytes: b"aaaa".to_vec(),
                size: 10,
                truncated: true
            })
        );
        assert_eq!(
            got["h2"],
            Ok(SftpRead {
                bytes: b"bbbb".to_vec(),
                size: 6,
                truncated: true
            })
        );
        assert!(
            h3.sftp_ops().is_empty(),
            "disabled host must not be touched"
        );
    }

    #[tokio::test]
    async fn sftp_read_capped_selected_only_touches_named() {
        let m1 = MockConnection::new("h1").with_file("/remote/f", b"alpha".to_vec());
        let m2 = MockConnection::new("h2").with_file("/remote/f", b"beta".to_vec());
        let h2 = m2.clone();
        let mut g = HostsGroup::new(
            vec![
                Target::with_connection("h1", TargetState::Enabled, Box::new(m1)),
                Target::with_connection("h2", TargetState::Enabled, Box::new(m2)),
            ],
            false,
        );

        let names: std::collections::BTreeSet<String> = ["h1".to_owned()].into();
        let got = g
            .sftp_read_capped(Path::new("/remote/f"), 3, Some(&names))
            .await;

        assert_eq!(got.len(), 1);
        assert_eq!(
            got["h1"],
            Ok(SftpRead {
                bytes: b"alp".to_vec(),
                size: 5,
                truncated: true
            })
        );
        assert!(
            h2.sftp_ops().is_empty(),
            "unselected host must not be touched"
        );
    }

    #[tokio::test]
    async fn sftp_put_bytes_group_all_hosts_receive_bytes_once() {
        let (m1, m2) = (MockConnection::new("h1"), MockConnection::new("h2"));
        let (h1, h2) = (m1.clone(), m2.clone());
        let mut g = HostsGroup::new(
            vec![
                Target::with_connection("h1", TargetState::Enabled, Box::new(m1)),
                Target::with_connection("h2", TargetState::Enabled, Box::new(m2)),
            ],
            false,
        );

        let got = g
            .sftp_put_bytes(b"payload-434", Path::new("/tmp/wd/f"), None)
            .await;

        assert_eq!(got.len(), 2);
        assert_eq!(got["h1"], Ok(()));
        assert_eq!(got["h2"], Ok(()));
        for handle in [&h1, &h2] {
            let ops = handle.sftp_ops();
            assert_eq!(ops.len(), 1, "exactly one upload per host, got {ops:?}");
            assert!(
                matches!(
                    &ops[0],
                    crate::connection::MockSftpOp::PutBytes { len, data, remote }
                        if *remote == Path::new("/tmp/wd/f")
                            && *len == b"payload-434".len()
                            && data == b"payload-434"
                ),
                "payload/remote mismatch: {ops:?}"
            );
        }
    }

    #[tokio::test]
    async fn sftp_put_bytes_group_skips_disabled_host() {
        let m1 = MockConnection::new("h1");
        let m2 = MockConnection::new("h2");
        let h2 = m2.clone();
        let mut g = HostsGroup::new(
            vec![
                Target::with_connection("h1", TargetState::Enabled, Box::new(m1)),
                Target::with_connection("h2", TargetState::Disabled, Box::new(m2)),
            ],
            false,
        );

        let got = g
            .sftp_put_bytes(b"payload", Path::new("/tmp/wd/f"), None)
            .await;

        assert_eq!(got.len(), 1, "disabled host absent from the map: {got:?}");
        assert_eq!(got["h1"], Ok(()));
        assert!(
            h2.sftp_ops().is_empty(),
            "disabled host must not be touched"
        );
    }

    #[tokio::test]
    async fn sftp_put_bytes_group_reports_failing_host() {
        let ok = MockConnection::new("h1");
        let bad = MockConnection::new("h2").with_sftp_put_failure("disk full");
        let h1 = ok.clone();
        let mut g = HostsGroup::new(
            vec![
                Target::with_connection("h1", TargetState::Enabled, Box::new(ok)),
                Target::with_connection("h2", TargetState::Enabled, Box::new(bad)),
            ],
            false,
        );

        let got = g
            .sftp_put_bytes(b"payload", Path::new("/tmp/wd/f"), None)
            .await;

        assert_eq!(got["h1"], Ok(()));
        let Err(reason) = &got["h2"] else {
            panic!("h2 must carry its failure: {got:?}");
        };
        assert!(reason.contains("disk full"), "reason was {reason:?}");
        // The healthy host's upload still happened.
        assert_eq!(h1.sftp_ops().len(), 1);
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

    // --- reboot lifecycle ---------------------------------------------------

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
        let outcomes = g
            .reboot_selected("systemctl reboot", "", &all)
            .await
            .outcomes;

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
        // Regression: a `-t`-scoped reboot must fire the reboot,
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
        let outcomes = g
            .reboot_selected("systemctl reboot", "", &only_h1)
            .await
            .outcomes;

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

    /// #677: a reservation held across a reboot must be re-applied: the
    /// reboot wipes `/var/lock` (emulated here by removing the file), so
    /// without a re-take the host would be left unlocked.
    #[tokio::test]
    async fn reboot_reapplies_a_reservation_it_found() {
        let m1 = rebooted_mock("h1");
        let mut h1 = m1.clone();
        let mut g = HostsGroup::new(
            vec![Target::with_connection(
                "h1",
                TargetState::Enabled,
                Box::new(m1),
            )],
            false,
        );
        g.get_mut("h1")
            .unwrap()
            .lock("operator hold")
            .await
            .expect("lock");
        // The reboot wipes /var/lock (tmpfs); the mock filesystem does not.
        h1.sftp_remove(std::path::Path::new(crate::target::TARGET_LOCK_PATH))
            .await
            .expect("wipe");

        let all: std::collections::BTreeSet<String> = ["h1".to_owned()].into_iter().collect();
        let outcomes = g
            .reboot_selected("systemctl reboot", "", &all)
            .await
            .outcomes;
        assert!(outcomes["h1"].is_ok());

        let contents = h1
            .file_contents(crate::target::TARGET_LOCK_PATH)
            .expect("reservation re-applied");
        assert!(
            String::from_utf8_lossy(&contents).ends_with(":operator hold"),
            "reservation comment restored: {contents:?}"
        );
    }

    /// #677: a pool claim held across a reboot must be re-applied.
    #[tokio::test]
    async fn reboot_reapplies_the_pool_claim_it_held() {
        let m1 = rebooted_mock("h1");
        let mut h1 = m1.clone();
        let mut g = HostsGroup::new(
            vec![Target::with_connection(
                "h1",
                TargetState::Enabled,
                Box::new(m1),
            )],
            false,
        );
        let rrid = "SUSE:Maintenance:1:2";
        let comment = format!("mtui pool {rrid} [bob]");
        g.get_mut("h1").unwrap().set_rrid(rrid);
        assert!(
            g.get_mut("h1")
                .unwrap()
                .pool_claim(&comment)
                .await
                .expect("pool_claim"),
            "claim won"
        );
        h1.sftp_remove(std::path::Path::new(crate::target::POOL_LOCK_PATH))
            .await
            .expect("wipe");

        let all: std::collections::BTreeSet<String> = ["h1".to_owned()].into_iter().collect();
        let outcomes = g
            .reboot_selected("systemctl reboot", "", &all)
            .await
            .outcomes;
        assert!(outcomes["h1"].is_ok());

        let contents = h1
            .file_contents(crate::target::POOL_LOCK_PATH)
            .expect("pool claim re-applied");
        assert!(
            String::from_utf8_lossy(&contents).ends_with(&comment),
            "pool claim comment restored: {contents:?}"
        );
    }

    /// #677: a hold taken by someone else in the reboot gap is reported, not
    /// overwritten — and never fails the reboot outcome. The loss surfaces in
    /// the returned report (not only in `tracing`), and the stale in-memory
    /// record is dropped, so a later plain `unlock` stops claiming the lock
    /// instead of trying to release someone else's.
    #[tokio::test]
    async fn reboot_reports_a_hold_lost_to_another_owner() {
        let m1 = rebooted_mock("h1");
        let mut h1 = m1.clone();
        let mut g = HostsGroup::new(
            vec![Target::with_connection(
                "h1",
                TargetState::Enabled,
                Box::new(m1),
            )],
            false,
        );
        g.get_mut("h1")
            .unwrap()
            .lock("operator hold")
            .await
            .expect("lock");
        h1.sftp_remove(std::path::Path::new(crate::target::TARGET_LOCK_PATH))
            .await
            .expect("wipe");
        // Another tester claims the host while it is down.
        h1.sftp_write(
            std::path::Path::new(crate::target::TARGET_LOCK_PATH),
            b"1700000000:alice:4242:busy",
            false,
        )
        .await
        .expect("foreign claim");

        let all: std::collections::BTreeSet<String> = ["h1".to_owned()].into_iter().collect();
        let report = g.reboot_selected("systemctl reboot", "", &all).await;
        assert!(
            report.outcomes["h1"].is_ok(),
            "a lost hold warns, it does not fail the reboot"
        );
        assert_eq!(
            report.lost_holds.len(),
            1,
            "the lost hold must surface in the report: {:?}",
            report.lost_holds
        );
        let loss = &report.lost_holds[0];
        assert_eq!(loss.host, "h1");
        assert_eq!(loss.kind, LostHoldKind::OperationLock);
        assert_eq!(loss.comment, "operator hold");
        assert!(
            loss.reason.contains("alice"),
            "the loss names the other owner: {}",
            loss.reason
        );
        assert!(
            loss.describe().contains("h1") && loss.describe().contains("operator hold"),
            "the report line names the host and the hold: {}",
            loss.describe()
        );
        let contents = h1
            .file_contents(crate::target::TARGET_LOCK_PATH)
            .expect("foreign claim left in place");
        assert!(
            String::from_utf8_lossy(&contents).contains("alice"),
            "never overwrite another owner's hold: {contents:?}"
        );
        // The confirmed loss drops the in-memory record: a plain `unlock`
        // then skips instead of trying to release someone else's lock.
        assert_eq!(g.get("h1").unwrap().held_comment(LockKind::Op), None);
        let unlock = g.unlock_taken(&all).await;
        assert!(
            matches!(unlock["h1"], LockOutcome::Skipped(_)),
            "no record left to release: {:?}",
            unlock["h1"]
        );
    }

    /// `describe` on an unproven loss never claims the reboot cleared the
    /// hold: the re-take could not read the host, so no wipe is proven.
    #[test]
    fn lost_hold_describe_reports_retake_failed_for_an_unproven_loss() {
        let loss = LostHold {
            host: "h1".to_owned(),
            kind: LostHoldKind::OperationLock,
            comment: "operator hold".to_owned(),
            reason: "open failed: /var/lock/mtui.lock; the host may be unlocked".to_owned(),
            proof: LostHoldProof::Unproven,
        };
        assert_eq!(
            loss.describe(),
            "h1: re-take of the operation lock (operator hold) failed; \
             open failed: /var/lock/mtui.lock; the host may be unlocked"
        );
    }

    /// `describe` on a comment-less operation lock prints just "operation
    /// lock": the update's own `lock("")` carries no comment, and a bare
    /// noun with empty parens would misname it.
    #[test]
    fn lost_hold_describe_drops_the_noun_for_a_commentless_operation_lock() {
        let loss = LostHold {
            host: "h1".to_owned(),
            kind: LostHoldKind::OperationLock,
            comment: String::new(),
            reason: "held by alice since Tuesday".to_owned(),
            proof: LostHoldProof::Proven,
        };
        assert_eq!(
            loss.describe(),
            "h1: reboot cleared the operation lock; held by alice since Tuesday"
        );
    }

    /// `describe` keeps the operation-lock noun (and the comment) when the
    /// lost hold carries one — the same noun covers a reservation and a PI
    /// testing lock.
    #[test]
    fn lost_hold_describe_keeps_the_noun_for_a_commented_operation_lock() {
        let loss = LostHold {
            host: "h1".to_owned(),
            kind: LostHoldKind::OperationLock,
            comment: "operator hold".to_owned(),
            reason: "held by alice since Tuesday".to_owned(),
            proof: LostHoldProof::Proven,
        };
        assert_eq!(
            loss.describe(),
            "h1: reboot cleared the operation lock (operator hold); \
             held by alice since Tuesday"
        );
    }

    /// #677: a pool claim taken by another template in the reboot gap is
    /// reported, not overwritten — and never fails the reboot outcome.
    #[tokio::test]
    async fn reboot_reports_a_pool_claim_lost_to_another_owner() {
        let m1 = rebooted_mock("h1");
        let mut h1 = m1.clone();
        let mut g = HostsGroup::new(
            vec![Target::with_connection(
                "h1",
                TargetState::Enabled,
                Box::new(m1),
            )],
            false,
        );
        let rrid = "SUSE:Maintenance:1:2";
        let comment = format!("mtui pool {rrid} [bob]");
        g.get_mut("h1").unwrap().set_rrid(rrid);
        assert!(
            g.get_mut("h1")
                .unwrap()
                .pool_claim(&comment)
                .await
                .expect("pool_claim"),
            "claim won"
        );
        h1.sftp_remove(std::path::Path::new(crate::target::POOL_LOCK_PATH))
            .await
            .expect("wipe");
        // Another template claims the host while it is down (fresh, so the
        // stale reap leaves it alone).
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_secs();
        let foreign = format!("{now}:carol:4242:mtui pool SUSE:Maintenance:9:9 [carol]");
        h1.sftp_write(
            std::path::Path::new(crate::target::POOL_LOCK_PATH),
            foreign.as_bytes(),
            false,
        )
        .await
        .expect("foreign claim");

        let all: std::collections::BTreeSet<String> = ["h1".to_owned()].into_iter().collect();
        let report = g.reboot_selected("systemctl reboot", "", &all).await;
        assert!(
            report.outcomes["h1"].is_ok(),
            "a lost pool claim warns, it does not fail the reboot"
        );
        assert_eq!(
            report.lost_holds.len(),
            1,
            "the lost claim must surface in the report: {:?}",
            report.lost_holds
        );
        let loss = &report.lost_holds[0];
        assert_eq!(loss.host, "h1");
        assert_eq!(loss.kind, LostHoldKind::PoolClaim);
        assert_eq!(loss.comment, comment);
        assert!(
            loss.reason.contains("carol"),
            "the loss names the other owner: {}",
            loss.reason
        );
        let contents = h1
            .file_contents(crate::target::POOL_LOCK_PATH)
            .expect("foreign claim left in place");
        assert!(
            String::from_utf8_lossy(&contents).contains("carol"),
            "never overwrite another owner's claim: {contents:?}"
        );
        // The confirmed loss drops the in-memory record.
        assert_eq!(g.get("h1").unwrap().held_comment(LockKind::Pool), None);
    }

    /// A corrupt pool line is reported like a transport failure, keeping its
    /// record: dropping it would make a later plain `unlock` skip the host
    /// and strand the corrupt file, while the kept record surfaces the
    /// corruption on the next release attempt. No mtime is scripted on
    /// purpose — a stale invalid file would be reaped (and re-taken cleanly)
    /// instead of reported.
    #[tokio::test]
    async fn reboot_keeps_the_pool_claim_when_the_retake_finds_a_corrupt_line() {
        let m1 = rebooted_mock("h1");
        let mut h1 = m1.clone();
        let mut g = HostsGroup::new(
            vec![Target::with_connection(
                "h1",
                TargetState::Enabled,
                Box::new(m1),
            )],
            false,
        );
        let rrid = "SUSE:Maintenance:1:2";
        let comment = format!("mtui pool {rrid} [bob]");
        g.get_mut("h1").unwrap().set_rrid(rrid);
        assert!(
            g.get_mut("h1")
                .unwrap()
                .pool_claim(&comment)
                .await
                .expect("pool_claim"),
            "claim won"
        );
        h1.sftp_remove(std::path::Path::new(crate::target::POOL_LOCK_PATH))
            .await
            .expect("wipe");
        h1.sftp_write(
            std::path::Path::new(crate::target::POOL_LOCK_PATH),
            b"not-a-lock-line",
            false,
        )
        .await
        .expect("corrupt line");

        let all: std::collections::BTreeSet<String> = ["h1".to_owned()].into_iter().collect();
        let report = g.reboot_selected("systemctl reboot", "", &all).await;
        assert!(
            report.outcomes["h1"].is_ok(),
            "a corrupt line never fails the reboot"
        );
        assert_eq!(
            report.lost_holds.len(),
            1,
            "a corrupt line must surface in the report: {:?}",
            report.lost_holds
        );
        let loss = &report.lost_holds[0];
        assert_eq!(loss.host, "h1");
        assert_eq!(loss.kind, LostHoldKind::PoolClaim);
        assert_eq!(loss.comment, comment);
        assert_eq!(
            loss.proof,
            LostHoldProof::Unproven,
            "a corrupt line keeps its record like a transport failure"
        );
        assert!(
            loss.reason.contains("invalid"),
            "the loss names the invalid file: {}",
            loss.reason
        );
        assert!(
            loss.reason.contains("may be unlocked"),
            "the loss names the unlocked risk: {}",
            loss.reason
        );
        assert!(
            loss.describe().contains("re-take")
                && loss.describe().contains("failed")
                && !loss.describe().contains("cleared"),
            "an unproven loss must not claim a wipe: {}",
            loss.describe()
        );
        assert_eq!(
            g.get("h1").unwrap().held_comment(LockKind::Pool).as_deref(),
            Some(comment.as_str()),
            "the unproven claim stays recorded"
        );
    }

    /// The pool twin of the unreadable-hold test: a pool re-take that fails
    /// with a transport error is reported without dropping its record. The
    /// stale foreign claim forces the re-take into the remove path, which
    /// fails; the claim is never proven gone, so the record stays.
    #[tokio::test]
    async fn reboot_keeps_the_pool_claim_when_the_retake_hits_a_transport_error() {
        let m1 = rebooted_mock("h1").failing_sftp_remove();
        let mut h1 = m1.clone();
        let mut g = HostsGroup::new(
            vec![Target::with_connection(
                "h1",
                TargetState::Enabled,
                Box::new(m1),
            )],
            false,
        );
        let rrid = "SUSE:Maintenance:1:2";
        let comment = format!("mtui pool {rrid} [bob]");
        g.get_mut("h1").unwrap().set_rrid(rrid);
        assert!(
            g.get_mut("h1")
                .unwrap()
                .pool_claim(&comment)
                .await
                .expect("pool_claim"),
            "claim won"
        );
        // Another template claimed the host while it was down, long enough
        // ago that the re-take tries to reap it first. Overwriting (not
        // wiping-then-claiming) reaches the same on-disk state.
        h1.sftp_write(
            std::path::Path::new(crate::target::POOL_LOCK_PATH),
            b"0:carol:4242:mtui pool SUSE:Maintenance:9:9 [carol]",
            false,
        )
        .await
        .expect("stale foreign claim");

        let all: std::collections::BTreeSet<String> = ["h1".to_owned()].into_iter().collect();
        let report = g.reboot_selected("systemctl reboot", "", &all).await;
        assert!(
            report.outcomes["h1"].is_ok(),
            "an unreadable claim never fails the reboot"
        );
        assert_eq!(
            report.lost_holds.len(),
            1,
            "an unreadable pool re-take must surface in the report: {:?}",
            report.lost_holds
        );
        let loss = &report.lost_holds[0];
        assert_eq!(loss.host, "h1");
        assert_eq!(loss.kind, LostHoldKind::PoolClaim);
        assert_eq!(loss.comment, comment);
        assert_eq!(
            loss.proof,
            LostHoldProof::Unproven,
            "a transport failure proves no wipe"
        );
        assert!(
            loss.reason.contains("may be unlocked"),
            "the loss names the unlocked risk: {}",
            loss.reason
        );
        assert!(
            loss.describe().contains("h1")
                && loss.describe().contains(&comment)
                && loss.describe().contains("may be unlocked"),
            "the report line names the host, the hold and the risk: {}",
            loss.describe()
        );
        assert!(
            loss.describe().contains("re-take")
                && loss.describe().contains("failed")
                && !loss.describe().contains("cleared"),
            "an unproven loss must not claim a wipe: {}",
            loss.describe()
        );
        assert_eq!(
            g.get("h1").unwrap().held_comment(LockKind::Pool).as_deref(),
            Some(comment.as_str()),
            "the unproven claim stays recorded"
        );
    }

    /// #677: the transactional reboot inside `update` re-applies holds too.
    #[tokio::test]
    async fn reboot_transactional_reapplies_a_reservation_it_found() {
        use mtui_types::system::SystemProduct;

        let m1 = rebooted_mock("h1");
        let mut h1 = m1.clone();
        let mut t = Target::with_connection("h1", TargetState::Enabled, Box::new(m1));
        t.set_system(
            System::new(
                SystemProduct::new("SLES", "15", "x86_64"),
                std::collections::BTreeSet::new(),
                false,
            ),
            true,
        );
        let mut g = HostsGroup::new(vec![t], false);
        g.get_mut("h1")
            .unwrap()
            .lock("operator hold")
            .await
            .expect("lock");
        h1.sftp_remove(std::path::Path::new(crate::target::TARGET_LOCK_PATH))
            .await
            .expect("wipe");

        let map: std::collections::BTreeMap<String, String> =
            [("h1".to_owned(), "transactional-update --reboot".to_owned())]
                .into_iter()
                .collect();
        let outcomes = g.reboot_transactional(&map).await.outcomes;
        assert!(outcomes["h1"].is_ok());

        let contents = h1
            .file_contents(crate::target::TARGET_LOCK_PATH)
            .expect("reservation re-applied");
        assert!(
            String::from_utf8_lossy(&contents).ends_with(":operator hold"),
            "reservation comment restored: {contents:?}"
        );
    }

    /// The re-apply gate is the reconnect, not the verified reboot: a host
    /// that came back but whose boot id did not verify still holds a wiped
    /// `/var/lock`, so its holds are re-taken even though the outcome stays
    /// an error.
    #[tokio::test]
    async fn reboot_reapplies_holds_when_reconnected_despite_failed_verify() {
        // Fixed boot id: pre- and post-reboot probes match, so the verify
        // phase records RebootDidNotHappen even though dispatch+reconnect ran.
        let m1 = reboot_mock("h1", "id-1");
        let mut h1 = m1.clone();
        let mut g = HostsGroup::new(
            vec![Target::with_connection(
                "h1",
                TargetState::Enabled,
                Box::new(m1),
            )],
            false,
        );
        g.get_mut("h1")
            .unwrap()
            .lock("operator hold")
            .await
            .expect("lock");
        // The reboot wipes /var/lock (tmpfs); the mock filesystem does not.
        h1.sftp_remove(std::path::Path::new(crate::target::TARGET_LOCK_PATH))
            .await
            .expect("wipe");

        let all: std::collections::BTreeSet<String> = ["h1".to_owned()].into_iter().collect();
        let report = g.reboot_selected("systemctl reboot", "", &all).await;
        assert!(report.outcomes["h1"].is_err(), "failed verify must surface");
        assert!(
            report.lost_holds.is_empty(),
            "a clean re-take is not a loss: {:?}",
            report.lost_holds
        );

        let contents = h1
            .file_contents(crate::target::TARGET_LOCK_PATH)
            .expect("hold re-taken after reconnect despite failed verify");
        assert!(
            String::from_utf8_lossy(&contents).ends_with(":operator hold"),
            "reservation comment restored: {contents:?}"
        );
    }

    /// The re-apply gate: a host whose reconnect failed keeps its outcome and
    /// its untouched holds — no re-take is attempted on a host that never
    /// came back, and the in-memory record survives.
    #[tokio::test]
    async fn reboot_does_not_reapply_holds_when_reconnect_failed() {
        // `failing_reconnect` models a host that went down and never came
        // back; the reboot outcome records the reconnect failure.
        let m1 = rebooted_mock("h1").failing_reconnect();
        let mut h1 = m1.clone();
        let mut g = HostsGroup::new(
            vec![Target::with_connection(
                "h1",
                TargetState::Enabled,
                Box::new(m1),
            )],
            false,
        );
        g.get_mut("h1")
            .unwrap()
            .lock("operator hold")
            .await
            .expect("lock");
        // The reboot wipes /var/lock (tmpfs); the mock filesystem does not.
        h1.sftp_remove(std::path::Path::new(crate::target::TARGET_LOCK_PATH))
            .await
            .expect("wipe");

        let all: std::collections::BTreeSet<String> = ["h1".to_owned()].into_iter().collect();
        let report = g.reboot_selected("systemctl reboot", "", &all).await;
        assert!(
            report.outcomes["h1"].is_err(),
            "a failed reconnect must surface"
        );
        assert!(
            report.lost_holds.is_empty(),
            "nothing was attempted, so nothing was lost: {:?}",
            report.lost_holds
        );
        assert!(
            h1.file_contents(crate::target::TARGET_LOCK_PATH).is_none(),
            "no re-take without a reconnect"
        );
        assert_eq!(
            g.get("h1").unwrap().held_comment(LockKind::Op).as_deref(),
            Some("operator hold"),
            "the untouched hold stays recorded"
        );
    }

    /// When `relock_comment` matches the snapshot, the relock phase must not
    /// stamp the lock file: phase 5 re-takes exactly that comment, so the
    /// file sees a single write.
    #[tokio::test]
    async fn reboot_relock_matching_snapshot_writes_the_lockfile_once() {
        use crate::connection::MockSftpOp;

        let m1 = rebooted_mock("h1");
        let mut h1 = m1.clone();
        let mut g = HostsGroup::new(
            vec![Target::with_connection(
                "h1",
                TargetState::Enabled,
                Box::new(m1),
            )],
            false,
        );
        g.get_mut("h1")
            .unwrap()
            .lock("PI testing")
            .await
            .expect("lock");
        h1.sftp_remove(std::path::Path::new(crate::target::TARGET_LOCK_PATH))
            .await
            .expect("wipe");

        let all: std::collections::BTreeSet<String> = ["h1".to_owned()].into_iter().collect();
        let report = g
            .reboot_selected("systemctl reboot", "PI testing", &all)
            .await;
        assert!(report.outcomes["h1"].is_ok());

        let writes = h1
            .sftp_ops()
            .iter()
            .filter(|op| {
                matches!(op, MockSftpOp::Write { path, .. } if path == std::path::Path::new(crate::target::TARGET_LOCK_PATH))
            })
            .count();
        // One for the initial `lock`, one for the phase-5 re-take; the
        // relock phase skipped its redundant stamp of the same comment.
        assert_eq!(writes, 2, "relock must not re-stamp the snapshot comment");
        let contents = h1
            .file_contents(crate::target::TARGET_LOCK_PATH)
            .expect("hold re-taken");
        assert!(
            String::from_utf8_lossy(&contents).ends_with(":PI testing"),
            "snapshot comment restored: {contents:?}"
        );
    }

    /// When the snapshot already holds an operation lock, the relock phase
    /// skips it: phase 5 re-takes exactly that comment, so the file sees a
    /// single post-reboot write and never a brief PI stamp. An explicit
    /// reservation survives a reboot that also carries a PI assignment lock.
    #[tokio::test]
    async fn reboot_reservation_survives_a_differing_pi_relock() {
        let m1 = rebooted_mock("h1");
        let mut h1 = m1.clone();
        let mut g = HostsGroup::new(
            vec![Target::with_connection(
                "h1",
                TargetState::Enabled,
                Box::new(m1),
            )],
            false,
        );
        g.get_mut("h1")
            .unwrap()
            .lock("operator hold")
            .await
            .expect("lock");
        h1.sftp_remove(std::path::Path::new(crate::target::TARGET_LOCK_PATH))
            .await
            .expect("wipe");

        let all: std::collections::BTreeSet<String> = ["h1".to_owned()].into_iter().collect();
        let report = g
            .reboot_selected("systemctl reboot", "PI testing", &all)
            .await;
        assert!(report.outcomes["h1"].is_ok());

        let writes = h1
            .sftp_ops()
            .iter()
            .filter(|op| {
                matches!(op, crate::connection::MockSftpOp::Write { path, .. } if path == std::path::Path::new(crate::target::TARGET_LOCK_PATH))
            })
            .count();
        // One for the initial `lock`, one for the phase-5 re-take; the
        // relock phase skips any host that already holds an operation lock,
        // so no PI stamp lands before the reservation.
        assert_eq!(writes, 2, "relock must not stamp a held host");
        let contents = h1
            .file_contents(crate::target::TARGET_LOCK_PATH)
            .expect("hold re-taken");
        assert!(
            String::from_utf8_lossy(&contents).ends_with(":operator hold"),
            "the reservation wins over the PI relock: {contents:?}"
        );
        assert_eq!(
            g.get("h1").unwrap().held_comment(LockKind::Op).as_deref(),
            Some("operator hold"),
            "the record still names the reservation"
        );
    }

    /// A re-take that fails with a transport error keeps the in-memory
    /// record — the hold may still be ours — but it is reported: `/var/lock`
    /// is already wiped, so the host may be unlocked while the record says
    /// held, and only a `warn!` would leave an MCP caller silent.
    #[tokio::test]
    async fn reboot_keeps_the_hold_when_the_retake_cannot_read_the_host() {
        let m1 = rebooted_mock("h1").with_open_error(crate::target::TARGET_LOCK_PATH);
        let mut h1 = m1.clone();
        let mut g = HostsGroup::new(
            vec![Target::with_connection(
                "h1",
                TargetState::Enabled,
                Box::new(m1),
            )],
            false,
        );
        g.get_mut("h1")
            .unwrap()
            .lock("operator hold")
            .await
            .expect("lock");
        h1.sftp_remove(std::path::Path::new(crate::target::TARGET_LOCK_PATH))
            .await
            .expect("wipe");
        // Another tester claims the host while it is down; the re-take's
        // reconciling read then fails with a transport error instead of
        // proving the loss.
        h1.sftp_write(
            std::path::Path::new(crate::target::TARGET_LOCK_PATH),
            b"1700000000:alice:4242:busy",
            false,
        )
        .await
        .expect("foreign claim");

        let all: std::collections::BTreeSet<String> = ["h1".to_owned()].into_iter().collect();
        let report = g.reboot_selected("systemctl reboot", "", &all).await;
        assert!(
            report.outcomes["h1"].is_ok(),
            "an unreadable hold never fails the reboot"
        );
        assert_eq!(
            report.lost_holds.len(),
            1,
            "an unreadable re-take must surface in the report: {:?}",
            report.lost_holds
        );
        let loss = &report.lost_holds[0];
        assert_eq!(loss.host, "h1");
        assert_eq!(loss.kind, LostHoldKind::OperationLock);
        assert_eq!(loss.comment, "operator hold");
        assert_eq!(
            loss.proof,
            LostHoldProof::Unproven,
            "a transport failure proves no wipe"
        );
        assert!(
            loss.reason.contains("may be unlocked"),
            "the loss names the unlocked risk: {}",
            loss.reason
        );
        assert!(
            loss.describe().contains("h1")
                && loss.describe().contains("operator hold")
                && loss.describe().contains("may be unlocked"),
            "the report line names the host, the hold and the risk: {}",
            loss.describe()
        );
        assert!(
            loss.describe().contains("re-take")
                && loss.describe().contains("failed")
                && !loss.describe().contains("cleared"),
            "an unproven loss must not claim a wipe: {}",
            loss.describe()
        );
        assert_eq!(
            g.get("h1").unwrap().held_comment(LockKind::Op).as_deref(),
            Some("operator hold"),
            "the unproven hold stays recorded"
        );
    }

    /// A corrupt lock line is reported like a transport failure, keeping its
    /// record: dropping it would make a later plain `unlock` skip the host
    /// and strand the corrupt file, while the kept record makes the next
    /// release attempt surface the corruption instead.
    #[tokio::test]
    async fn reboot_keeps_the_hold_when_the_retake_finds_a_corrupt_lock_line() {
        // An old mtime settles the invalid file at once instead of polling
        // the settle window; the op re-take never reaps, so it changes no
        // outcome here.
        let m1 = rebooted_mock("h1").with_mtime(crate::target::TARGET_LOCK_PATH, 0);
        let mut h1 = m1.clone();
        let mut g = HostsGroup::new(
            vec![Target::with_connection(
                "h1",
                TargetState::Enabled,
                Box::new(m1),
            )],
            false,
        );
        g.get_mut("h1")
            .unwrap()
            .lock("operator hold")
            .await
            .expect("lock");
        h1.sftp_remove(std::path::Path::new(crate::target::TARGET_LOCK_PATH))
            .await
            .expect("wipe");
        h1.sftp_write(
            std::path::Path::new(crate::target::TARGET_LOCK_PATH),
            b"not-a-lock-line",
            false,
        )
        .await
        .expect("corrupt line");

        let all: std::collections::BTreeSet<String> = ["h1".to_owned()].into_iter().collect();
        let report = g.reboot_selected("systemctl reboot", "", &all).await;
        assert!(
            report.outcomes["h1"].is_ok(),
            "a corrupt line never fails the reboot"
        );
        assert_eq!(
            report.lost_holds.len(),
            1,
            "a corrupt line must surface in the report: {:?}",
            report.lost_holds
        );
        let loss = &report.lost_holds[0];
        assert_eq!(loss.host, "h1");
        assert_eq!(loss.kind, LostHoldKind::OperationLock);
        assert_eq!(loss.comment, "operator hold");
        assert_eq!(
            loss.proof,
            LostHoldProof::Unproven,
            "a corrupt line keeps its record like a transport failure"
        );
        assert!(
            loss.reason.contains("invalid"),
            "the loss names the invalid file: {}",
            loss.reason
        );
        assert!(
            loss.reason.contains("may be unlocked"),
            "the loss names the unlocked risk: {}",
            loss.reason
        );
        assert!(
            loss.describe().contains("re-take")
                && loss.describe().contains("failed")
                && !loss.describe().contains("cleared"),
            "an unproven loss must not claim a wipe: {}",
            loss.describe()
        );
        assert_eq!(
            g.get("h1").unwrap().held_comment(LockKind::Op).as_deref(),
            Some("operator hold"),
            "the unproven hold stays recorded"
        );
        let unlock = g.unlock_taken(&all).await;
        assert!(
            matches!(unlock["h1"], LockOutcome::Failed(_)),
            "the kept record surfaces the corruption on release: {:?}",
            unlock["h1"]
        );
    }

    #[tokio::test]
    async fn reboot_empty_group_is_noop() {
        let mut g = HostsGroup::new(vec![], false);
        // Must not panic; nothing to reboot, no per-host outcomes.
        let none: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        assert!(
            g.reboot_selected("systemctl reboot", "relock", &none)
                .await
                .outcomes
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
        let outcomes = g
            .reboot_selected("systemctl reboot", "", &all)
            .await
            .outcomes;
        assert_eq!(h1.reconnect_count(), 1);
        // Reconnect succeeded but the boot id was unchanged -> recorded failure.
        assert!(
            matches!(&outcomes["h1"], Err(HostError::RebootDidNotHappen { host }) if host == "h1"),
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
        let outcomes = g
            .reboot_selected("systemctl reboot", "", &all)
            .await
            .outcomes;
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
        let outcomes = g
            .reboot_selected("systemctl reboot", "", &all)
            .await
            .outcomes;
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
        let (m1, m2) = (rebooted_mock("h1"), rebooted_mock("h2"));
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
        let failures = OperationGroup::reboot(&mut g, map).await.failures();

        // The healthy path, which nothing else asserts: a transactional host
        // that really rebooted must produce **no** failure. Discarding this
        // `Vec` is what let the fixture drift to a fixed boot id — modelling a
        // host that never went down — without any test noticing.
        assert!(failures.is_empty(), "{failures:?}");
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
    async fn operation_reboot_a_dead_link_is_unreachable_not_undispatched() {
        // The regression this guards: a link that dies *before* the dispatch
        // makes `fire_and_forget` fail, which phase 2 records as
        // `RebootNotDispatched` — a variant that asserts the host is still
        // reachable. If phase 2 simply won, that claim would stand unchallenged
        // and `update` would revert every patched host on behalf of a machine
        // nothing can talk to. Phase 3 actually probes reachability, so its
        // failure has to outrank the phase-2 diagnosis.
        let m1 = MockConnection::new("h1")
            .with_default(CommandLog::new("", "ok", "", 0, 0))
            .failing_fire_and_forget()
            .failing_reconnect();
        let h1 = m1.clone();
        let mut g = HostsGroup::new(
            vec![Target::with_connection(
                "h1",
                TargetState::Enabled,
                Box::new(m1),
            )],
            false,
        );

        let map: HostCommandMap = vec![("h1".to_owned(), "systemctl reboot".to_owned())];
        let failures = OperationGroup::reboot(&mut g, map).await.failures();

        assert_eq!(h1.reconnect_count(), 1);
        assert_eq!(failures.len(), 1, "{failures:?}");
        assert_eq!(
            failures[0].cause,
            RebootFailureCause::Unreachable,
            "a host that failed BOTH dispatch and reconnect is unreachable"
        );
        assert!(
            !failures[0].cause.host_still_reachable(),
            "must not claim reachability that would trigger a rollback"
        );
    }

    #[tokio::test]
    async fn reboot_selected_a_dead_link_reports_the_reconnect_failure() {
        let m1 = MockConnection::new("h1")
            .with_default(CommandLog::new("", "ok", "", 0, 0))
            .failing_fire_and_forget()
            .failing_reconnect();
        let mut g = HostsGroup::new(
            vec![Target::with_connection(
                "h1",
                TargetState::Enabled,
                Box::new(m1),
            )],
            false,
        );
        let all: BTreeSet<String> = ["h1".to_owned()].into();

        let report = g.reboot_selected("systemctl reboot", "", &all).await;

        assert!(
            matches!(
                report.outcomes["h1"],
                Err(HostError::ReconnectFailed { .. })
            ),
            "a failed reconnect outranks the dispatch failure: {:?}",
            report.outcomes["h1"]
        );
    }

    #[tokio::test]
    async fn operation_reboot_undispatched_but_alive_stays_repairable() {
        // The complement: dispatch fails and the reconnect *succeeds* (the
        // session was never torn down). Here the host really is up, so the
        // phase-2 verdict must survive rather than be erased by the Ok.
        let m1 = MockConnection::new("h1")
            .with_default(CommandLog::new("", "ok", "", 0, 0))
            .with_changing_boot_id()
            .failing_fire_and_forget();
        let h1 = m1.clone();
        let mut g = HostsGroup::new(
            vec![Target::with_connection(
                "h1",
                TargetState::Enabled,
                Box::new(m1),
            )],
            false,
        );

        let map: HostCommandMap = vec![("h1".to_owned(), "systemctl reboot".to_owned())];
        let failures = OperationGroup::reboot(&mut g, map).await.failures();

        assert_eq!(h1.reconnect_count(), 1);
        assert_eq!(failures.len(), 1, "{failures:?}");
        assert_eq!(failures[0].cause, RebootFailureCause::NotDispatched);
        assert!(failures[0].cause.host_still_reachable());
    }

    #[tokio::test]
    async fn reboot_selected_reports_a_reboot_it_could_not_dispatch() {
        // The explicit `reboot` command had the same hole as the operation
        // path: a failed dispatch left the session live, so the reconnect
        // succeeded and the host was reported as rebooted.
        let m1 = MockConnection::new("h1")
            .with_default(CommandLog::new("", "ok", "", 0, 0))
            .with_changing_boot_id()
            .failing_fire_and_forget();
        let mut g = HostsGroup::new(
            vec![Target::with_connection(
                "h1",
                TargetState::Enabled,
                Box::new(m1),
            )],
            false,
        );

        let only_h1: std::collections::BTreeSet<String> = ["h1".to_owned()].into_iter().collect();
        let outcomes = g
            .reboot_selected("systemctl reboot", "", &only_h1)
            .await
            .outcomes;
        assert!(
            matches!(&outcomes["h1"], Err(HostError::RebootNotDispatched { .. })),
            "{:?}",
            outcomes["h1"]
        );
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
        let failures = OperationGroup::reboot(&mut g, Vec::new()).await.failures();
        assert!(h1.fired_commands().is_empty());
        assert_eq!(h1.reconnect_count(), 0);
        assert!(failures.is_empty());
    }

    #[tokio::test]
    async fn operation_reboot_skips_a_disabled_host_in_every_phase() {
        // A disabled host is excluded from the command fan-out, so it staged no
        // snapshot and has nothing to activate. `plans()` still resolves a plan
        // for it, so it reaches the reboot map and must be skipped here --
        // rebooting it restarts a machine the operator took out of the run.
        //
        // Deliberately a *fixed* boot id. With a changing one this test would
        // pass even if only the dispatch were skipped, because the probes would
        // report a restart that never happened. A fixed id is what a real
        // untouched host returns, and it is what catches a phase that forgot
        // the state check: phase 4 would then record `NotRebooted`, which
        // `update` reads as still-reachable and routes a rollback on --
        // reverting every patched host on behalf of a disabled one.
        let m1 = reboot_mock("h1", "id-1");
        let h1 = m1.clone();
        let mut g = HostsGroup::new(
            vec![Target::with_connection(
                "h1",
                TargetState::Disabled,
                Box::new(m1),
            )],
            false,
        );

        let map: HostCommandMap = vec![("h1".to_owned(), "transactional-update reboot".to_owned())];
        let failures = OperationGroup::reboot(&mut g, map).await.failures();

        assert!(
            h1.fired_commands().is_empty(),
            "a disabled host must not be rebooted: {:?}",
            h1.fired_commands()
        );
        assert!(failures.is_empty(), "skipped, not failed: {failures:?}");
        // Every phase, not just the dispatch: no boot-id probe, no reconnect.
        assert!(
            h1.commands().is_empty(),
            "a skipped host must cost no SSH reads either: {:?}",
            h1.commands()
        );
        assert_eq!(h1.reconnect_count(), 0);
    }

    #[tokio::test]
    async fn operation_reboot_still_reboots_an_enabled_host_alongside_a_disabled_one() {
        // The inertness control for the test above: the state check must skip
        // the disabled host only, not collapse the whole fan-out.
        let (m1, m2) = (rebooted_mock("h1"), reboot_mock("h2", "id-2"));
        let (h1, h2) = (m1.clone(), m2.clone());
        let mut g = HostsGroup::new(
            vec![
                Target::with_connection("h1", TargetState::Enabled, Box::new(m1)),
                Target::with_connection("h2", TargetState::Disabled, Box::new(m2)),
            ],
            false,
        );

        let map: HostCommandMap = vec![
            ("h1".to_owned(), "transactional-update reboot".to_owned()),
            ("h2".to_owned(), "transactional-update reboot".to_owned()),
        ];
        let failures = OperationGroup::reboot(&mut g, map).await.failures();

        assert_eq!(
            h1.fired_commands(),
            vec!["transactional-update reboot".to_owned()]
        );
        assert_eq!(h1.reconnect_count(), 1);
        assert!(h2.fired_commands().is_empty());
        assert!(failures.is_empty(), "{failures:?}");
    }

    #[tokio::test]
    async fn reboot_transactional_records_reconnect_failure() {
        // A transactional host that reboots and never reconnects must be named
        // in the outcome map, not silently dropped: `Operation::run` (and every
        // flow above it) relies on this to fail the operation instead of
        // reporting success on a host it lost.
        let m1 = reboot_mock("h1", "id-1").failing_reconnect();
        let h1 = m1.clone();
        let mut g = HostsGroup::new(
            vec![Target::with_connection(
                "h1",
                TargetState::Enabled,
                Box::new(m1),
            )],
            false,
        );

        let map: HostCommandMap = vec![("h1".to_owned(), "systemctl reboot".to_owned())];
        let failures = OperationGroup::reboot(&mut g, map).await.failures();

        assert_eq!(h1.reconnect_count(), 1);
        assert_eq!(failures.len(), 1);
        assert_eq!(failures[0].host, "h1");
        // The cause, not just the fact: a host that went away and never came
        // back is the one case the rollback must NOT be run for.
        assert_eq!(failures[0].cause, RebootFailureCause::Unreachable);
        assert!(!failures[0].cause.host_still_reachable());
    }

    // --- update_lock / lock / unlock fan-out --------------------------------

    #[tokio::test]
    async fn update_lock_locks_all_free_hosts() {
        let mut g = HostsGroup::new(vec![enabled("h1"), enabled("h2")], false);
        g.update_lock(None).await.expect("all free -> ok");
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

        let err = g
            .update_lock(None)
            .await
            .expect_err("foreign lock -> abort");
        assert!(matches!(err, HostError::HostsLocked(_)));
        // h1's lock was released during the abort.
        assert!(!g.get_mut("h1").unwrap().is_locked().await.unwrap());
    }

    fn invalid_lock_host(name: &str) -> (Target, MockConnection) {
        let conn = MockConnection::new(name)
            .with_default(CommandLog::new("", "ok", "", 0, 0))
            .with_file(TARGET_LOCK_PATH, Vec::new());
        let handle = conn.clone();
        (
            Target::with_connection(name, TargetState::Enabled, Box::new(conn)),
            handle,
        )
    }

    #[tokio::test]
    async fn update_lock_names_an_invalid_lock_file_instead_of_a_phantom_owner() {
        let (h2, h2_handle) = invalid_lock_host("h2");
        let mut g = HostsGroup::new(vec![enabled("h1"), h2], false);

        let err = g
            .update_lock(None)
            .await
            .expect_err("invalid lock -> abort");

        assert!(matches!(err, HostError::HostsLocked(_)), "{err:?}");
        let msg = err.to_string();
        assert!(
            msg.contains("h2") && msg.contains("invalid (empty or unparseable)"),
            "{msg}"
        );
        assert!(!msg.contains("held by"), "{msg}");
        assert_eq!(h2_handle.file_contents(TARGET_LOCK_PATH).unwrap(), b"");
        assert!(!g.get_mut("h1").unwrap().is_locked().await.unwrap());
    }

    #[tokio::test]
    async fn lock_and_unlock_report_an_invalid_lock_file_as_failed_not_contended() {
        let (h1, handle) = invalid_lock_host("h1");
        let mut g = HostsGroup::new(vec![h1], false);

        let locked = g.lock("", None).await;
        let LockOutcome::Failed(reason) = &locked["h1"] else {
            panic!("expected Failed, got {:?}", locked["h1"]);
        };
        assert!(
            reason.contains("invalid (empty or unparseable)"),
            "{reason}"
        );
        let unlocked = g.unlock(None).await;
        assert!(
            matches!(&unlocked["h1"], LockOutcome::Failed(r) if r.contains("--force")),
            "{:?}",
            unlocked["h1"]
        );
        assert_eq!(handle.file_contents(TARGET_LOCK_PATH).unwrap(), b"");

        let names: BTreeSet<String> = ["h1".to_owned()].into();
        let collected = std::sync::Mutex::new(BTreeMap::new());
        g.unlock_force(&names, &collected).await;
        assert_eq!(collected.into_inner().unwrap()["h1"], LockOutcome::Released);
        assert!(handle.file_contents(TARGET_LOCK_PATH).is_none());
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
            .update_lock(None)
            .await
            .expect_err("non-contention lock failure -> abort");
        assert!(matches!(err, HostError::HostsLocked(_)));
        // The message names the real per-host cause instead of the bare
        // "Hosts locked" literal.
        let msg = err.to_string();
        assert!(msg.starts_with("Hosts locked: "));
        assert!(msg.contains("h2:"));
        assert!(msg.contains("scripted exclusive-create failure"));
        assert!(!g.get_mut("h1").unwrap().is_locked().await.unwrap());
    }

    /// h1 held by another owner, h2 unwritable, h3 free.
    fn two_unlockable_hosts() -> HostsGroup {
        let foreign = MockConnection::new("h1")
            .with_default(CommandLog::new("", "ok", "", 0, 0))
            .with_file(TARGET_LOCK_PATH, b"1700000000:alice:4242:busy".to_vec());
        let failing = MockConnection::new("h2")
            .with_default(CommandLog::new("", "ok", "", 0, 0))
            .with_exclusive_write_error(TARGET_LOCK_PATH);
        HostsGroup::new(
            vec![
                Target::with_connection("h1", TargetState::Enabled, Box::new(foreign)),
                Target::with_connection("h2", TargetState::Enabled, Box::new(failing)),
                enabled("h3"),
            ],
            false,
        )
    }

    #[tokio::test]
    async fn update_lock_names_each_unlockable_host_in_host_order() {
        let mut g = two_unlockable_hosts();

        let err = g
            .update_lock(None)
            .await
            .expect_err("two hosts cannot be locked");

        let HostError::HostsLocked(refusals) = err else {
            panic!("expected one entry per host, got {err:?}");
        };
        assert_eq!(
            refusals,
            vec![
                LockRefusal {
                    host: "h1".to_owned(),
                    reason: "held by alice since Tuesday, 14.11.2023 22:13 UTC".to_owned(),
                },
                LockRefusal {
                    host: "h2".to_owned(),
                    reason: "sftp error on h2: scripted exclusive-create failure: \
                             /var/lock/mtui.lock"
                        .to_owned(),
                },
            ]
        );
        assert!(!g.get_mut("h3").unwrap().is_locked().await.unwrap());
    }

    #[tokio::test]
    async fn update_lock_names_a_foreign_owner_from_a_single_lock_read() {
        let foreign = MockConnection::new("h1")
            .with_default(CommandLog::new("", "ok", "", 0, 0))
            .with_file(TARGET_LOCK_PATH, b"1700000000:alice:4242:busy".to_vec());
        let handle = foreign.clone();
        let mut g = HostsGroup::new(
            vec![Target::with_connection(
                "h1",
                TargetState::Enabled,
                Box::new(foreign),
            )],
            false,
        );

        g.update_lock(None).await.expect_err("foreign lock aborts");

        let reads = handle
            .sftp_ops()
            .into_iter()
            .filter(|op| matches!(op, crate::connection::MockSftpOp::Open(p) if p == std::path::Path::new(TARGET_LOCK_PATH)))
            .count();
        assert_eq!(reads, 3);
    }

    #[tokio::test]
    async fn update_lock_failure_still_renders_as_one_hosts_locked_line() {
        let mut g = two_unlockable_hosts();

        let err = g
            .update_lock(None)
            .await
            .expect_err("two hosts cannot be locked");

        assert_eq!(
            err.to_string(),
            "Hosts locked: h1: held by alice since Tuesday, 14.11.2023 22:13 UTC; \
             h2: sftp error on h2: scripted exclusive-create failure: /var/lock/mtui.lock"
        );
    }

    #[tokio::test]
    async fn lock_and_unlock_fan_out_over_group() {
        let mut g = HostsGroup::new(vec![enabled("h1"), enabled("h2")], false);
        let locked = g.lock("", None).await;
        assert_eq!(locked["h1"], LockOutcome::Acquired);
        assert_eq!(locked["h2"], LockOutcome::Acquired);
        assert!(g.get_mut("h1").unwrap().is_locked().await.unwrap());
        assert!(g.get_mut("h2").unwrap().is_locked().await.unwrap());

        let unlocked = g.unlock(None).await;
        assert_eq!(unlocked["h1"], LockOutcome::Released);
        assert_eq!(unlocked["h2"], LockOutcome::Released);
        assert!(!g.get_mut("h1").unwrap().is_locked().await.unwrap());
        assert!(!g.get_mut("h2").unwrap().is_locked().await.unwrap());
    }

    /// `unlock_held` releases only what *this group* took: a same-process lock
    /// it never acquired is left alone and is absent from the outcome map.
    ///
    /// The case that matters is a refhost shared with another loaded template
    /// or another MCP session. Wire ownership is per user+PID, so the sibling's
    /// live hold reads back as "mine" and the whole-group [`unlock`] removes it
    /// mid-transaction — as the second half of this test shows.
    #[tokio::test]
    async fn unlock_held_releases_only_this_groups_own_holds() {
        // `Target::with_connection` builds its lock from `Config::default()`, so
        // that is the identity a same-process sibling's line carries.
        let mine = format!(
            "1700000000:{}:{}",
            mtui_config::Config::default().session_user,
            std::process::id()
        );
        let sibling = MockConnection::new("h2")
            .with_default(CommandLog::new("", "ok", "", 0, 0))
            .with_file(TARGET_LOCK_PATH, mine.clone().into_bytes());
        let sibling_handle = sibling.clone();
        let mut g = HostsGroup::new(
            vec![
                enabled("h1"),
                Target::with_connection("h2", TargetState::Enabled, Box::new(sibling)),
            ],
            false,
        );

        // Only h1 is locked *by this group*; h2 already carries a same-process
        // lock this group never took.
        let names: std::collections::BTreeSet<String> = ["h1".to_owned()].into_iter().collect();
        let locked = g.lock("", Some(&names)).await;
        assert_eq!(locked["h1"], LockOutcome::Acquired);

        let released = g.unlock_held().await;
        assert_eq!(released["h1"], LockOutcome::Released);
        assert!(
            !released.contains_key("h2"),
            "a host this group never locked must not even appear: {released:?}"
        );
        assert!(!g.get_mut("h1").unwrap().is_locked().await.unwrap());
        assert!(
            sibling_handle.file_contents(TARGET_LOCK_PATH).is_some(),
            "the sibling's live lock was removed"
        );

        // The contrast: the unscoped fan-out does remove it, because on the
        // wire it is indistinguishable from ours.
        let swept = g.unlock(None).await;
        assert_eq!(swept["h2"], LockOutcome::Released);
        assert!(sibling_handle.file_contents(TARGET_LOCK_PATH).is_none());
    }

    /// `unlock_held` skips a comment-marked (exclusive) hold — the PI
    /// assignment lock, or an operator's `lock -c <text>` reservation — while
    /// still releasing an ordinary uncommented operation hold.
    #[tokio::test]
    async fn unlock_held_skips_comment_marked_reservations() {
        let reserved = MockConnection::new("h2").with_default(CommandLog::new("", "ok", "", 0, 0));
        let reserved_handle = reserved.clone();
        let mut g = HostsGroup::new(
            vec![
                enabled("h1"),
                Target::with_connection("h2", TargetState::Enabled, Box::new(reserved)),
            ],
            false,
        );

        let marked: std::collections::BTreeSet<String> = ["h2".to_owned()].into_iter().collect();
        g.lock("PI assignment", Some(&marked)).await;
        let plain: std::collections::BTreeSet<String> = ["h1".to_owned()].into_iter().collect();
        g.lock("", Some(&plain)).await;

        let released = g.unlock_held().await;
        assert_eq!(released["h1"], LockOutcome::Released);
        assert!(
            !released.contains_key("h2"),
            "a comment-marked reservation must not be swept: {released:?}"
        );
        assert!(
            reserved_handle.file_contents(TARGET_LOCK_PATH).is_some(),
            "the reservation was removed"
        );
    }

    #[tokio::test]
    async fn unlock_taken_leaves_a_sibling_groups_hold_untouched() {
        // Two `HostsGroup`s over the *same* wire connection — as if one
        // refhost were attached to two loaded templates sharing this
        // process, the shape defect C fixes: group A locks, group B's
        // `unlock_taken` must not release it even though the wire's
        // PID-based `is_mine` cannot tell the two `Target` objects apart.
        // Mutation to catch: swapping `unlock_taken` for `unlock_selected`
        // removes A's lock from group B.
        let conn = MockConnection::new("h1").with_default(CommandLog::new("", "ok", "", 0, 0));
        let mut group_a = HostsGroup::new(
            vec![Target::with_connection(
                "h1",
                TargetState::Enabled,
                Box::new(conn.clone()),
            )],
            false,
        );
        let mut group_b = HostsGroup::new(
            vec![Target::with_connection(
                "h1",
                TargetState::Enabled,
                Box::new(conn.clone()),
            )],
            false,
        );

        let names: BTreeSet<String> = ["h1".to_owned()].into();
        let locked = group_a.lock("", Some(&names)).await;
        assert_eq!(locked["h1"], LockOutcome::Acquired);

        let outcome = group_b.unlock_taken(&names).await;
        assert!(
            matches!(&outcome["h1"], LockOutcome::Skipped(reason) if reason.contains("not locked by this session")),
            "{outcome:?}"
        );
        assert!(
            conn.file_contents(TARGET_LOCK_PATH).is_some(),
            "group B must not release group A's hold"
        );

        // Group A's own `unlock_taken` still releases what it took.
        let released = group_a.unlock_taken(&names).await;
        assert_eq!(released["h1"], LockOutcome::Released);
        assert!(conn.file_contents(TARGET_LOCK_PATH).is_none());
    }

    #[tokio::test]
    async fn update_lock_aborts_on_a_sibling_groups_hold_and_keeps_it() {
        let conn = MockConnection::new("h1").with_default(CommandLog::new("", "ok", "", 0, 0));
        let holders = OpLockHolders::default();
        let mut group_a = HostsGroup::new(
            vec![Target::with_connection(
                "h1",
                TargetState::Enabled,
                Box::new(conn.clone()),
            )],
            false,
        );
        let mut group_b = HostsGroup::new(
            vec![Target::with_connection(
                "h1",
                TargetState::Enabled,
                Box::new(conn.clone()),
            )],
            false,
        );
        group_a.set_op_lock_holders(holders.clone());
        group_b.set_op_lock_holders(holders);

        let names: BTreeSet<String> = ["h1".to_owned()].into();
        group_a.lock("", Some(&names)).await;

        let err = group_b
            .update_lock(None)
            .await
            .expect_err("a sibling's hold must abort the update");
        assert!(matches!(err, HostError::HostsLocked(_)), "{err}");
        assert!(
            conn.file_contents(TARGET_LOCK_PATH).is_some(),
            "the abort rollback removed the sibling's lockfile"
        );
        assert!(group_a.get("h1").unwrap().holds_operation_lock());
    }

    #[tokio::test]
    async fn a_target_added_later_shares_the_groups_op_lock_holders() {
        let holders = OpLockHolders::default();
        let mut g = HostsGroup::new(vec![enabled("h1")], false);
        g.set_op_lock_holders(holders.clone());
        g.add(enabled("h2"));

        for host in ["h1", "h2"] {
            assert!(
                g.get(host).unwrap().op_lock_holders().ptr_eq(&holders),
                "{host} is on its own registry"
            );
        }
    }

    #[tokio::test]
    async fn select_split_carries_the_op_lock_holders_to_both_halves() {
        let holders = OpLockHolders::default();
        let mut g = HostsGroup::new(vec![enabled("h1"), enabled("h2")], false);
        g.set_op_lock_holders(holders.clone());

        let (mut selected, remainder) = g.select_split(Some(&["h1".to_owned()]), false).unwrap();
        // The halves are re-merged after an operation; a target moved between
        // them must not drop off the shared registry.
        selected.merge(remainder);
        selected.add(enabled("h3"));
        for host in ["h1", "h2", "h3"] {
            assert!(
                selected
                    .get(host)
                    .unwrap()
                    .op_lock_holders()
                    .ptr_eq(&holders),
                "{host} is on its own registry"
            );
        }
    }

    #[tokio::test]
    async fn unlock_taken_names_a_sibling_hold_and_does_not_offer_force() {
        let conn = MockConnection::new("h1").with_default(CommandLog::new("", "ok", "", 0, 0));
        let holders = OpLockHolders::default();
        let build = |rrid: &str| {
            let mut g = HostsGroup::new(
                vec![Target::with_connection(
                    "h1",
                    TargetState::Enabled,
                    Box::new(conn.clone()),
                )],
                false,
            );
            g.set_op_lock_holders(holders.clone());
            for t in g.targets_mut() {
                t.set_rrid(rrid);
            }
            g
        };
        let mut group_a = build("SUSE:Maintenance:1:1");
        let mut group_b = build("SUSE:Maintenance:2:2");
        let names: BTreeSet<String> = ["h1".to_owned()].into();
        group_a.lock("", Some(&names)).await;

        let outcome = group_b.unlock_taken(&names).await;
        assert!(
            matches!(&outcome["h1"], LockOutcome::Skipped(reason)
                if reason.contains("this mtui process (SUSE:Maintenance:1:1)")
                    && !reason.contains("--force")),
            "{outcome:?}"
        );
        assert!(conn.file_contents(TARGET_LOCK_PATH).is_some());
    }

    #[tokio::test]
    async fn unlock_selected_keeps_this_sessions_reservation() {
        let conn = MockConnection::new("h1").with_default(CommandLog::new("", "ok", "", 0, 0));
        let mut g = HostsGroup::new(
            vec![Target::with_connection(
                "h1",
                TargetState::Enabled,
                Box::new(conn.clone()),
            )],
            false,
        );
        let names: BTreeSet<String> = ["h1".to_owned()].into();
        g.lock("X", Some(&names)).await;

        let outcome = g.unlock(Some(&names)).await;
        assert!(
            matches!(&outcome["h1"], LockOutcome::Skipped(r) if r.contains("reservation kept")),
            "{outcome:?}"
        );
        assert!(
            conn.file_contents(TARGET_LOCK_PATH).is_some(),
            "an operation release removed the reservation"
        );
        let swept = g.unlock(None).await;
        assert!(matches!(&swept["h1"], LockOutcome::Skipped(_)), "{swept:?}");
        assert!(conn.file_contents(TARGET_LOCK_PATH).is_some());

        let released = g.unlock_taken(&names).await;
        assert_eq!(released["h1"], LockOutcome::Released);
        assert!(conn.file_contents(TARGET_LOCK_PATH).is_none());
    }

    #[tokio::test]
    async fn unlock_selected_still_releases_an_operation_hold_beside_a_reservation() {
        let mut g = HostsGroup::new(vec![enabled("h1"), enabled("h2")], false);
        let reserved: BTreeSet<String> = ["h1".to_owned()].into();
        let plain: BTreeSet<String> = ["h2".to_owned()].into();
        g.lock("X", Some(&reserved)).await;
        g.lock("", Some(&plain)).await;

        let both: BTreeSet<String> = ["h1".to_owned(), "h2".to_owned()].into();
        let outcome = g.unlock(Some(&both)).await;
        assert_eq!(outcome["h2"], LockOutcome::Released);
        assert!(g.get_mut("h1").unwrap().is_locked().await.unwrap());
        assert!(!g.get_mut("h2").unwrap().is_locked().await.unwrap());
    }

    #[tokio::test]
    async fn unlock_taken_releases_a_comment_marked_hold_from_the_same_session() {
        // `lock -c "reservation"` then a plain `unlock` in the *same* group
        // must still release it: `unlock_taken` checks
        // `holds_operation_lock` (marked or not), not
        // `holds_unmarked_operation_lock` (`unlock_held`'s predicate, which
        // deliberately excludes a comment-marked reservation).
        // Mutation to catch: swapping `holds_operation_lock` for
        // `holds_unmarked_operation_lock` makes this skip instead of release.
        let mut g = HostsGroup::new(vec![enabled("h1")], false);
        let names: BTreeSet<String> = ["h1".to_owned()].into();
        let locked = g.lock("reservation", Some(&names)).await;
        assert_eq!(locked["h1"], LockOutcome::Acquired);

        let released = g.unlock_taken(&names).await;
        assert_eq!(released["h1"], LockOutcome::Released);
        assert!(!g.get_mut("h1").unwrap().is_locked().await.unwrap());
    }

    /// The hold record survives a plain read and is cleared by a release, so
    /// `unlock_held` is idempotent rather than repeatedly re-releasing.
    #[tokio::test]
    async fn holds_unmarked_operation_lock_tracks_acquire_and_release() {
        // h2 carries a same-process lock that this group never took — the
        // shared-refhost shape.
        let foreign_pid = format!(
            "1700000000:{}:{}",
            mtui_config::Config::default().session_user,
            std::process::id()
        );
        let sibling = MockConnection::new("h2")
            .with_default(CommandLog::new("", "ok", "", 0, 0))
            .with_file(TARGET_LOCK_PATH, foreign_pid.into_bytes());
        let mut g = HostsGroup::new(
            vec![
                enabled("h1"),
                Target::with_connection("h2", TargetState::Enabled, Box::new(sibling)),
            ],
            false,
        );
        assert!(
            !g.get("h1").unwrap().holds_unmarked_operation_lock(),
            "a fresh target holds nothing"
        );

        let plain: std::collections::BTreeSet<String> = ["h1".to_owned()].into_iter().collect();
        g.lock("", Some(&plain)).await;
        assert!(g.get("h1").unwrap().holds_unmarked_operation_lock());

        // Reading a *foreign* host's remote state is not taking it. This is the
        // whole reason the record cannot be derived from the remote line: the
        // line says "mine" (same user + PID) and is not.
        assert!(g.get_mut("h2").unwrap().is_locked().await.unwrap());
        assert!(
            !g.get("h2").unwrap().holds_unmarked_operation_lock(),
            "a read must never establish a hold"
        );

        let first = g.unlock_held().await;
        assert_eq!(first["h1"], LockOutcome::Released);
        assert!(
            !g.get("h1").unwrap().holds_unmarked_operation_lock(),
            "the record must clear on release"
        );
        let second = g.unlock_held().await;
        assert!(
            second.is_empty(),
            "a second pass has nothing left to do: {second:?}"
        );
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

        let outcomes = g.lock("", None).await;
        assert_eq!(outcomes["h1"], LockOutcome::Acquired);
        assert_eq!(outcomes["h2"], contended_by_alice());
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
        let outcomes = g.lock("", Some(&names)).await;

        assert_eq!(outcomes["h1"], LockOutcome::Acquired);
        assert_eq!(outcomes["h2"], contended_by_alice());
        assert!(
            !outcomes.contains_key("h3"),
            "unselected host must be absent from the outcome map: {outcomes:?}"
        );
        // The unselected host was never locked.
        assert!(!g.get_mut("h3").unwrap().is_locked().await.unwrap());

        // Rolling back the acquired subset releases h1 without touching h3.
        let released = g.unlock(Some(&names)).await;
        assert_eq!(released["h1"], LockOutcome::Released);
        assert!(!released.contains_key("h3"));
        assert!(!g.get_mut("h1").unwrap().is_locked().await.unwrap());
    }

    #[tokio::test]
    async fn lock_and_unlock_skip_an_unconnected_host_without_touching_the_wire() {
        // Mutation to catch: deleting `lock_where`'s/`unlock_where`'s
        // `has_operation_lock` guard reports `Acquired`/`Released` for a
        // target that built no `TargetLock` at all (`Target::lock`/
        // `unlock_reporting` are no-op `Ok(())` when `self.lock == None`).
        let unconnected = Target::new(
            &mtui_config::Config::default(),
            "unconnected",
            TargetState::Enabled,
        );
        let mut g = HostsGroup::new(vec![enabled("h1"), unconnected], false);

        let locked = g.lock("", None).await;
        assert_eq!(locked["h1"], LockOutcome::Acquired);
        assert_eq!(
            locked["unconnected"],
            LockOutcome::Skipped("not connected".to_owned())
        );

        let unlocked = g.unlock(None).await;
        assert_eq!(unlocked["h1"], LockOutcome::Released);
        assert_eq!(
            unlocked["unconnected"],
            LockOutcome::Skipped("not connected".to_owned())
        );
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
        let _ = g.lock("", None).await; // locks h1; h2 stays foreign-locked

        let outcomes = g.unlock(None).await;
        assert_eq!(outcomes["h1"], LockOutcome::Released);
        assert_eq!(outcomes["h2"], contended_by_alice());
    }

    #[tokio::test]
    async fn unlock_force_removes_a_foreign_lock_unlock_leaves_it() {
        // The same fixture through both entry points: `force` is what turns a
        // foreign lock from Contended into a removed file.
        let foreign = || {
            MockConnection::new("h1")
                .with_default(CommandLog::new("", "ok", "", 0, 0))
                .with_file(TARGET_LOCK_PATH, b"1700000000:alice:4242:busy".to_vec())
        };
        let group = |c: MockConnection| {
            HostsGroup::new(
                vec![Target::with_connection(
                    "h1",
                    TargetState::Enabled,
                    Box::new(c),
                )],
                false,
            )
        };

        let plain = foreign();
        let mut g = group(plain.clone());
        assert_eq!(g.unlock(None).await["h1"], contended_by_alice());
        assert!(plain.file_contents(TARGET_LOCK_PATH).is_some());

        let forced = foreign();
        let mut g = group(forced.clone());
        let names: BTreeSet<String> = ["h1".to_owned()].into();
        let collected = std::sync::Mutex::new(BTreeMap::new());
        g.unlock_force(&names, &collected).await;
        assert_eq!(collected.into_inner().unwrap()["h1"], LockOutcome::Released);
        assert!(forced.file_contents(TARGET_LOCK_PATH).is_none());
    }

    #[tokio::test]
    async fn operation_group_unlock_forwards_a_failed_release() {
        // Pins that the `impl OperationGroup for HostsGroup` binding forwards
        // the inherent `unlock`'s outcome map rather than re-swallowing it
        // (as the pre-fix `let _ = HostsGroup::unlock(self).await;` did).
        let conn = MockConnection::new("h1")
            .with_default(CommandLog::new("", "ok", "", 0, 0))
            .failing_sftp_remove();
        let mut g = HostsGroup::new(
            vec![Target::with_connection(
                "h1",
                TargetState::Enabled,
                Box::new(conn),
            )],
            false,
        );
        let _ = g.lock("", None).await;

        let outcomes = OperationGroup::unlock(&mut g).await;
        assert!(
            matches!(&outcomes["h1"], LockOutcome::Failed(_)),
            "outcomes: {outcomes:?}"
        );
    }

    #[tokio::test]
    async fn pool_unlock_fans_out_over_group() {
        use crate::target::POOL_LOCK_PATH;
        // Two hosts each carry our pool claim; the fan-out removes both. The
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
        let names: BTreeSet<String> = ["h1".to_owned(), "h2".to_owned()].into();
        let collected = std::sync::Mutex::new(BTreeMap::new());
        g.pool_unlock_collecting(false, &names, &collected).await;
        assert!(h1.file_contents(POOL_LOCK_PATH).is_none());
        assert!(h2.file_contents(POOL_LOCK_PATH).is_none());
        let collected = collected.into_inner().unwrap();
        assert_eq!(collected["h1"], LockOutcome::Released);
        assert_eq!(collected["h2"], LockOutcome::Released);
    }

    #[tokio::test]
    async fn pool_unlock_skips_an_unconnected_host_without_touching_the_wire() {
        // Mutation to catch: deleting the `has_pool_lock` guard reports
        // `Released` for a target with no built `PoolLock` at all.
        let unconnected = Target::new(
            &mtui_config::Config::default(),
            "unconnected",
            TargetState::Enabled,
        );
        let mut g = HostsGroup::new(vec![unconnected], false);
        let names: BTreeSet<String> = ["unconnected".to_owned()].into();
        let collected = std::sync::Mutex::new(BTreeMap::new());
        g.pool_unlock_collecting(false, &names, &collected).await;
        assert_eq!(
            collected.into_inner().unwrap()["unconnected"],
            LockOutcome::Skipped("not connected".to_owned())
        );
    }

    #[tokio::test]
    async fn pool_unlock_reports_a_foreign_claim_as_contended_and_continues() {
        use crate::target::POOL_LOCK_PATH;
        // h1 is ours, h2 is a foreign template's claim: the fan-out reports h2
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
        let names: BTreeSet<String> = ["h1".to_owned(), "h2".to_owned()].into();
        let collected = std::sync::Mutex::new(BTreeMap::new());
        g.pool_unlock_collecting(false, &names, &collected).await;
        assert!(h1.file_contents(POOL_LOCK_PATH).is_none());
        // h2's foreign claim is left in place, and named — not swallowed, as
        // the deleted `pool_unlock` did.
        assert!(h2.file_contents(POOL_LOCK_PATH).is_some());
        let collected = collected.into_inner().unwrap();
        assert_eq!(collected["h1"], LockOutcome::Released);
        assert!(
            matches!(&collected["h2"], LockOutcome::Contended(o) if o.by == "alice"),
            "{:?}",
            collected["h2"]
        );
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
        g.lock("", None).await; // locks h1; h2 stays foreign-locked
        g.unlock(None).await;
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
        g.fanout_set_repo(RepoOp::Add, &report, None).await;
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
        g.package_check(false, None).await;
        let before = g.get("h1").unwrap().packages()[0].before().cloned();
        assert_eq!(before, Some(RPMVersion::parse("5.1-1").unwrap()));
        assert!(g.get("h1").unwrap().packages()[0].after().is_none());

        // Post-pass records `after` (same scripted output ⇒ equals `before`,
        // the "package was not updated" state), leaving `before` intact.
        g.package_check(true, None).await;
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

        g.package_check(false, None).await;
        let before_check = g.get("h1").unwrap().packages()[0].before_check().clone();
        assert!(g.get("h1").unwrap().packages()[0].before().is_none());
        // #437: an rpm "is not installed" line is a real observation of
        // absence, not the ambiguous "never checked" state.
        assert_eq!(before_check, VersionCheck::NotInstalled);
    }

    /// #437: a package the version query never mentions at all must be
    /// distinguished from one it reports absent.
    #[tokio::test]
    async fn package_check_marks_unqueried_package_as_unchecked() {
        use mtui_types::package::Package;

        let mut pkg = Package::new("baz");
        pkg.set_required(Some("1.0-1")).unwrap();
        let mut t = host_with_rpm_output("h1", "");
        t.set_packages(vec![pkg]);
        let mut g = HostsGroup::new(vec![t], false);

        g.package_check(false, None).await;
        let before_check = g.get("h1").unwrap().packages()[0].before_check().clone();
        assert_eq!(before_check, VersionCheck::NotChecked);
    }

    #[tokio::test]
    async fn package_check_selected_leaves_the_omitted_host_untouched() {
        use mtui_types::package::Package;

        let conn1 =
            MockConnection::new("h1").with_default(CommandLog::new("", "bash 5.1-1\n", "", 0, 0));
        let conn2 =
            MockConnection::new("h2").with_default(CommandLog::new("", "bash 5.1-1\n", "", 0, 0));
        let h2 = conn2.clone();
        let mut t1 = Target::with_connection("h1", TargetState::Enabled, Box::new(conn1));
        let mut t2 = Target::with_connection("h2", TargetState::Enabled, Box::new(conn2));
        let mut pkg1 = Package::new("bash");
        pkg1.set_required(Some("5.2-1")).unwrap();
        let mut pkg2 = Package::new("bash");
        pkg2.set_required(Some("5.2-1")).unwrap();
        t1.set_packages(vec![pkg1]);
        t2.set_packages(vec![pkg2]);
        let mut g = HostsGroup::new(vec![t1, t2], false);
        let names: BTreeSet<String> = ["h1".to_owned()].into_iter().collect();

        g.package_check(false, Some(&names)).await;

        assert!(g.get("h1").unwrap().packages()[0].before().is_some());
        assert!(
            g.get("h2").unwrap().packages()[0].before().is_none(),
            "the omitted host's before/after must stay untouched"
        );
        assert!(
            h2.commands().is_empty(),
            "the omitted host must draw no query_versions command: {:?}",
            h2.commands()
        );
    }

    #[tokio::test]
    async fn query_versions_selected_leaves_the_omitted_host_untouched() {
        use mtui_types::package::Package;

        let conn1 =
            MockConnection::new("h1").with_default(CommandLog::new("", "bash 5.1-1\n", "", 0, 0));
        let conn2 =
            MockConnection::new("h2").with_default(CommandLog::new("", "bash 5.1-1\n", "", 0, 0));
        let h2 = conn2.clone();
        let mut t1 = Target::with_connection("h1", TargetState::Enabled, Box::new(conn1));
        let mut t2 = Target::with_connection("h2", TargetState::Enabled, Box::new(conn2));
        t1.set_packages(vec![Package::new("bash")]);
        t2.set_packages(vec![Package::new("bash")]);
        let mut g = HostsGroup::new(vec![t1, t2], false);
        let names: BTreeSet<String> = ["h1".to_owned()].into_iter().collect();

        g.query_versions(Some(&names)).await;

        assert!(g.get("h1").unwrap().packages()[0].current().is_some());
        assert_eq!(
            g.get("h2").unwrap().packages()[0].current_check(),
            &VersionCheck::NotChecked,
            "the omitted host's current version must stay unchecked"
        );
        assert!(
            h2.commands().is_empty(),
            "the omitted host must draw no query_versions command: {:?}",
            h2.commands()
        );
    }
}
