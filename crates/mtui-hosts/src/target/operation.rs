//! The install/uninstall [`Operation`] template — `lock → run → check →
//! reboot → unlock` in one place.
//!
//! ## Overview
//!
//! The shared skeleton lives behind a template method so it is not duplicated
//! across install and uninstall. A concrete operation
//! supplies two hooks — the *doer* (the installer/uninstaller command +
//! transactional reboot templates) and the paired *check* callable — and the
//! base drives:
//!
//! 1. [`collect`](Operation::collect) the per-host command map + the
//!    transactional-only reboot map (early-return on the configured
//!    `missing_error`),
//! 2. `group.update_lock()`,
//! 3. in a fallible section: `group.run(commands)` → per-host `check(...)` →
//!    `group.reboot(reboot)`,
//! 4. **always** `group.unlock()` afterwards.
//!
//! ## Scope: template + seams
//!
//! This module owns the **template and its seams**. The template
//! consumes machinery that is deliberately *not* owned by `mtui-hosts`:
//!
//! * the *doer*/*check* dispatch (each target's doer/check for a role)
//!   is fed by the update-workflow registries that live in `mtui-testreport`;
//!   taking a direct dependency on them here would make `mtui-hosts` depend on
//!   `mtui-testreport` and **break the acyclic crate graph**,
//! * the `reboot` / reconnect lifecycle on [`HostsGroup`](super::HostsGroup).
//!
//! So the template drives the four group operations and the two per-host hooks
//! through the object-safe [`OperationGroup`] seam rather than calling
//! `HostsGroup` directly. The concrete `impl OperationGroup for HostsGroup`
//! binding — resolving each target's [`Doer`] / [`Check`] via the injected
//! [`PlanProvider`] and delegating the reboot/reconnect lifecycle to the
//! inherent [`HostsGroup`](super::HostsGroup) methods — lives in
//! [`hostgroup`](super::hostgroup). The template is driven against fully mocked
//! targets and a mocked group, so it is unit-testable offline.

use mtui_types::shellquote::quote_args;

use crate::error::HostError;

/// A per-host command (or reboot) map as ordered `(hostname, command)` pairs.
///
/// An ordered `(hostname, command)` command/reboot map, kept as
/// an ordered `Vec` so fan-out order is deterministic (sorted iteration).
pub type HostCommandMap = Vec<(String, String)>;

/// A resolved *doer*: the command and (transactional) reboot templates for one
/// target
/// (`{"command": Template, "reboot": Template}`).
///
/// The templates store values interpolated via variable substitution. The
/// only variable the install/uninstall command templates interpolate is
/// `$packages`; the reboot template takes none. [`Doer::command`] performs that
/// single substitution and [`Doer::reboot`] returns the reboot command verbatim.
/// Full `string.Template` parity (`$$`, `${name}`) is unnecessary here; the
/// real doers are constructed by the [`PlanProvider`] implementation in
/// `mtui-testreport`.
#[derive(Debug, Clone)]
pub struct Doer {
    /// The command template, with `$packages` as the sole interpolated variable.
    command_template: String,
    /// The reboot command, run only on transactional (read-only-root) hosts.
    reboot_template: String,
}

impl Doer {
    /// Builds a doer from its command and reboot templates.
    #[must_use]
    pub fn new(command_template: impl Into<String>, reboot_template: impl Into<String>) -> Self {
        Self {
            command_template: command_template.into(),
            reboot_template: reboot_template.into(),
        }
    }

    /// Substitutes `$packages` in the command template.
    ///
    /// Substitutes the shell-quoted, space-joined package list into the
    /// command template.
    #[must_use]
    fn command(&self, packages: &str) -> String {
        self.command_template.replace("$packages", packages)
    }

    /// The reboot command for a transactional host.
    ///
    /// Takes no variables.
    #[must_use]
    fn reboot(&self) -> String {
        self.reboot_template.clone()
    }
}

/// The post-run *check* callable for one target.
///
/// Invoked once per target with that host's post-run output, and returns
/// `Err(reason)` when it recognises a failure. Boxed so it is object-safe and
/// can be produced per target by the doer/check registry seam.
pub type Check = Box<dyn FnMut(CheckArgs<'_>) -> Result<(), String> + Send>;

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

/// One host's post-run output snapshot, read back by
/// [`OperationGroup::last_output`] for the [`Check`] call.
///
/// Owned (not borrowed) so the read does not hold the group borrowed across
/// the check call sandwiched between it and the reboot fan-out.
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

/// One host's contribution to an [`Operation`], resolved during
/// [`collect`](Operation::collect).
///
/// Groups the fields the template reads per target so the [`OperationGroup`]
/// seam can hand them over in one shot: the doer (for the command + reboot
/// templates), whether the host is transactional (gates the reboot map), and
/// the paired check callable.
pub struct HostPlan {
    /// The host this plan applies to.
    pub(crate) hostname: String,
    /// Whether the host is transactional (read-only-root); only transactional
    /// hosts contribute to the reboot map.
    pub(crate) transactional: bool,
    /// The resolved doer for this host.
    pub(crate) doer: Doer,
    /// The paired check callable for this host.
    pub(crate) check: Check,
}

/// The subset of [`HostsGroup`](super::HostsGroup) behaviour the [`Operation`]
/// template drives.
///
/// This object-safe seam is what keeps `mtui-hosts` acyclic: the template calls
/// `update_lock` / `run` / `reboot` / `unlock` and reads the per-host
/// [`HostPlan`]s through this trait instead of touching `HostsGroup`,
/// `Target::doer`, or the reboot lifecycle directly. The concrete binding lives
/// in [`hostgroup`](super::hostgroup) (`impl OperationGroup for HostsGroup`).
#[async_trait::async_trait]
pub trait OperationGroup: Send {
    /// Resolves the per-host plans for `role`.
    ///
    /// `role` is `"installer"` or `"uninstaller"`. An implementation looks up
    /// each target's doer/check for the role and returns one [`HostPlan`] per
    /// host, in a deterministic order. Returning `Err(missing_error)` signals
    /// that a doer is undefined for some host's product release — the template
    /// logs and returns before any lock is taken.
    fn plans(&mut self, role: &str) -> Result<Vec<HostPlan>, HostError>;

    /// Acquires the shared operation lock across the group.
    ///
    /// On success every host is
    /// locked for this process; on failure (some host is locked by another
    /// owner) the group has already released the locks it took and returns
    /// [`HostError::Update`], so the template aborts before running.
    ///
    /// # Errors
    ///
    /// Returns [`HostError::Update`] when one or more hosts were locked by
    /// another owner.
    async fn update_lock(&mut self) -> Result<(), HostError>;

    /// Runs the per-host command map.
    async fn run(&mut self, commands: HostCommandMap);

    /// Reads back `hostname`'s post-run output, for the [`Check`] call.
    ///
    /// Returns `None` for a host outside the group (should not happen for a
    /// host with a [`HostPlan`]); the template treats that as an empty
    /// snapshot rather than panicking.
    fn last_output(&self, hostname: &str) -> Option<HostOutput>;

    /// Reboots the transactional hosts named in `reboot`.
    ///
    /// Returns `(hostname, reason)` for every host that did not reconnect —
    /// a transactional host that rebooted and never came back must fail the
    /// operation, not report success on a host it lost.
    async fn reboot(&mut self, reboot: HostCommandMap) -> Vec<(String, String)>;

    /// Releases the shared operation lock.
    async fn unlock(&mut self);
}

/// The outcome of a completed [`Operation::run`]: the run *started* (the
/// template got past `plans()` and `update_lock()`), but a host's check can
/// still fail and a transactional host's post-reboot reconnect can still fail.
///
/// `Err` from [`Operation::run`] keeps meaning "never started" (no plans, or a
/// foreign lock); `Ok(report)` means it ran, and the two failure lists name any
/// host that failed its check or was left unreachable by its reboot.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct OperationReport {
    /// `(hostname, reason)` for every host whose post-run [`Check`] failed.
    pub check_failures: Vec<(String, String)>,
    /// `(hostname, reason)` for every transactional host that rebooted and did
    /// not reconnect. A host whose check already failed is excluded — its
    /// reboot was skipped, not attempted and lost.
    pub reboot_failures: Vec<(String, String)>,
}

/// The install/uninstall template method.
///
/// A concrete operation names its [`role`](Operation::role) (`"installer"` /
/// `"uninstaller"`) and how to build its [`missing_error`](Operation::missing_error);
/// the provided [`collect`](Operation::collect) and [`run`](Operation::run)
/// methods reproduce the control flow behaviour-for-behaviour.
#[async_trait::async_trait]
pub trait Operation: Send + Sync {
    /// The doer/check dispatch role: `"installer"` or `"uninstaller"`.
    fn role(&self) -> &'static str;

    /// The packages to interpolate into each host's command template.
    fn packages(&self) -> &[String];

    /// Builds the "missing doer" error for `release`, used both as the
    /// early-return sentinel and for logging.
    fn missing_error(&self, release: &str) -> HostError;

    /// Builds the per-host command map and the transactional-only reboot map.
    ///
    /// One command entry per host (with
    /// `$packages` substituted by the shell-quoted, space-joined package list)
    /// and a reboot entry only for transactional hosts. Consumes `plans` (each
    /// carries a `FnMut` check) and returns them alongside the two maps so `run`
    /// can drive the checks after the commands complete.
    fn collect(&self, plans: Vec<HostPlan>) -> (HostCommandMap, HostCommandMap, Vec<HostPlan>) {
        // Package names are substituted into a root command template; quote each
        // so a malicious name is a single literal argument, not injected shell.
        let packages = quote_args(self.packages());
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
    /// * resolve per-host plans; on the configured `missing_error`, return
    ///   **without** taking any lock,
    /// * `update_lock()`,
    /// * run the commands, invoke each host's check over its post-run output,
    ///   then reboot the transactional hosts whose check passed,
    /// * `unlock()` unconditionally afterwards.
    ///
    /// A host whose check fails is excluded from the reboot map for that host
    /// only — a healthy transactional host still reboots so its snapshot
    /// activates; a host left inert by a failed check is named at WARN.
    ///
    /// Returns the started run's [`OperationReport`], which names any host
    /// that failed its check and any transactional host that rebooted and did
    /// not reconnect — a caller that discards it cannot tell either apart from
    /// a host that came back healthy.
    ///
    /// # Errors
    ///
    /// Returns the resolver's error when no host plan can be built (no
    /// [`PlanProvider`] injected, or no doer registered for a host's
    /// `(release, transactional)` key), and [`HostError::Update`] when
    /// `update_lock` finds a host held by another owner. In both cases nothing
    /// ran on any host.
    ///
    /// mtui reports both instead of logging and swallowing them: a caller that
    /// cannot distinguish "installed"
    /// from "could not even start" will print success for an update that never
    /// touched a host — the same reasoning that makes
    /// `UpdateFailure::MissingUpdater` a hard failure in the update flow.
    async fn run(&self, group: &mut dyn OperationGroup) -> Result<OperationReport, HostError> {
        let plans = group.plans(self.role())?;

        let (commands, reboot, mut plans) = self.collect(plans);

        // `update_lock` runs *outside* the unlock-always section: when it fails
        // (a host is locked by another owner) it has already released the locks
        // it took, so `run` aborts here without entering the run/unlock section
        // — no separate unlock is issued.
        group.update_lock().await?;

        // Everything past the lock must reach `unlock()`, so this section stays
        // free of `?`. It is infallible over the group seam today (run/reboot
        // return `()`); a future fallible step must capture its error and fall
        // through to the unlock rather than returning early.
        group.run(commands).await;

        let mut check_failures: Vec<(String, String)> = Vec::new();
        for plan in &mut plans {
            let output = group.last_output(&plan.hostname).unwrap_or_default();
            let args = CheckArgs {
                hostname: &plan.hostname,
                stdout: &output.stdout,
                stdin: &output.stdin,
                stderr: &output.stderr,
                exitcode: output.exitcode,
            };
            if let Err(reason) = (plan.check)(args) {
                check_failures.push((plan.hostname.clone(), reason));
            }
        }

        // A failed check excludes its host from the reboot map: rebooting a
        // host whose install/uninstall failed would activate a snapshot the
        // operator has not yet seen the failure for.
        let failed: std::collections::HashSet<&str> =
            check_failures.iter().map(|(h, _)| h.as_str()).collect();
        let reboot: HostCommandMap = reboot
            .into_iter()
            .filter(|(host, _)| {
                let ok = !failed.contains(host.as_str());
                if !ok {
                    tracing::warn!(host = %host, "check failed; skipping reboot");
                }
                ok
            })
            .collect();
        let reboot_failures = group.reboot(reboot).await;

        group.unlock().await;
        Ok(OperationReport {
            check_failures,
            reboot_failures,
        })
    }
}

/// Install `packages` on every target in the group.
///
/// Role `"installer"`, missing sentinel
/// [`HostError::MissingInstaller`].
pub struct InstallOperation {
    packages: Vec<String>,
}

impl InstallOperation {
    /// Builds an install operation for `packages`.
    #[must_use]
    pub fn new(packages: Vec<String>) -> Self {
        Self { packages }
    }
}

impl Operation for InstallOperation {
    fn role(&self) -> &'static str {
        "installer"
    }

    fn packages(&self) -> &[String] {
        &self.packages
    }

    fn missing_error(&self, release: &str) -> HostError {
        HostError::MissingInstaller {
            release: release.to_owned(),
        }
    }
}

/// Uninstall `packages` from every target in the group.
///
/// Role `"uninstaller"`, missing sentinel
/// [`HostError::MissingUninstaller`]. Note the uninstaller deliberately
/// consults the *install* checks; that role→check mapping lives in the doer/check
/// registry injected via [`PlanProvider`] (implemented in `mtui-testreport`),
/// not here.
pub struct UninstallOperation {
    packages: Vec<String>,
}

impl UninstallOperation {
    /// Builds an uninstall operation for `packages`.
    #[must_use]
    pub fn new(packages: Vec<String>) -> Self {
        Self { packages }
    }
}

impl Operation for UninstallOperation {
    fn role(&self) -> &'static str {
        "uninstaller"
    }

    fn packages(&self) -> &[String] {
        &self.packages
    }

    fn missing_error(&self, release: &str) -> HostError {
        HostError::MissingUninstaller {
            release: release.to_owned(),
        }
    }
}

/// The injectable seam that resolves one target's [`Doer`] + [`Check`] for a
/// role, keyed on the target's `(release, transactional)` state.
///
/// This is the `mtui-hosts`-local half of the injection: it is defined here (in
/// terms of `mtui-hosts` types only — [`Doer`] / [`Check`]) so
/// [`HostsGroup`](super::HostsGroup) can hold it and drive
/// [`OperationGroup::plans`] **without** depending on `mtui-testreport`. The
/// concrete implementation lives in `mtui-testreport` (its `WorkflowRegistry`
/// adapts its own `Role` / `ActionCommands` / `CheckFn` tables into a `Doer` and
/// a `Check`) and is injected by that crate's `update_flow::perform_install` /
/// `perform_uninstall`, immediately before the template runs.
///
/// Keys the
/// registry lookup by `(self.system.get_release(), self.transactional)`.
pub trait PlanProvider: Send + Sync {
    /// Resolves the [`Doer`] (command + reboot templates) for `role` at
    /// `(release, transactional)`.
    ///
    /// `role` is the role string (`"installer"` / `"uninstaller"` /
    /// `"updater"` / `"preparer"` / `"downgrader"`).
    ///
    /// # Errors
    ///
    /// Returns the role's [`HostError::MissingInstaller`] / etc. when the
    /// registry has no entry for the key.
    fn doer(&self, role: &str, release: &str, transactional: bool) -> Result<Doer, HostError>;

    /// Resolves the post-run [`Check`] for `role` at `(release, transactional)`.
    ///
    /// A registry with no entry yields a no-op check,
    /// so this is infallible.
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

    /// One recorded interaction with the group, in call order.
    #[derive(Debug, Clone, PartialEq, Eq)]
    enum Event {
        UpdateLock,
        Run(Vec<(String, String)>),
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
        /// [`HostError::Update`] to model a foreign-locked host.
        fail_update_lock: bool,
        /// What `reboot` should report as failed, modelling a transactional
        /// host that rebooted and did not reconnect.
        reboot_failures: Vec<(String, String)>,
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
            }
        }

        /// Marks `update_lock` to fail, modelling a foreign-locked host.
        fn failing_update_lock(mut self) -> Self {
            self.fail_update_lock = true;
            self
        }

        /// Scripts `reboot` to report `failures`, modelling a transactional
        /// host whose post-reboot reconnect never came back.
        fn with_reboot_failures(mut self, failures: Vec<(String, String)>) -> Self {
            self.reboot_failures = failures;
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
                return Err(HostError::Update("Hosts locked".to_owned()));
            }
            Ok(())
        }

        async fn run(&mut self, commands: HostCommandMap) {
            self.events.lock().unwrap().push(Event::Run(commands));
        }

        fn last_output(&self, _hostname: &str) -> Option<HostOutput> {
            // The tests below assert on the Check event log, not the output
            // snapshot, so an empty snapshot is enough here.
            Some(HostOutput::default())
        }

        async fn reboot(&mut self, reboot: HostCommandMap) -> Vec<(String, String)> {
            self.events.lock().unwrap().push(Event::Reboot(reboot));
            self.reboot_failures.clone()
        }

        async fn unlock(&mut self) {
            self.events.lock().unwrap().push(Event::Unlock);
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
        result: Result<(), String>,
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

        let op = InstallOperation::new(strs(&["pkg-a"]));
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
        let op = InstallOperation::new(strs(&["pkg-a", "pkg-b", "pkg-c"]));
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
        let op = InstallOperation::new(strs(&["foo; rm -rf /"]));
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

        let op = InstallOperation::new(strs(&["pkg-a"]));
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

        let op = InstallOperation::new(strs(&["pkg-a"]));
        op.run(&mut group).await.expect("a clean run succeeds");

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

        let op = InstallOperation::new(strs(&["pkg-a"]));
        let err = op
            .run(&mut group)
            .await
            .expect_err("a foreign-held lock is reported, not swallowed");
        assert!(matches!(err, HostError::Update(_)), "{err:?}");

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

        let op = InstallOperation::new(strs(&["pkg-a"]));
        op.run(&mut group).await.expect("a clean run succeeds");

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

        InstallOperation::new(strs(&["pkg-a"]))
            .run(&mut group)
            .await
            .expect("a clean run succeeds");

        let events = group.events();
        // lock → run → check → reboot → unlock.
        assert!(matches!(events[0], Event::UpdateLock));
        assert!(matches!(events[1], Event::Run(_)));
        assert!(matches!(events[2], Event::Check(..)));
        assert!(matches!(events[3], Event::Reboot(_)));
        assert!(matches!(events[4], Event::Unlock));
        // The transactional host contributed to the reboot map.
        if let Event::Reboot(map) = &events[3] {
            assert_eq!(map, &vec![("h1".to_owned(), "systemctl reboot".to_owned())]);
        }
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
        let mut group = MockGroup::new(Ok(plans))
            .with_reboot_failures(vec![("h1".to_owned(), "reconnect failed".to_owned())]);

        let op = InstallOperation::new(strs(&["pkg-a"]));
        let report = op
            .run(&mut group)
            .await
            .expect("the run started; the reboot failure is carried in the report, not Err");

        assert_eq!(
            report.reboot_failures,
            vec![("h1".to_owned(), "reconnect failed".to_owned())]
        );
        // Unlock must still be the last event: a lost host does not skip
        // releasing the operation lock.
        assert_eq!(group.events().last(), Some(&Event::Unlock));
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
                Err("boom".to_owned()),
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

        let op = InstallOperation::new(strs(&["pkg-a"]));
        let report = op.run(&mut group).await.expect("the run started");

        assert_eq!(
            report.check_failures,
            vec![("h1".to_owned(), "boom".to_owned())]
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

    // --- role strings + missing_error sentinels -----------------------------

    #[test]
    fn install_operation_uses_installer_role_and_sentinel() {
        let op = InstallOperation::new(strs(&["pkg"]));
        assert_eq!(op.role(), "installer");
        assert_eq!(
            op.missing_error("rel").to_string(),
            "Missing Installer for rel"
        );
        assert!(matches!(
            op.missing_error("rel"),
            HostError::MissingInstaller { .. }
        ));
    }

    #[test]
    fn uninstall_operation_uses_uninstaller_role_and_sentinel() {
        let op = UninstallOperation::new(strs(&["pkg"]));
        assert_eq!(op.role(), "uninstaller");
        assert_eq!(
            op.missing_error("rel").to_string(),
            "Missing Uninstaller for rel"
        );
        assert!(matches!(
            op.missing_error("rel"),
            HostError::MissingUninstaller { .. }
        ));
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

        UninstallOperation::new(strs(&["pkg"]))
            .run(&mut group)
            .await
            .expect("a clean run succeeds");

        assert_eq!(group.roles(), vec!["uninstaller".to_owned()]);
    }
}
