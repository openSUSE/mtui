//! The install/uninstall [`Operation`] template — `lock → run → check →
//! reboot → unlock` in one place.
//!
//! A concrete operation supplies the *doer* (installer/uninstaller command plus
//! transactional reboot templates) and the paired *check*. The doer/check tables live
//! in `mtui-testreport` and the reboot lifecycle on `HostsGroup`; depending on them
//! would break the acyclic crate graph, so the template drives the group through the
//! object-safe [`OperationGroup`] seam (bound for `HostsGroup` in
//! [`hostgroup`](super::hostgroup), resolving doers via the injected
//! [`PlanProvider`]) and runs offline against mocks.

use std::collections::{BTreeMap, BTreeSet};

use mtui_types::shellquote::quote_args;

use super::removal::DryRuns;

use super::hostgroup::{LockOutcome, LostHold, RebootReport};
use crate::error::HostError;

/// A per-host command (or reboot) map as `(hostname, command)` pairs, kept as a `Vec`
/// so fan-out order is deterministic.
pub type HostCommandMap = Vec<(String, String)>;

/// A resolved *doer*: the command and (transactional) reboot templates for one
/// target. `Doer::command` substitutes the sole variable, `$packages`;
/// `Doer::reboot` returns the reboot command verbatim. Real doers are built by the
/// [`PlanProvider`] implementation in `mtui-testreport`.
#[derive(Debug, Clone)]
pub struct Doer {
    /// The command template, with `$packages` as the sole interpolated variable.
    command_template: String,
    /// The reboot command, run only on transactional (read-only-root) hosts.
    reboot_template: String,
    /// The command's dry run, read by the removal guard; `None` where the
    /// package manager has none the guard can read.
    probe_template: Option<String>,
}

impl Doer {
    /// Builds a doer from its command and reboot templates.
    #[must_use]
    pub fn new(command_template: impl Into<String>, reboot_template: impl Into<String>) -> Self {
        Self {
            command_template: command_template.into(),
            reboot_template: reboot_template.into(),
            probe_template: None,
        }
    }

    /// Sets the dry-run template, interpolating `$packages` like the command.
    #[must_use]
    pub fn with_probe(mut self, probe_template: Option<String>) -> Self {
        self.probe_template = probe_template;
        self
    }

    /// Substitutes `$packages` in the dry-run template, if there is one.
    #[must_use]
    fn probe(&self, packages: &str) -> Option<String> {
        self.probe_template
            .as_ref()
            .map(|t| t.replace("$packages", packages))
    }

    /// Substitutes the shell-quoted, space-joined package list into the command
    /// template.
    #[must_use]
    fn command(&self, packages: &str) -> String {
        self.command_template.replace("$packages", packages)
    }

    /// The reboot command for a transactional host (takes no variables).
    #[must_use]
    fn reboot(&self) -> String {
        self.reboot_template.clone()
    }
}

/// The post-run *check* for one target: called once with that host's output and
/// returning `Err(failure)` on a recognised failure. Boxed so it is object-safe.
pub type Check = Box<dyn FnMut(CheckArgs<'_>) -> Result<(), CheckFailure> + Send>;

/// Why a post-run [`Check`] failed.
///
/// Mirrors [`RebootFailure`]'s `reason` + cause shape so a cancellation stays
/// representable across this seam and `reports::update_flow` can route it away from
/// the rollback. The producer is `mtui-testreport`'s `WorkflowRegistry::check`
/// adapter, which forwards `UpdateError::cancelled`; no live check cancels today.
///
/// Deliberately not produced for `checks::update`'s `probe_failed`: the update role
/// drives its own template, not [`Operation`]/[`OperationGroup`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckFailure {
    /// The failure reason, shown to the operator.
    pub reason: String,
    /// `true` when the check stopped at a cancellation checkpoint rather than
    /// failing.
    pub cancelled: bool,
}

impl CheckFailure {
    /// Builds a plain (non-cancelled) check failure.
    #[must_use]
    pub fn new(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
            cancelled: false,
        }
    }

    /// Builds a check failure recording a cooperative cancellation.
    #[must_use]
    pub fn cancelled(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
            cancelled: true,
        }
    }
}

/// The argument tuple passed to a [`Check`], keeping the call site readable.
#[derive(Debug, Clone, Copy)]
pub struct CheckArgs<'a> {
    /// The host the command ran on.
    pub hostname: &'a str,
    /// The command's stdout.
    pub stdout: &'a str,
    /// The command that was run.
    pub stdin: &'a str,
    /// The command's stderr.
    pub stderr: &'a str,
    /// The command's exit code.
    pub exitcode: i32,
}

/// One host's post-run output snapshot for a [`Check`] call, read back by
/// [`OperationGroup::last_output`]. Owned so the read does not hold the group
/// borrowed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HostOutput {
    /// The command's stdout.
    pub stdout: String,
    /// The command that was run.
    pub stdin: String,
    /// The command's stderr.
    pub stderr: String,
    /// The command's exit code.
    pub exitcode: i32,
}

/// One host's contribution to an [`Operation`]: its doer, whether it is
/// transactional (gates the reboot map), and the paired check.
pub struct HostPlan {
    /// The host this plan applies to.
    pub(crate) hostname: String,
    /// Whether the host is transactional (read-only-root); only transactional
    /// hosts contribute to the reboot map.
    pub(crate) transactional: bool,
    /// The resolved doer for this host.
    pub(crate) doer: Doer,
    /// Whether the host is enabled; a disabled host runs nothing, so it has no
    /// dry run to judge.
    pub(crate) enabled: bool,
    /// The paired check callable for this host.
    pub(crate) check: Check,
}

/// The subset of [`HostsGroup`](super::HostsGroup) behaviour the [`Operation`]
/// template drives. This object-safe seam keeps `mtui-hosts` acyclic; the binding is
/// `impl OperationGroup for HostsGroup` in [`hostgroup`](super::hostgroup).
#[async_trait::async_trait]
pub trait OperationGroup: Send {
    /// Resolves the per-host plans for `role` (`"installer"` or `"uninstaller"`), one
    /// per host in deterministic order. An `Err` (see [`HostError::missing_for`])
    /// means a doer is undefined for some host's release; the template returns before
    /// any lock is taken.
    fn plans(&mut self, role: &str) -> Result<Vec<HostPlan>, HostError>;

    /// Acquires the shared operation lock across the group. On failure (a host is
    /// locked by another owner) the group has already released the locks it took.
    ///
    /// # Errors
    ///
    /// Returns [`HostError::HostsLocked`] when one or more hosts were locked by
    /// another owner.
    async fn update_lock(&mut self) -> Result<(), HostError>;

    /// Runs the per-host command map.
    async fn run(&mut self, commands: HostCommandMap);

    /// Appends one colon-joined row to the remote history file of each enabled host in
    /// `hosts`.
    ///
    /// On the group seam because the reboot happens inside the template: a row written
    /// after `run` returns would be lost on exactly the hosts that never came back.
    async fn add_history(&mut self, hosts: &BTreeSet<String>, fields: &[String]);

    /// Reads back `hostname`'s post-run output, for the [`Check`] call.
    ///
    /// `None` is a bug by construction (the host just produced a [`HostPlan`]). The
    /// template falls back to [`HostOutput::default`], which is **exit 0**; every
    /// [`Check`] reads that as success, so the host silently passes.
    fn last_output(&self, hostname: &str) -> Option<HostOutput>;

    /// Reboots the transactional hosts named in `reboot`, returning the full
    /// [`RebootReport`]: a [`RebootFailure`] per host whose reboot did not take effect
    /// (the cause routes recovery), plus the holds the reboot wiped that could not be
    /// re-taken.
    async fn reboot(&mut self, reboot: HostCommandMap) -> RebootReport;

    /// Whether cooperative cancellation has been requested, read at the template's
    /// checkpoint before the command dispatches.
    fn cancel_requested(&self) -> bool;

    /// Releases the shared operation lock, returning each host's [`LockOutcome`] so the
    /// caller can tell a benign [`Contended`](LockOutcome::Contended) lock from a
    /// stranded [`Failed`](LockOutcome::Failed) one.
    async fn unlock(&mut self) -> BTreeMap<String, LockOutcome>;
}

/// The outcome of a completed [`Operation::run`]. `Err` means "never started" (no
/// plans, or a foreign lock); `Ok(report)` means it ran, and the failure lists name
/// any host that failed its check or whose reboot did not take effect.
#[must_use = "the report names the hosts that failed; dropping it silently \
              reports a failed operation as a clean one"]
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct OperationReport {
    /// `(hostname, failure)` for every host whose post-run [`Check`] failed.
    pub check_failures: Vec<(String, CheckFailure)>,
    /// Every transactional host whose reboot did not demonstrably take effect.
    /// A host whose check already failed is excluded — its reboot was skipped,
    /// not attempted and lost.
    pub reboot_failures: Vec<RebootFailure>,
    /// One entry per hold the reboot wiped that could not be re-taken. A lost
    /// hold never fails the run; the caller reports it as degradation.
    pub lost_holds: Vec<LostHold>,
    /// `(hostname, reason)` for every host whose operation-lock release failed with
    /// [`LockOutcome::Failed`]; a benign [`Contended`](LockOutcome::Contended) release
    /// is excluded.
    ///
    /// A stranded lock does not fail a good install, so `Operation::run`'s verdict never
    /// depends on this. The aggregate warning belongs to `perform_operation` in
    /// `mtui-testreport`, which knows the operation name.
    pub unlock_failures: Vec<(String, String)>,
    /// The removal guard's verdicts, and the hosts it could not check. A
    /// refused host ran nothing past its dry run.
    pub dry_runs: DryRuns,
    /// A cancel arrived before the command dispatched: nothing ran past the
    /// dry run on any host, and the unlock was attempted (a failure is in
    /// [`unlock_failures`](Self::unlock_failures)).
    pub cancelled: bool,
}

/// Why a transactional host's post-operation reboot did not take effect.
///
/// The distinction is load-bearing: it decides whether the host can still be
/// repaired. Two causes leave it **up** on the old snapshot while the group moved on
/// (the split-brain a rollback undoes); the third leaves nothing to talk to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RebootFailureCause {
    /// The reboot was dispatched, the host went away and never came back.
    /// **Unreachable** — a rollback cannot run on it.
    Unreachable,
    /// The reboot command never reached the host. It is **still up**, on the
    /// old snapshot.
    NotDispatched,
    /// The host answered with an unchanged boot id: it never went down. It is
    /// **still up**, with the new snapshot inert.
    NotRebooted,
}

impl RebootFailureCause {
    /// Whether the host is still reachable, and so still repairable.
    #[must_use]
    pub fn host_still_reachable(self) -> bool {
        match self {
            Self::NotDispatched | Self::NotRebooted => true,
            Self::Unreachable => false,
        }
    }
}

/// One transactional host whose reboot did not take effect, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RebootFailure {
    /// The host that did not come through its reboot.
    pub host: String,
    /// Which of the three ways it failed — see [`RebootFailureCause`].
    pub cause: RebootFailureCause,
    /// The rendered underlying error, for the operator-facing message.
    pub reason: String,
}

/// Which install-family template an [`Operation`] drives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OperationKind {
    /// Install packages; doer role `"installer"`.
    Install,
    /// Uninstall packages; doer role `"uninstaller"`, which deliberately consults the
    /// *install* checks (the mapping lives in the registry behind [`PlanProvider`]).
    Uninstall,
}

impl OperationKind {
    /// The doer/check dispatch role: `"installer"` or `"uninstaller"`.
    const fn role(self) -> &'static str {
        match self {
            Self::Install => "installer",
            Self::Uninstall => "uninstaller",
        }
    }

    /// The operation's label in the remote history file: `"install"` or `"uninstall"`.
    ///
    /// Deliberately not derived from [`role`](Self::role): `/var/log/mtui.log` is a parsed
    /// interop contract whose readers match the command verb.
    const fn history_label(self) -> &'static str {
        match self {
            Self::Install => "install",
            Self::Uninstall => "uninstall",
        }
    }
}

/// The install/uninstall template: `lock → run → check → reboot → unlock` over
/// an [`OperationGroup`], for one [`OperationKind`] and package list.
#[derive(Debug, Clone)]
pub struct Operation {
    kind: OperationKind,
    packages: Vec<String>,
    /// Whether to skip the removal guard (`--allow-removal`).
    allow_removal: bool,
}

impl Operation {
    /// An operation of `kind` over `packages`.
    #[must_use]
    pub fn new(kind: OperationKind, packages: Vec<String>) -> Self {
        Self {
            kind,
            packages,
            allow_removal: false,
        }
    }

    /// Skips the removal guard when `allow` is set.
    #[must_use]
    pub fn with_allow_removal(mut self, allow: bool) -> Self {
        self.allow_removal = allow;
        self
    }

    /// Installs `packages` on every target in the group.
    #[must_use]
    pub fn install(packages: Vec<String>) -> Self {
        Self::new(OperationKind::Install, packages)
    }

    /// Uninstalls `packages` from every target in the group.
    #[must_use]
    pub fn uninstall(packages: Vec<String>) -> Self {
        Self::new(OperationKind::Uninstall, packages)
    }

    /// Builds the per-host command map (with `$packages` substituted) and the
    /// transactional-only reboot map, returning the plans so `run` can drive their
    /// `FnMut` checks afterwards.
    fn collect(&self, plans: Vec<HostPlan>) -> (HostCommandMap, HostCommandMap, Vec<HostPlan>) {
        // Package names are substituted into a root command template; quote each
        // so a malicious name is a single literal argument, not injected shell.
        let packages = quote_args(&self.packages);
        let mut commands = Vec::with_capacity(plans.len());
        let mut reboot = Vec::new();
        for plan in &plans {
            commands.push((plan.hostname.clone(), plan.doer.command(&packages)));
            if plan.transactional {
                reboot.push((plan.hostname.clone(), plan.doer.reboot()));
            }
        }
        (commands, reboot, plans)
    }

    /// Executes the full `lock → run → check → reboot → unlock` skeleton.
    ///
    /// If plan resolution fails, returns without taking any lock. Unless the operation
    /// allows removals, each enabled host's dry run runs after the lock and every host
    /// it refuses is left out of the rest of the run (see [`removal`](super::removal));
    /// the verdicts land in [`OperationReport::dry_runs`]. A cancel requested by then
    /// skips to the unlock and is reported as [`OperationReport::cancelled`]. A host
    /// whose check fails is excluded from the reboot map; healthy transactional hosts
    /// still reboot and the inert one is named at WARN.
    ///
    /// The reboot re-takes even the comment-less operation lock, though `unlock()`
    /// releases it right after, so a foreign claim in the gap is reported as a lost hold
    /// instead of leaving a stale one.
    ///
    /// Returns the started run's [`OperationReport`]; a caller that discards it cannot
    /// tell failures from a healthy host.
    ///
    /// # Errors
    ///
    /// Returns the resolver's error when no host plan can be built (no
    /// [`PlanProvider`] injected, or no doer registered for a host's
    /// `(release, transactional)` key), and [`HostError::HostsLocked`] when
    /// `update_lock` finds a host held by another owner. In both cases nothing
    /// ran on any host.
    ///
    /// Both are reported rather than logged and swallowed, so a caller cannot print
    /// success for an update that never touched a host.
    pub async fn run(&self, group: &mut dyn OperationGroup) -> Result<OperationReport, HostError> {
        let plans = group.plans(self.kind.role())?;

        let (commands, reboot, mut plans) = self.collect(plans);

        // `update_lock` releases whatever it took on failure, so a failure here skips the
        // unlock section.
        group.update_lock().await?;

        // Everything past the lock must reach `unlock()`, so no `?` in this section.
        let dry_runs = if self.allow_removal {
            DryRuns::default()
        } else {
            let packages = quote_args(&self.packages);
            let probes = plans
                .iter()
                .filter(|p| p.enabled)
                .map(|p| (p.hostname.clone(), p.doer.probe(&packages)))
                .collect();
            super::removal::probe_removals(group, probes, &self.packages).await
        };
        // The dry run is a fan-out and is never gated on the token; this
        // checkpoint sits after it, before the dispatch it protects.
        let cancelled = group.cancel_requested();
        let (check_failures, reboot_report) = if cancelled {
            (Vec::new(), RebootReport::default())
        } else {
            let history = [
                self.kind.history_label().to_owned(),
                self.packages.join(" "),
            ];
            dispatch(group, &dry_runs, commands, reboot, &mut plans, &history).await
        };
        let reboot_failures = reboot_report.failures();

        let unlock_failures = group
            .unlock()
            .await
            .into_iter()
            .filter_map(|(host, outcome)| match outcome {
                LockOutcome::Failed(reason) => Some((host, reason)),
                LockOutcome::Acquired
                | LockOutcome::Released
                | LockOutcome::Contended(_)
                | LockOutcome::Skipped(_) => None,
            })
            .collect();
        Ok(OperationReport {
            check_failures,
            reboot_failures,
            unlock_failures,
            lost_holds: reboot_report.lost_holds,
            dry_runs,
            cancelled,
        })
    }
}

/// [`Operation::run`]'s dispatch: runs the commands on every host the guard did not
/// refuse, records `history`, checks each host, and reboots the transactional hosts
/// whose check passed.
async fn dispatch(
    group: &mut dyn OperationGroup,
    dry_runs: &DryRuns,
    commands: HostCommandMap,
    reboot: HostCommandMap,
    plans: &mut [HostPlan],
    history: &[String],
) -> (Vec<(String, CheckFailure)>, RebootReport) {
    let refused: BTreeSet<String> = dry_runs
        .verdicts
        .iter()
        .filter(|(_, verdict)| verdict.is_err())
        .map(|(host, _)| host.clone())
        .collect();
    let commands: HostCommandMap = commands
        .into_iter()
        .filter(|(host, _)| !refused.contains(host))
        .collect();
    let dispatched: BTreeSet<String> = commands.iter().map(|(h, _)| h.clone()).collect();
    group.run(commands).await;

    // Record history after the command dispatched but before the reboot: a host that
    // never comes back cannot be written to afterwards.
    group.add_history(&dispatched, history).await;

    let mut check_failures: Vec<(String, CheckFailure)> = Vec::new();
    for plan in plans.iter_mut().filter(|p| !refused.contains(&p.hostname)) {
        let output = group.last_output(&plan.hostname).unwrap_or_else(|| {
            // A planned host cannot be missing from the group; warn rather than pass silently.
            tracing::warn!(
                host = %plan.hostname,
                "no post-run output for a planned host; treating as exit 0"
            );
            HostOutput::default()
        });
        let args = CheckArgs {
            hostname: &plan.hostname,
            stdout: &output.stdout,
            stdin: &output.stdin,
            stderr: &output.stderr,
            exitcode: output.exitcode,
        };
        if let Err(failure) = (plan.check)(args) {
            check_failures.push((plan.hostname.clone(), failure));
        }
    }

    // A failed check excludes its host from the reboot map, so a failed operation's
    // snapshot is not activated unseen.
    let failed: std::collections::HashSet<&str> =
        check_failures.iter().map(|(h, _)| h.as_str()).collect();
    let reboot: HostCommandMap = reboot
        .into_iter()
        .filter(|(host, _)| {
            if refused.contains(host) {
                return false;
            }
            let ok = !failed.contains(host.as_str());
            if !ok {
                tracing::warn!(host = %host, "check failed; skipping reboot");
            }
            ok
        })
        .collect();
    (check_failures, group.reboot(reboot).await)
}

/// The injectable seam that resolves one target's [`Doer`] + [`Check`] for a role,
/// keyed on `(release, transactional)`.
///
/// Defined here in `mtui-hosts` types only so [`HostsGroup`](super::HostsGroup) can
/// drive [`OperationGroup::plans`] without depending on `mtui-testreport`, whose
/// `WorkflowRegistry` implements it and is injected by
/// `update_flow::perform_install` / `perform_uninstall` just before the template
/// runs.
pub trait PlanProvider: Send + Sync {
    /// Resolves the [`Doer`] for `role` (`"installer"` / `"uninstaller"` / `"updater"` /
    /// `"preparer"` / `"downgrader"`) at `(release, transactional)`.
    ///
    /// # Errors
    ///
    /// Returns the role's [`HostError::MissingInstaller`] / etc. when the
    /// registry has no entry for the key.
    fn doer(&self, role: &str, release: &str, transactional: bool) -> Result<Doer, HostError>;

    /// Resolves the post-run [`Check`] for `role` at `(release, transactional)`;
    /// infallible, as a missing entry yields a no-op check.
    fn check(&self, role: &str, release: &str, transactional: bool) -> Check;
}

#[cfg(test)]
mod plan_provider_tests {
    //! Tests for the reboot-map plumbing that exercises [`PlanProvider`] via a
    //! test double; the `impl OperationGroup for HostsGroup` binding is
    //! integration-tested in `crates/mtui-hosts/tests/operation_group.rs`.

    use super::*;

    struct FakeProvider;

    impl PlanProvider for FakeProvider {
        fn doer(&self, role: &str, release: &str, _transactional: bool) -> Result<Doer, HostError> {
            if release == "unknown" {
                return Err(HostError::MissingInstaller {
                    release: release.to_owned(),
                });
            }
            let _ = role;
            Ok(Doer::new("zypper -n in $packages", "systemctl reboot"))
        }
        fn check(&self, _role: &str, _release: &str, _transactional: bool) -> Check {
            Box::new(|_a: CheckArgs<'_>| Ok(()))
        }
    }

    #[test]
    fn provider_resolves_doer_and_no_op_check() {
        let p = FakeProvider;
        let doer = p.doer("installer", "15", false).expect("doer");
        assert_eq!(doer.command("pkg"), "zypper -n in pkg");
        // The no-op check does not panic and returns Ok.
        let mut check = p.check("installer", "15", false);
        assert!(
            check(CheckArgs {
                hostname: "h1",
                stdout: "",
                stdin: "",
                stderr: "",
                exitcode: 0,
            })
            .is_ok()
        );
    }

    #[test]
    fn provider_missing_doer_surfaces_error() {
        let p = FakeProvider;
        let err = p.doer("installer", "unknown", false).unwrap_err();
        assert!(matches!(err, HostError::MissingInstaller { .. }));
    }
}

#[cfg(test)]
mod tests {
    //! Drives the
    //! template against [`MockGroup`], an [`OperationGroup`] that records the ordered
    //! sequence of calls and serves scripted plans.

    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::target::{LockOwner, LostHoldKind, LostHoldProof};

    /// One recorded interaction with the group, in call order.
    #[derive(Debug, Clone, PartialEq, Eq)]
    enum Event {
        UpdateLock,
        Run(Vec<(String, String)>),
        /// A remote-history append, carrying the colon-joined fields.
        AddHistory(Vec<String>),
        Reboot(Vec<(String, String)>),
        Unlock,
        /// A per-host check invocation, carrying the host it ran on.
        Check(String),
    }

    /// A scriptable [`OperationGroup`] test double.
    struct MockGroup {
        /// What `plans(role)` should return; `Err` models a missing doer.
        plans: Option<Result<Vec<HostPlan>, HostError>>,
        /// Roles `plans` was called with, for role-assertion tests.
        roles_seen: Arc<Mutex<Vec<String>>>,
        /// The ordered event log.
        events: Arc<Mutex<Vec<Event>>>,
        /// When `true`, `update_lock` records its event then returns
        /// [`HostError::HostsLocked`] to model a foreign-locked host.
        fail_update_lock: bool,
        /// What `reboot` should report as failed, modelling a transactional
        /// host that rebooted and did not reconnect.
        reboot_failures: Vec<RebootFailure>,
        /// What `reboot` should report as lost, modelling holds another owner
        /// claimed in the reboot gap.
        lost_holds: Vec<LostHold>,
        /// What `unlock` should report per host.
        unlock_outcomes: BTreeMap<String, LockOutcome>,
        /// When `true`, `last_output` returns `None` for every host, modelling
        /// a host outside the group despite having a [`HostPlan`].
        missing_output: bool,
        /// What `last_output` answers per host.
        outputs: BTreeMap<String, HostOutput>,
        /// The host set of each `add_history` call, in order.
        history_hosts: Vec<BTreeSet<String>>,
        /// When `true`, a cancel lands while the first `run` (the dry run) is
        /// in flight.
        cancel_on_first_run: bool,
        /// Whether `run` has been called.
        ran: bool,
    }

    impl MockGroup {
        fn new(plans: Result<Vec<HostPlan>, HostError>) -> Self {
            Self::with_event_log(plans, Arc::new(Mutex::new(Vec::new())))
        }

        /// Like [`new`](Self::new) but reuses a caller-owned event log, so
        /// per-host checks can record into the *same* timeline as
        /// lock/run/reboot/unlock for strict ordering assertions.
        fn with_event_log(
            plans: Result<Vec<HostPlan>, HostError>,
            events: Arc<Mutex<Vec<Event>>>,
        ) -> Self {
            Self {
                plans: Some(plans),
                roles_seen: Arc::new(Mutex::new(Vec::new())),
                events,
                fail_update_lock: false,
                reboot_failures: Vec::new(),
                lost_holds: Vec::new(),
                unlock_outcomes: BTreeMap::new(),
                missing_output: false,
                outputs: BTreeMap::new(),
                history_hosts: Vec::new(),
                cancel_on_first_run: false,
                ran: false,
            }
        }

        /// Scripts a cancel to arrive during the first `run`.
        fn cancelling_on_first_run(mut self) -> Self {
            self.cancel_on_first_run = true;
            self
        }

        /// Scripts `last_output` to answer `stdout` and `exitcode` for `host`.
        fn with_output(mut self, host: &str, stdout: &str, exitcode: i32) -> Self {
            self.outputs.insert(
                host.to_owned(),
                HostOutput {
                    stdout: stdout.to_owned(),
                    exitcode,
                    ..HostOutput::default()
                },
            );
            self
        }

        /// Marks `update_lock` to fail, modelling a foreign-locked host.
        fn failing_update_lock(mut self) -> Self {
            self.fail_update_lock = true;
            self
        }

        /// Scripts `reboot` to report `failures`, modelling a transactional
        /// host whose post-reboot reconnect never came back.
        fn with_reboot_failures(mut self, failures: Vec<RebootFailure>) -> Self {
            self.reboot_failures = failures;
            self
        }

        /// Scripts `reboot` to report `losses`, modelling holds another owner
        /// claimed in the reboot gap.
        fn with_lost_holds(mut self, losses: Vec<LostHold>) -> Self {
            self.lost_holds = losses;
            self
        }

        /// Scripts `unlock` to report `outcomes`, modelling a host whose lock
        /// release failed or was contended.
        fn with_unlock_outcomes(mut self, outcomes: BTreeMap<String, LockOutcome>) -> Self {
            self.unlock_outcomes = outcomes;
            self
        }

        /// Scripts `last_output` to return `None` for every host, modelling
        /// the unreachable-by-construction case documented on
        /// [`OperationGroup::last_output`].
        fn with_missing_output(mut self) -> Self {
            self.missing_output = true;
            self
        }

        fn events(&self) -> Vec<Event> {
            self.events.lock().unwrap().clone()
        }

        fn roles(&self) -> Vec<String> {
            self.roles_seen.lock().unwrap().clone()
        }
    }

    #[async_trait::async_trait]
    impl OperationGroup for MockGroup {
        fn plans(&mut self, role: &str) -> Result<Vec<HostPlan>, HostError> {
            self.roles_seen.lock().unwrap().push(role.to_owned());
            self.plans
                .take()
                .expect("plans() called more than once in a test")
        }

        async fn update_lock(&mut self) -> Result<(), HostError> {
            self.events.lock().unwrap().push(Event::UpdateLock);
            if self.fail_update_lock {
                return Err(HostError::HostsLocked(vec![crate::LockRefusal {
                    host: "h1".to_owned(),
                    reason: "held by alice".to_owned(),
                }]));
            }
            Ok(())
        }

        async fn run(&mut self, commands: HostCommandMap) {
            self.ran = true;
            self.events.lock().unwrap().push(Event::Run(commands));
        }

        fn cancel_requested(&self) -> bool {
            self.cancel_on_first_run && self.ran
        }

        async fn add_history(&mut self, hosts: &BTreeSet<String>, fields: &[String]) {
            self.history_hosts.push(hosts.clone());
            self.events
                .lock()
                .unwrap()
                .push(Event::AddHistory(fields.to_vec()));
        }

        fn last_output(&self, hostname: &str) -> Option<HostOutput> {
            if self.missing_output {
                return None;
            }
            // Unscripted hosts answer an empty snapshot: most tests below
            // assert on the Check event log, not the output.
            Some(self.outputs.get(hostname).cloned().unwrap_or_default())
        }

        async fn reboot(&mut self, reboot: HostCommandMap) -> RebootReport {
            self.events.lock().unwrap().push(Event::Reboot(reboot));
            // `Update` round-trips through `failures()` verbatim; other causes
            // would gain wrapper wording, so no test scripts them.
            let outcomes = self
                .reboot_failures
                .iter()
                .map(|f| {
                    let err = match f.cause {
                        RebootFailureCause::NotDispatched => HostError::RebootNotDispatched {
                            host: f.host.clone(),
                            reason: f.reason.clone(),
                        },
                        RebootFailureCause::NotRebooted => HostError::RebootDidNotHappen {
                            host: f.host.clone(),
                        },
                        RebootFailureCause::Unreachable => HostError::ReconnectFailed {
                            host: f.host.clone(),
                        },
                    };
                    (f.host.clone(), Err(err))
                })
                .collect();
            RebootReport {
                outcomes,
                lost_holds: self.lost_holds.clone(),
            }
        }

        async fn unlock(&mut self) -> BTreeMap<String, LockOutcome> {
            self.events.lock().unwrap().push(Event::Unlock);
            self.unlock_outcomes.clone()
        }
    }

    /// Builds a [`HostPlan`] whose check records a [`Event::Check`] into `sink`
    /// and passes.
    fn plan_with_recording_check(
        hostname: &str,
        transactional: bool,
        doer: Doer,
        sink: Arc<Mutex<Vec<Event>>>,
    ) -> HostPlan {
        plan_with_check(hostname, transactional, doer, sink, Ok(()))
    }

    /// Builds a [`HostPlan`] whose check records a [`Event::Check`] into `sink`
    /// and returns `result`, so a failing check can be scripted per host.
    fn plan_with_check(
        hostname: &str,
        transactional: bool,
        doer: Doer,
        sink: Arc<Mutex<Vec<Event>>>,
        result: Result<(), CheckFailure>,
    ) -> HostPlan {
        let check: Check = Box::new(move |a: CheckArgs<'_>| {
            sink.lock()
                .unwrap()
                .push(Event::Check(a.hostname.to_owned()));
            result.clone()
        });
        HostPlan {
            hostname: hostname.to_owned(),
            transactional,
            doer,
            enabled: true,
            check,
        }
    }

    fn strs(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| (*s).to_owned()).collect()
    }

    // --- Doer::command / reboot substitution --------------------------------

    #[test]
    fn doer_substitutes_packages_in_command() {
        let doer = Doer::new("zypper -n in $packages", "systemctl reboot");
        assert_eq!(doer.command("pkg-a pkg-b"), "zypper -n in pkg-a pkg-b");
        assert_eq!(doer.reboot(), "systemctl reboot");
    }

    // --- collect(): commands per host; reboot only for transactional --------

    #[test]
    fn collect_emits_command_per_host_and_reboot_only_for_transactional() {
        let sink = Arc::new(Mutex::new(Vec::new()));
        let plans = vec![
            plan_with_recording_check(
                "h1",
                false,
                Doer::new("zypper in $packages", "reboot-1"),
                sink.clone(),
            ),
            plan_with_recording_check(
                "h2",
                true,
                Doer::new("zypper in $packages", "systemctl reboot"),
                sink.clone(),
            ),
        ];

        let op = Operation::install(strs(&["pkg-a"]));
        let (commands, reboot, returned) = op.collect(plans);

        assert_eq!(
            commands,
            vec![
                ("h1".to_owned(), "zypper in pkg-a".to_owned()),
                ("h2".to_owned(), "zypper in pkg-a".to_owned()),
            ]
        );
        // h1 is non-transactional → omitted from reboot map; only h2 present.
        assert_eq!(
            reboot,
            vec![("h2".to_owned(), "systemctl reboot".to_owned())]
        );
        // collect() hands the plans back so run() can drive the checks.
        assert_eq!(returned.len(), 2);
    }

    #[test]
    fn collect_joins_multiple_packages_with_spaces() {
        let sink = Arc::new(Mutex::new(Vec::new()));
        let plans = vec![plan_with_recording_check(
            "h1",
            false,
            Doer::new("in $packages", "r"),
            sink,
        )];
        let op = Operation::install(strs(&["pkg-a", "pkg-b", "pkg-c"]));
        let (commands, _reboot, _plans) = op.collect(plans);
        assert_eq!(commands[0].1, "in pkg-a pkg-b pkg-c");
    }

    #[test]
    fn collect_shell_quotes_malicious_package_name() {
        // A crafted package name must be substituted as a single quoted arg, so
        // the resulting root command re-splits back to `in` + the literal name.
        let sink = Arc::new(Mutex::new(Vec::new()));
        let plans = vec![plan_with_recording_check(
            "h1",
            false,
            Doer::new("in $packages", "r"),
            sink,
        )];
        let op = Operation::install(strs(&["foo; rm -rf /"]));
        let (commands, _reboot, _plans) = op.collect(plans);
        let cmd = &commands[0].1;
        assert!(
            !cmd.ends_with("in foo; rm -rf /"),
            "metacharacters leaked unquoted: {cmd:?}"
        );
        assert_eq!(
            shlex::split(cmd).unwrap(),
            vec!["in".to_owned(), "foo; rm -rf /".to_owned()],
            "package name not a single literal token: {cmd:?}"
        );
    }

    // --- run(): early return on missing doer, no lock/run/unlock/reboot -----

    #[tokio::test]
    async fn run_returns_early_without_touching_lock_when_plans_errors() {
        let mut group = MockGroup::new(Err(HostError::MissingInstaller {
            release: "opensuse-15.4".to_owned(),
        }));

        let op = Operation::install(strs(&["pkg-a"]));
        let err = op
            .run(&mut group)
            .await
            .expect_err("a missing doer is reported");
        assert!(matches!(err, HostError::MissingInstaller { .. }), "{err:?}");

        assert!(
            group.events().is_empty(),
            "no lock/run/unlock/reboot when the doer is missing, got {:?}",
            group.events()
        );
    }

    // --- run(): unlock always happens after run -----------------------------
    // The Rust template's run→check→reboot section is infallible over the group
    // seam today, so we assert the *ordering contract*: unlock is the final
    // event and always follows update_lock+run.

    #[tokio::test]
    async fn run_always_unlocks_after_running() {
        let sink = Arc::new(Mutex::new(Vec::new()));
        let plans = vec![plan_with_recording_check(
            "h1",
            false,
            Doer::new("in $packages", "r"),
            sink,
        )];
        let mut group = MockGroup::new(Ok(plans));

        let op = Operation::install(strs(&["pkg-a"]));
        // This test asserts on the recorded events, not the report.
        let _ = op.run(&mut group).await.expect("a clean run succeeds");

        let events = group.events();
        assert_eq!(events.first(), Some(&Event::UpdateLock));
        assert_eq!(events.last(), Some(&Event::Unlock));
        // Exactly one lock and one unlock.
        assert_eq!(
            events.iter().filter(|e| **e == Event::UpdateLock).count(),
            1
        );
        assert_eq!(events.iter().filter(|e| **e == Event::Unlock).count(), 1);
    }

    // --- run(): update_lock failure aborts before run/unlock ----------------
    // `update_lock()` is called outside the fallible section; if it raises
    // `UpdateError` (a host is locked by another owner) it has already released
    // the locks it took, so `run` returns without entering the run/unlock body.

    #[tokio::test]
    async fn run_aborts_when_update_lock_fails() {
        let sink = Arc::new(Mutex::new(Vec::new()));
        let plans = vec![plan_with_recording_check(
            "h1",
            false,
            Doer::new("in $packages", "r"),
            sink,
        )];
        let mut group = MockGroup::new(Ok(plans)).failing_update_lock();

        let op = Operation::install(strs(&["pkg-a"]));
        let err = op
            .run(&mut group)
            .await
            .expect_err("a foreign-held lock is reported, not swallowed");
        assert!(matches!(err, HostError::HostsLocked(_)), "{err:?}");

        // update_lock was attempted, but no run / check / reboot / unlock
        // followed: the failing lock self-cleaned and aborted the operation.
        assert_eq!(group.events(), vec![Event::UpdateLock]);
    }

    // --- run(): check invoked once per target -------------------------------

    #[tokio::test]
    async fn check_is_called_per_target() {
        let sink = Arc::new(Mutex::new(Vec::new()));
        let plans = vec![
            plan_with_recording_check("h1", false, Doer::new("in $packages", "r"), sink.clone()),
            plan_with_recording_check("h2", false, Doer::new("in $packages", "r"), sink.clone()),
        ];
        let mut group = MockGroup::new(Ok(plans));

        let op = Operation::install(strs(&["pkg-a"]));
        // This test asserts on the recorded events, not the report.
        let _ = op.run(&mut group).await.expect("a clean run succeeds");

        let checks: Vec<Event> = sink.lock().unwrap().clone();
        assert_eq!(
            checks,
            vec![Event::Check("h1".to_owned()), Event::Check("h2".to_owned())]
        );
        // Reboot still runs (with an empty map, since neither host is transactional).
        assert!(
            group.events().iter().any(|e| matches!(e, Event::Reboot(_))),
            "reboot must be driven once per run"
        );
    }

    #[tokio::test]
    async fn run_drives_events_in_template_order() {
        // Share one event log between the group and the per-host check so the
        // full lock → run → check → reboot → unlock timeline is observable in
        // a single ordered vector.
        let log = Arc::new(Mutex::new(Vec::new()));
        let plans = vec![plan_with_recording_check(
            "h1",
            true,
            Doer::new("in $packages", "systemctl reboot"),
            log.clone(),
        )];
        let mut group = MockGroup::with_event_log(Ok(plans), log);

        let _ = Operation::install(strs(&["pkg-a"]))
            .run(&mut group)
            .await
            .expect("a clean run succeeds");

        let events = group.events();
        // lock → run → history → check → reboot → unlock.
        assert!(matches!(events[0], Event::UpdateLock));
        assert!(matches!(events[1], Event::Run(_)));
        assert!(matches!(events[2], Event::AddHistory(_)));
        assert!(matches!(events[3], Event::Check(..)));
        assert!(matches!(events[4], Event::Reboot(_)));
        assert!(matches!(events[5], Event::Unlock));
        // The history row must land *after* the command dispatched and
        // *before* the reboot: written earlier it would claim work that never
        // started, written later it is lost on a host that never comes back.
        // Asserting the index alone would not say which side of the reboot it
        // fell on, so pin both neighbours.
        if let Event::AddHistory(fields) = &events[2] {
            // The label is the command verb, not the doer-table role key
            // (`installer`) — `/var/log/mtui.log` is parsed by other tools.
            assert_eq!(fields, &vec!["install".to_owned(), "pkg-a".to_owned()]);
        }
        // The transactional host contributed to the reboot map.
        if let Event::Reboot(map) = &events[4] {
            assert_eq!(map, &vec![("h1".to_owned(), "systemctl reboot".to_owned())]);
        }
    }

    #[tokio::test]
    async fn uninstall_history_label_is_the_verb_not_the_role_key() {
        // `role()` returns "uninstaller"; the history label must be
        // "uninstall". A `contains("uninstall")` assertion cannot tell the two
        // apart — "uninstaller" contains "uninstall" — so compare exactly.
        let log = Arc::new(Mutex::new(Vec::new()));
        let plans = vec![plan_with_recording_check(
            "h1",
            false,
            Doer::new("rm $packages", "systemctl reboot"),
            log.clone(),
        )];
        let mut group = MockGroup::with_event_log(Ok(plans), log);

        let _ = Operation::uninstall(strs(&["pkg-a"]))
            .run(&mut group)
            .await
            .expect("a clean run succeeds");

        let fields = group
            .events()
            .into_iter()
            .find_map(|e| match e {
                Event::AddHistory(f) => Some(f),
                _ => None,
            })
            .expect("the uninstall must write a history row");
        assert_eq!(fields[0], "uninstall");
    }

    // --- run(): a reboot failure is reported, not swallowed -----------------

    #[tokio::test]
    async fn run_reports_a_transactional_reboot_failure_and_still_unlocks() {
        let sink = Arc::new(Mutex::new(Vec::new()));
        let plans = vec![plan_with_recording_check(
            "h1",
            true,
            Doer::new("in $packages", "systemctl reboot"),
            sink,
        )];
        let mut group = MockGroup::new(Ok(plans)).with_reboot_failures(vec![RebootFailure {
            host: "h1".to_owned(),
            cause: RebootFailureCause::Unreachable,
            reason: "failed to reconnect to h1".to_owned(),
        }]);

        let op = Operation::install(strs(&["pkg-a"]));
        let report = op
            .run(&mut group)
            .await
            .expect("the run started; the reboot failure is carried in the report, not Err");

        assert_eq!(
            report.reboot_failures,
            vec![RebootFailure {
                host: "h1".to_owned(),
                cause: RebootFailureCause::Unreachable,
                reason: "failed to reconnect to h1".to_owned(),
            }]
        );
        // Unlock must still be the last event: a lost host does not skip
        // releasing the operation lock.
        assert_eq!(group.events().last(), Some(&Event::Unlock));
    }

    // --- run(): a lost hold is carried, not dropped -------------------------

    #[tokio::test]
    async fn run_carries_a_hold_lost_in_the_reboot_gap() {
        // The reboot succeeded but another owner claimed the hold in the gap:
        // the run stays a success and the loss rides the report, so the
        // caller can surface it instead of losing it silently.
        let sink = Arc::new(Mutex::new(Vec::new()));
        let plans = vec![plan_with_recording_check(
            "h1",
            true,
            Doer::new("in $packages", "systemctl reboot"),
            sink,
        )];
        let mut group = MockGroup::new(Ok(plans)).with_lost_holds(vec![LostHold {
            host: "h1".to_owned(),
            kind: LostHoldKind::OperationLock,
            comment: "operator hold".to_owned(),
            reason: "held by alice since Tuesday".to_owned(),
            proof: LostHoldProof::Proven,
        }]);

        let op = Operation::install(strs(&["pkg-a"]));
        let report = op
            .run(&mut group)
            .await
            .expect("a lost hold never fails the run");

        assert!(
            report.reboot_failures.is_empty(),
            "no host failed its reboot: {:?}",
            report.reboot_failures
        );
        assert_eq!(report.lost_holds.len(), 1);
        let loss = &report.lost_holds[0];
        assert_eq!(loss.host, "h1");
        assert_eq!(loss.comment, "operator hold");
        assert!(loss.describe().contains("h1"));
    }

    // --- run(): a failed check excludes its host from the reboot map --------

    #[tokio::test]
    async fn failed_check_excludes_its_host_from_the_reboot_map() {
        // Two transactional hosts; h1's check fails, h2's passes. Only h2 may
        // be rebooted — activating a snapshot for a host whose install/
        // uninstall failed would hide the failure behind a routine reboot.
        let log = Arc::new(Mutex::new(Vec::new()));
        let plans = vec![
            plan_with_check(
                "h1",
                true,
                Doer::new("in $packages", "systemctl reboot"),
                log.clone(),
                Err(CheckFailure::new("boom")),
            ),
            plan_with_check(
                "h2",
                true,
                Doer::new("in $packages", "systemctl reboot"),
                log.clone(),
                Ok(()),
            ),
        ];
        let mut group = MockGroup::with_event_log(Ok(plans), log);

        let op = Operation::install(strs(&["pkg-a"]));
        let report = op.run(&mut group).await.expect("the run started");

        assert_eq!(
            report.check_failures,
            vec![("h1".to_owned(), CheckFailure::new("boom"))]
        );

        let events = group.events();
        // Both checks ran before the (single) reboot event.
        let reboot_idx = events
            .iter()
            .position(|e| matches!(e, Event::Reboot(_)))
            .expect("a reboot event was recorded");
        let last_check_idx = events
            .iter()
            .rposition(|e| matches!(e, Event::Check(_)))
            .expect("a check event was recorded");
        assert!(
            last_check_idx < reboot_idx,
            "checks must run before the reboot: {events:?}"
        );
        if let Event::Reboot(map) = &events[reboot_idx] {
            assert_eq!(
                map,
                &vec![("h2".to_owned(), "systemctl reboot".to_owned())],
                "only the passing host's reboot may run: {map:?}"
            );
        }
    }

    // --- run(): a missing last_output silently passes the check ------------

    #[tokio::test]
    async fn missing_last_output_silently_passes_the_check_and_keeps_the_reboot() {
        // Pins the hazard documented on `OperationGroup::last_output`: a
        // `None` snapshot falls back to `HostOutput::default()` (exit 0),
        // which the check below reads as a failure only on a non-zero exit
        // code — so it passes, and the host stays in the reboot map. If the
        // fallback ever changes to fail the check instead, this goes red.
        let check: Check = Box::new(|a: CheckArgs<'_>| {
            if a.exitcode == 0 {
                Ok(())
            } else {
                Err(CheckFailure::new("nonzero exit"))
            }
        });
        let plans = vec![HostPlan {
            hostname: "h1".to_owned(),
            transactional: true,
            doer: Doer::new("in $packages", "systemctl reboot"),
            enabled: true,
            check,
        }];
        let mut group = MockGroup::new(Ok(plans)).with_missing_output();
        // Liveness guard: the fixture *is* the test. If `with_missing_output`
        // stopped being consulted, `last_output` would answer
        // `Some(HostOutput::default())` — also exit 0 — and every assertion
        // below would keep passing while covering nothing.
        assert!(
            group.last_output("h1").is_none(),
            "the fixture must really be scripting a missing snapshot"
        );

        let op = Operation::install(strs(&["pkg-a"]));
        let report = op.run(&mut group).await.expect("the run started");

        assert!(
            report.check_failures.is_empty(),
            "a missing snapshot must not fail the check: {:?}",
            report.check_failures
        );
        if let Some(Event::Reboot(map)) = group
            .events()
            .into_iter()
            .find(|e| matches!(e, Event::Reboot(_)))
        {
            assert_eq!(
                map,
                vec![("h1".to_owned(), "systemctl reboot".to_owned())],
                "the host with no output still stays in the reboot map: {map:?}"
            );
        } else {
            panic!("expected a Reboot event");
        }
    }

    // --- run(): unlock_failures names a Failed host, not a Contended one ----

    #[tokio::test]
    async fn run_reports_a_failed_unlock_but_not_a_contended_one() {
        let sink = Arc::new(Mutex::new(Vec::new()));
        let plans = vec![
            plan_with_recording_check("h1", false, Doer::new("in $packages", "r"), sink.clone()),
            plan_with_recording_check("h2", false, Doer::new("in $packages", "r"), sink),
        ];
        let mut group = MockGroup::new(Ok(plans)).with_unlock_outcomes(
            [
                ("h1".to_owned(), LockOutcome::Failed("boom".to_owned())),
                (
                    "h2".to_owned(),
                    LockOutcome::Contended(LockOwner::default()),
                ),
            ]
            .into_iter()
            .collect(),
        );

        let op = Operation::install(strs(&["pkg-a"]));
        let report = op.run(&mut group).await.expect("the run started");

        assert_eq!(
            report.unlock_failures,
            vec![("h1".to_owned(), "boom".to_owned())],
            "a Contended unlock is benign and must not be reported: {:?}",
            report.unlock_failures
        );
    }

    // --- role strings + missing-doer sentinels ------------------------------

    #[test]
    fn install_operation_uses_installer_role_and_sentinel() {
        let role = Operation::install(strs(&["pkg"])).kind.role();
        assert_eq!(role, "installer");
        let err = HostError::missing_for(role, "rel");
        assert_eq!(err.to_string(), "Missing Installer for rel");
        assert!(matches!(err, HostError::MissingInstaller { .. }));
    }

    #[test]
    fn uninstall_operation_uses_uninstaller_role_and_sentinel() {
        let role = Operation::uninstall(strs(&["pkg"])).kind.role();
        assert_eq!(role, "uninstaller");
        let err = HostError::missing_for(role, "rel");
        assert_eq!(err.to_string(), "Missing Uninstaller for rel");
        assert!(matches!(err, HostError::MissingUninstaller { .. }));
    }

    #[tokio::test]
    async fn run_looks_up_plans_with_its_own_role() {
        let sink = Arc::new(Mutex::new(Vec::new()));
        let plans = vec![plan_with_recording_check(
            "h1",
            false,
            Doer::new("in $packages", "r"),
            sink,
        )];
        let mut group = MockGroup::new(Ok(plans));

        let _ = Operation::uninstall(strs(&["pkg"]))
            .run(&mut group)
            .await
            .expect("a clean run succeeds");

        assert_eq!(group.roles(), vec!["uninstaller".to_owned()]);
    }

    // --- run(): the removal guard -------------------------------------------

    /// A complete `zypper -x` dry-run stream whose summary removes `names`.
    fn dry_run_removing(names: &[&str]) -> String {
        let solvables: String = names
            .iter()
            .map(|n| format!("<solvable type=\"package\" name=\"{n}\"/>\n"))
            .collect();
        format!(
            "<?xml version='1.0'?>\n<stream>\n<install-summary>\n<to-remove>\n{solvables}\
             </to-remove>\n</install-summary>\n</stream>\n"
        )
    }

    fn guarded_doer() -> Doer {
        Doer::new("rm $packages", "systemctl reboot").with_probe(Some("dry-rm $packages".into()))
    }

    #[tokio::test]
    async fn a_refused_host_runs_nothing_past_its_dry_run() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let plans = vec![
            plan_with_recording_check("h1", true, guarded_doer(), log.clone()),
            plan_with_recording_check("h2", true, guarded_doer(), log.clone()),
        ];
        let mut group = MockGroup::with_event_log(Ok(plans), log)
            .with_output("h1", &dry_run_removing(&["libfoo", "zypper"]), 0)
            .with_output("h2", &dry_run_removing(&["libfoo", "libfoo-devel"]), 0);

        let report = Operation::uninstall(strs(&["libfoo"]))
            .run(&mut group)
            .await
            .expect("the run started");

        let pair = |h: &str, c: &str| (h.to_owned(), c.to_owned());
        assert_eq!(
            group.events(),
            vec![
                Event::UpdateLock,
                Event::Run(vec![
                    pair("h1", "dry-rm libfoo"),
                    pair("h2", "dry-rm libfoo")
                ]),
                Event::Run(vec![pair("h2", "rm libfoo")]),
                Event::AddHistory(strs(&["uninstall", "libfoo"])),
                Event::Check("h2".to_owned()),
                Event::Reboot(vec![pair("h2", "systemctl reboot")]),
                Event::Unlock,
            ]
        );
        assert_eq!(group.history_hosts, vec![BTreeSet::from(["h2".to_owned()])]);
        assert!(
            report.dry_runs.verdicts["h1"]
                .as_ref()
                .unwrap_err()
                .contains("zypper")
        );
        assert_eq!(
            report.dry_runs.verdicts["h2"],
            Ok(strs(&["libfoo", "libfoo-devel"]))
        );
    }

    #[tokio::test]
    async fn allow_removal_runs_no_dry_run() {
        let sink = Arc::new(Mutex::new(Vec::new()));
        let plans = vec![plan_with_recording_check("h1", false, guarded_doer(), sink)];
        let mut group =
            MockGroup::new(Ok(plans)).with_output("h1", &dry_run_removing(&["zypper"]), 0);

        let report = Operation::uninstall(strs(&["libfoo"]))
            .with_allow_removal(true)
            .run(&mut group)
            .await
            .expect("the run started");

        let runs: Vec<Event> = group
            .events()
            .into_iter()
            .filter(|e| matches!(e, Event::Run(_)))
            .collect();
        assert_eq!(
            runs,
            vec![Event::Run(vec![("h1".into(), "rm libfoo".into())])]
        );
        assert_eq!(report.dry_runs, DryRuns::default());
    }

    #[tokio::test]
    async fn a_disabled_host_or_one_without_a_dry_run_is_not_probed() {
        let sink = Arc::new(Mutex::new(Vec::new()));
        let mut disabled = plan_with_recording_check("h1", false, guarded_doer(), sink.clone());
        disabled.enabled = false;
        let plans = vec![
            disabled,
            plan_with_recording_check("h2", false, Doer::new("rm $packages", "r"), sink.clone()),
            plan_with_recording_check("h3", false, guarded_doer(), sink),
        ];
        let mut group =
            MockGroup::new(Ok(plans)).with_output("h3", &dry_run_removing(&["libfoo"]), 0);

        let report = Operation::install(strs(&["libfoo"]))
            .run(&mut group)
            .await
            .expect("the run started");

        let runs: Vec<Event> = group
            .events()
            .into_iter()
            .filter(|e| matches!(e, Event::Run(_)))
            .collect();
        assert_eq!(
            runs[0],
            Event::Run(vec![("h3".into(), "dry-rm libfoo".into())])
        );
        assert_eq!(
            runs[1],
            Event::Run(vec![
                ("h1".into(), "rm libfoo".into()),
                ("h2".into(), "rm libfoo".into()),
                ("h3".into(), "rm libfoo".into()),
            ])
        );
        assert_eq!(
            report.dry_runs.verdicts.keys().collect::<Vec<_>>(),
            vec!["h3"]
        );
        assert_eq!(report.dry_runs.unchecked, strs(&["h2"]));
    }

    #[tokio::test]
    async fn a_cancel_during_the_dry_run_dispatches_nothing_and_unlocks() {
        let sink = Arc::new(Mutex::new(Vec::new()));
        let plans = vec![plan_with_recording_check("h1", true, guarded_doer(), sink)];
        let mut group = MockGroup::new(Ok(plans))
            .with_output("h1", &dry_run_removing(&["libfoo"]), 0)
            .cancelling_on_first_run();

        let report = Operation::uninstall(strs(&["libfoo"]))
            .run(&mut group)
            .await
            .expect("the run started");

        assert_eq!(
            group.events(),
            vec![
                Event::UpdateLock,
                Event::Run(vec![("h1".into(), "dry-rm libfoo".into())]),
                Event::Unlock,
            ]
        );
        assert!(report.cancelled);
        assert!(group.history_hosts.is_empty());
    }
}
