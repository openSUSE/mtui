//! Downgrade command templates (upstream `actions/downgrade.py`, role
//! `downgrader`).
//!
//! Downgrade carries a `list_command` (enumerate installed/available versions)
//! and a `command`. The zypper list loop embeds `$$p` (a shell loop variable)
//! and awk `$2`/`$4` field refs alongside the real `$packages`, and the command
//! interpolates `$package` / `$version`; the call sites use `.safe_substitute`
//! (see `hostgroup.py::perform_downgrade`) so the shell/awk `$`-tokens survive.
//! The `slmicro` entry is transactional with a reboot.
//!
//! Template strings are ported **verbatim**, including leading newlines.

use crate::update_workflow::actions::{ActionCommands, SubstMode};

/// zypper/slmicro downgrade list command (upstream `list_command_template`),
/// verbatim and shared by both.
const LIST_COMMAND: &str = r#"
for p in $packages; do \
zypper -n se -s --match-exact -t package $$p; \
done \
| grep -v "(System" \
| grep ^[iv] \
| sed "s, ,,g" \
| awk -F "|" '{{ print $2,"=",$4 }}'
"#;

/// zypper downgrade command (upstream `zypper()["command"]`), verbatim.
const ZYPPER_CMD: &str = "rpm -q $package &>/dev/null  && zypper -n in -C --force-resolution --oldpackage -y $package=$version";

/// slmicro downgrade command (upstream `slmicro()["command"]`), verbatim.
const SLM_CMD: &str = "transactional-update -n pkg in --force-resolution --oldpackage -y $package";

/// yum downgrade command (upstream `yum["command"]`), verbatim.
const YUM_CMD: &str = "yum -y downgrade $package";

/// zypper downgrade (upstream `zypper()`).
fn zypper() -> ActionCommands {
    ActionCommands {
        command: ZYPPER_CMD.to_owned(),
        list_command: Some(LIST_COMMAND.to_owned()),
        reboot: None,
        installed_only: None,
        mode: SubstMode::Safe,
    }
}

/// slmicro (transactional) downgrade (upstream `slmicro()`).
fn slmicro() -> ActionCommands {
    ActionCommands {
        command: SLM_CMD.to_owned(),
        list_command: Some(LIST_COMMAND.to_owned()),
        reboot: Some("systemctl reboot".to_owned()),
        installed_only: None,
        mode: SubstMode::Safe,
    }
}

/// yum downgrade (upstream `yum`).
fn yum() -> ActionCommands {
    ActionCommands {
        command: YUM_CMD.to_owned(),
        list_command: None,
        reboot: None,
        installed_only: None,
        mode: SubstMode::Safe,
    }
}

/// The downgrade command for `(release, transactional)`, or `None` for an
/// unknown key (provider maps `None` to `MissingDowngraderError`).
#[must_use]
pub fn downgrader(release: &str, transactional: bool) -> Option<ActionCommands> {
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

    #[test]
    fn zypper_command_expands_package_and_version() {
        let cmds = downgrader("15", false).unwrap();
        let vars: HashMap<&str, &str> = [("package", "kernel"), ("version", "1.0")]
            .into_iter()
            .collect();
        assert_eq!(
            cmds.render_command(&vars).unwrap(),
            "rpm -q kernel &>/dev/null  && zypper -n in -C --force-resolution --oldpackage -y kernel=1.0"
        );
    }

    #[test]
    fn list_command_preserves_shell_and_awk_dollars() {
        let cmds = downgrader("15", false).unwrap();
        let vars: HashMap<&str, &str> = [("packages", "a b")].into_iter().collect();
        let listed = cmds.render_list_command(&vars).unwrap().unwrap();
        // `$packages` expanded.
        assert!(listed.contains("for p in a b; do"));
        // `$$p` -> literal `$p` for the shell loop.
        assert!(listed.contains("-t package $p;"));
        // awk field refs preserved.
        assert!(listed.contains("print $2,\"=\",$4"));
    }

    #[test]
    fn yum_has_no_list_command() {
        let cmds = downgrader("YUM", false).unwrap();
        assert!(cmds.list_command.is_none());
        let vars: HashMap<&str, &str> = [("package", "p")].into_iter().collect();
        assert_eq!(cmds.render_command(&vars).unwrap(), "yum -y downgrade p");
    }

    #[test]
    fn slmicro_is_transactional_with_reboot() {
        let cmds = downgrader("slmicro", true).unwrap();
        assert_eq!(
            cmds.render_reboot().unwrap(),
            Some("systemctl reboot".into())
        );
        let vars: HashMap<&str, &str> = [("package", "a=1 b=2")].into_iter().collect();
        assert_eq!(
            cmds.render_command(&vars).unwrap(),
            "transactional-update -n pkg in --force-resolution --oldpackage -y a=1 b=2"
        );
    }

    #[test]
    fn unknown_key_is_none() {
        assert!(downgrader("99", false).is_none());
    }
}
