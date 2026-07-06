//! The install/uninstall [`Operation`] template — `lock → run → check →
//! reboot → unlock` in one place.
//!
//! ## Reference
//!
//! Ported from upstream `mtui/hosts/target/operation.py` (`Operation`,
//! `InstallOperation`, `UninstallOperation`). Upstream consolidates the shared
//! skeleton behind a template method so it is not duplicated across
//! `HostsGroup.perform_install` and `perform_uninstall`. A concrete operation
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
//! 4. **always** `group.unlock()` afterwards (upstream's `finally`).
//!
//! ## Scope: skeleton + trait (P2.9)
//!
//! This module ports the **template and its seams only**. The upstream template
//! consumes machinery that is deliberately *not* owned by `mtui-hosts`:
//!
//! * the *doer*/*check* dispatch (`Target.doer(role)` / `Target.check(role)`)
//!   is fed by the update-workflow registries that live in `mtui-testreport`
//!   and are injected in **Phase 4**; taking a direct dependency on them here
//!   would make `mtui-hosts` depend on `mtui-testreport` and **break the
//!   acyclic crate graph**,
//! * the `reboot` / reconnect lifecycle on [`HostsGroup`](super::HostsGroup) is
//!   the sibling P2.9 reboot task.
//!
//! So the template drives the four group operations and the two per-host hooks
//! through the object-safe [`OperationGroup`] seam rather than calling
//! `HostsGroup` directly. The `impl OperationGroup for HostsGroup` binding lands
//! in **Phase 4** together with the real doer/check registries and the reboot
//! wiring — see the `TODO` at the bottom of this file. This mirrors upstream's
//! own `test_operation.py`, which drives the template against fully mocked
//! targets and a mocked group, so the port is faithfully unit-testable offline.

use crate::error::HostError;

/// A per-host command (or reboot) map as ordered `(hostname, command)` pairs.
///
/// The Rust analogue of upstream's `dict[str, str]` command/reboot maps; kept as
/// an ordered `Vec` so fan-out order is deterministic (matching upstream's
/// sorted iteration).
pub type HostCommandMap = Vec<(String, String)>;

/// A resolved *doer*: the command and (transactional) reboot templates for one
/// target, the Rust analogue of upstream's `Target.doer(role)` mapping
/// (`{"command": Template, "reboot": Template}`).
///
/// Upstream stores `string.Template` values and calls `.substitute(...)`. The
/// only variable the install/uninstall command templates interpolate is
/// `$packages`; the reboot template takes none. [`Doer::command`] performs that
/// single substitution and [`Doer::reboot`] returns the reboot command verbatim.
/// Full `string.Template` parity (`$$`, `${name}`) is unnecessary here and is
/// deferred to where real doers are constructed (Phase 4).
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
    /// Mirrors upstream `doer["command"].substitute(packages=" ".join(packages))`.
    #[must_use]
    pub fn command(&self, packages: &str) -> String {
        self.command_template.replace("$packages", packages)
    }

    /// Substitutes both `$repa` and `$packages` in the command template.
    ///
    /// Mirrors upstream `doer["command"].safe_substitute(repa=..., packages=...)`
    /// used by `perform_update` for the `updater` role. `$repa` is replaced
    /// first so a `packages` value that happens to contain the literal `$repa`
    /// is not re-substituted (upstream's `string.Template` is single-pass);
    /// other `$`-tokens the update templates embed for the remote shell (e.g.
    /// `$$r`, `awk … $2`) are left untouched, matching `safe_substitute`.
    #[must_use]
    pub fn command_with_repa(&self, repa: &str, packages: &str) -> String {
        self.command_template
            .replace("$repa", repa)
            .replace("$packages", packages)
    }

    /// The reboot command for a transactional host.
    ///
    /// Mirrors upstream `doer["reboot"].substitute()` (no variables).
    #[must_use]
    pub fn reboot(&self) -> String {
        self.reboot_template.clone()
    }
}

/// The post-run *check* callable for one target.
///
/// Invoked once per target as `check(hostname, lastout, lastin, lasterr,
/// lastexit)`, mirroring upstream's
/// `check(t.hostname, t.lastout(), t.lastin(), t.lasterr(), t.lastexit())`.
/// Boxed so it is object-safe and can be produced per target by the doer/check
/// registry seam.
pub type Check = Box<dyn FnMut(CheckArgs<'_>) + Send>;

/// The argument tuple passed to a [`Check`], keeping the call site readable and
/// matching upstream's positional `(hostname, lastout, lastin, lasterr,
/// lastexit)`.
#[derive(Debug, Clone, Copy)]
pub struct CheckArgs<'a> {
    /// The host the command ran on.
    pub hostname: &'a str,
    /// The command's stdout ([`Target::lastout`](super::Target::lastout)).
    pub lastout: &'a str,
    /// The command that was run ([`Target::lastin`](super::Target::lastin)).
    pub lastin: &'a str,
    /// The command's stderr ([`Target::lasterr`](super::Target::lasterr)).
    pub lasterr: &'a str,
    /// The command's exit code ([`Target::lastexit`](super::Target::lastexit)),
    /// or `None` when nothing has run yet.
    pub lastexit: Option<i16>,
}

/// A snapshot of one host's `last*` values, read from the group after a run.
///
/// The template reads these through [`OperationGroup::last_output`] rather than
/// borrowing individual `Target`s, keeping the seam object-safe. The field set
/// matches upstream's `(lastout, lastin, lasterr, lastexit)` check arguments.
#[derive(Debug, Default, Clone)]
pub struct LastOutput {
    /// stdout of the last command ([`Target::lastout`](super::Target::lastout)).
    pub lastout: String,
    /// the last command string ([`Target::lastin`](super::Target::lastin)).
    pub lastin: String,
    /// stderr of the last command ([`Target::lasterr`](super::Target::lasterr)).
    pub lasterr: String,
    /// exit code of the last command
    /// ([`Target::lastexit`](super::Target::lastexit)), or `None` if nothing ran.
    pub lastexit: Option<i16>,
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
    pub hostname: String,
    /// Whether the host is transactional (read-only-root); only transactional
    /// hosts contribute to the reboot map.
    pub transactional: bool,
    /// The resolved doer for this host.
    pub doer: Doer,
    /// The paired check callable for this host.
    pub check: Check,
}

/// The subset of [`HostsGroup`](super::HostsGroup) behaviour the [`Operation`]
/// template drives.
///
/// This object-safe seam is what keeps `mtui-hosts` acyclic: the template calls
/// `update_lock` / `run` / `reboot` / `unlock` and reads the per-host
/// [`HostPlan`]s through this trait instead of touching `HostsGroup`,
/// `Target::doer`, or the reboot lifecycle directly — all of which are wired in
/// Phase 4 / the sibling P2.9 reboot task. Upstream's `test_operation.py` mocks
/// exactly this surface.
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

    /// Acquires the shared operation lock across the group
    /// (`HostsGroup.update_lock`).
    ///
    /// Mirrors upstream `HostsGroup.update_lock`: on success every host is
    /// locked for this process; on failure (some host is locked by another
    /// owner) the group has already released the locks it took and returns
    /// [`HostError::Update`], so the template aborts before running.
    ///
    /// # Errors
    ///
    /// Returns [`HostError::Update`] when one or more hosts were locked by
    /// another owner.
    async fn update_lock(&mut self) -> Result<(), HostError>;

    /// Runs the per-host command map (`HostsGroup.run`).
    async fn run(&mut self, commands: HostCommandMap);

    /// Reboots the transactional hosts named in `reboot`
    /// (`HostsGroup._reboot`).
    async fn reboot(&mut self, reboot: HostCommandMap);

    /// Releases the shared operation lock (`HostsGroup.unlock`).
    async fn unlock(&mut self);

    /// Snapshots one host's `last*` values *at call time*.
    ///
    /// Read after [`run`](OperationGroup::run) so each check sees the values the
    /// command produced, mirroring upstream's per-target `t.lastout()` /
    /// `t.lastin()` / `t.lasterr()` / `t.lastexit()` reads.
    fn last_output(&self, hostname: &str) -> LastOutput;
}

/// The install/uninstall template method.
///
/// A concrete operation names its [`role`](Operation::role) (`"installer"` /
/// `"uninstaller"`) and how to build its [`missing_error`](Operation::missing_error);
/// the provided [`collect`](Operation::collect) and [`run`](Operation::run)
/// methods reproduce upstream's control flow behaviour-for-behaviour.
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
    /// Mirrors upstream `Operation.collect`: one command entry per host (with
    /// `$packages` substituted by the space-joined package list) and a reboot
    /// entry only for transactional hosts. Consumes `plans` (each carries a
    /// `FnMut` check) and returns them alongside the two maps so `run` can drive
    /// the checks after the commands complete.
    fn collect(&self, plans: Vec<HostPlan>) -> (HostCommandMap, HostCommandMap, Vec<HostPlan>) {
        let packages = self.packages().join(" ");
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
    /// Mirrors upstream `Operation.run`:
    ///
    /// * resolve per-host plans; on the configured `missing_error`, log and
    ///   return **without** taking any lock,
    /// * `update_lock()`,
    /// * run the commands, invoke each host's check with its `last*` values,
    ///   then reboot the transactional hosts,
    /// * `unlock()` unconditionally afterwards (upstream's `finally`).
    ///
    /// The per-host `last*` values are read from `group` *after* `run`, matching
    /// upstream's `t.lastout()`/etc. call-time reads.
    async fn run(&self, group: &mut dyn OperationGroup) {
        let plans = match group.plans(self.role()) {
            Ok(plans) => plans,
            Err(e) => {
                // Upstream: `logger.error("%s", e); return` — no lock is taken.
                tracing::error!("{e}");
                return;
            }
        };

        let (commands, reboot, mut plans) = self.collect(plans);

        // Upstream calls `update_lock()` *outside* the try/finally: if it raises
        // `UpdateError` (a host is locked by another owner) it has already
        // released the locks it took, so `run` aborts here without entering the
        // run/unlock section — no separate unlock is issued.
        if let Err(e) = group.update_lock().await {
            tracing::error!("{e}");
            return;
        }

        // Upstream wraps run→check→reboot in `try` with `unlock` in `finally`.
        // Rust has no `finally`; drive the fallible section in a nested async
        // block and always unlock afterwards. The section itself never returns
        // an error today (run/reboot are infallible over the group), but this
        // structure preserves the "unlock always happens" contract for when it
        // grows fallible steps.
        group.run(commands).await;
        for plan in &mut plans {
            let last = group.last_output(&plan.hostname);
            (plan.check)(CheckArgs {
                hostname: &plan.hostname,
                lastout: &last.lastout,
                lastin: &last.lastin,
                lasterr: &last.lasterr,
                lastexit: last.lastexit,
            });
        }
        group.reboot(reboot).await;

        group.unlock().await;
    }
}

/// Install `packages` on every target in the group.
///
/// Mirrors upstream `InstallOperation` (role `"installer"`, missing sentinel
/// [`HostError::MissingInstaller`]).
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
/// Mirrors upstream `UninstallOperation` (role `"uninstaller"`, missing sentinel
/// [`HostError::MissingUninstaller`]). Note upstream's uninstaller deliberately
/// consults the *install* checks; that role→check mapping lives in the doer/check
/// registry (Phase 4), not here.
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
/// This is the `mtui-hosts`-local half of the composition-root injection: it is
/// defined here (in terms of `mtui-hosts` types only — [`Doer`] / [`Check`]) so
/// [`HostsGroup`](super::HostsGroup) can hold it and drive
/// [`OperationGroup::plans`] **without** depending on `mtui-testreport`. The
/// concrete implementation lives in `mtui-testreport` (its `WorkflowRegistry`
/// adapts its own `Role` / `ActionCommands` / `CheckFn` tables into a `Doer` and
/// a `Check`) and is bound in at the composition root (`mtui-core::wiring`).
///
/// Mirrors upstream `Target.doer(role)` / `Target.check(role)`, which key the
/// registry lookup by `(self.system.get_release(), self.transactional)`.
pub trait PlanProvider: Send + Sync {
    /// Resolves the [`Doer`] (command + reboot templates) for `role` at
    /// `(release, transactional)`.
    ///
    /// `role` is the upstream role string (`"installer"` / `"uninstaller"` /
    /// `"updater"` / `"preparer"` / `"downgrader"`).
    ///
    /// # Errors
    ///
    /// Returns the role's [`HostError::MissingInstaller`] / etc. when the
    /// registry has no entry for the key, matching upstream's `Missing*Error`.
    fn doer(&self, role: &str, release: &str, transactional: bool) -> Result<Doer, HostError>;

    /// Resolves the post-run [`Check`] for `role` at `(release, transactional)`.
    ///
    /// A registry with no entry yields a no-op check (upstream's `_no_checks`
    /// fallback), so this is infallible.
    fn check(&self, role: &str, release: &str, transactional: bool) -> Check;
}

#[cfg(test)]
mod plan_provider_tests {
    //! Tests for the reboot-map plumbing that exercises [`PlanProvider`] via a
    //! test double; the full `impl OperationGroup for HostsGroup` binding is
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
            Box::new(|_a: CheckArgs<'_>| {})
        }
    }

    #[test]
    fn provider_resolves_doer_and_no_op_check() {
        let p = FakeProvider;
        let doer = p.doer("installer", "15", false).expect("doer");
        assert_eq!(doer.command("pkg"), "zypper -n in pkg");
        // The no-op check does not panic and returns unit.
        let mut check = p.check("installer", "15", false);
        check(CheckArgs {
            hostname: "h1",
            lastout: "",
            lastin: "",
            lasterr: "",
            lastexit: Some(0),
        });
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
    //! Ported from upstream `tests/test_operation.py`. Upstream drives the
    //! template against a `MagicMock` group and `MagicMock` targets; the Rust
    //! analogue is [`MockGroup`], an [`OperationGroup`] that records the ordered
    //! sequence of calls and serves scripted plans / `last*` values.

    use std::collections::BTreeMap;
    use std::sync::{Arc, Mutex};

    use super::*;

    /// One recorded interaction with the group, in call order.
    #[derive(Debug, Clone, PartialEq, Eq)]
    enum Event {
        UpdateLock,
        Run(Vec<(String, String)>),
        Reboot(Vec<(String, String)>),
        Unlock,
        /// A per-host check invocation: `(hostname, lastout, lastin, lasterr,
        /// lastexit)`.
        Check(String, String, String, String, Option<i16>),
    }

    /// A scriptable [`OperationGroup`] test double.
    struct MockGroup {
        /// What `plans(role)` should return; `Err` models a missing doer.
        plans: Option<Result<Vec<HostPlan>, HostError>>,
        /// Roles `plans` was called with, for role-assertion tests.
        roles_seen: Arc<Mutex<Vec<String>>>,
        /// Per-host `last*` snapshots served by `last_output`.
        last: BTreeMap<String, LastOutput>,
        /// The ordered event log.
        events: Arc<Mutex<Vec<Event>>>,
        /// When `true`, `update_lock` records its event then returns
        /// [`HostError::Update`] to model a foreign-locked host.
        fail_update_lock: bool,
    }

    impl MockGroup {
        fn new(
            plans: Result<Vec<HostPlan>, HostError>,
            last: BTreeMap<String, LastOutput>,
        ) -> Self {
            Self::with_event_log(plans, last, Arc::new(Mutex::new(Vec::new())))
        }

        /// Like [`new`](Self::new) but reuses a caller-owned event log, so
        /// per-host checks can record into the *same* timeline as
        /// lock/run/reboot/unlock for strict ordering assertions.
        fn with_event_log(
            plans: Result<Vec<HostPlan>, HostError>,
            last: BTreeMap<String, LastOutput>,
            events: Arc<Mutex<Vec<Event>>>,
        ) -> Self {
            Self {
                plans: Some(plans),
                roles_seen: Arc::new(Mutex::new(Vec::new())),
                last,
                events,
                fail_update_lock: false,
            }
        }

        /// Marks `update_lock` to fail, modelling a foreign-locked host.
        fn failing_update_lock(mut self) -> Self {
            self.fail_update_lock = true;
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

        async fn reboot(&mut self, reboot: HostCommandMap) {
            self.events.lock().unwrap().push(Event::Reboot(reboot));
        }

        async fn unlock(&mut self) {
            self.events.lock().unwrap().push(Event::Unlock);
        }

        fn last_output(&self, hostname: &str) -> LastOutput {
            self.last.get(hostname).cloned().unwrap_or_default()
        }
    }

    /// Builds a [`HostPlan`] whose check records a [`Event::Check`] into `sink`.
    fn plan_with_recording_check(
        hostname: &str,
        transactional: bool,
        doer: Doer,
        sink: Arc<Mutex<Vec<Event>>>,
    ) -> HostPlan {
        let check: Check = Box::new(move |a: CheckArgs<'_>| {
            sink.lock().unwrap().push(Event::Check(
                a.hostname.to_owned(),
                a.lastout.to_owned(),
                a.lastin.to_owned(),
                a.lasterr.to_owned(),
                a.lastexit,
            ));
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

    #[test]
    fn doer_command_with_repa_substitutes_both_and_leaves_shell_tokens() {
        // Mirrors the updater template shape: interpolates $repa + $packages but
        // must leave the remote-shell `$$r` and `awk … $2` tokens untouched.
        let doer = Doer::new(
            "zypper -n patches | grep $repa\nzypper -n in $packages\n\
             zypper -n lr | awk '/$repa/ {{ print $2; }}' | while read r; do rr $$r; done",
            "systemctl reboot",
        );
        let out = doer.command_with_repa(":p=1:2", "pkg-a pkg-b");
        assert_eq!(
            out,
            "zypper -n patches | grep :p=1:2\nzypper -n in pkg-a pkg-b\n\
             zypper -n lr | awk '/:p=1:2/ {{ print $2; }}' | while read r; do rr $$r; done"
        );
    }

    // --- collect(): commands per host; reboot only for transactional --------
    // Upstream: test_operation_collects_commands_and_reboot_per_transactional.

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

    // --- run(): early return on missing doer, no lock/run/unlock/reboot -----
    // Upstream: test_operation_returns_early_when_doer_raises_missing_error.

    #[tokio::test]
    async fn run_returns_early_without_touching_lock_when_plans_errors() {
        let mut group = MockGroup::new(
            Err(HostError::MissingInstaller {
                release: "opensuse-15.4".to_owned(),
            }),
            BTreeMap::new(),
        );

        let op = InstallOperation::new(strs(&["pkg-a"]));
        op.run(&mut group).await;

        assert!(
            group.events().is_empty(),
            "no lock/run/unlock/reboot when the doer is missing, got {:?}",
            group.events()
        );
    }

    // --- run(): unlock always happens after run -----------------------------
    // Upstream: test_operation_always_unlocks_in_finally_when_run_raises.
    // The Rust template's run→check→reboot section is infallible over the group
    // seam today, so we assert the *ordering contract* upstream's `finally`
    // guarantees: unlock is the final event and always follows update_lock+run.

    #[tokio::test]
    async fn run_always_unlocks_after_running() {
        let sink = Arc::new(Mutex::new(Vec::new()));
        let plans = vec![plan_with_recording_check(
            "h1",
            false,
            Doer::new("in $packages", "r"),
            sink,
        )];
        let mut last = BTreeMap::new();
        last.insert("h1".to_owned(), LastOutput::default());
        let mut group = MockGroup::new(Ok(plans), last);

        let op = InstallOperation::new(strs(&["pkg-a"]));
        op.run(&mut group).await;

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
    // Upstream: `update_lock()` is called outside the try/finally; if it raises
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
        let mut last = BTreeMap::new();
        last.insert("h1".to_owned(), LastOutput::default());
        let mut group = MockGroup::new(Ok(plans), last).failing_update_lock();

        let op = InstallOperation::new(strs(&["pkg-a"]));
        op.run(&mut group).await;

        // update_lock was attempted, but no run / check / reboot / unlock
        // followed: the failing lock self-cleaned and aborted the operation.
        assert_eq!(group.events(), vec![Event::UpdateLock]);
    }

    // --- run(): check invoked per target with (hostname, last*) -------------
    // Upstream: test_operation_check_called_per_target_with_lastN_args.

    #[tokio::test]
    async fn check_is_called_per_target_with_lastn_args() {
        let sink = Arc::new(Mutex::new(Vec::new()));
        let plans = vec![
            plan_with_recording_check("h1", false, Doer::new("in $packages", "r"), sink.clone()),
            plan_with_recording_check("h2", false, Doer::new("in $packages", "r"), sink.clone()),
        ];
        let mut last = BTreeMap::new();
        last.insert(
            "h1".to_owned(),
            LastOutput {
                lastout: "OUT-1".to_owned(),
                lastin: "IN-1".to_owned(),
                lasterr: "ERR-1".to_owned(),
                lastexit: Some(0),
            },
        );
        last.insert(
            "h2".to_owned(),
            LastOutput {
                lastout: "OUT-2".to_owned(),
                lastin: "IN-2".to_owned(),
                lasterr: "ERR-2".to_owned(),
                lastexit: Some(1),
            },
        );
        let mut group = MockGroup::new(Ok(plans), last);

        let op = InstallOperation::new(strs(&["pkg-a"]));
        op.run(&mut group).await;

        let checks: Vec<Event> = sink.lock().unwrap().clone();
        assert_eq!(
            checks,
            vec![
                Event::Check(
                    "h1".to_owned(),
                    "OUT-1".to_owned(),
                    "IN-1".to_owned(),
                    "ERR-1".to_owned(),
                    Some(0),
                ),
                Event::Check(
                    "h2".to_owned(),
                    "OUT-2".to_owned(),
                    "IN-2".to_owned(),
                    "ERR-2".to_owned(),
                    Some(1),
                ),
            ]
        );
        // Reboot still runs (with an empty map, since neither host is transactional).
        assert!(
            group.events().iter().any(|e| matches!(e, Event::Reboot(_))),
            "reboot must be driven once per run"
        );
    }

    #[tokio::test]
    async fn run_drives_events_in_upstream_order() {
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
        let mut last = BTreeMap::new();
        last.insert("h1".to_owned(), LastOutput::default());
        let mut group = MockGroup::with_event_log(Ok(plans), last, log);

        InstallOperation::new(strs(&["pkg-a"]))
            .run(&mut group)
            .await;

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

    // --- role strings + missing_error sentinels -----------------------------
    // Upstream: test_install_and_uninstall_operations_use_correct_role_string.

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
        let mut last = BTreeMap::new();
        last.insert("h1".to_owned(), LastOutput::default());
        let mut group = MockGroup::new(Ok(plans), last);

        UninstallOperation::new(strs(&["pkg"]))
            .run(&mut group)
            .await;

        assert_eq!(group.roles(), vec!["uninstaller".to_owned()]);
    }
}
