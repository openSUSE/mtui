//! Downgrade command templates (role `downgrader`).
//!
//! Downgrade carries a `list_command` (enumerate installed/available versions)
//! and a `command`. The zypper list probes every package in a **single** zypper
//! invocation (the real `$packages`) and pipes it through awk (`$2`/`$4` field
//! refs survive `SubstMode::Safe`), and the command interpolates `$package` /
//! `$version`; the call sites use safe substitution so the shell/awk `$`-tokens
//! survive.
//! The `slmicro` entry is transactional with a reboot.
//!
//! `LIST_COMMAND`'s leading newline keeps the first command off the prompt line
//! in the transcript `show_log` prints.

use crate::update_workflow::actions::{ActionCommands, SubstMode};

/// zypper/slmicro downgrade list command, shared by both.
///
/// One `zypper -n se -s` invocation for the whole package list: a per-package `for p in $packages; do zypper ... $$p; done` loop
/// loads the repo metadata once per package and, piped through a block-buffered
/// awk with no PTY, emits nothing until the last iteration — on a slow host a
/// long package list blows the SSH no-output timeout (`connection_timeout`,
/// default 300s) and the probe dies with no versions resolved.
const LIST_COMMAND: &str = r#"
zypper -n se -s --match-exact -t package $packages \
| grep -v "(System" \
| grep ^[iv] \
| sed "s, ,,g" \
| awk -F "|" '{ print $2,"=",$4 }'
"#;

/// zypper downgrade command, verbatim.
const ZYPPER_CMD: &str = "rpm -q $package &>/dev/null  && zypper -n in -C --force-resolution --oldpackage -y $package=$version";

/// slmicro downgrade command, verbatim.
const SLM_CMD: &str = "transactional-update -n pkg in --force-resolution --oldpackage -y $package";

/// yum downgrade command, verbatim.
const YUM_CMD: &str = "yum -y downgrade $package";

/// zypper downgrade.
fn zypper() -> ActionCommands {
    ActionCommands {
        command: ZYPPER_CMD.to_owned(),
        list_command: Some(LIST_COMMAND.to_owned()),
        reboot: None,
        installed_only: None,
        mode: SubstMode::Safe,
    }
}

/// slmicro (transactional) downgrade.
fn slmicro() -> ActionCommands {
    ActionCommands {
        command: SLM_CMD.to_owned(),
        list_command: Some(LIST_COMMAND.to_owned()),
        reboot: Some("systemctl reboot".to_owned()),
        installed_only: None,
        mode: SubstMode::Safe,
    }
}

/// yum downgrade.
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
pub(crate) fn downgrader(release: &str, transactional: bool) -> Option<ActionCommands> {
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
    fn list_command_probes_all_packages_in_one_call() {
        // The version probe runs ONE zypper invocation for the whole list.
        // A per-package `for` loop, piped through a
        // block-buffered awk, produced no output until the last iteration and
        // blew the SSH no-output timeout on slow hosts.
        let cmds = downgrader("15", false).unwrap();
        let vars: HashMap<&str, &str> = [("packages", "a b c")].into_iter().collect();
        let listed = cmds.render_list_command(&vars).unwrap().unwrap();
        // No per-package loop.
        assert!(!listed.contains("for p in"), "{listed}");
        // Exactly one zypper invocation, over the whole list.
        assert_eq!(listed.matches("zypper").count(), 1, "{listed}");
        assert!(
            listed.contains("zypper -n se -s --match-exact -t package a b c"),
            "{listed}"
        );
        // awk field refs preserved.
        assert!(listed.contains("{ print $2,\"=\",$4 }"));
        // Discriminating: the doubled form contains the single-brace substring.
        assert!(!listed.contains("{{"), "{listed}");
    }

    #[test]
    fn slmicro_list_command_probes_all_packages_in_one_call() {
        // The slmicro probe is the same single-invocation shape as zypper's.
        let cmds = downgrader("slmicro", true).unwrap();
        let vars: HashMap<&str, &str> = [("packages", "a b")].into_iter().collect();
        let listed = cmds.render_list_command(&vars).unwrap().unwrap();
        assert!(!listed.contains("for p in"), "{listed}");
        assert_eq!(listed.matches("zypper").count(), 1, "{listed}");
        assert!(
            listed.contains("zypper -n se -s --match-exact -t package a b"),
            "{listed}"
        );
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
