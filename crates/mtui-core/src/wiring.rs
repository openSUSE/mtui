//! The composition root: binds the lower crates' injectable seams together.
//!
//! `mtui-hosts` and `mtui-testreport` are deliberately decoupled — the update
//! workflow's doer/check tables live in `mtui-testreport`, but the host layer
//! that drives them (`mtui_hosts::OperationGroup`) must not depend on that crate
//! (it would create a cycle). Each side therefore exposes a trait seam:
//!
//! * `mtui-testreport` provides [`WorkflowRegistry`] implementing its own
//!   [`DoerProvider`] / [`CheckProvider`] (keyed by a [`Role`] enum), yielding
//!   `ActionCommands` and `CheckFn`.
//! * `mtui-hosts` expects an [`mtui_hosts::PlanProvider`] yielding its own
//!   [`Doer`](mtui_hosts::Doer) / [`Check`](mtui_hosts::Check) for a role
//!   *string*.
//!
//! This module owns the adapter — [`WorkflowPlanProvider`] — that translates
//! between the two, and the [`build_plan_provider`] / [`inject_plan_provider`]
//! helpers the session builder uses to wire it into a [`HostsGroup`].
//!
//! Scope (P5.5): this adapter wires the install/uninstall path (the
//! [`Operation`](mtui_hosts::Operation) template resolving `installer` /
//! `uninstaller` doers through the injected [`PlanProvider`]). The bespoke
//! `update` / `prepare` / `downgrade` flows (`mtui-rs-9lf`) resolve their doers
//! directly from the [`WorkflowRegistry`] on the report side
//! (`mtui-testreport::reports::update_flow`) rather than through this adapter,
//! because they need `ActionCommands` fields (`installed_only` / `list_command`
//! / `$repa`) the lossy [`Doer`](mtui_hosts::Doer) does not carry; the group
//! reboot/lock lifecycle they use landed in `mtui-rs-owd` / `mtui-rs-fly`.

use std::sync::Arc;

use mtui_hosts::{Check, CheckArgs, Doer, HostError, HostsGroup, PlanProvider};
use mtui_testreport::update_workflow::checks::CheckArgs as WfCheckArgs;
use mtui_testreport::{CheckProvider, DoerProvider, Role, WorkflowRegistry};

/// Maps an upstream role string (`Target.doer(role)` vocabulary) to the
/// [`Role`] enum the workflow registry is keyed on.
///
/// Mirrors upstream `Target.doer`'s role names: `"installer"`, `"uninstaller"`,
/// `"updater"`, `"preparer"`, `"downgrader"`. An unknown role has no registry
/// entry, which the caller surfaces as a missing-doer error.
fn role_from_str(role: &str) -> Option<Role> {
    match role {
        "installer" => Some(Role::Install),
        "uninstaller" => Some(Role::Uninstall),
        "updater" => Some(Role::Update),
        "preparer" => Some(Role::Prepare),
        "downgrader" => Some(Role::Downgrade),
        _ => None,
    }
}

/// Adapts a [`WorkflowRegistry`] to the [`PlanProvider`] the host layer expects.
///
/// Resolves a role string to a [`Role`], looks the action/check tables up by
/// `(release, transactional)`, and converts the results to `mtui-hosts` types: an
/// [`ActionCommands`](mtui_testreport::update_workflow::actions::ActionCommands)
/// becomes a [`Doer`] (its `command` template carries `$packages`, which
/// `Doer::command` substitutes; its `reboot` template, if any, becomes the
/// reboot command), and a [`CheckFn`](mtui_testreport::update_workflow::checks::CheckFn)
/// becomes a [`Check`] that logs the recognised [`UpdateError`] on failure.
pub struct WorkflowPlanProvider {
    registry: WorkflowRegistry,
}

impl WorkflowPlanProvider {
    /// Builds an adapter over `registry`.
    #[must_use]
    pub fn new(registry: WorkflowRegistry) -> Self {
        Self { registry }
    }
}

impl PlanProvider for WorkflowPlanProvider {
    fn doer(&self, role: &str, release: &str, transactional: bool) -> Result<Doer, HostError> {
        // An unknown role string has no table; treat it as a missing installer
        // for the release (the safe default sentinel).
        let Some(wf_role) = role_from_str(role) else {
            return Err(HostError::MissingInstaller {
                release: release.to_owned(),
            });
        };
        let commands = self.registry.doer(wf_role, release, transactional)?;
        // `Doer::command` substitutes `$packages` in the command template, so
        // pass the template through verbatim. The reboot command is only present
        // for transactional entries; a missing one leaves an empty reboot.
        Ok(Doer::new(
            commands.command,
            commands.reboot.unwrap_or_default(),
        ))
    }

    fn check(&self, role: &str, release: &str, transactional: bool) -> Check {
        let check_fn = role_from_str(role)
            .and_then(|wf_role| self.registry.check(wf_role, release, transactional));

        match check_fn {
            Some(f) => Box::new(move |a: CheckArgs<'_>| {
                // Bridge the arg shapes: hosts' `lastexit: Option<i16>` maps to
                // the check table's `exitcode: i32` (a missing exit code — the
                // never-ran / timed-out sentinel — becomes -1, matching the
                // `Target::run` `-1` convention).
                let args = WfCheckArgs {
                    hostname: a.hostname,
                    stdout: a.lastout,
                    stdin: a.lastin,
                    stderr: a.lasterr,
                    exitcode: a.lastexit.map_or(-1, i32::from),
                };
                if let Err(e) = f(args) {
                    // Upstream raises `UpdateError`; the fan-out driver logs and
                    // continues per host. Surface it as an error breadcrumb.
                    tracing::error!(host = a.hostname, error = %e, "update check failed");
                }
            }),
            // No registered check → a no-op, mirroring upstream's `_no_checks`.
            None => Box::new(|_a: CheckArgs<'_>| {}),
        }
    }
}

/// Builds the default update-workflow [`PlanProvider`] from `force` / `testing`
/// (the prepare-only flags upstream threads into `t.doer("preparer", …)`).
///
/// The other roles ignore these flags; they are carried on the registry so the
/// install/uninstall path resolves the right preparer variant. The bespoke
/// `prepare`/`update`/`downgrade` flows build their own registry per call (they
/// need the `force`/`testing`/`installed_only` combination at invocation time).
#[must_use]
pub fn build_plan_provider(force: bool, testing: bool) -> Arc<dyn PlanProvider> {
    Arc::new(WorkflowPlanProvider::new(WorkflowRegistry::new(
        force, testing,
    )))
}

/// Injects the default update-workflow provider into `group`, returning the
/// wired group.
///
/// This is the one call the session/host builder makes so an operation
/// (`install` / `uninstall`) can resolve doers; without it a group's
/// [`OperationGroup::plans`](mtui_hosts::OperationGroup::plans) returns
/// [`HostError::NoPlanProvider`].
#[must_use]
pub fn inject_plan_provider(group: HostsGroup, force: bool, testing: bool) -> HostsGroup {
    group.with_plan_provider(build_plan_provider(force, testing))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;
    use mtui_hosts::{InstallOperation, MockConnection, Operation, Target, UninstallOperation};
    use mtui_types::enums::{ExecutionMode, TargetState};
    use mtui_types::hostlog::CommandLog;
    use mtui_types::system::{System, SystemProduct};

    fn sles_target(hostname: &str) -> (Target, MockConnection) {
        let conn =
            MockConnection::new(hostname).with_default(CommandLog::new("", "done", "", 0, 1));
        let handle = conn.clone();
        let mut t = Target::with_connection(
            hostname,
            TargetState::Enabled,
            ExecutionMode::Parallel,
            Box::new(conn),
        );
        t.set_system(
            System::new(
                SystemProduct::new("SLES", "15.5", "x86_64"),
                BTreeSet::new(),
                false,
            ),
            false,
        );
        (t, handle)
    }

    #[test]
    fn role_mapping_covers_all_upstream_roles() {
        assert_eq!(role_from_str("installer"), Some(Role::Install));
        assert_eq!(role_from_str("uninstaller"), Some(Role::Uninstall));
        assert_eq!(role_from_str("updater"), Some(Role::Update));
        assert_eq!(role_from_str("preparer"), Some(Role::Prepare));
        assert_eq!(role_from_str("downgrader"), Some(Role::Downgrade));
        assert_eq!(role_from_str("bogus"), None);
    }

    #[test]
    fn provider_resolves_installer_doer_for_known_release() {
        let provider = WorkflowPlanProvider::new(WorkflowRegistry::default());
        let doer = provider.doer("installer", "15", false).expect("installer");
        // The default installer table renders `zypper -n in -y -l $packages`.
        assert_eq!(doer.command("pkg"), "zypper -n in -y -l pkg");
    }

    #[test]
    fn provider_unknown_role_is_missing_installer() {
        let provider = WorkflowPlanProvider::new(WorkflowRegistry::default());
        let err = provider.doer("bogus", "15", false).unwrap_err();
        assert!(matches!(err, HostError::MissingInstaller { .. }));
    }

    #[test]
    fn provider_unknown_release_surfaces_missing_doer() {
        let provider = WorkflowPlanProvider::new(WorkflowRegistry::default());
        let err = provider.doer("installer", "99", false).unwrap_err();
        assert!(matches!(err, HostError::MissingInstaller { .. }));
    }

    #[tokio::test]
    async fn injected_group_drives_a_real_install() {
        let (t, handle) = sles_target("h1");
        let mut group = inject_plan_provider(HostsGroup::new(vec![t], false), false, false);

        InstallOperation::new(vec!["pkg-a".to_owned()])
            .run(&mut group)
            .await;

        assert_eq!(
            handle.commands(),
            vec!["zypper -n in -y -l pkg-a".to_owned()]
        );
    }

    #[tokio::test]
    async fn injected_group_drives_a_real_uninstall() {
        let (t, handle) = sles_target("h1");
        let mut group = inject_plan_provider(HostsGroup::new(vec![t], false), false, false);

        UninstallOperation::new(vec!["pkg".to_owned()])
            .run(&mut group)
            .await;

        // The default uninstaller renders `zypper -n rm $packages`.
        assert_eq!(handle.commands().len(), 1);
        assert!(handle.commands()[0].contains("pkg"));
    }
}
