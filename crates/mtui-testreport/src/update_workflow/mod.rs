//! The update-workflow engine: action command tables, post-run check tables,
//! and the `${}` command-template helper.
//!
//! ## Reference
//!
//! Ports upstream `mtui/update_workflow/{actions,checks}/` plus the
//! `string.Template` usage those tables depend on. Upstream models the workflow
//! as two families of tables keyed by a `(release, transactional)` tuple:
//!
//! * **actions** ([`actions`]) map a key to a set of `string.Template` command
//!   strings (`command`, and — depending on the action — `reboot`,
//!   `installed_only`, `list_command`). They are stored in a `DictWithInjections`
//!   that raises a role-specific `MissingDoerError` on an unknown key.
//! * **checks** ([`checks`]) map the same key to a function
//!   `(hostname, stdout, stdin, stderr, exitcode) -> None` that raises
//!   [`UpdateError`] when it recognises a failure.
//!
//! ## Scope (P4.6): tables + traits, no wiring
//!
//! This lands the tables, the [`template`] helper, and the injectable
//! [`DoerProvider`] / [`CheckProvider`] seams that `mtui-core::wiring` will later
//! bind to `mtui_hosts::OperationGroup`. It deliberately does **not** implement
//! `impl OperationGroup for HostsGroup` — that binding (and the
//! `(release, transactional)` key derivation from live `Target` state) is the
//! composition-root task, kept out of here so `mtui-hosts` never depends on
//! `mtui-testreport`.

pub mod actions;
pub mod checks;
pub mod template;

use mtui_hosts::HostError;
use thiserror::Error;

pub use checks::Diagnostic;
pub use template::{TemplateError, safe_substitute, substitute};

/// The lookup key shared by every action and check table.
///
/// Mirrors upstream's `(str, bool)` dict key: a *release* token — a product
/// major version (`"11"`, `"12"`, `"15"`, `"16"`), the package-manager family
/// `"YUM"`, or `"slmicro"` — paired with a *transactional* flag (read-only-root
/// hosts, e.g. SL Micro). The `Vec`-free `(String, bool)` here keeps ownership
/// simple; lookups accept `(&str, bool)`.
pub type WorkflowKey = (String, bool);

/// A failure recognised by a post-run [`checks`] function.
///
/// Ports upstream `mtui.support.exceptions.UpdateError(reason, host)`. Its
/// `Display` matches upstream `__str__`: `"{host}: {reason}"` when a host is
/// present, otherwise just `"{reason}"`. The `reason` strings are stable
/// contract values consumed by callers ("package not found", "update stack
/// locked", "RPM Error", "Dependency Error", "Unknown Error", "Unspecified
/// Error").
#[derive(Debug, Error, PartialEq, Eq, Clone)]
pub struct UpdateError {
    /// The failure reason (a stable, upstream-matching short string).
    pub reason: String,
    /// The host the command ran on, if known.
    pub host: Option<String>,
}

impl UpdateError {
    /// Builds an [`UpdateError`] with a `reason` and the `host` it occurred on.
    #[must_use]
    pub fn new(reason: impl Into<String>, host: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
            host: Some(host.into()),
        }
    }

    /// Builds a host-less [`UpdateError`] (upstream `UpdateError(reason)`).
    #[must_use]
    pub fn reason_only(reason: impl Into<String>) -> Self {
        Self {
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
/// Each maps to one upstream action module and its `MissingDoerError` subclass.
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
    /// Builds the role's "missing doer" [`HostError`] for `release`.
    ///
    /// Mirrors the per-module `key_error=Missing*Error` on upstream's
    /// `DictWithInjections`.
    #[must_use]
    pub fn missing_error(self, release: &str) -> HostError {
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
/// The composition root (`mtui-core::wiring`) implements this over the
/// [`actions`] tables and hands it to `mtui-hosts` so `OperationGroup::plans`
/// can build a `mtui_hosts::Doer` without `mtui-hosts` depending on this crate.
pub trait DoerProvider: Send + Sync {
    /// Resolves the action command set for `role` at `(release, transactional)`.
    ///
    /// # Errors
    ///
    /// Returns the role's [`HostError::MissingInstaller`] / etc. when no entry
    /// exists for the key, matching upstream's `Missing*Error`.
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
/// The composition root implements this over the [`checks`] tables. A check is
/// returned as a boxed function with the upstream signature; an unknown key
/// yields `None` (upstream's plain `dict.get`, which the caller treats as
/// "no check to run").
pub trait CheckProvider: Send + Sync {
    /// Resolves the post-run check for `role` at `(release, transactional)`, or
    /// `None` when no check is registered for the key.
    fn check(&self, role: Role, release: &str, transactional: bool) -> Option<checks::CheckFn>;
}

/// The default [`DoerProvider`] / [`CheckProvider`], backed by the ported
/// [`actions`] and [`checks`] tables.
///
/// This is the concrete registry `mtui-core::wiring` injects. It carries the
/// prepare-only `force` / `testing` flags (the other actions ignore them),
/// mirroring how upstream threads them into `t.doer("preparer", force, testing)`.
#[derive(Debug, Clone, Copy, Default)]
pub struct WorkflowRegistry {
    /// The `--force-resolution` flag threaded into `prepare` doers.
    pub force: bool,
    /// The `testing`-repos flag threaded into `prepare` doers.
    pub testing: bool,
}

impl WorkflowRegistry {
    /// Builds a registry with the given prepare flags.
    #[must_use]
    pub fn new(force: bool, testing: bool) -> Self {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn update_error_display_with_host_matches_upstream() {
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
        // uninstall shares the install check table (upstream behaviour).
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
        assert!(reg.check(Role::Update, "slmicro", true).is_none());
    }
}
