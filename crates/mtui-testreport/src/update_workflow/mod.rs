//! The update-workflow engine: action command tables, post-run check tables,
//! and the `${}` command-template helper.
//!
//! ## Model
//!
//! The workflow is modelled as two families of tables keyed by a
//! `(release, transactional)` tuple:
//!
//! * **actions** ([`actions`]) map a key to a set of `${}` command-template
//!   strings (`command`, and — depending on the action — `reboot`,
//!   `installed_only`, `list_command`). They are stored in a table
//!   that raises a role-specific `MissingDoerError` on an unknown key.
//! * **checks** ([`checks`]) map the same key to a function
//!   `(hostname, stdout, stdin, stderr, exitcode) -> None` that raises
//!   [`UpdateError`] when it recognises a failure.
//!
//! ## How the tables reach the hosts
//!
//! [`WorkflowRegistry`] implements three seams over these tables: the crate-local
//! [`DoerProvider`] / [`CheckProvider`] (used directly by the bespoke
//! prepare/update/downgrade flows) and `mtui_hosts::PlanProvider` (used by the
//! shared install/uninstall template). The `PlanProvider` impl lives in this
//! crate because the trait is foreign and the type is local — and because the
//! check tables' `CheckArgs` fields are `pub(crate)`. `impl OperationGroup for
//! HostsGroup` stays in `mtui-hosts`, so that crate never depends on this one.

pub mod actions;
pub mod checks;
pub mod template;

use mtui_hosts::HostError;
use thiserror::Error;

pub use checks::Diagnostic;

/// The lookup key shared by every action and check table.
///
/// The key is a `(str, bool)` pair: a *release* token — a product
/// major version (`"11"`, `"12"`, `"15"`, `"16"`), the package-manager family
/// `"YUM"`, or `"slmicro"` — paired with a *transactional* flag (read-only-root
/// hosts, e.g. SL Micro). The `Vec`-free `(String, bool)` here keeps ownership
/// simple; lookups accept `(&str, bool)`.
pub(crate) type WorkflowKey = (String, bool);

/// A failure recognised by a post-run [`checks`] function.
///
/// Its
/// `Display` renders `"{host}: {reason}"` when a host is
/// present, otherwise just `"{reason}"`. The `reason` strings are diagnoses
/// shown to an operator and asserted on by tests ("package not found", "update
/// stack locked", "RPM Error", "Dependency Error", "could not determine what to
/// patch", "Unknown Error", "Unspecified Error", and the per-role never-ran
/// verdicts "update/downgrade/prepare command timed out or failed to run" plus
/// install/uninstall's role-neutral "command timed out or failed to run"); no
/// code branches on them, so
/// a check is free to pick the most accurate one for a given transcript. What
/// *is* a contract is the `UpdateFailure` variant a failure routes to, which
/// decides whether the group is rolled back — which is why the probe failure
/// travels as the typed `probe_failed` flag and not as its string.
#[derive(Debug, Error, PartialEq, Eq, Clone)]
pub struct UpdateError {
    /// The failure reason: a short diagnosis for the operator, not a value
    /// callers match on (see the type doc).
    pub(crate) reason: String,
    /// The host the command ran on, if known.
    pub(crate) host: Option<String>,
    /// `true` when the flow stopped at a cancellation checkpoint rather than
    /// failing.
    ///
    /// Lets the command layer report a cancelled flow as cancelled *without*
    /// inferring it from the session token — inferring would misreport a
    /// genuine host failure that merely coincided with a cancel.
    pub(crate) cancelled: bool,
    /// `true` when the update command ran but never dispatched a patch,
    /// because it could not work out what to patch (`checks::update`'s
    /// `probe_failure`).
    ///
    /// Carried as a flag for the same reason `cancelled` is: the flow has to
    /// route this away from the group-wide rollback downgrade, and the only
    /// alternative — matching on the `reason` string in
    /// `reports::update_flow` — would be the first place in the tree where
    /// control flow depended on the text of a reason.
    pub(crate) probe_failed: bool,
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
        }
    }

    /// `true` when this error records a cancellation, not a failure.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.cancelled
    }

    /// Builds an [`UpdateError`] for an update that could not determine what
    /// to patch, so never dispatched one.
    ///
    /// The flag is what `reports::update_flow` routes on; see
    /// [`probe_failed`](Self::probe_failed).
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

    /// Builds the role's "missing doer" [`HostError`] for `release`.
    ///
    /// The per-role `Missing*Error` for an unknown `(release, transactional)`
    /// key.
    #[must_use]
    fn missing_error(self, release: &str) -> HostError {
        let release = release.to_owned();
        match self {
            Role::Install => HostError::MissingInstaller { release },
            Role::Uninstall => HostError::MissingUninstaller { release },
            Role::Update => HostError::MissingUpdater { release },
            Role::Prepare => HostError::MissingPreparer { release },
            Role::Downgrade => HostError::MissingDowngrader { release },
        }
    }
}

/// The injectable seam that resolves an action's command templates for a
/// `(release, transactional)` key.
///
/// Implemented by [`WorkflowRegistry`] over the [`actions`] tables. The
/// install/uninstall path reaches it through that type's
/// `mtui_hosts::PlanProvider` impl, so `OperationGroup::plans` can build a
/// `mtui_hosts::Doer` without `mtui-hosts` depending on this crate.
pub trait DoerProvider: Send + Sync {
    /// Resolves the action command set for `role` at `(release, transactional)`.
    ///
    /// # Errors
    ///
    /// Returns the role's [`HostError::MissingInstaller`] / etc. when no entry
    /// exists for the key.
    fn doer(
        &self,
        role: Role,
        release: &str,
        transactional: bool,
    ) -> Result<actions::ActionCommands, HostError>;
}

/// The injectable seam that resolves a post-run check for a
/// `(release, transactional)` key.
///
/// Implemented by [`WorkflowRegistry`] over the [`checks`] tables. A check is
/// returned as a boxed function; an unknown key yields `None`, which the caller
/// treats as "no check to run".
pub trait CheckProvider: Send + Sync {
    /// Resolves the post-run check for `role` at `(release, transactional)`, or
    /// `None` when no check is registered for the key.
    fn check(&self, role: Role, release: &str, transactional: bool) -> Option<checks::CheckFn>;
}

/// The default [`DoerProvider`] / [`CheckProvider`], backed by the
/// [`actions`] and [`checks`] tables.
///
/// The concrete registry every flow builds and, for install/uninstall, injects
/// into the `HostsGroup` as a `mtui_hosts::PlanProvider`. It carries the
/// prepare-only `force` / `testing` flags (the other actions ignore them),
/// threading them into the `prepare` doers.
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
}

impl DoerProvider for WorkflowRegistry {
    fn doer(
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
}

impl CheckProvider for WorkflowRegistry {
    fn check(&self, role: Role, release: &str, transactional: bool) -> Option<checks::CheckFn> {
        match role {
            // Upstream's uninstaller consults the *install* checks; install and
            // uninstall share the same check table.
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
/// [`mtui_hosts::PlanProvider`], a trait declared in `mtui-hosts` purely in
/// terms of `mtui-hosts` types so that crate never has to depend on this one.
/// This impl is the other half: it is legal here (foreign trait, local type)
/// and nowhere else, because the check tables' `CheckArgs` fields are
/// `pub(crate)`.
///
/// Injected by `update_flow::perform_install` / `perform_uninstall` immediately
/// before the template runs.
impl mtui_hosts::PlanProvider for WorkflowRegistry {
    fn doer(
        &self,
        role: &str,
        release: &str,
        transactional: bool,
    ) -> Result<mtui_hosts::Doer, HostError> {
        // An unrecognised role string cannot resolve to a table; report it as a
        // missing installer rather than panicking. Only "installer" and
        // "uninstaller" ever reach here (`Operation::role`).
        let Some(role) = Role::from_operation_role(role) else {
            return Err(HostError::MissingInstaller {
                release: release.to_owned(),
            });
        };
        let commands = DoerProvider::doer(self, role, release, transactional)?;
        // The raw templates go across the seam, not rendered strings: the
        // package list is only known inside `mtui-hosts`, at
        // `Operation::collect` time. `Doer` substitutes `$packages` with a plain
        // `replace`, which agrees with this crate's [`substitute`]
        // semantics for every install/uninstall template (each holds exactly one
        // `$packages` and no `$$` or `${}`); `doer_templates_render_like_the_action_tables`
        // pins that agreement.
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
            .and_then(|role| CheckProvider::check(self, role, release, transactional));

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
            // Defensive only: every key with an installer or uninstaller now
            // has an install-table check (#406 registered the last two,
            // `slmicro` and `YUM`), so no real key reaches here. It stays for
            // the window in which a doer is added before its check — falling
            // back to the exit code alone, which is what this arm has always
            // done. Never "any stderr is a failure": `transactional-update`
            // and `yum` both write progress and warnings to stderr on a
            // successful run.
            //
            // Unreachable in production means untested by the flows, so
            // `plan_provider_check_falls_back_to_the_exit_code_for_an_unknown_key`
            // drives it directly.
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
        let doer = reg.doer(Role::Install, "15", false).expect("installer");
        let vars: HashMap<&str, &str> = [("packages", "pkg")].into_iter().collect();
        assert_eq!(
            doer.render_command(&vars).unwrap(),
            "zypper -n in -y -l pkg"
        );
    }

    #[test]
    fn registry_missing_doer_maps_to_role_error() {
        let reg = WorkflowRegistry::default();
        let err = reg.doer(Role::Install, "99", false).unwrap_err();
        assert!(matches!(err, HostError::MissingInstaller { .. }));
        let err = reg.doer(Role::Downgrade, "99", false).unwrap_err();
        assert!(matches!(err, HostError::MissingDowngrader { .. }));
    }

    #[test]
    fn registry_prepare_honours_force_flag() {
        use std::collections::HashMap;
        let vars: HashMap<&str, &str> = [("package", "p")].into_iter().collect();
        let forced = WorkflowRegistry::new(true, false)
            .doer(Role::Prepare, "15", false)
            .unwrap();
        assert!(
            forced
                .render_command(&vars)
                .unwrap()
                .contains("--force-resolution")
        );
        let unforced = WorkflowRegistry::new(false, false)
            .doer(Role::Prepare, "15", false)
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
        assert!(reg.check(Role::Uninstall, "15", false).is_some());
    }

    #[test]
    fn registry_resolves_and_runs_a_check() {
        let reg = WorkflowRegistry::default();
        let check = reg
            .check(Role::Install, "15", false)
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
        // Was `("slmicro", true)` — which is now a *registered* update key, so
        // it no longer demonstrates anything. Use keys with no doer either.
        assert!(reg.check(Role::Update, "nonesuch", false).is_none());
        assert!(reg.check(Role::Update, "15", true).is_none());
    }

    #[test]
    fn registry_resolves_the_update_checks_that_used_to_be_missing() {
        // `("slmicro", true)` and `("YUM", false)` have had an *updater* since
        // the port but no check, so `run_checks` skipped them via its
        // `else { continue }` and `update` reported success on those hosts
        // whatever the command did. Pins the table wiring, not just the
        // function: resolving through the registry is the path `run_checks`
        // actually takes.
        let reg = WorkflowRegistry::default();
        assert!(reg.check(Role::Update, "slmicro", true).is_some());
        assert!(reg.check(Role::Update, "YUM", false).is_some());
    }

    #[test]
    fn registry_resolves_the_prepare_and_install_checks_that_used_to_be_missing() {
        // The sibling holes to the `update` ones above (#406). Both keys have
        // had a preparer, an installer and an uninstaller since the port but no
        // check for any of the three: `prepare` skipped the host in
        // `run_checks`, while `install`/`uninstall` fell through to the
        // `PlanProvider` adapter's exit-code-only fallback.
        //
        // Resolved through the registry, not the tables, because that is the
        // path `run_checks` and the adapter actually take — and `Uninstall`
        // is listed explicitly because it reaches the *install* table through
        // this match, which a role-blind rewrite would silently break.
        let reg = WorkflowRegistry::default();
        for (release, transactional) in [("slmicro", true), ("YUM", false)] {
            for role in [Role::Prepare, Role::Install, Role::Uninstall] {
                assert!(
                    reg.check(role, release, transactional).is_some(),
                    "{role:?} @ ({release}, {transactional}) must resolve a check"
                );
            }
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
        // The role string must actually select the *matching* table: an
        // adapter comparing the two doers for mere inequality would also pass
        // on a role swap (installer <-> uninstaller). Compare each resolved
        // doer against one built straight from that role's own table entry, so
        // only the correct mapping passes. `Doer::command` / `Doer::reboot`
        // are crate-private, so `Debug` is the only way to compare from here.
        let install = PlanProvider::doer(&reg, "installer", "15", false).expect("installer");
        let uninstall = PlanProvider::doer(&reg, "uninstaller", "15", false).expect("uninstaller");

        let install_commands =
            DoerProvider::doer(&reg, Role::Install, "15", false).expect("install table entry");
        let expected_install = Doer::new(
            install_commands.command_template(),
            install_commands.reboot_template().unwrap_or_default(),
        );
        let uninstall_commands =
            DoerProvider::doer(&reg, Role::Uninstall, "15", false).expect("uninstall table entry");
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

        // Since #406 every key with an install/uninstall doer also has a
        // check, so no production key reaches the adapter's `None` arms. They
        // stay as the safety net for a doer added before its check — and a
        // safety net nothing exercises is one nobody notices breaking, hence
        // this direct drive.
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
        // install, which is why it is spelled out here as well as in the flow.
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
    /// while this crate renders the same templates through [`substitute`]
    /// semantics. They agree for every install/uninstall entry today; this
    /// fails if a future template gains `$$` or `${}`, which `replace` would
    /// mangle.
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
        ] {
            for role in [Role::Install, Role::Uninstall] {
                let commands = WorkflowRegistry::default()
                    .doer(role, release, transactional)
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
