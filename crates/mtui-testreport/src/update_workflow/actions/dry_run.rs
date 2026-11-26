//! Dry-run templates for the removal guard (`mtui_hosts::target::removal`).
//!
//! Each resolves the same transaction as its action's command with
//! `zypper -x … --dry-run`, whose `<to-remove>` list the guard reads, and
//! interpolates `$packages` (for `downgrade`, the `name=version` specs).
//!
//! The transactional keys probe with plain `zypper` against the booted root:
//! zypper skips its read-only-root refusal for a dry run, and mtui reboots into
//! each transactional operation's snapshot, so the booted root is what the next
//! `transactional-update` starts from. A snapshot staged by something else and
//! not yet booted is not what the dry run resolves against.
//!
//! `YUM` has none: `yum`/`dnf` report a transaction only as a human-readable
//! table whose layout differs between yum 3 and dnf 4/5, so those hosts are not
//! checked.

use crate::update_workflow::{Family, Role, family};

/// The dry run of `role`'s command for `(release, transactional)`, or `None`
/// where the guard does not apply (`update`, `YUM`, an unknown key).
///
/// `force` mirrors `prepare --force`'s `--force-resolution`.
#[must_use]
pub(crate) fn dry_run(
    role: Role,
    release: &str,
    transactional: bool,
    force: bool,
) -> Option<String> {
    let transactional = match family(release, transactional)? {
        Family::Zypper => false,
        Family::Transactional => true,
        Family::Yum => return None,
    };
    match role {
        Role::Install => Some("zypper -x -n in --dry-run -y -l $packages".to_owned()),
        Role::Uninstall => Some("zypper -x -n rm --dry-run $packages".to_owned()),
        Role::Prepare => {
            let parameter = if force { " --force-resolution" } else { "" };
            Some(format!(
                "zypper -x -n in --dry-run -y -l{parameter} $packages"
            ))
        }
        Role::Downgrade if transactional => Some(
            "zypper -x -n in --dry-run --force-resolution --oldpackage -y $packages".to_owned(),
        ),
        Role::Downgrade => Some(
            "zypper -x -n in --dry-run -C --force-resolution --oldpackage -y $packages".to_owned(),
        ),
        Role::Update => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_zypper_key_resolves_its_commands_transaction() {
        for (release, transactional) in [
            ("15", false),
            ("16", false),
            ("slmicro", true),
            ("16", true),
        ] {
            assert_eq!(
                dry_run(Role::Uninstall, release, transactional, false).as_deref(),
                Some("zypper -x -n rm --dry-run $packages")
            );
            assert_eq!(
                dry_run(Role::Install, release, transactional, false).as_deref(),
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
        assert!(
            dry_run(Role::Downgrade, "slmicro", true, false)
                .unwrap()
                .contains("--force-resolution --oldpackage")
        );
    }

    #[test]
    fn yum_update_and_unknown_keys_have_none() {
        assert_eq!(dry_run(Role::Uninstall, "YUM", false, false), None);
        assert_eq!(dry_run(Role::Update, "15", false, false), None);
        assert_eq!(dry_run(Role::Install, "15", true, false), None);
        assert_eq!(dry_run(Role::Install, "99", false, false), None);
    }
}
