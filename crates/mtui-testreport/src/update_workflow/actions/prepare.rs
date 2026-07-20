//! Prepare command templates (upstream `actions/prepare.py`, role `preparer`).
//!
//! Unlike the other actions, prepare entries are **parameterized** by `force`
//! and `testing` (upstream `zypper_prepare(force, testing)` etc.): the flag is
//! baked into the command string at construction. Prepare carries an
//! `installed_only` variant ("run only if the package is already installed") and
//! interpolates `$package`. The `slmicro` entry is transactional with a reboot.
//!
//! ## Substitution mode: `Safe` (deviates from upstream's `.substitute`)
//!
//! Upstream renders these with `.substitute(package=...)`. That is a **latent
//! bug** for the `installed_only` template, whose shell command-substitution
//! `$(rpm -q ...)` is an invalid `string.Template` placeholder — `.substitute`
//! raises `ValueError` (verified against CPython), so the intended command can
//! never actually run under strict mode. We therefore use `safe_substitute`,
//! which leaves `$(` verbatim and produces the command upstream clearly
//! intended. For the `command` template (no `$(`), `safe_substitute` is
//! byte-identical to `substitute` when `package` is supplied, so the happy path
//! is preserved exactly while the crash is fixed.

use crate::update_workflow::actions::ActionCommands;

/// zypper prepare (upstream `zypper_prepare`).
///
/// `force` toggles `--force-resolution`; `testing` is accepted for signature
/// parity but unused by zypper (matching upstream).
fn zypper(force: bool, _testing: bool) -> ActionCommands {
    let parameter = if force { "--force-resolution" } else { "" };
    ActionCommands {
        command: format!("zypper -n in -y -l {parameter} $package"),
        installed_only: Some(format!(
            "if $(rpm -q $package &>/dev/null); then zypper -n in -y -l {parameter} $package ; fi"
        )),
        reboot: None,
        list_command: None,
        mode: crate::update_workflow::actions::SubstMode::Safe,
    }
}

/// yum prepare (upstream `yum_prepare`).
///
/// `testing` toggles `--disablerepo=*testing*` (present when **not** testing);
/// `force` is accepted for parity but unused by yum (matching upstream).
fn yum(_force: bool, testing: bool) -> ActionCommands {
    let parameter = if testing {
        ""
    } else {
        "--disablerepo=*testing*"
    };
    ActionCommands {
        command: format!("yum -y {parameter} install $package"),
        installed_only: Some(format!(
            "rpm -q $package &>/dev/null && yum {parameter} -y install $package"
        )),
        reboot: None,
        list_command: None,
        mode: crate::update_workflow::actions::SubstMode::Safe,
    }
}

/// slmicro (transactional) prepare (upstream `slm_prepare`).
fn slmicro(force: bool, _testing: bool) -> ActionCommands {
    let parameter = if force { "--force-resolution" } else { "" };
    ActionCommands {
        command: format!("transactional-update -n pkg in -l {parameter} $package"),
        installed_only: Some(format!(
            "if $(rpm -q $package &>/dev/null); then transactional-update -n pkg in -l {parameter} $package ; fi"
        )),
        reboot: Some("systemctl reboot".to_owned()),
        list_command: None,
        mode: crate::update_workflow::actions::SubstMode::Safe,
    }
}

/// The prepare command for `(release, transactional)` with the given `force` /
/// `testing` flags, or `None` for an unknown key (provider maps `None` to
/// `MissingPreparerError`).
#[must_use]
pub fn preparer(
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
        // No force flag -> empty parameter leaves a double space (matches
        // upstream's `f"...-l {parameter} $package"` with parameter == "").
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
    fn zypper_installed_only_variant_renders() {
        let cmds = preparer("12", false, false, false).unwrap();
        let io = cmds.render_installed_only(&pkg("kernel")).unwrap().unwrap();
        assert!(io.starts_with("if $(rpm -q kernel &>/dev/null); then zypper"));
        assert!(io.ends_with("kernel ; fi"));
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
