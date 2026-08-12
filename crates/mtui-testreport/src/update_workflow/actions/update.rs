//! Update command templates (role `updater`).
//!
//! These command templates interpolate `$repa` (the patch repo/RRID selector)
//! and `$packages` **via safe substitution**, because they also embed shell/awk
//! `$`-tokens
//! (`awk … print $2`, `while read r … $$r`) that must reach the remote shell
//! unaltered. The `slmicro` entry is transactional with a reboot.
//!
//! Each template's leading newline keeps the first command off the prompt line
//! in the transcript `show_log` prints.
//!
//! # The exit status the zypper / slmicro scripts report
//!
//! Both are multi-line scripts run as **one** remote `exec`, so the shell's
//! last-command-wins rule decides what the update check gets to inspect. Left
//! alone that is the trailing repo-cleanup loop's status — `0` whenever the
//! loop body never runs — and a genuinely failed patch reported success.
//!
//! So the patch's own status is captured into `$mtui_status` on the line after
//! the patch, before the post-state `zypper -n patches | grep` can clobber
//! `$?`, and the script ends with `exit $mtui_status`. The two commands after
//! the patch stay: the `grep` is transcript signal, and the cleanup loop is
//! real work. Neither may decide the verdict — an `update` check failure fires
//! the **group-wide** rollback downgrade, so a cosmetic `zypper -n rr` hiccup
//! would revert every healthy host in the group.
//!
//! The patch list is captured into `$mtui_patches` first so the empty case is
//! distinguishable: a host carrying none of the update's products is a no-op,
//! not a failure, and running the patch command with no operands would make the
//! package manager complain about its own arguments — a status that is now
//! real. That verdict is unchanged; only the way it is reached is.
//!
//! The emptiness test is `set -- $mtui_patches` followed by `[ "$#" -gt 0 ]`,
//! **not** `[ -n "$mtui_patches" ]`. The two differ on a list of whitespace:
//! `-n` calls it non-empty, but the unquoted expansion that follows splits it
//! into *zero* words, so the patch would run with no operands after all — the
//! precise case the guard exists to prevent, and one the awk can produce from a
//! matching row whose second `|` field is blank. Going through the positional
//! parameters makes the guard test what the patch command will actually
//! receive, and lets the patch itself use a quoted `"$@"` instead of a bare
//! unquoted expansion.
//!
//! # The residual: a refresh that fails silently
//!
//! `zypper -n refresh` is **not** guarded, and the empty-list no-op above is
//! what makes that a real gap: if the issue repo is unreachable or its key is
//! untrusted, the refresh fails, `zypper -n patches` matches no row, the guard
//! skips the patch and the script exits `0`. An update that installed nothing
//! passes its check. Deferred rather than fixed, because `refresh` returns `4`
//! whether one repository failed or all of them did, so its status cannot tell
//! "the issue repo went missing" from "an unrelated stale repo did" — and
//! failing on the latter would abort updates on refhosts that would have
//! patched correctly, which is routine on QAM refhosts. Closing it needs a
//! check that the *issue* repo specifically is present, not a status test.
//!
//! Two constraints on the text:
//!
//! * every `$` meant for the remote shell is written `$$`
//!   (`$$mtui_status`, `$$?`, `$$(`, `$$#`, `$$@`, `$$r`). A bare `$?` happens
//!   to survive [`SubstMode::Safe`] today, but `$$` is the documented escape
//!   and the only form that stays correct if the mode is ever tightened. The
//!   awk field refs (`print $2`) stay bare — that is the tested convention.
//!   Shell variables are `mtui_`-prefixed *and* `$$`-escaped, so no future
//!   `$name` template variable can silently expand one away.
//! * the literal token `zypper` stays in `ZYPPER_UPDATE`: the *zypper* update
//!   check gates its exit-code rules on the command text containing it
//!   (`checks::update::zypper`), so losing it there would silently disable
//!   them. `SLM_UPDATE` carries the token too, via its `zypper -n lr` /
//!   `patches` lines, but nothing depends on that —
//!   `checks::update::transactional_update` deliberately has no such gate.
//!
//! One portability note, pre-existing and unchanged: `/$repa\>/` uses `\>`,
//! which is a **GNU-awk** word-boundary operator. The SUSE hosts these run on
//! ship gawk. Other awks degrade it to a literal `>`, which matters only to the
//! tests below — they execute this text under the *build host's* awk, and pick
//! a stub row that matches under either reading.

use crate::update_workflow::actions::{ActionCommands, SubstMode};

/// yum update command.
const YUM_UPDATE: &str = "
export LANG=
yum repolist
yum -y update $packages
";

/// zypper update command.
///
/// See the module docs for why the patch's status is captured rather than left
/// to the shell's last-command-wins rule.
const ZYPPER_UPDATE: &str = r#"
export LANG=
zypper -n lr -puU
zypper -n refresh
zypper -n patches | grep $repa
mtui_patches=$$(zypper -n patches | awk -F "|" '/$repa\>/ { print $2; }')
mtui_status=0
set -- $$mtui_patches
if [ "$$#" -gt 0 ]; then
  zypper -n in -l -y -t patch "$$@"
  mtui_status=$$?
fi
zypper -n patches | grep $repa
zypper -n lr | awk -F "|" '/$repa\>/ { print $2; }' | while read r; do zypper -n rr $$r; done
exit $$mtui_status
"#;

/// slmicro update command.
///
/// See the module docs for why the patch's status is captured rather than left
/// to the shell's last-command-wins rule.
const SLM_UPDATE: &str = r#"
export LANG=
zypper -n lr -puU
zypper -n patches | grep $repa
mtui_patches=$$(zypper -n patches | awk -F "|" '/$repa\>/ { print $2; }')
mtui_status=0
set -- $$mtui_patches
if [ "$$#" -gt 0 ]; then
  transactional-update -n pkg in -l -y -t patch "$$@"
  mtui_status=$$?
fi
zypper -n patches | grep $repa
zypper -n lr | awk -F "|" '/$repa\>/ { print $2; }' | while read r; do zypper -n rr $$r; done
exit $$mtui_status
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
        // Braces are literal to the substituter — only `$` is special — so the
        // awk action reaches the remote shell exactly as written.
        assert!(rendered.contains("{ print $2; }"));
        // Discriminating: `"{{ print $2; }}"` *contains* `"{ print $2; }"`, so
        // the assertion above alone would also pass on the old doubled form.
        assert!(!rendered.contains("{{"), "{rendered}");
        assert_status_comes_from_the_patch(
            "zypper",
            &rendered,
            ":p=1:2",
            "zypper -n in -l -y -t patch",
        );
    }

    /// The `$$`-escaping and status-capture contract, asserted on the
    /// **rendered** text rather than the source constant — a `$` that was meant
    /// for the remote shell but written bare still *looks* right in the source.
    ///
    /// `patch` is the patch command's leading text, so the shape being pinned
    /// is that nothing at all sits between the patch and the capture of its
    /// status.
    fn assert_status_comes_from_the_patch(name: &str, rendered: &str, repa: &str, patch: &str) {
        // The single discriminating check on the escaping: after substitution
        // no `$$` may survive. Every `$$` in these templates is an escape for
        // the remote shell, so one left doubled is one the shell would read as
        // its own PID.
        assert!(
            !rendered.contains("$$"),
            "{name}: an unresolved `$$` reached the remote shell: {rendered}"
        );
        // The patch list is captured before the patch so the empty case is
        // distinguishable.
        assert!(
            rendered.contains("mtui_patches=$(zypper -n patches | awk -F \"|\""),
            "{name}: the patch list must be captured first: {rendered}"
        );
        // Guarded on the *word count*, not on the string being non-empty: a
        // whitespace-only list is `-n`-true but splits to zero words, which
        // would run the patch with no operands after all.
        assert!(
            rendered.contains("set -- $mtui_patches\nif [ \"$#\" -gt 0 ]; then\n"),
            "{name}: the patch must be guarded on the split word count: {rendered}"
        );
        // Nothing between the patch and the capture of its status: the
        // post-state `grep` two lines down would otherwise clobber `$?`.
        assert!(
            rendered.contains(&format!("  {patch} \"$@\"\n  mtui_status=$?\n")),
            "{name}: the patch's status must be captured on the very next line: {rendered}"
        );
        // Everything after the patch, as one contiguous block anchored to the
        // end of the script. `contains` would prove only presence: the
        // repo-cleanup loop moved *above* the `if` still contains, but removes
        // the issue repos before the patch can resolve against them. This pins
        // that the post-state `grep` (the command whose `$?`-clobbering
        // motivates the whole fix) and the cleanup loop are still there, still
        // in that order, still after the patch — and that `exit` is last.
        let tail = format!(
            "fi\nzypper -n patches | grep {repa}\nzypper -n lr | awk -F \"|\" '/{repa}\\>/ {{ print $2; }}' | while read r; do zypper -n rr $r; done\nexit $mtui_status\n"
        );
        assert!(
            rendered.ends_with(&tail),
            "{name}: the script must end with grep, cleanup loop, exit — in that order.\nwant tail:\n{tail}\ngot:\n{rendered}"
        );
        // The other half of the contract — that `checks::update::zypper`'s
        // `zypper`-token gate still fires on this text — is pinned in that
        // module, by feeding it the rendered template. Asserting the token
        // here would only pin the template against itself.
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
        assert!(rendered.contains("{ print $2; }"));
        assert!(!rendered.contains("{{"), "{rendered}");
        assert_status_comes_from_the_patch(
            "slmicro",
            &rendered,
            ":p=1:2",
            "transactional-update -n pkg in -l -y -t patch",
        );
    }

    #[test]
    fn yum_is_untouched_by_the_status_capture() {
        // `YUM_UPDATE`'s last line *is* `yum -y update`, so its status was
        // never masked and there is nothing to fix. The yum check's narrow
        // did-it-run verdict rests on a different argument entirely (one key
        // for every RHEL version, `yum` is `dnf` on 8/9, and mtui hands the
        // host the update's whole package list) — extending the capture here
        // would be a new, unargued rule on a transcript it was never written
        // for.
        let rendered = updater("YUM", false)
            .expect("yum updater")
            .render_command(&vars("", "p1 p2"))
            .unwrap();
        assert!(!rendered.contains("mtui_status"), "{rendered}");
        assert!(
            rendered.trim_end().ends_with("yum -y update p1 p2"),
            "{rendered}"
        );
    }

    #[test]
    fn unknown_key_is_none() {
        assert!(updater("99", false).is_none());
        assert!(updater("slmicro", false).is_none());
    }

    /// Executes the *rendered* update scripts under a real `/bin/sh`, with stub
    /// `zypper` / `transactional-update` executables first on `PATH`.
    ///
    /// The subject of this module is shell semantics — which command's status
    /// the script reports — and no mock can express that: `MockConnection` keys
    /// on the whole command string and scripts one exit code per command, so
    /// "the patch failed but the last line succeeded" is a state it cannot
    /// produce. A check-level test that simply passes `exitcode: 1` proves the
    /// classifier, not the script.
    ///
    /// The stubs append to a sentinel file on every invocation, so a test can
    /// assert the patch actually ran (or actually did not). Without that, a
    /// `PATH` mistake would make the script a no-op and every assertion here
    /// vacuous.
    #[cfg(unix)]
    mod rendered_script {
        use std::ffi::OsString;
        use std::fs;
        use std::os::unix::fs::PermissionsExt;
        use std::path::Path;
        use std::process::Command;

        use super::*;

        /// The `$repa` selector the scripts are rendered with.
        const REPA: &str = ":p=42:7";

        /// A `zypper -n patches` row: `|`-field 2 is the patch name the awk in
        /// the template extracts, and field 1 carries [`REPA`] immediately
        /// followed by `>`.
        ///
        /// That `>` is deliberate and load-bearing for portability. The
        /// template's regex is `/:p=42:7\>/`, and `\>` is a **GNU-awk**
        /// word-boundary operator; other awks (macOS ships BWK awk, and this
        /// job also runs there) degrade an unknown escape to the literal
        /// character. A row containing `:p=42:7>` satisfies **both** readings:
        /// gawk sees a word boundary after `7` because `>` is a non-word
        /// character, and a non-GNU awk sees the literal `>`. Verified against
        /// gawk and against `gawk --traditional`, which reproduces the non-GNU
        /// reading. `the_stub_rows_match_under_this_hosts_awk` re-checks it on
        /// whatever awk actually runs, so a third dialect fails loudly with an
        /// explanation instead of silently extracting nothing.
        const PATCH_ROW: &str =
            "issue-:p=42:7>repo | patch-alpha | security | important | --- | needed";

        /// A `zypper -n patches` row whose second field is **whitespace**.
        ///
        /// `[ -n "$mtui_patches" ]` calls this non-empty while the unquoted
        /// expansion that follows splits it into zero words — the reason the
        /// guard counts split words instead.
        const BLANK_PATCH_ROW: &str = "issue-:p=42:7>repo |    | security";

        /// A `zypper -n lr` row the repo-cleanup loop matches, so the loop body
        /// (`zypper -n rr`) actually runs and its status can be controlled.
        const REPO_ROW: &str = "1 | issue-:p=42:7>repo | Yes | Yes";

        /// The stub `zypper`. Every call in both templates is
        /// `zypper -n <subcommand> …`, so `$2` discriminates.
        ///
        /// Note this means `zypper -n lr -puU` also lands in the `lr)` arm;
        /// harmless, since its output is not piped anywhere.
        ///
        /// Every arm records its invocation. The `patches`/`lr` probes matter
        /// as much as the patch: without them "the patch never ran" — the
        /// assertion the empty-list case rests on — is also what a broken
        /// `PATH` produces, and the test would pass with no stubs at all.
        const ZYPPER_STUB: &str = r#"#!/bin/sh
printf '%s\n' "$2" >> "$MTUI_STUB_DIR/probe.ran"
case "$2" in
patches)
    if [ -n "$MTUI_STUB_PATCH_ROW" ]; then printf '%s\n' "$MTUI_STUB_PATCH_ROW"; fi
    ;;
lr)
    if [ -n "$MTUI_STUB_REPO_ROW" ]; then printf '%s\n' "$MTUI_STUB_REPO_ROW"; fi
    ;;
rr)
    printf '%s\n' "$*" >> "$MTUI_STUB_DIR/cleanup.ran"
    exit "$MTUI_STUB_CLEANUP_EXIT"
    ;;
in)
    printf '%s\n' "$*" >> "$MTUI_STUB_DIR/patch.ran"
    exit "$MTUI_STUB_PATCH_EXIT"
    ;;
esac
exit 0
"#;

        /// The stub `transactional-update`: the slmicro patch command, and the
        /// only thing that binary is called for in `SLM_UPDATE`.
        const TU_STUB: &str = r#"#!/bin/sh
printf '%s\n' "$*" >> "$MTUI_STUB_DIR/patch.ran"
exit "$MTUI_STUB_PATCH_EXIT"
"#;

        /// A temp dir holding the stub executables plus the sentinel files they
        /// append to.
        struct Stubs {
            dir: tempfile::TempDir,
            path: OsString,
        }

        impl Stubs {
            fn new() -> Self {
                let dir = tempfile::tempdir().expect("tempdir");
                write_exe(dir.path(), "zypper", ZYPPER_STUB);
                write_exe(dir.path(), "transactional-update", TU_STUB);
                // Keep the inherited PATH behind the stubs: the templates also
                // call the real `awk` and `grep`.
                let mut path = dir.path().as_os_str().to_owned();
                if let Some(inherited) = std::env::var_os("PATH") {
                    path.push(":");
                    path.push(inherited);
                }
                Self { dir, path }
            }

            /// The lines the patch stub appended, one per invocation.
            fn patch_invocations(&self) -> Vec<String> {
                sentinel(&self.dir.path().join("patch.ran"))
            }

            /// The lines the `zypper -n rr` cleanup stub appended.
            fn cleanup_invocations(&self) -> Vec<String> {
                sentinel(&self.dir.path().join("cleanup.ran"))
            }

            /// Every `zypper` subcommand the stub saw, in order.
            ///
            /// The liveness signal for the cases whose headline assertion is a
            /// *negative* ("the patch never ran"): that is equally what an
            /// empty `PATH` produces, so a case asserting it must also show the
            /// script reached the stub at all.
            fn probe_invocations(&self) -> Vec<String> {
                sentinel(&self.dir.path().join("probe.ran"))
            }
        }

        fn write_exe(dir: &Path, name: &str, body: &str) {
            let p = dir.join(name);
            fs::write(&p, body).expect("write stub");
            fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).expect("chmod stub");
        }

        fn sentinel(p: &Path) -> Vec<String> {
            fs::read_to_string(p)
                .unwrap_or_default()
                .lines()
                .map(ToOwned::to_owned)
                .collect()
        }

        /// The two templates whose exit status must be the patch's.
        fn templates() -> Vec<(&'static str, ActionCommands)> {
            vec![("zypper", zypper()), ("slmicro", slmicro())]
        }

        /// Renders `cmds` and runs it under `/bin/sh -c`, returning the script's
        /// own exit status.
        ///
        /// `patch_row` empty means the update matches no patch on this host;
        /// `repo_row` empty means the cleanup loop finds nothing to remove (its
        /// body never runs, so the loop's status is `0` — the masking the whole
        /// issue is about).
        fn run_script(
            cmds: &ActionCommands,
            stubs: &Stubs,
            patch_exit: i32,
            cleanup_exit: i32,
            patch_row: &str,
            repo_row: &str,
        ) -> i32 {
            let rendered = cmds
                .render_command(&vars(REPA, "pkg-a"))
                .expect("safe substitute never fails");
            let out = Command::new("/bin/sh")
                .arg("-c")
                .arg(&rendered)
                .env("PATH", &stubs.path)
                .env("MTUI_STUB_DIR", stubs.dir.path())
                .env("MTUI_STUB_PATCH_EXIT", patch_exit.to_string())
                .env("MTUI_STUB_CLEANUP_EXIT", cleanup_exit.to_string())
                .env("MTUI_STUB_PATCH_ROW", patch_row)
                .env("MTUI_STUB_REPO_ROW", repo_row)
                .output()
                .expect("run rendered script under /bin/sh");
            out.status
                .code()
                .unwrap_or_else(|| panic!("script died on a signal: {:?}", out.status))
        }

        #[test]
        fn the_stub_rows_match_under_this_hosts_awk() {
            // The portability guard for every case below. The template's
            // `/$repa\>/` is GNU-awk syntax; this runs the *production*
            // expression, through the host's real awk, against the stub rows,
            // and asserts the patch name comes back out.
            //
            // Without this, a host whose awk reads `\>` a third way would fail
            // the other cases with "the patch stub must have run" — true but
            // wildly misleading. Here it fails with the actual cause. It is a
            // hard failure, not a skip: a skip would be a green result on a
            // host where the acceptance tests proved nothing, which is the
            // shape of every false-green in this repo's history.
            let prog = format!("awk -F \"|\" '/{REPA}\\>/ {{ print $2; }}'");
            let out = Command::new("/bin/sh")
                .arg("-c")
                .arg(format!("printf '%s\\n' \"$1\" | {prog}"))
                .arg("sh")
                .arg(PATCH_ROW)
                .output()
                .expect("run the template's awk under /bin/sh");
            let got = String::from_utf8_lossy(&out.stdout).trim().to_owned();
            assert_eq!(
                got,
                "patch-alpha",
                "this host's awk does not extract the patch name from the stub row.\n\
                 The template uses `\\>` (a GNU-awk word-boundary operator); the stub row \
                 carries `{REPA}>` so that gawk (word boundary before a non-word `>`) and a \
                 non-GNU awk (literal `>`) both match. This awk reads it a third way, so the \
                 acceptance cases below would report 'the patch stub must have run' instead of \
                 the real cause.\nprogram: {prog}\nrow: {PATCH_ROW}\nstderr: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }

        #[test]
        fn a_failed_patch_fails_the_script_even_when_the_cleanup_succeeds() {
            // The bug in #400: the script's last line is the repo-cleanup loop,
            // whose status is `0` whenever its body never runs. The patch's
            // non-zero status was discarded before anything could read it, so
            // a genuinely failed patch reported success.
            for (name, cmds) in templates() {
                let stubs = Stubs::new();
                let code = run_script(&cmds, &stubs, 8, 0, PATCH_ROW, "");
                assert_eq!(
                    stubs.patch_invocations().len(),
                    1,
                    "{name}: the patch stub must have run, or this test is vacuous"
                );
                assert!(
                    stubs.cleanup_invocations().is_empty(),
                    "{name}: no repo row, so the cleanup loop body must not run"
                );
                assert_eq!(code, 8, "{name}: the script must report the patch's status");
            }
        }

        #[test]
        fn a_failing_cleanup_does_not_fail_a_successful_patch() {
            // The other direction, and the reason a bare "report the last
            // command's status" would be wrong: the repo cleanup is cosmetic,
            // and an update check failure fires the *group-wide* rollback
            // downgrade, reverting every healthy host in the group.
            for (name, cmds) in templates() {
                let stubs = Stubs::new();
                let code = run_script(&cmds, &stubs, 0, 3, PATCH_ROW, REPO_ROW);
                assert_eq!(
                    stubs.patch_invocations().len(),
                    1,
                    "{name}: the patch stub must have run, or this test is vacuous"
                );
                assert_eq!(
                    stubs.cleanup_invocations().len(),
                    1,
                    "{name}: the cleanup loop body must have run and failed"
                );
                assert_eq!(
                    code, 0,
                    "{name}: a cosmetic cleanup hiccup must not fail the update"
                );
            }
        }

        #[test]
        fn an_empty_patch_list_skips_the_patch_and_succeeds() {
            // A host carrying none of the update's products is a no-op, not a
            // failure. Running the patch command with no operands would make
            // the package manager complain about its own arguments, and that
            // status would now be real.
            for (name, cmds) in templates() {
                let stubs = Stubs::new();
                let code = run_script(&cmds, &stubs, 8, 0, "", "");
                // Liveness first. The headline assertion here is a *negative*,
                // and an empty `PATH` produces exactly the same observation —
                // so show the script actually reached the stub.
                assert!(
                    stubs.probe_invocations().iter().any(|c| c == "patches"),
                    "{name}: the script never reached the stub zypper at all: {:?}",
                    stubs.probe_invocations()
                );
                assert!(
                    stubs.patch_invocations().is_empty(),
                    "{name}: with no matching patch the patch command must not run: {:?}",
                    stubs.patch_invocations()
                );
                assert_eq!(code, 0, "{name}: a host with nothing to patch passes");
            }
        }

        #[test]
        fn a_whitespace_only_patch_list_skips_the_patch_and_succeeds() {
            // The gap `[ -n "$mtui_patches" ]` left. A matching `zypper
            // patches` row whose second `|` field is blank makes the awk emit
            // whitespace: `-n` calls that non-empty, the unquoted expansion
            // then splits it into zero words, and the patch runs with **no
            // operands** — the very case the guard exists to prevent. Its
            // status is real now, so the package manager's complaint about its
            // own arguments reaches the check as a failure instead of being
            // masked — as which reason depends on the code and the transcript
            // (the stub's `8` reads as "RPM Error" when it writes `Error:` to
            // stderr, "package not found" when it does not), which is why this
            // asserts on the exit status rather than on a reason string.
            for (name, cmds) in templates() {
                let stubs = Stubs::new();
                let code = run_script(&cmds, &stubs, 8, 0, BLANK_PATCH_ROW, "");
                assert!(
                    stubs.probe_invocations().iter().any(|c| c == "patches"),
                    "{name}: the script never reached the stub zypper at all: {:?}",
                    stubs.probe_invocations()
                );
                assert!(
                    stubs.patch_invocations().is_empty(),
                    "{name}: a whitespace-only list must not run the patch: {:?}",
                    stubs.patch_invocations()
                );
                assert_eq!(code, 0, "{name}: nothing to patch is not a failure");
            }
        }
    }
}
