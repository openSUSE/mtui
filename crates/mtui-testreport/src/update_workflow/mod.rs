//! The update-workflow engine: action command tables, post-run check tables,
//! and the `${}` command-template helper.
//!
//! Two families of tables keyed by a `(release, transactional)` tuple:
//! **actions** ([`actions`]) map a key to `${}` command-template strings and
//! raise a role-specific `MissingDoerError` on an unknown one; **checks**
//! ([`checks`]) map the same key to a function over
//! `(hostname, stdout, stdin, stderr, exitcode)` that raises [`UpdateError`] on
//! a recognised failure.
//!
//! [`WorkflowRegistry`] resolves them for the bespoke prepare/update/downgrade
//! flows, and implements `mtui_hosts::PlanProvider` for the shared
//! install/uninstall template. The `PlanProvider` impl is legal here
//! and nowhere else — foreign trait, local type, and the check tables'
//! `CheckArgs` fields are `pub(crate)`. `impl OperationGroup for HostsGroup`
//! stays in `mtui-hosts`, so that crate never depends on this one.

pub mod actions;
pub mod checks;
pub mod template;

use mtui_hosts::HostError;
use thiserror::Error;

pub use checks::Diagnostic;

/// The package-manager family an action or check table entry is keyed on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Family {
    /// `zypper` on a read-write root.
    Zypper,
    /// `yum`.
    Yum,
    /// `transactional-update` on a read-only root.
    Transactional,
}

/// The [`Family`] shared by every action and check table, from a *release*
/// token — a product major version (`"11"`, `"12"`, `"15"`, `"16"`), the
/// package-manager family `"YUM"`, or `"slmicro"` — and a *transactional* flag
/// (read-only-root hosts, e.g. SL Micro). `("16", true)`, SLES 16 with a
/// read-only root, resolves SL Micro's `transactional-update` entries. `None`
/// for an unknown key.
#[must_use]
pub(crate) fn family(release: &str, transactional: bool) -> Option<Family> {
    match (release, transactional) {
        ("11" | "12" | "15" | "16", false) => Some(Family::Zypper),
        ("YUM", false) => Some(Family::Yum),
        ("slmicro" | "16", true) => Some(Family::Transactional),
        _ => None,
    }
}

/// A failure recognised by a post-run [`checks`] function.
///
/// `Display` renders `"{host}: {reason}"`, or just `"{reason}"` with no host.
/// The `reason` strings are operator-facing diagnoses asserted on by tests
/// ("package not found", "update stack locked", "RPM Error", "Dependency
/// Error", "could not determine what to patch", "Unknown Error", "Unspecified
/// Error", the per-role never-ran verdicts "update/downgrade/prepare command
/// timed out or failed to run" and install/uninstall's role-neutral "command
/// timed out or failed to run"); no code branches on them, so a check may pick
/// the most accurate one for a transcript. The contract is the `UpdateFailure`
/// variant a failure routes to, which decides whether the group is rolled back
/// — hence the probe failure travelling as the typed `probe_failed` flag rather
/// than as its string.
#[derive(Debug, Error, PartialEq, Eq, Clone)]
pub struct UpdateError {
    /// The failure reason: a short diagnosis for the operator, not a value
    /// callers match on (see the type doc).
    pub(crate) reason: String,
    /// The host the command ran on, if known.
    pub(crate) host: Option<String>,
    /// `true` when the flow stopped at a cancellation checkpoint rather than
    /// failing. Lets the command layer report a cancel *without* inferring it
    /// from the session token, which would misreport a genuine host failure
    /// that merely coincided with a cancel.
    pub(crate) cancelled: bool,
    /// `true` when the update command ran but could not work out what to patch,
    /// so dispatched none (`checks::update`'s `probe_failure`).
    ///
    /// A flag for the same reason `cancelled` is: the flow must route this away
    /// from the rollback downgrade, and the alternative — matching on the
    /// `reason` string — would be the first place in the tree where control
    /// flow depended on a reason's text.
    pub(crate) probe_failed: bool,
    /// The hosts the operation completed on despite this error, sorted. Set
    /// only where every one of them is known to be done; empty otherwise.
    pub(crate) completed: Vec<String>,
}

impl UpdateError {
    /// Builds an [`UpdateError`] with a `reason` and the `host` it occurred on.
    #[must_use]
    pub fn new(reason: impl Into<String>, host: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
            host: Some(host.into()),
            cancelled: false,
            probe_failed: false,
            completed: Vec::new(),
        }
    }

    /// Builds a host-less [`UpdateError`] marking a cooperative cancellation.
    #[must_use]
    pub fn cancelled(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
            host: None,
            cancelled: true,
            probe_failed: false,
            completed: Vec::new(),
        }
    }

    /// `true` when this error records a cancellation, not a failure.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.cancelled
    }

    /// Records the hosts the operation completed on despite this error.
    #[must_use]
    pub fn with_completed(mut self, hosts: Vec<String>) -> Self {
        self.completed = hosts;
        self
    }

    /// The hosts the operation completed on despite this error; see
    /// [`with_completed`](Self::with_completed).
    #[must_use]
    pub fn completed(&self) -> &[String] {
        &self.completed
    }

    /// Builds an [`UpdateError`] for an update that could not determine what to
    /// patch, so never dispatched one. The flag is what `reports::update_flow`
    /// routes on; see [`probe_failed`](Self::probe_failed).
    #[must_use]
    pub(crate) fn probe_failure(reason: impl Into<String>, host: impl Into<String>) -> Self {
        Self {
            probe_failed: true,
            ..Self::new(reason, host)
        }
    }

    /// Builds a host-less [`UpdateError`].
    #[must_use]
    pub(crate) fn reason_only(reason: impl Into<String>) -> Self {
        Self {
            cancelled: false,
            probe_failed: false,
            reason: reason.into(),
            host: None,
            completed: Vec::new(),
        }
    }
}

impl std::fmt::Display for UpdateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.host {
            Some(host) => write!(f, "{host}: {}", self.reason),
            None => write!(f, "{}", self.reason),
        }
    }
}

/// The five update-workflow *action* roles.
///
/// Each maps to one action module and its `MissingDoerError` variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Role {
    /// `install` — `installer` / `MissingInstallerError`.
    Install,
    /// `uninstall` — `uninstaller` / `MissingUninstallerError`.
    Uninstall,
    /// `update` — `updater` / `MissingUpdaterError`.
    Update,
    /// `prepare` — `preparer` / `MissingPreparerError`.
    Prepare,
    /// `downgrade` — `downgrader` / `MissingDowngraderError`.
    Downgrade,
}

impl Role {
    /// The role string `mtui-hosts` dispatches on
    /// (`mtui_hosts::Operation::role`).
    #[must_use]
    pub const fn as_operation_role(self) -> &'static str {
        match self {
            Role::Install => "installer",
            Role::Uninstall => "uninstaller",
            Role::Update => "updater",
            Role::Prepare => "preparer",
            Role::Downgrade => "downgrader",
        }
    }

    /// Parses the role string `mtui-hosts` dispatches on back into a typed role.
    ///
    /// `mtui_hosts::PlanProvider` is keyed by `&str` because `mtui-hosts` cannot
    /// name this enum without a crate cycle; this is the inverse of
    /// [`as_operation_role`](Self::as_operation_role) and the only place that
    /// stringly-typed seam is decoded.
    #[must_use]
    pub fn from_operation_role(role: &str) -> Option<Self> {
        match role {
            "installer" => Some(Role::Install),
            "uninstaller" => Some(Role::Uninstall),
            "updater" => Some(Role::Update),
            "preparer" => Some(Role::Prepare),
            "downgrader" => Some(Role::Downgrade),
            _ => None,
        }
    }

    /// Builds the role's "missing doer" [`HostError`] for an unknown
    /// `(release, transactional)` key.
    #[must_use]
    fn missing_error(self, release: &str) -> HostError {
        HostError::missing_for(self.as_operation_role(), release)
    }
}

/// The registry of action and check tables, keyed by
/// `(role, release, transactional)`.
///
/// The one concrete registry every flow builds and, for install/uninstall,
/// injects into the `HostsGroup` as a `mtui_hosts::PlanProvider`. Carries the
/// prepare-only `force` / `testing` flags, which the other actions ignore.
#[derive(Debug, Clone, Copy, Default)]
pub struct WorkflowRegistry {
    /// The `--force-resolution` flag threaded into `prepare` doers.
    force: bool,
    /// The `testing`-repos flag threaded into `prepare` doers.
    testing: bool,
}

impl WorkflowRegistry {
    /// Builds a registry with the given prepare flags.
    #[must_use]
    pub(crate) fn new(force: bool, testing: bool) -> Self {
        Self { force, testing }
    }

    /// Resolves the action command set for `role` at `(release, transactional)`.
    ///
    /// # Errors
    ///
    /// Returns the role's [`HostError::MissingInstaller`] / etc. when no entry
    /// exists for the key.
    pub(crate) fn action_commands(
        &self,
        role: Role,
        release: &str,
        transactional: bool,
    ) -> Result<actions::ActionCommands, HostError> {
        let resolved = match role {
            Role::Install => actions::install::installer(release, transactional),
            Role::Uninstall => actions::uninstall::uninstaller(release, transactional),
            Role::Update => actions::update::updater(release, transactional),
            Role::Prepare => {
                actions::prepare::preparer(release, transactional, self.force, self.testing)
            }
            Role::Downgrade => actions::downgrade::downgrader(release, transactional),
        };
        resolved.ok_or_else(|| role.missing_error(release))
    }

    /// Resolves the post-run check for `role` at `(release, transactional)`, or
    /// `None` when no check is registered for the key, which the caller treats
    /// as "no check to run".
    pub(crate) fn check_fn(
        &self,
        role: Role,
        release: &str,
        transactional: bool,
    ) -> Option<checks::CheckFn> {
        match role {
            // Install and uninstall share one check table.
            Role::Install | Role::Uninstall => {
                checks::install::install_check(release, transactional)
            }
            Role::Update => checks::update::update_check(release, transactional),
            Role::Prepare => checks::prepare::prepare_check(release, transactional),
            Role::Downgrade => checks::downgrade::downgrade_check(release, transactional),
        }
    }
}

/// Adapts the registry to the `mtui-hosts` install/uninstall template seam.
///
/// [`mtui_hosts::Operation`] resolves each host's command through
/// [`mtui_hosts::PlanProvider`], declared in `mtui-hosts` purely in terms of
/// `mtui-hosts` types so that crate never depends on this one. Injected by
/// `update_flow::perform_install` / `perform_uninstall` immediately before the
/// template runs.
impl mtui_hosts::PlanProvider for WorkflowRegistry {
    fn doer(
        &self,
        role: &str,
        release: &str,
        transactional: bool,
    ) -> Result<mtui_hosts::Doer, HostError> {
        // Only "installer" and "uninstaller" ever reach here
        // (`Operation::role`); anything else is reported rather than panicked on.
        let Some(role) = Role::from_operation_role(role) else {
            return Err(HostError::missing_for(role, release));
        };
        let commands = self.action_commands(role, release, transactional)?;
        // Raw templates, not rendered strings: the package list is only known
        // inside `mtui-hosts`, at `Operation::collect` time. `Doer` substitutes
        // `$packages` with a plain `replace`, which agrees with this crate's
        // substitution for every install/uninstall template (each holds exactly
        // one `$packages` and no `$$` or `${}`), as
        // `doer_templates_render_like_the_action_tables` pins.
        Ok(mtui_hosts::Doer::new(
            commands.command_template(),
            // Only read back for a transactional host, which is exactly when the
            // table carries a reboot.
            commands.reboot_template().unwrap_or_default(),
        ))
    }

    fn check(&self, role: &str, release: &str, transactional: bool) -> mtui_hosts::Check {
        // An unrecognised role resolves to no check table below, which is
        // itself a no-op fallback (`op` only labels a raw-exit failure).
        let op = match Role::from_operation_role(role) {
            Some(Role::Install) => "install",
            Some(Role::Uninstall) => "uninstall",
            _ => "operation",
        };
        let check_fn = Role::from_operation_role(role)
            .and_then(|role| self.check_fn(role, release, transactional));

        Box::new(move |a: mtui_hosts::CheckArgs<'_>| match &check_fn {
            Some(check) => {
                let args = checks::CheckArgs {
                    hostname: a.hostname,
                    stdout: a.stdout,
                    stdin: a.stdin,
                    stderr: a.stderr,
                    exitcode: a.exitcode,
                };
                match check(args) {
                    Ok(diagnostics) => {
                        for d in diagnostics {
                            tracing::info!("{}", d.text);
                        }
                        Ok(())
                    }
                    Err(e) => Err(mtui_hosts::CheckFailure {
                        reason: e.reason,
                        cancelled: e.cancelled,
                    }),
                }
            }
            // Defensive only: since #406 every key with an installer or
            // uninstaller has an install-table check, so no real key reaches
            // here. It stays for the window in which a doer is added before its
            // check, falling back to the exit code alone — never "any stderr is
            // a failure", which `transactional-update` and `yum` both produce on
            // a successful run. Driven directly by
            // `plan_provider_check_falls_back_to_the_exit_code_for_an_unknown_key`,
            // since nothing in production reaches it.
            None if a.exitcode != 0 => Err(mtui_hosts::CheckFailure::new(format!(
                "{op} command failed"
            ))),
            None => Ok(()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn update_error_display_with_host_is_stable() {
        let e = UpdateError::new("package not found", "host.example");
        assert_eq!(e.to_string(), "host.example: package not found");
    }

    #[test]
    fn update_error_display_without_host_is_reason_only() {
        let e = UpdateError::reason_only("RPM Error");
        assert_eq!(e.to_string(), "RPM Error");
    }

    #[test]
    fn role_missing_error_maps_to_matching_host_error() {
        assert!(matches!(
            Role::Install.missing_error("15"),
            HostError::MissingInstaller { .. }
        ));
        assert!(matches!(
            Role::Uninstall.missing_error("15"),
            HostError::MissingUninstaller { .. }
        ));
        assert!(matches!(
            Role::Update.missing_error("15"),
            HostError::MissingUpdater { .. }
        ));
        assert!(matches!(
            Role::Prepare.missing_error("15"),
            HostError::MissingPreparer { .. }
        ));
        assert!(matches!(
            Role::Downgrade.missing_error("15"),
            HostError::MissingDowngrader { .. }
        ));
    }

    #[test]
    fn role_missing_error_carries_release_in_message() {
        assert_eq!(
            Role::Update.missing_error("opensuse-15.4").to_string(),
            "Missing Updater for opensuse-15.4"
        );
    }

    #[test]
    fn registry_resolves_installer_doer() {
        use std::collections::HashMap;
        let reg = WorkflowRegistry::default();
        let doer = reg
            .action_commands(Role::Install, "15", false)
            .expect("installer");
        let vars: HashMap<&str, &str> = [("packages", "pkg")].into_iter().collect();
        assert_eq!(
            doer.render_command(&vars).unwrap(),
            "zypper -n in -y -l pkg"
        );
    }

    #[test]
    fn registry_missing_doer_maps_to_role_error() {
        let reg = WorkflowRegistry::default();
        let err = reg.action_commands(Role::Install, "99", false).unwrap_err();
        assert!(matches!(err, HostError::MissingInstaller { .. }));
        let err = reg
            .action_commands(Role::Downgrade, "99", false)
            .unwrap_err();
        assert!(matches!(err, HostError::MissingDowngrader { .. }));
    }

    #[test]
    fn registry_prepare_honours_force_flag() {
        use std::collections::HashMap;
        let vars: HashMap<&str, &str> = [("package", "p")].into_iter().collect();
        let forced = WorkflowRegistry::new(true, false)
            .action_commands(Role::Prepare, "15", false)
            .unwrap();
        assert!(
            forced
                .render_command(&vars)
                .unwrap()
                .contains("--force-resolution")
        );
        let unforced = WorkflowRegistry::new(false, false)
            .action_commands(Role::Prepare, "15", false)
            .unwrap();
        assert!(
            !unforced
                .render_command(&vars)
                .unwrap()
                .contains("--force-resolution")
        );
    }

    #[test]
    fn registry_uninstall_uses_install_check_table() {
        let reg = WorkflowRegistry::default();
        // uninstall shares the install check table.
        assert!(reg.check_fn(Role::Uninstall, "15", false).is_some());
    }

    #[test]
    fn registry_resolves_and_runs_a_check() {
        let reg = WorkflowRegistry::default();
        let check = reg
            .check_fn(Role::Install, "15", false)
            .expect("install check");
        let ok = check(checks::CheckArgs {
            hostname: "h1",
            stdout: "",
            stdin: "zypper in",
            stderr: "",
            exitcode: 0,
        });
        assert!(ok.is_ok());
        let err = check(checks::CheckArgs {
            hostname: "h1",
            stdout: "",
            stdin: "zypper in",
            stderr: "",
            exitcode: 104,
        });
        assert_eq!(err.unwrap_err().reason, "package not found");
    }

    #[test]
    fn registry_check_unknown_key_is_none() {
        let reg = WorkflowRegistry::default();
        // Keys with no doer either: `("slmicro", true)` is now a *registered*
        // update key and would demonstrate nothing.
        assert!(reg.check_fn(Role::Update, "nonesuch", false).is_none());
        assert!(reg.check_fn(Role::Update, "15", true).is_none());
    }

    #[test]
    fn registry_resolves_the_update_checks_that_used_to_be_missing() {
        // `("slmicro", true)` and `("YUM", false)` had an *updater* but no
        // check, so `run_checks_where` skipped them and `update` reported
        // success on those hosts whatever the command did. Resolved through the
        // registry, the path that check takes, so the table wiring is pinned
        // too.
        let reg = WorkflowRegistry::default();
        assert!(reg.check_fn(Role::Update, "slmicro", true).is_some());
        assert!(reg.check_fn(Role::Update, "YUM", false).is_some());
    }

    #[test]
    fn registry_resolves_the_prepare_and_install_checks_that_used_to_be_missing() {
        // The sibling holes to the `update` ones above (#406): both keys had a
        // preparer, installer and uninstaller but no check, so `prepare` skipped
        // the host and `install`/`uninstall` fell through to the `PlanProvider`
        // adapter's exit-code-only fallback. `Uninstall` is listed explicitly
        // because it reaches the *install* table through the registry's match,
        // which a role-blind rewrite would silently break.
        let reg = WorkflowRegistry::default();
        for (release, transactional) in [("slmicro", true), ("YUM", false)] {
            for role in [Role::Prepare, Role::Install, Role::Uninstall] {
                assert!(
                    reg.check_fn(role, release, transactional).is_some(),
                    "{role:?} @ ({release}, {transactional}) must resolve a check"
                );
            }
        }
    }

    const ROLES: [Role; 5] = [
        Role::Install,
        Role::Uninstall,
        Role::Update,
        Role::Prepare,
        Role::Downgrade,
    ];

    /// Asserts `role`'s check gives `actual` the verdict it gives `expected` on
    /// every transcript in a matrix of exit codes and output markers.
    fn assert_checked_alike(role: Role, actual: (&str, bool), expected: (&str, bool)) {
        let reg = WorkflowRegistry::default();
        let want = reg
            .check_fn(role, expected.0, expected.1)
            .unwrap_or_else(|| panic!("{role:?} @ {expected:?} has no check"));
        let got = reg
            .check_fn(role, actual.0, actual.1)
            .unwrap_or_else(|| panic!("{role:?} @ {actual:?} has no check"));
        for (stdout, stderr, exitcode) in [
            ("", "", 0),
            ("", "", 1),
            ("", "", checks::EXIT_NOT_RUN),
            ("", "", 104),
            ("", "", 106),
            ("", "", 107),
            ("", "System management is locked", 0),
            ("", "A ZYpp transaction is already in progress.", 0),
            ("(c): c", "", 0),
            ("", "Error: rpm failed", 0),
            (
                "mtui: could not determine what to patch: awk exited 2",
                "",
                2,
            ),
        ] {
            // Only the command without `zypper` separates the zypper update
            // check (markers alone) from the transactional one. The real update
            // scripts all name `zypper`, so there the two nearly coincide.
            for stdin in ["zypper -n in", "transactional-update --continue -n pkg in"] {
                let args = || checks::CheckArgs {
                    hostname: "h1",
                    stdout,
                    stdin,
                    stderr,
                    exitcode,
                };
                assert_eq!(
                    got(args()),
                    want(args()),
                    "{role:?} @ {actual:?} vs {expected:?} on \
                     ({stdin:?}, {stdout:?}, {stderr:?}, {exitcode})"
                );
            }
        }
    }

    #[test]
    fn a_read_only_sles_16_host_resolves_the_sl_micro_command_sets() {
        // Equal entries, not merely present ones: a `("16", true)` host given
        // zypper's would run it against its read-only root.
        for reg in [
            WorkflowRegistry::new(false, false),
            WorkflowRegistry::new(true, true),
        ] {
            for role in ROLES {
                let micro = reg
                    .action_commands(role, "slmicro", true)
                    .expect("the slmicro entry");
                let sles = reg
                    .action_commands(role, "16", true)
                    .unwrap_or_else(|e| panic!("{role:?} @ (\"16\", true): {e}"));
                assert_eq!(sles, micro, "{role:?}");
            }
        }
    }

    #[test]
    fn only_package_commands_that_follow_a_refresh_continue_its_snapshot() {
        // Install and uninstall run no refresh, so they branch off the booted snapshot.
        for reg in [
            WorkflowRegistry::new(false, false),
            WorkflowRegistry::new(true, true),
        ] {
            for release in ["slmicro", "16"] {
                for role in ROLES {
                    let cmds = reg
                        .action_commands(role, release, true)
                        .unwrap_or_else(|e| panic!("{role:?} @ ({release:?}, true): {e}"));
                    let template = cmds.command_template();
                    let (wanted, unwanted) = match role {
                        Role::Prepare | Role::Update | Role::Downgrade => (
                            "transactional-update --continue -n pkg ",
                            "transactional-update -n pkg ",
                        ),
                        Role::Install | Role::Uninstall => {
                            ("transactional-update -n pkg ", "--continue")
                        }
                    };
                    assert!(
                        template.contains(wanted),
                        "{role:?} @ ({release:?}, true): {template}"
                    );
                    assert!(
                        !template.contains(unwanted),
                        "{role:?} @ ({release:?}, true): {template}"
                    );
                }
            }
        }
    }

    #[test]
    fn a_read_only_sles_16_host_is_checked_like_sl_micro() {
        for role in ROLES {
            assert_checked_alike(role, ("16", true), ("slmicro", true));
        }
    }

    #[test]
    fn a_read_write_sles_16_host_keeps_the_zypper_command_sets() {
        for reg in [
            WorkflowRegistry::new(false, false),
            WorkflowRegistry::new(true, true),
        ] {
            for role in ROLES {
                let zypper = reg
                    .action_commands(role, "15", false)
                    .expect("the zypper entry");
                let sles = reg
                    .action_commands(role, "16", false)
                    .expect("the read-write entry");
                assert_eq!(sles, zypper, "{role:?}");
                assert_eq!(sles.render_reboot().unwrap(), None, "{role:?}");
            }
        }
        for role in ROLES {
            assert_checked_alike(role, ("16", false), ("15", false));
        }
    }

    // --- the mtui-hosts PlanProvider adapter --------------------------------

    #[test]
    fn operation_role_strings_round_trip() {
        for role in [
            Role::Install,
            Role::Uninstall,
            Role::Update,
            Role::Prepare,
            Role::Downgrade,
        ] {
            assert_eq!(
                Role::from_operation_role(role.as_operation_role()),
                Some(role)
            );
        }
        assert_eq!(Role::from_operation_role("nonesuch"), None);
    }

    #[test]
    fn plan_provider_resolves_the_role_specific_table() {
        use mtui_hosts::{Doer, PlanProvider};

        let reg = WorkflowRegistry::default();
        // Each resolved doer is compared against one built straight from that
        // role's own table entry: merely asserting the two differ would also
        // pass on a role swap. `Doer::command` / `Doer::reboot` are
        // crate-private, so `Debug` is the only way to compare from here.
        let install = PlanProvider::doer(&reg, "installer", "15", false).expect("installer");
        let uninstall = PlanProvider::doer(&reg, "uninstaller", "15", false).expect("uninstaller");

        let install_commands = reg
            .action_commands(Role::Install, "15", false)
            .expect("install table entry");
        let expected_install = Doer::new(
            install_commands.command_template(),
            install_commands.reboot_template().unwrap_or_default(),
        );
        let uninstall_commands = reg
            .action_commands(Role::Uninstall, "15", false)
            .expect("uninstall table entry");
        let expected_uninstall = Doer::new(
            uninstall_commands.command_template(),
            uninstall_commands.reboot_template().unwrap_or_default(),
        );

        assert_eq!(format!("{install:?}"), format!("{expected_install:?}"));
        assert_eq!(format!("{uninstall:?}"), format!("{expected_uninstall:?}"));
    }

    #[test]
    fn plan_provider_check_falls_back_to_the_exit_code_for_an_unknown_key() {
        use mtui_hosts::PlanProvider;

        // Since #406 no production key reaches the adapter's `None` arms; they
        // are the safety net for a doer added before its check, and a net
        // nothing exercises is one nobody notices breaking.
        let reg = WorkflowRegistry::default();
        let mut check = PlanProvider::check(&reg, "installer", "nonesuch", false);
        let args = |exitcode| mtui_hosts::CheckArgs {
            hostname: "h1",
            stdout: "",
            stdin: "some install command",
            stderr: "",
            exitcode,
        };
        assert_eq!(
            check(args(1)).unwrap_err(),
            mtui_hosts::CheckFailure::new("install command failed"),
            "a non-zero exit with no check table must still fail"
        );
        assert!(check(args(0)).is_ok(), "a clean exit passes");
        // Not "any stderr is a failure": that rule would fail every SL Micro
        // install.
        let chatty = mtui_hosts::CheckArgs {
            hostname: "h1",
            stdout: "",
            stdin: "some install command",
            stderr: "warning: chatty",
            exitcode: 0,
        };
        assert!(check(chatty).is_ok(), "stderr alone is not a failure");
    }

    #[test]
    fn plan_provider_surfaces_missing_and_unknown_roles() {
        use mtui_hosts::PlanProvider;

        let reg = WorkflowRegistry::default();
        assert!(matches!(
            PlanProvider::doer(&reg, "installer", "99", false),
            Err(HostError::MissingInstaller { .. })
        ));
        assert!(matches!(
            PlanProvider::doer(&reg, "uninstaller", "99", false),
            Err(HostError::MissingUninstaller { .. })
        ));
        assert!(
            PlanProvider::doer(&reg, "nonesuch", "15", false).is_err(),
            "an unrecognised role must not resolve to a command"
        );
    }

    /// `mtui_hosts::Doer` substitutes `$packages` with a plain `str::replace`,
    /// while this crate renders the same templates through `substitute`. They
    /// agree for every install/uninstall entry today; this fails if a future
    /// template gains `$$` or `${}`, which `replace` would mangle.
    #[test]
    fn doer_templates_render_like_the_action_tables() {
        use std::collections::HashMap;

        for (release, transactional) in [
            ("11", false),
            ("12", false),
            ("15", false),
            ("16", false),
            ("YUM", false),
            ("slmicro", true),
            ("16", true),
        ] {
            for role in [Role::Install, Role::Uninstall] {
                let commands = WorkflowRegistry::default()
                    .action_commands(role, release, transactional)
                    .expect("a table entry");
                let vars: HashMap<&str, &str> = [("packages", "pkg-a pkg-b")].into_iter().collect();
                let expected = commands.render_command(&vars).expect("renders");
                let naive = commands
                    .command_template()
                    .replace("$packages", "pkg-a pkg-b");
                assert_eq!(
                    naive, expected,
                    "{role:?} @ ({release}, {transactional}) renders differently across the seam"
                );
            }
        }
    }
}
