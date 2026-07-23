//! Uninstall command templates (upstream `actions/uninstall.py`, role
//! `uninstaller`).
//!
//! Interpolates `$packages`. The `slmicro` entry is transactional and carries a
//! `reboot`.

use crate::update_workflow::actions::ActionCommands;

/// zypper uninstall (upstream `zypper_uninstall`).
fn zypper() -> ActionCommands {
    ActionCommands::command_only("zypper -n rm $packages")
}

/// yum uninstall (upstream `yum_uninstall`).
fn yum() -> ActionCommands {
    ActionCommands::command_only("yum -y remove $packages")
}

/// slmicro (transactional) uninstall (upstream `slmicro_uninstall`).
fn slmicro() -> ActionCommands {
    ActionCommands::with_reboot(
        "transactional-update -n pkg remove $packages",
        "systemctl reboot",
    )
}

/// The uninstall command for `(release, transactional)`, or `None` for an
/// unknown key (provider maps `None` to `MissingUninstallerError`).
#[must_use]
pub(crate) fn uninstaller(release: &str, transactional: bool) -> Option<ActionCommands> {
    match (release, transactional) {
        ("11", false) | ("12", false) | ("15", false) | ("16", false) => Some(zypper()),
        ("YUM", false) => Some(yum()),
        ("slmicro", true) => Some(slmicro()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    fn pkgs(v: &str) -> HashMap<&str, &str> {
        [("packages", v)].into_iter().collect()
    }

    #[test]
    fn zypper_keys_resolve_and_render() {
        for rel in ["11", "12", "15", "16"] {
            let cmds = uninstaller(rel, false).expect("zypper uninstaller");
            assert_eq!(
                cmds.render_command(&pkgs("p1 p2")).unwrap(),
                "zypper -n rm p1 p2"
            );
        }
    }

    #[test]
    fn yum_key_resolves() {
        let cmds = uninstaller("YUM", false).expect("yum uninstaller");
        assert_eq!(cmds.render_command(&pkgs("p")).unwrap(), "yum -y remove p");
    }

    #[test]
    fn slmicro_is_transactional_with_reboot() {
        let cmds = uninstaller("slmicro", true).expect("slmicro uninstaller");
        assert_eq!(
            cmds.render_command(&pkgs("p")).unwrap(),
            "transactional-update -n pkg remove p"
        );
        assert_eq!(
            cmds.render_reboot().unwrap(),
            Some("systemctl reboot".into())
        );
    }

    #[test]
    fn unknown_key_is_none() {
        assert!(uninstaller("99", false).is_none());
    }
}
