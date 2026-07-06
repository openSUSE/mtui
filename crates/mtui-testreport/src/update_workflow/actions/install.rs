//! Install command templates (upstream `actions/install.py`, role `installer`).
//!
//! Interpolates `$packages` (the space-joined package list). The `slmicro`
//! entry is transactional and carries a `reboot`.

use crate::update_workflow::WorkflowKey;
use crate::update_workflow::actions::ActionCommands;

/// zypper install (upstream `zypper_install`).
fn zypper() -> ActionCommands {
    ActionCommands::command_only("zypper -n in -y -l $packages")
}

/// yum install (upstream `yum_install`).
fn yum() -> ActionCommands {
    ActionCommands::command_only("yum -y install $packages")
}

/// slmicro (transactional) install (upstream `slmicro_install`).
fn slmicro() -> ActionCommands {
    ActionCommands::with_reboot(
        "transactional-update -n pkg install $packages",
        "systemctl reboot",
    )
}

/// The install command for `(release, transactional)`, or `None` for an unknown
/// key (upstream's `DictWithInjections` raises `MissingInstallerError`; the
/// provider maps `None` to that error).
#[must_use]
pub fn installer(release: &str, transactional: bool) -> Option<ActionCommands> {
    let key: WorkflowKey = (release.to_owned(), transactional);
    match (key.0.as_str(), key.1) {
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
            let cmds = installer(rel, false).expect("zypper installer");
            assert_eq!(
                cmds.render_command(&pkgs("pkg-a pkg-b")).unwrap(),
                "zypper -n in -y -l pkg-a pkg-b"
            );
            assert!(cmds.reboot.is_none());
        }
    }

    #[test]
    fn yum_key_resolves() {
        let cmds = installer("YUM", false).expect("yum installer");
        assert_eq!(cmds.render_command(&pkgs("p")).unwrap(), "yum -y install p");
    }

    #[test]
    fn slmicro_is_transactional_with_reboot() {
        let cmds = installer("slmicro", true).expect("slmicro installer");
        assert_eq!(
            cmds.render_command(&pkgs("p")).unwrap(),
            "transactional-update -n pkg install p"
        );
        assert_eq!(
            cmds.render_reboot().unwrap(),
            Some("systemctl reboot".into())
        );
    }

    #[test]
    fn unknown_key_is_none() {
        assert!(installer("99", false).is_none());
        assert!(installer("15", true).is_none());
        assert!(installer("slmicro", false).is_none());
    }
}
