//! Prepare command templates (role `preparer`).
//!
//! Unlike the other actions, prepare entries are **parameterized** by `force`
//! and `testing`: the flag is baked into the command string at construction.
//! Prepare interpolates `$package` and carries a `list_command`, the probe
//! `--installed` narrows each host's list with. The `slmicro` entry is
//! transactional with a reboot.
//!
//! ## The installed-package probe
//!
//! `rpm -q <names>` exits `1` for a merely-absent name, so a broken rpmdb is
//! indistinguishable from a negative answer (#451). `rpm -qa` exits `0` on
//! success, so a non-zero status means the probe itself failed.

use crate::update_workflow::actions::ActionCommands;

/// Enumerates the installed package names, one per line — the `--installed`
/// probe, shared by all three families. It carries no `$`, so it renders
/// identically under [`Strict`](crate::update_workflow::actions::SubstMode).
const INSTALLED_PROBE: &str = "rpm -qa --qf '%{NAME}\\n'";

/// zypper prepare.
///
/// `force` toggles `--force-resolution`; `testing` is accepted for signature
/// parity but unused by zypper.
fn zypper(force: bool, _testing: bool) -> ActionCommands {
    let parameter = if force { "--force-resolution" } else { "" };
    ActionCommands {
        command: format!("zypper -n in -y -l {parameter} $package"),
        reboot: None,
        list_command: Some(INSTALLED_PROBE.to_owned()),
        mode: crate::update_workflow::actions::SubstMode::Strict,
    }
}

/// yum prepare.
///
/// `testing` toggles `--disablerepo=*testing*` (present when **not** testing);
/// `force` is accepted for parity but unused by yum.
fn yum(_force: bool, testing: bool) -> ActionCommands {
    let parameter = if testing {
        ""
    } else {
        "--disablerepo=*testing*"
    };
    ActionCommands {
        command: format!("yum -y {parameter} install $package"),
        reboot: None,
        list_command: Some(INSTALLED_PROBE.to_owned()),
        mode: crate::update_workflow::actions::SubstMode::Strict,
    }
}

/// slmicro (transactional) prepare.
fn slmicro(force: bool, _testing: bool) -> ActionCommands {
    let parameter = if force { "--force-resolution" } else { "" };
    ActionCommands {
        command: format!("transactional-update -n pkg in -l {parameter} $package"),
        reboot: Some("systemctl reboot".to_owned()),
        list_command: Some(INSTALLED_PROBE.to_owned()),
        mode: crate::update_workflow::actions::SubstMode::Strict,
    }
}

/// The prepare command for `(release, transactional)` with the given `force` /
/// `testing` flags, or `None` for an unknown key (provider maps `None` to
/// `MissingPreparerError`).
#[must_use]
pub(crate) fn preparer(
    release: &str,
    transactional: bool,
    force: bool,
    testing: bool,
) -> Option<ActionCommands> {
    match (release, transactional) {
        ("11", false) | ("12", false) | ("15", false) | ("16", false) => {
            Some(zypper(force, testing))
        }
        ("YUM", false) => Some(yum(force, testing)),
        ("slmicro", true) => Some(slmicro(force, testing)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    fn pkg(v: &str) -> HashMap<&str, &str> {
        [("package", v)].into_iter().collect()
    }

    #[test]
    fn zypper_without_force_omits_flag() {
        let cmds = preparer("15", false, false, false).unwrap();
        // No force flag -> empty parameter leaves a double space
        // (parameter == "").
        assert_eq!(
            cmds.render_command(&pkg("kernel")).unwrap(),
            "zypper -n in -y -l  kernel"
        );
    }

    #[test]
    fn zypper_with_force_adds_force_resolution() {
        let cmds = preparer("15", false, true, false).unwrap();
        assert_eq!(
            cmds.render_command(&pkg("kernel")).unwrap(),
            "zypper -n in -y -l --force-resolution kernel"
        );
    }

    #[test]
    fn prepare_probe_enumerates_installed_names() {
        // #501. The probe is the same string for every family, and the
        // conditional `$(rpm -q ...)` wrapper is gone from all of them.
        for (release, transactional) in [("15", false), ("YUM", false), ("slmicro", true)] {
            let cmds = preparer(release, transactional, false, false).unwrap();
            assert_eq!(
                cmds.render_list_command(&HashMap::new()).unwrap(),
                Some("rpm -qa --qf '%{NAME}\\n'".to_owned()),
                "{release}"
            );
            let command = cmds.render_command(&pkg("pkg-a")).unwrap();
            assert!(!command.contains("$(rpm -q"), "{release}: {command}");
            assert!(!command.contains("rpm -q pkg-a &>"), "{release}: {command}");
        }
    }

    #[test]
    fn yum_testing_toggles_disablerepo() {
        let not_testing = preparer("YUM", false, false, false).unwrap();
        assert!(
            not_testing
                .render_command(&pkg("p"))
                .unwrap()
                .contains("--disablerepo=*testing*")
        );
        let testing = preparer("YUM", false, false, true).unwrap();
        assert!(
            !testing
                .render_command(&pkg("p"))
                .unwrap()
                .contains("--disablerepo")
        );
    }

    #[test]
    fn slmicro_is_transactional_with_reboot() {
        let cmds = preparer("slmicro", true, false, false).unwrap();
        assert_eq!(
            cmds.render_reboot().unwrap(),
            Some("systemctl reboot".into())
        );
        assert!(
            cmds.render_command(&pkg("p"))
                .unwrap()
                .starts_with("transactional-update -n pkg in -l")
        );
    }

    #[test]
    fn unknown_key_is_none() {
        assert!(preparer("99", false, false, false).is_none());
    }
}
