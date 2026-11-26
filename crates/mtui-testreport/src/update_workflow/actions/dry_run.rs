//! Dry-run templates for the removal guard (`mtui_hosts::target::removal`).
//!
//! Each resolves the same transaction as its action's command with
//! `zypper -x … --dry-run`, whose `<to-remove>` list the guard reads, and
//! interpolates `$packages` (for `downgrade`, the `name=version` specs).
//!
//! Transactional hosts (SL Micro, transactional SLES 16) have none: their
//! transaction runs in a new snapshot whose repositories and trusted keys can
//! differ from the booted root's (`prepare` refreshes inside the snapshot it
//! stages), and no dry run on a real transactional host has been captured to
//! show a booted-root probe resolves the same transaction.
//!
//! `YUM` has none: `yum`/`dnf` report a transaction only as a human-readable
//! table whose layout differs between yum 3 and dnf 4/5, so those hosts are not
//! checked. Neither is SLE 11: its zypper 1.6 predates the releases whose XML
//! summary the guard's parser is pinned against.

use crate::update_workflow::{Family, Role, family};

/// The two commands every template starts with; see [`is_dry_run`].
const DRY_RUN_IN: &str = "zypper -x -n in --dry-run";
const DRY_RUN_RM: &str = "zypper -x -n rm --dry-run";

/// Whether `command` is one of the removal guard's dry runs, which commit
/// nothing and so stay out of the exported install logs.
#[must_use]
pub fn is_dry_run(command: &str) -> bool {
    [DRY_RUN_IN, DRY_RUN_RM].iter().any(|prefix| {
        command
            .strip_prefix(prefix)
            .is_some_and(|rest| rest.starts_with(' '))
    })
}

/// The dry run of `role`'s command for `(release, transactional)`, or `None`
/// where the guard does not apply (`update`, `YUM`, SLE 11, a transactional
/// host, an unknown key).
///
/// `force` mirrors `prepare --force`'s `--force-resolution`.
#[must_use]
pub(crate) fn dry_run(
    role: Role,
    release: &str,
    transactional: bool,
    force: bool,
) -> Option<String> {
    if release == "11" || family(release, transactional) != Some(Family::Zypper) {
        return None;
    }
    match role {
        Role::Install => Some(format!("{DRY_RUN_IN} -y -l $packages")),
        Role::Uninstall => Some(format!("{DRY_RUN_RM} $packages")),
        Role::Prepare => {
            let parameter = if force { " --force-resolution" } else { "" };
            Some(format!("{DRY_RUN_IN} -y -l{parameter} $packages"))
        }
        Role::Downgrade => Some(format!(
            "{DRY_RUN_IN} -C --force-resolution --oldpackage -y $packages"
        )),
        Role::Update => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_zypper_key_resolves_its_commands_transaction() {
        for release in ["12", "15", "16"] {
            assert_eq!(
                dry_run(Role::Uninstall, release, false, false).as_deref(),
                Some("zypper -x -n rm --dry-run $packages")
            );
            assert_eq!(
                dry_run(Role::Install, release, false, false).as_deref(),
                Some("zypper -x -n in --dry-run -y -l $packages")
            );
        }
        assert_eq!(
            dry_run(Role::Prepare, "15", false, true).as_deref(),
            Some("zypper -x -n in --dry-run -y -l --force-resolution $packages")
        );
        assert_eq!(
            dry_run(Role::Prepare, "15", false, false).as_deref(),
            Some("zypper -x -n in --dry-run -y -l $packages")
        );
        assert!(
            dry_run(Role::Downgrade, "15", false, false)
                .unwrap()
                .contains("-C --force-resolution --oldpackage")
        );
    }

    #[test]
    fn every_template_is_recognised_as_a_dry_run() {
        for role in [
            Role::Install,
            Role::Uninstall,
            Role::Prepare,
            Role::Downgrade,
        ] {
            for force in [false, true] {
                let template = dry_run(role, "15", false, force).unwrap();
                assert!(
                    is_dry_run(&template.replace("$packages", "libfoo")),
                    "{template}"
                );
            }
        }
        for command in [
            "zypper -n in -y -l libfoo",
            "zypper -n rm libfoo",
            "zypper -x -n in --dry-runs libfoo",
            "transactional-update -n pkg install libfoo",
        ] {
            assert!(!is_dry_run(command), "{command}");
        }
    }

    #[test]
    fn yum_sle11_transactional_update_and_unknown_keys_have_none() {
        assert_eq!(dry_run(Role::Uninstall, "YUM", false, false), None);
        assert_eq!(dry_run(Role::Uninstall, "11", false, false), None);
        assert_eq!(dry_run(Role::Update, "15", false, false), None);
        assert_eq!(dry_run(Role::Install, "15", true, false), None);
        for role in [
            Role::Install,
            Role::Uninstall,
            Role::Prepare,
            Role::Downgrade,
        ] {
            assert_eq!(dry_run(role, "slmicro", true, false), None);
            assert_eq!(dry_run(role, "16", true, false), None);
        }
        assert_eq!(dry_run(Role::Install, "99", false, false), None);
    }
}
