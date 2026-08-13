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
//! # The probe guard
//!
//! `$mtui_patches` decides whether the patch runs at all, so the commands that
//! *produce* it are the script's premise. If one of them fails the list comes
//! back empty, the guard skips the patch, and the script exits `0` — mtui
//! reporting an update it never installed. That is issue #447, and it is not
//! merely a discarded status: `mtui_patches=$(zypper -n patches | awk …)` took
//! the **pipeline's** status, which POSIX defines as the *last* stage's, so it
//! was awk's — and awk exits `0` on empty input. zypper's status was not
//! discarded there, it was unobservable.
//!
//! `set -o pipefail` (not POSIX) and `${PIPESTATUS[0]}` (bash-only) are both
//! out: the acceptance tests below run this text under `/bin/sh`, which is
//! dash on Debian-family builders. The portable lever is POSIX XCU 2.9.1 — an
//! assignment-only command takes the status of its last command substitution —
//! so each probe is captured on its own (`mtui_patch_rows=$(zypper -n
//! patches)`) and its `$?` read on the next line. The rows then reach awk
//! through the **builtin** `printf '%s\n'`, not `echo` or `xargs`, so on any
//! shell where `printf` is a builtin — dash, bash and busybox, i.e. every
//! shell these run under — no `ARG_MAX` limit sits between them. Were it ever
//! to resolve to `/usr/bin/printf`, a large enough patch table would fail the
//! probe rather than truncate it: the fail-safe direction.
//!
//! `mtui_probe_ok <label> <status>` is the guard. On `0` it returns; on
//! anything else it prints the marker line
//! `mtui: could not determine what to patch: <label> exited <status>` and
//! exits with **the probe's own status**. The marker is the signal — the check
//! gates on it ahead of the exit-code rules
//! ([`crate::update_workflow::checks::update`]) — because no exit code could
//! carry it: a POSIX `exit N` truncates to `0..=255`, so any value a template
//! chose would sit inside the package manager's own space. Nothing in this
//! workspace exits with a status of its own choosing; every `exit` in shell
//! text passes a tool's status through, and this one does too.
//!
//! The marker is assembled from the `printf` format string **and** its
//! argument (`printf 'mtui: could not %s\n' "determine what to patch: …"`) so
//! that the script's own text does not contain the string the check greps for.
//! Otherwise a transcript that echoed the command — a `set -x` in the host's
//! profile, a verbose transport — would report a probe failure on every
//! healthy update. `the_rendered_update_templates_do_not_trip_the_gate` pins
//! it.
//!
//! ## Why the accepted set is `0`, plus `106` only when the repo is there
//!
//! **Deliberately narrower than `classify_exit`'s success set, and the two
//! must not be unified.** That set is argued from "the transaction committed",
//! which is a statement about the *patch*. Read as a verdict on a metadata
//! *probe* its members invert: `106` (`ZYPPER_EXIT_INF_REPO_SKIPPED`) says a
//! repository was skipped, and if the skipped one is the issue repo then the
//! list legitimately lacks the patch — precisely the silent no-op this guard
//! exists to stop.
//!
//! But `106` cannot simply be *rejected* either, and the reason is the same
//! one that leaves `refresh` unguarded below: `init_repos()` raises it for
//! **any** skipped repository and cannot say which. A refhost routinely
//! carries one stale or unreachable repo alongside working ones, and those
//! hosts patch correctly today. Rejecting `106` would abort exactly the hosts
//! that leaving `refresh` unguarded was meant to protect — the two rules would
//! cancel out, and the fleet, not one code path, would pay for it.
//!
//! So the status is not asked to carry the verdict at all. On `106` the script
//! asks the question the guard actually cares about — *is the update's own
//! repo still there?* — with `mtui_issue_repo_present`, which greps `$repa` out
//! of `zypper -n lr`, the same selector and the same listing the cleanup loop
//! already matches on. Repo present: the skipped one was somebody else's, so
//! note it in the transcript and carry on. Repo absent: the list cannot be
//! trusted, and the probe fails.
//!
//! `mtui_issue_repo_present` is a pipeline, and here the last-stage rule is
//! wanted rather than worked around: awk's `END { exit !found }` *is* the
//! verdict. An `lr` that fails outright therefore reads as "absent" and fails
//! the probe, which is the conservative direction.
//!
//! A host carrying none of the update's products is untouched by this: no repo
//! was added for it, so `patches` answers `0`, no `106` arises, and the empty
//! list stays the legitimate no-op it always was. The alias question is only
//! ever asked on the `106` path, which is why it cannot turn that no-op into a
//! failure.
//!
//! ## What is and is not guarded
//!
//! Guarded: `zypper -n patches` (both templates) and **the awk that filters
//! its output**. awk is the last stage of the filter pipeline, so `$?` on the
//! next line is already awk's and the capture is free — and without it #447 is
//! still reachable: an awk that exits `2`, or is missing from `PATH`, yields an
//! empty list, a skipped patch and `exit 0`, which is the original verdict
//! verbatim.
//!
//! **`zypper -n refresh` is deliberately NOT guarded**, although it is the
//! other command whose failure leaves the patch absent from the metadata. Its
//! status cannot support the verdict: verified against `openSUSE/zypper`
//! `src/commands/repos/refresh.cc:337-345`, the explicit `refresh` command
//! returns `ZYPPER_EXIT_ERR_ZYPP` (`4`) both when *every* repository failed and
//! when only *some* did — and it never returns `106`, which is set by
//! `init_repos()` during an operation command, not here. So a refhost with one
//! stale, unreachable or unsigned-key repository alongside working ones is
//! indistinguishable from a host whose issue repo failed, and guarding it would
//! abort updates that are fine. The dominant case it would have caught is not
//! lost: for a freshly added repo with no local cache, a refresh failure means
//! `zypper -n patches` — which *does* go through `init_repos` — answers `106`,
//! and the alias question above then finds the repo missing or disabled.
//!
//! **One residual remains, narrowed rather than closed.** Repos are added with
//! `zypper ar` and no `-f` (`mtui-hosts` `target/repo_manager.rs`), so
//! `patches` will not autorefresh them. A repo whose cache is *stale but
//! present* — an `update` re-run after an earlier successful `prepare`, with
//! the repo now unreachable — lets `patches` answer `0` from that cache with
//! the new patch absent, and the script exits `0`: the original silent no-op,
//! on that one path. Closing it needs a freshness assertion, not a status
//! test.
//!
//! Also not guarded: the post-state `grep` (a successful patch makes it exit
//! `1` — the reason `$mtui_status` exists), the repo-cleanup loop (a cosmetic
//! `zypper -n rr` hiccup must not revert the fleet), and the pre-state `grep`
//! (exit `1` there is the legitimate "this host carries none of these
//! products" case).
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
//! Two constraints on the text:
//!
//! * every `$` meant for the remote shell is written `$$`
//!   (`$$mtui_status`, `$$?`, `$$(`, `$$#`, `$$@`, `$$r`, and the guard
//!   function's own `$$1` / `$$2`). A bare `$?` happens
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
//! One portability note, pre-existing and unchanged: **these templates require
//! GNU awk on the refhost.** `/$repa\>/` uses `\>`, a GNU-awk word-boundary
//! operator; mawk, busybox awk and `gawk --traditional` degrade an unknown
//! escape to the literal character, and against a realistic
//! `zypper -n patches` row that extracts nothing — an empty patch list, a
//! skipped patch and a silent no-op. The SUSE hosts these run on ship gawk, so
//! the dependency is met in production, and it is stated here rather than
//! guarded because the guard cannot see it (awk succeeds; it simply matches
//! nothing).
//!
//! Do not mistake the tests below for awk coverage. They execute this text
//! under the *build host's* awk, and the stub row is deliberately chosen to
//! match under **either** dialect — that keeps the macOS CI leg (BWK awk)
//! working, and it is exactly why those tests cannot notice a dialect that
//! extracts nothing.

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
/// to the shell's last-command-wins rule, why the two commands that produce the
/// patch list are guarded, and why `zypper -n refresh` is not one of them.
const ZYPPER_UPDATE: &str = r#"
export LANG=
mtui_issue_repo_present() {
  zypper -n lr | awk -F "|" '/$repa\>/ { found = 1 } END { exit !found }'
}
mtui_probe_ok() {
  case "$$2" in
  0) return 0 ;;
  106)
    if mtui_issue_repo_present; then
      printf 'mtui: note: %s exited 106, a repository was skipped; the update repo is present, continuing\n' "$$1"
      return 0
    fi
    ;;
  esac
  printf 'mtui: could not %s\n' "determine what to patch: $$1 exited $$2"
  exit "$$2"
}
zypper -n lr -puU
zypper -n refresh
zypper -n patches | grep $repa
mtui_patch_rows=$$(zypper -n patches)
mtui_probe_ok "zypper -n patches" $$?
mtui_patches=$$(printf '%s\n' "$$mtui_patch_rows" | awk -F "|" '/$repa\>/ { print $2; }')
mtui_probe_ok "awk" $$?
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
/// to the shell's last-command-wins rule, and why the two commands that produce
/// the patch list are guarded. Note this template carries no `zypper -n
/// refresh` line at all — it never refreshes, so stale metadata here yields a
/// legitimately empty list that no guard can see.
const SLM_UPDATE: &str = r#"
export LANG=
mtui_issue_repo_present() {
  zypper -n lr | awk -F "|" '/$repa\>/ { found = 1 } END { exit !found }'
}
mtui_probe_ok() {
  case "$$2" in
  0) return 0 ;;
  106)
    if mtui_issue_repo_present; then
      printf 'mtui: note: %s exited 106, a repository was skipped; the update repo is present, continuing\n' "$$1"
      return 0
    fi
    ;;
  esac
  printf 'mtui: could not %s\n' "determine what to patch: $$1 exited $$2"
  exit "$$2"
}
zypper -n lr -puU
zypper -n patches | grep $repa
mtui_patch_rows=$$(zypper -n patches)
mtui_probe_ok "zypper -n patches" $$?
mtui_patches=$$(printf '%s\n' "$$mtui_patch_rows" | awk -F "|" '/$repa\>/ { print $2; }')
mtui_probe_ok "awk" $$?
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
    use crate::update_workflow::checks::update::PROBE_FAILURE_MARKER;

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
        // The guard the probe status is fed to. Without the definition
        // `mtui_probe_ok …` would be a "command not found" — status 127 on
        // stderr — and the script would carry on to the empty-list skip and
        // exit 0, which is the bug it exists to close.
        //
        // Pinned by shape rather than as one literal blob, so that editing the
        // guard's prose does not fail this while a semantic change slips
        // through it. Each of these is a separate way the guard could stop
        // guarding.
        let guard_at = rendered
            .find("mtui_probe_ok() {")
            .unwrap_or_else(|| panic!("{name}: the probe guard is not defined at all: {rendered}"));
        let first_call = rendered
            .find("mtui_probe_ok \"")
            .unwrap_or_else(|| panic!("{name}: nothing ever calls the probe guard: {rendered}"));
        assert!(
            guard_at < first_call,
            "{name}: the guard must be defined before any probe runs, else it is \
             'command not found' (127) and the script carries on to the empty-list \
             skip and exits 0 — the bug it exists to close: {rendered}"
        );
        // `0` is the only status accepted outright.
        assert!(
            rendered.contains("  case \"$2\" in\n  0) return 0 ;;\n"),
            "{name}: the guard must accept `0` and only `0` outright: {rendered}"
        );
        // `106` is accepted *conditionally*, on the issue repo still being
        // listed — the whole of the answer to "a refhost carrying one
        // unrelated broken repo must still patch". Dropping the condition and
        // accepting `106` flat would reopen #447 for a skipped issue repo.
        assert!(
            rendered.contains("  106)\n    if mtui_issue_repo_present; then\n"),
            "{name}: `106` must be accepted only when the issue repo is present: {rendered}"
        );
        assert!(
            rendered.contains(&format!(
                "mtui_issue_repo_present() {{\n  zypper -n lr | awk -F \"|\" '/{repa}\\>/ \
                 {{ found = 1 }} END {{ exit !found }}'\n}}"
            )),
            "{name}: the presence probe must ask `lr` about this update's own repo: {rendered}"
        );
        // Every other status prints the marker and exits with the probe's own
        // status. The marker is split across the format string and its
        // argument so the script's own text does not contain it.
        assert!(
            rendered.contains(
                "  printf 'mtui: could not %s\\n' \"determine what to patch: $1 exited $2\"\n  exit \"$2\"\n}\n"
            ),
            "{name}: an unrecognised probe status must mark and exit: {rendered}"
        );
        // The other half of the split: the check's marker must not appear in
        // the script text, or a transcript echoing the command would report a
        // probe failure on a host that patched fine.
        assert!(
            !rendered.contains(PROBE_FAILURE_MARKER),
            "{name}: the script text must not contain the marker it prints: {rendered}"
        );
        // The patch list is captured before the patch so the empty case is
        // distinguishable — and captured in three steps, as one contiguous
        // block, because each step is load-bearing:
        //
        // * the probe runs **alone** in a command substitution, so the
        //   assignment takes zypper's own status (POSIX XCU 2.9.1). Piped
        //   straight into awk, as it was, the status is the pipeline's — the
        //   *last* stage's — and awk exits 0 on empty input, so zypper's was
        //   not observable at all;
        // * the guard reads that status on the very next line, before
        //   anything else can clobber `$?`;
        // * only then does awk filter the captured rows, through the builtin
        //   `printf` rather than `echo`/`xargs`, so no `ARG_MAX` limit sits
        //   between the probe and the filter;
        // * and awk's own status is guarded in turn — it is the last stage of
        //   that pipeline, so `$?` on the next line is already awk's. An awk
        //   that exits 2 (or is missing from `PATH`) otherwise produces an
        //   empty list, a skipped patch and `exit 0`: #447's verdict verbatim,
        //   reached through the one stage the probe guard does not cover.
        assert!(
            rendered.contains(&format!(
                "mtui_patch_rows=$(zypper -n patches)\nmtui_probe_ok \"zypper -n patches\" $?\nmtui_patches=$(printf '%s\\n' \"$mtui_patch_rows\" | awk -F \"|\" '/{repa}\\>/ {{ print $2; }}')\nmtui_probe_ok \"awk\" $?\n"
            )),
            "{name}: the patch list must be captured, guarded, filtered, guarded: {rendered}"
        );
        // Discriminating: the block above proves the guarded form is present,
        // not that the unguarded one is gone. A template carrying both would
        // satisfy it while the second, unguarded assignment silently won.
        assert!(
            !rendered.contains("mtui_patches=$(zypper"),
            "{name}: no unguarded `zypper … | awk` capture may survive: {rendered}"
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

        /// A `zypper -n patches` row for a **different** update.
        ///
        /// The production shape of "this host carries none of the update's
        /// products": the probe answers `0` and prints a full table, and none
        /// of its rows match `$repa`. An empty *table* (`PATCH_ROW = ""`) is
        /// the rarer case — a host with no patches at all — so pinning only
        /// that one would leave the common no-op unpinned.
        const OTHER_PATCH_ROW: &str =
            "issue-:p=99:1>repo | patch-omega | security | important | --- | needed";

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
        ///
        /// `patches` carries its own exit knob because it is the script's
        /// *probe*: what it returns decides whether the patch runs at all, and
        /// the template must not read its failure as "this host has nothing to
        /// patch". `refresh` carries one too, for the opposite reason — to
        /// show that a failing refresh does **not** abort the update (its
        /// status cannot distinguish a broken issue repo from an unrelated
        /// broken repo; see the module docs).
        const ZYPPER_STUB: &str = r#"#!/bin/sh
printf '%s\n' "$2" >> "$MTUI_STUB_DIR/probe.ran"
case "$2" in
patches)
    if [ -n "$MTUI_STUB_PATCH_ROW" ]; then printf '%s\n' "$MTUI_STUB_PATCH_ROW"; fi
    exit "$MTUI_STUB_PATCHES_EXIT"
    ;;
refresh)
    exit "$MTUI_STUB_REFRESH_EXIT"
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

        /// The knobs one scripted run is driven with.
        ///
        /// A struct rather than six more parameters: `run_script` would
        /// otherwise trip `clippy::too_many_arguments`, and a call site reading
        /// `Knobs { patches_exit: 7, ..Knobs::default() }` names the one thing
        /// the case is about. [`Default`] is the wholly healthy run — every
        /// command exits `0` and [`PATCH_ROW`] matches.
        #[derive(Clone, Copy)]
        struct Knobs {
            /// `zypper -n refresh`'s status. Only `ZYPPER_UPDATE` runs a
            /// refresh at all; on `SLM_UPDATE` this knob has nothing to act
            /// on, which is itself asserted below.
            refresh_exit: i32,
            /// `zypper -n patches`'s status — the probe whose output becomes
            /// the patch list, on both templates.
            patches_exit: i32,
            /// awk's status. Non-zero replaces `awk` on `PATH` with a stub
            /// that consumes stdin and exits with it — the shape of a broken
            /// or missing awk, which is the *other* way the patch list can
            /// come back empty without anything having gone wrong visibly.
            awk_exit: i32,
            /// The patch command's own status.
            patch_exit: i32,
            /// `zypper -n rr`'s status in the repo-cleanup loop.
            cleanup_exit: i32,
            /// The `zypper -n patches` row. Empty means the update matches no
            /// patch on this host.
            patch_row: &'static str,
            /// The `zypper -n lr` row. Empty means the cleanup loop finds
            /// nothing to remove — its body never runs, so the loop's status is
            /// `0`, the masking #400 was about.
            repo_row: &'static str,
        }

        impl Default for Knobs {
            fn default() -> Self {
                Self {
                    refresh_exit: 0,
                    patches_exit: 0,
                    awk_exit: 0,
                    patch_exit: 0,
                    cleanup_exit: 0,
                    patch_row: PATCH_ROW,
                    repo_row: "",
                }
            }
        }

        /// What one scripted run produced.
        struct Ran {
            /// The script's own exit status.
            code: i32,
            /// The script's stdout — where the probe guard's marker line
            /// lands, and so what the update check would classify.
            stdout: String,
        }

        /// Renders `cmds` and runs it under `/bin/sh -c`.
        ///
        /// A non-zero `awk_exit` shadows the real `awk` with a stub for this
        /// run; the stub dir is already first on `PATH`, and `Stubs` is
        /// per-case, so nothing leaks into another run.
        fn run_script(cmds: &ActionCommands, stubs: &Stubs, knobs: Knobs) -> Ran {
            if knobs.awk_exit != 0 {
                write_exe(
                    stubs.dir.path(),
                    "awk",
                    &format!("#!/bin/sh\ncat > /dev/null\nexit {}\n", knobs.awk_exit),
                );
            }
            let rendered = cmds
                .render_command(&vars(REPA, "pkg-a"))
                .expect("safe substitute never fails");
            let out = Command::new("/bin/sh")
                .arg("-c")
                .arg(&rendered)
                .env("PATH", &stubs.path)
                .env("MTUI_STUB_DIR", stubs.dir.path())
                .env("MTUI_STUB_REFRESH_EXIT", knobs.refresh_exit.to_string())
                .env("MTUI_STUB_PATCHES_EXIT", knobs.patches_exit.to_string())
                .env("MTUI_STUB_PATCH_EXIT", knobs.patch_exit.to_string())
                .env("MTUI_STUB_CLEANUP_EXIT", knobs.cleanup_exit.to_string())
                .env("MTUI_STUB_PATCH_ROW", knobs.patch_row)
                .env("MTUI_STUB_REPO_ROW", knobs.repo_row)
                .output()
                .expect("run rendered script under /bin/sh");
            Ran {
                code: out
                    .status
                    .code()
                    .unwrap_or_else(|| panic!("script died on a signal: {:?}", out.status)),
                stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            }
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
                let ran = run_script(
                    &cmds,
                    &stubs,
                    Knobs {
                        patch_exit: 8,
                        ..Knobs::default()
                    },
                );
                assert_eq!(
                    stubs.patch_invocations().len(),
                    1,
                    "{name}: the patch stub must have run, or this test is vacuous"
                );
                assert!(
                    stubs.cleanup_invocations().is_empty(),
                    "{name}: no repo row, so the cleanup loop body must not run"
                );
                assert_eq!(
                    ran.code, 8,
                    "{name}: the script must report the patch's status"
                );
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
                let ran = run_script(
                    &cmds,
                    &stubs,
                    Knobs {
                        cleanup_exit: 3,
                        repo_row: REPO_ROW,
                        ..Knobs::default()
                    },
                );
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
                    ran.code, 0,
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
                let ran = run_script(
                    &cmds,
                    &stubs,
                    Knobs {
                        patch_exit: 8,
                        patch_row: "",
                        ..Knobs::default()
                    },
                );
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
                assert_eq!(ran.code, 0, "{name}: a host with nothing to patch passes");
                // And it must not even *look* like a failure. The probe guard
                // added for #447 sits directly upstream of this skip, and the
                // one thing it must never do is turn "this host carries none
                // of the update's products" into "could not determine what to
                // patch" — the empty list here is the probe's honest answer,
                // reached with a `0` status.
                assert!(
                    !ran.stdout.contains(PROBE_FAILURE_MARKER),
                    "{name}: a legitimately empty patch list is not a probe failure: {}",
                    ran.stdout
                );
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
                let ran = run_script(
                    &cmds,
                    &stubs,
                    Knobs {
                        patch_exit: 8,
                        patch_row: BLANK_PATCH_ROW,
                        ..Knobs::default()
                    },
                );
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
                assert_eq!(ran.code, 0, "{name}: nothing to patch is not a failure");
            }
        }

        #[test]
        fn a_failed_patch_list_probe_fails_the_script_and_skips_the_patch() {
            // Issue #447. `zypper -n patches` is the script's premise: its
            // output *is* the patch list. When it fails — `6` (the host has no
            // repositories at all), `7` (a ZYpp lock), an unreadable rpmdb —
            // the list comes back empty and the guard below skips the patch.
            // Before this, that skip exited `0` and mtui reported an update it
            // never installed.
            //
            // Note what is *not* in that list: an update repo that was simply
            // never added. zypper answers that perfectly well, with a table
            // that lacks this update's rows, so it is indistinguishable from a
            // host carrying none of the update's products — see
            // `a_patch_list_with_no_matching_rows_skips_the_patch_and_succeeds`.
            //
            // The old capture could not even see it: `mtui_patches=$(zypper -n
            // patches | awk …)` takes the *pipeline's* status, which is awk's,
            // and awk exits 0 on empty input.
            for (name, cmds) in templates() {
                let stubs = Stubs::new();
                let ran = run_script(
                    &cmds,
                    &stubs,
                    Knobs {
                        patches_exit: 7,
                        ..Knobs::default()
                    },
                );
                assert!(
                    stubs.probe_invocations().iter().any(|c| c == "patches"),
                    "{name}: the script never reached the stub zypper at all: {:?}",
                    stubs.probe_invocations()
                );
                assert!(
                    stubs.patch_invocations().is_empty(),
                    "{name}: a failed probe must not be followed by a patch: {:?}",
                    stubs.patch_invocations()
                );
                // The signal the check gates on, naming the probe that broke.
                assert!(
                    ran.stdout.contains(&format!(
                        "{PROBE_FAILURE_MARKER}: zypper -n patches exited 7"
                    )),
                    "{name}: the marker must name the failed probe and its status: {}",
                    ran.stdout
                );
                // Pass-through, not a sentinel: a POSIX `exit N` truncates to
                // `0..=255`, so any value of the template's own choosing would
                // sit inside the package manager's space. The marker carries
                // the meaning; the status stays honest.
                assert_eq!(
                    ran.code, 7,
                    "{name}: the script must exit with the probe's own status"
                );
            }
        }

        #[test]
        fn only_a_zero_probe_status_is_accepted() {
            // The probe's success set is `0` **alone**, and deliberately not
            // `classify_exit`'s — which is the set the *patch*'s status is read
            // against. Every member of that set means "the transaction
            // committed", a statement about a command that installs something;
            // as a verdict on a metadata probe they invert. `106`
            // (`ZYPPER_EXIT_INF_REPO_SKIPPED`) is the one that matters: it says
            // a repository was skipped, and if the skipped one is the issue
            // repo then the list legitimately lacks the patch — the silent
            // no-op this guard exists to stop.
            //
            // The whole vocabulary, not a sample: the informational band
            // (`100`-`103`, `106`, `107`) which the patch's classifier passes,
            // the package-not-found set (`104`, `4`, `5`, `8`), the band's
            // boundaries (`99`, `108`), and plain errors. A code that becomes
            // acceptable on a probe has to be argued for here, individually.
            //
            // `106` is in the list and refused *because* `Knobs::default()`
            // leaves `repo_row` empty — the update's repo is not listed, so
            // its conditional acceptance does not apply. Both halves of that
            // condition are pinned at the end of
            // `only_a_zero_probe_status_is_accepted`.
            for code in [
                1, 2, 3, 4, 5, 6, 7, 8, 99, 100, 101, 102, 103, 104, 105, 106, 107, 108, 255,
            ] {
                for (name, cmds) in templates() {
                    let stubs = Stubs::new();
                    let ran = run_script(
                        &cmds,
                        &stubs,
                        Knobs {
                            patches_exit: code,
                            ..Knobs::default()
                        },
                    );
                    assert!(
                        ran.stdout.contains(&format!(
                            "{PROBE_FAILURE_MARKER}: zypper -n patches exited {code}"
                        )),
                        "{name}: probe exit {code} must be refused: {}",
                        ran.stdout
                    );
                    assert!(
                        stubs.patch_invocations().is_empty(),
                        "{name}: probe exit {code} must skip the patch: {:?}",
                        stubs.patch_invocations()
                    );
                    assert_eq!(
                        ran.code, code,
                        "{name}: probe exit {code} must be reported verbatim"
                    );
                }
            }
            // And `0` is accepted, or the loop above proves only that the
            // script always fails.
            // Both directions of the one conditional acceptance.
            //
            // `106` is `ZYPPER_EXIT_INF_REPO_SKIPPED`, which `init_repos()`
            // raises for *any* skipped repository without saying which. A
            // refhost carrying one stale or unreachable repo alongside working
            // ones is routine and patches correctly today, so refusing `106`
            // outright would abort exactly the hosts that leaving
            // `zypper -n refresh` unguarded exists to protect — the two rules
            // would cancel out. So the status is not asked to carry the
            // verdict: the script asks `lr` whether the update's own repo is
            // still listed.
            //
            // Deleting the `106` arm fails the first case; accepting `106`
            // unconditionally fails the second, which is #447 for a skipped
            // issue repo.
            for (name, cmds) in templates() {
                // Repo listed: somebody else's repo was skipped. Patch runs.
                let stubs = Stubs::new();
                let ran = run_script(
                    &cmds,
                    &stubs,
                    Knobs {
                        patches_exit: 106,
                        repo_row: REPO_ROW,
                        ..Knobs::default()
                    },
                );
                assert_eq!(
                    stubs.patch_invocations().len(),
                    1,
                    "{name}: 106 with the update repo listed must still patch: {:?}",
                    stubs.patch_invocations()
                );
                assert!(
                    !ran.stdout.contains(PROBE_FAILURE_MARKER),
                    "{name}: 106 with the update repo listed must not mark a probe failure: {}",
                    ran.stdout
                );
                assert_eq!(ran.code, 0, "{name}: and must not fail the update");

                // Repo absent from `lr`: the skipped one may well have been
                // ours, so the empty list cannot be trusted.
                let stubs = Stubs::new();
                let ran = run_script(
                    &cmds,
                    &stubs,
                    Knobs {
                        patches_exit: 106,
                        repo_row: "",
                        ..Knobs::default()
                    },
                );
                assert!(
                    ran.stdout.contains(&format!(
                        "{PROBE_FAILURE_MARKER}: zypper -n patches exited 106"
                    )),
                    "{name}: 106 without the update repo must be refused: {}",
                    ran.stdout
                );
                assert!(
                    stubs.patch_invocations().is_empty(),
                    "{name}: and must skip the patch: {:?}",
                    stubs.patch_invocations()
                );
                assert_eq!(ran.code, 106, "{name}: reported verbatim");
            }
            for (name, cmds) in templates() {
                let stubs = Stubs::new();
                let ran = run_script(&cmds, &stubs, Knobs::default());
                assert_eq!(
                    stubs.patch_invocations().len(),
                    1,
                    "{name}: a probe that answered must patch: {:?}",
                    stubs.patch_invocations()
                );
                assert!(
                    !ran.stdout.contains(PROBE_FAILURE_MARKER),
                    "{name}: a probe exit 0 is not a probe failure: {}",
                    ran.stdout
                );
            }
        }

        #[test]
        fn a_failed_refresh_does_not_abort_the_update() {
            // `zypper -n refresh` is the other command whose failure can leave
            // the patch out of the metadata, and it is deliberately **not**
            // guarded. Verified against `openSUSE/zypper`
            // `src/commands/repos/refresh.cc:337-345`: the explicit `refresh`
            // command returns `ZYPPER_EXIT_ERR_ZYPP` (`4`) both when every
            // repository failed and when only some did, and never returns
            // `106`. So its status cannot distinguish "the issue repo failed"
            // from "an unrelated stale repo failed" — and a QAM refhost
            // routinely carries the latter. Guarding it would abort updates
            // that are fine.
            //
            // Pinned as a positive: exit 4 from the refresh, and the patch
            // still runs. The case it would have caught is not lost — a
            // wholesale refresh failure resurfaces as a non-zero
            // `zypper -n patches`, which `only_a_zero_probe_status_is_accepted`
            // covers.
            for (name, cmds) in templates() {
                let stubs = Stubs::new();
                let ran = run_script(
                    &cmds,
                    &stubs,
                    Knobs {
                        refresh_exit: 4,
                        ..Knobs::default()
                    },
                );
                assert_eq!(
                    stubs.patch_invocations().len(),
                    1,
                    "{name}: a failed refresh must not stop the patch: {:?}",
                    stubs.patch_invocations()
                );
                assert!(
                    !ran.stdout.contains(PROBE_FAILURE_MARKER),
                    "{name}: a failed refresh is not a probe failure: {}",
                    ran.stdout
                );
                assert_eq!(ran.code, 0, "{name}: the patch's own status is reported");
            }
            // The zypper template really does run a refresh (or the knob above
            // acted on nothing); the slmicro template really does not.
            let stubs = Stubs::new();
            run_script(&zypper(), &stubs, Knobs::default());
            assert!(
                stubs.probe_invocations().iter().any(|c| c == "refresh"),
                "zypper: this template refreshes: {:?}",
                stubs.probe_invocations()
            );
            let stubs = Stubs::new();
            run_script(&slmicro(), &stubs, Knobs::default());
            assert!(
                !stubs.probe_invocations().iter().any(|c| c == "refresh"),
                "slmicro: this template refreshes nothing: {:?}",
                stubs.probe_invocations()
            );
        }

        #[test]
        fn a_broken_awk_fails_the_script_and_skips_the_patch() {
            // The stage that actually *produces* the list, and the one the
            // probe guard alone does not cover. awk is the last stage of the
            // filter pipeline, so an awk that exits 2 — or is missing from
            // `PATH`, which is exit 127 — yields an empty list, a skipped
            // patch and `exit 0`: #447's verdict verbatim, reached through a
            // different command. Its status is free to capture, because `$?`
            // on the line after the pipeline is already awk's.
            for (name, cmds) in templates() {
                let stubs = Stubs::new();
                let ran = run_script(
                    &cmds,
                    &stubs,
                    Knobs {
                        awk_exit: 2,
                        ..Knobs::default()
                    },
                );
                assert!(
                    stubs.probe_invocations().iter().any(|c| c == "patches"),
                    "{name}: the script never reached the stub zypper at all: {:?}",
                    stubs.probe_invocations()
                );
                assert!(
                    stubs.patch_invocations().is_empty(),
                    "{name}: a broken awk must not be followed by a patch: {:?}",
                    stubs.patch_invocations()
                );
                assert!(
                    ran.stdout
                        .contains(&format!("{PROBE_FAILURE_MARKER}: awk exited 2")),
                    "{name}: the marker must name awk: {}",
                    ran.stdout
                );
                assert_eq!(ran.code, 2, "{name}: awk's own status is reported");
            }
        }

        #[test]
        fn a_patch_list_with_no_matching_rows_skips_the_patch_and_succeeds() {
            // The production shape of the legitimate no-op: the probe answers
            // `0` and prints a perfectly good table, and none of its rows carry
            // this update's `$repa`. The other empty case
            // (`an_empty_patch_list_skips_the_patch_and_succeeds`) has the
            // probe print nothing at all, which is the rarer one — a host with
            // no patches whatsoever.
            //
            // This is the case a probe guard is most likely to get wrong, since
            // "the filter matched nothing" and "the filter could not run" reach
            // the same empty variable.
            for (name, cmds) in templates() {
                let stubs = Stubs::new();
                let ran = run_script(
                    &cmds,
                    &stubs,
                    Knobs {
                        patch_exit: 8,
                        patch_row: OTHER_PATCH_ROW,
                        ..Knobs::default()
                    },
                );
                assert!(
                    stubs.probe_invocations().iter().any(|c| c == "patches"),
                    "{name}: the script never reached the stub zypper at all: {:?}",
                    stubs.probe_invocations()
                );
                assert!(
                    stubs.patch_invocations().is_empty(),
                    "{name}: no row matches this update, so no patch may run: {:?}",
                    stubs.patch_invocations()
                );
                assert!(
                    !ran.stdout.contains(PROBE_FAILURE_MARKER),
                    "{name}: a host carrying another update's patches is a no-op, \
                     not a probe failure: {}",
                    ran.stdout
                );
                assert_eq!(
                    ran.code, 0,
                    "{name}: a host with nothing of ours to patch passes"
                );
            }
        }
    }
}
