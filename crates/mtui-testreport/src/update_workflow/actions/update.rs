//! Update command templates (upstream `actions/update.py`, role `updater`).
//!
//! These command templates interpolate `$repa` (the patch repo/RRID selector)
//! and `$packages` **via `safe_substitute`** upstream (see
//! `hostgroup.py::perform_update`), because they also embed shell/awk `$`-tokens
//! (`awk … print $2`, `while read r … $$r`) that must reach the remote shell
//! unaltered. The `slmicro` (`slm_update`) entry is transactional with a reboot.
//!
//! The template strings are ported **verbatim**, including their leading
//! newline, so the emitted commands are byte-identical to upstream.

use crate::update_workflow::actions::{ActionCommands, SubstMode};

/// yum update command (upstream `yum_update["command"]`), verbatim.
const YUM_UPDATE: &str = "
export LANG=
yum repolist
yum -y update $packages
";

/// zypper update command (upstream `zypper_update["command"]`), verbatim.
const ZYPPER_UPDATE: &str = r#"
export LANG=
zypper -n lr -puU
zypper -n refresh
zypper -n patches | grep $repa
zypper -n in -l -y -t patch $(zypper -n patches | awk -F "|" '/$repa\>/ {{ print $2; }}')
zypper -n patches | grep $repa
zypper -n lr | awk -F "|" '/$repa\>/ {{ print $2; }}' | while read r; do zypper -n rr $$r; done
"#;

/// slmicro update command (upstream `slm_update["command"]`), verbatim.
const SLM_UPDATE: &str = r#"
export LANG=
zypper -n lr -puU
zypper -n patches | grep $repa
transactional-update -n pkg in -l -y -t patch $(zypper -n patches | awk -F "|" '/$repa\>/ {{ print $2; }}')
zypper -n patches | grep $repa
zypper -n lr | awk -F "|" '/$repa\>/ {{ print $2; }}' | while read r; do zypper -n rr $$r; done
"#;

fn yum() -> ActionCommands {
    ActionCommands::command_only(YUM_UPDATE).with_mode(SubstMode::Safe)
}

fn zypper() -> ActionCommands {
    ActionCommands::command_only(ZYPPER_UPDATE).with_mode(SubstMode::Safe)
}

fn slmicro() -> ActionCommands {
    ActionCommands::with_reboot(SLM_UPDATE, "systemctl reboot").with_mode(SubstMode::Safe)
}

/// The update command for `(release, transactional)`, or `None` for an unknown
/// key (provider maps `None` to `MissingUpdaterError`).
#[must_use]
pub(crate) fn updater(release: &str, transactional: bool) -> Option<ActionCommands> {
    match (release, transactional) {
        ("YUM", false) => Some(yum()),
        ("11", false) | ("12", false) | ("15", false) | ("16", false) => Some(zypper()),
        ("slmicro", true) => Some(slmicro()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    fn vars<'a>(repa: &'a str, packages: &'a str) -> HashMap<&'a str, &'a str> {
        [("repa", repa), ("packages", packages)]
            .into_iter()
            .collect()
    }

    #[test]
    fn zypper_keys_resolve() {
        for rel in ["11", "12", "15", "16"] {
            assert!(
                updater(rel, false).is_some(),
                "expected zypper updater for {rel}"
            );
        }
    }

    #[test]
    fn zypper_render_expands_repa_and_preserves_awk_and_escaped_dollar() {
        let cmds = zypper();
        let rendered = cmds
            .render_command(&vars(":p=1:2", "pkg-a"))
            .expect("safe substitute never fails");
        // $repa expanded everywhere.
        assert!(rendered.contains("grep :p=1:2"));
        assert!(rendered.contains(r"/:p=1:2\>/"));
        // awk field ref `$2` preserved (safe_substitute leaves it).
        assert!(rendered.contains("print $2;"));
        // `$$r` -> literal `$r` for the shell loop.
        assert!(rendered.contains("zypper -n rr $r; done"));
        // Template braces `{{ }}` are literal in string.Template.
        assert!(rendered.contains("{{ print $2; }}"));
    }

    #[test]
    fn yum_command_expands_packages() {
        let cmds = updater("YUM", false).expect("yum updater");
        let rendered = cmds.render_command(&vars("", "p1 p2")).unwrap();
        assert!(rendered.contains("yum -y update p1 p2"));
    }

    #[test]
    fn slmicro_is_transactional_with_reboot() {
        let cmds = updater("slmicro", true).expect("slmicro updater");
        assert_eq!(
            cmds.render_reboot().unwrap(),
            Some("systemctl reboot".into())
        );
        let rendered = cmds.render_command(&vars(":p=1:2", "p")).unwrap();
        assert!(rendered.contains("transactional-update -n pkg in -l -y -t patch"));
    }

    #[test]
    fn unknown_key_is_none() {
        assert!(updater("99", false).is_none());
        assert!(updater("slmicro", false).is_none());
    }
}
