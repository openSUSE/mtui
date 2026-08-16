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
//!
//! # The probe guard
//!
//! `LIST_COMMAND`'s output *is* the downgrade: every package the flow rolls
//! back is one this probe resolved a released version for, so a probe that
//! fails silently is a rollback that runs zero commands and reports success
//! ([`crate::reports::update_flow::perform_downgrade`] would find an empty
//! version map and skip every package). That is issue #451 — the same defect
//! [`crate::update_workflow::actions::update`] carries as #447, in the path
//! that matters most, since the rollback runs *after* an update has already
//! failed on the group.
//!
//! It was not merely a discarded status. The probe was one pipeline, and POSIX
//! defines a pipeline's status as its **last** stage's: awk's, and awk exits
//! `0` after a successful run over empty input. A failed `zypper se` — a ZYpp
//! lock, broken repo metadata, an unreadable rpmdb, zypper missing — was
//! therefore not observable at all, and `dead_probes`' `lastexit != 0` gate
//! could only ever catch SSH-level death.
//!
//! The technique is the update templates', and the argument for each step is
//! made in full in that module's docs (§ "The probe guard"); only what differs
//! is restated here. In outline: each producing stage is captured on its own
//! line so an assignment's status is the substitution's (POSIX XCU 2.9.1), the
//! rows reach the filter through the **builtin** `printf '%s\n'` so no
//! `ARG_MAX` limit sits between probe and filter, `mtui_probe_ok` prints a
//! start-of-line marker and exits with **the tool's own status** (never an
//! invented sentinel — a POSIX `exit` truncates to `0..=255`, so any chosen
//! value would collide with zypper's own space), and the marker is assembled
//! from `printf`'s format string *and* its argument so the script's own text
//! never carries the sentence it prints.
//!
//! awk is guarded too, for the same reason it is in the update templates: it is
//! the stage that actually produces the list, and `$?` after the filter
//! pipeline is already awk's, so the capture is free.
//!
//! ## Why the accepted set is `0`, `104` and `106`
//!
//! `104` is `ZYPPER_EXIT_INF_CAP_NOT_FOUND`, which `zypper search` sets when
//! its result table comes back empty. That is the legitimate "this host carries
//! none of these packages", routine in a mixed group and a no-op the flow has
//! always treated as one. Refusing it would fail the rollback on every host the
//! update never touched.
//!
//! `106` is `ZYPPER_EXIT_INF_REPO_SKIPPED`, accepted on the fleet argument
//! `a249a00d` made for the update path: `init_repos()` raises it for **any**
//! skipped repository and cannot say which, a refhost routinely carries one
//! stale or unreachable repo alongside working ones, and here the run *is* the
//! recovery from a failed update — aborting it on hosts that would have rolled
//! back correctly is the more expensive mistake. The update templates answer
//! `106` by asking whether the update's own repo survived; that question cannot
//! be asked here, and not for want of trying: `downgrade_body` removes the issue
//! repository from every host on purpose before the probe runs, so its absence
//! is the expected state. The released versions the rollback wants come from the
//! host's ordinary repositories.
//!
//! **The residual that leaves, named rather than hidden:** if the skipped
//! repository is the one carrying the released versions, the list is silently
//! short and those packages stay at the update version. On the rollback path
//! that is caught downstream — `downgrade_verdict` compares each package
//! against the update's `required` version and fails the command — but on a
//! standalone `downgrade` with no required versions loaded, it is not. A status
//! cannot carry that verdict; only a comparison against what should be
//! installed can, which is what the transcript note exists to prompt.
//!
//! ## The notes share a stream with the parser
//!
//! Both accepted-status notes print to **stdout** — the same stream
//! [`crate::reports::update_flow::parse_downgrade_versions`] reads back as the
//! version list. That parser takes *any* line containing a space-equals-space
//! as one `name`/`version` row, so the notes are safe only because neither
//! contains that separator. It is a real coupling between a shell string here
//! and a Rust parser two modules away: rewording a note into, say, "no package
//! = no rollback" would inject a bogus package into the version map and make
//! the flow try to downgrade it. A short comment at the `printf` sites names
//! the constraint, and
//! `rendered_script::the_probe_notes_never_parse_as_versions` pins it against
//! the real script's real output.
//!
//! Routing the notes to stderr instead is not the free escape it looks. The
//! flow's general rule for judging a fan-out is
//! [`crate::reports::update_flow`]'s `host_command_failures`, which counts
//! *any* stderr as a failure — it is what judges the repo removal that runs
//! immediately before this probe. The probe step itself is the exception,
//! gated on the exit status alone (`dead_probes`), so a note on stderr would
//! survive *today*; but that is one call site's choice, not a property of the
//! flow, and the arm exists precisely to keep `104` from failing the command.
//! Putting the note one snapshot field away from the rule that would refuse it
//! buys nothing a comment and a test do not.
//!
//! ## What is not guarded
//!
//! The middle of the filter chain. `grep`, `sed` or a missing binary among them
//! yields an empty list at awk's `0`, exactly as before — awk is the last stage
//! and succeeds on no input. That is the same accepted residual the update
//! templates carry, and closing it needs `pipefail` (not POSIX) or
//! `${PIPESTATUS[@]}` (bash-only), while these scripts must run under the
//! `/bin/sh` a refhost provides.

use crate::update_workflow::actions::{ActionCommands, SubstMode};

/// zypper/slmicro downgrade list command, shared by both.
///
/// One `zypper -n se -s` invocation for the whole package list: a per-package `for p in $packages; do zypper ... $$p; done` loop
/// loads the repo metadata once per package and, piped through a block-buffered
/// awk with no PTY, emits nothing until the last iteration — on a slow host a
/// long package list blows the SSH no-output timeout (`connection_timeout`,
/// default 300s) and the probe dies with no versions resolved.
///
/// See the module docs for why the two commands that produce the list are
/// guarded, why the guard exits with the tool's own status, and why `104` and
/// `106` are accepted while everything else is refused.
///
/// Every `$` meant for the remote shell is written `$$` (`$$?`, `$$(`, `$$1`,
/// `$$2`, `$$mtui_rows`); the awk field refs (`$2`, `$4`) stay bare, which is
/// the tested convention for [`SubstMode::Safe`]. Shell variables are
/// `mtui_`-prefixed *and* `$$`-escaped, so no future `$name` template variable
/// can silently expand one away.
const LIST_COMMAND: &str = r#"
mtui_probe_ok() {
  case "$$2" in
  0) return 0 ;;
  # Both notes print to stdout, which parse_downgrade_versions also reads. It
  # splits on space-equals-space, so no note may ever contain that separator.
  104)
    printf 'mtui: note: %s exited 104, no package matched; nothing to downgrade here\n' "$$1"
    return 0
    ;;
  106)
    printf 'mtui: note: %s exited 106, a repository was skipped; the version list may be short\n' "$$1"
    return 0
    ;;
  esac
  printf 'mtui: could not %s\n' "determine what to downgrade: $$1 exited $$2"
  exit "$$2"
}
mtui_rows=$$(zypper -n se -s --match-exact -t package $packages)
mtui_probe_ok "zypper -n se" $$?
mtui_versions=$$(printf '%s\n' "$$mtui_rows" | grep -v "(System" | grep ^[iv] | sed "s, ,,g" | awk -F "|" '{ print $2,"=",$4 }')
mtui_probe_ok "awk" $$?
printf '%s\n' "$$mtui_versions"
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
        // Exactly one zypper invocation, over the whole list. Counted on the
        // whole invocation rather than the bare token `zypper`, which the probe
        // guard's own label ("zypper -n se") also carries.
        assert_eq!(
            listed
                .matches("zypper -n se -s --match-exact -t package")
                .count(),
            1,
            "{listed}"
        );
        assert!(
            listed.contains("zypper -n se -s --match-exact -t package a b c"),
            "{listed}"
        );
        // awk field refs preserved.
        assert!(listed.contains("{ print $2,\"=\",$4 }"));
        // Discriminating: the doubled form contains the single-brace substring.
        assert!(!listed.contains("{{"), "{listed}");
        assert_the_probe_is_guarded("zypper", &listed, "a b c");
    }

    #[test]
    fn slmicro_list_command_probes_all_packages_in_one_call() {
        // The slmicro probe is the same single-invocation shape as zypper's.
        let cmds = downgrader("slmicro", true).unwrap();
        let vars: HashMap<&str, &str> = [("packages", "a b")].into_iter().collect();
        let listed = cmds.render_list_command(&vars).unwrap().unwrap();
        assert!(!listed.contains("for p in"), "{listed}");
        assert_eq!(
            listed
                .matches("zypper -n se -s --match-exact -t package")
                .count(),
            1,
            "{listed}"
        );
        assert!(
            listed.contains("zypper -n se -s --match-exact -t package a b"),
            "{listed}"
        );
        assert_the_probe_is_guarded("slmicro", &listed, "a b");
    }

    /// The `$$`-escaping and probe-guard contract, asserted on the **rendered**
    /// text rather than the source constant — a `$` that was meant for the
    /// remote shell but written bare still *looks* right in the source, and a
    /// mangled escape would render a broken script that no
    /// `MockConnection`-level test could notice.
    ///
    /// Pinned by shape rather than as one literal blob, so that editing the
    /// guard's prose does not fail this while a semantic change slips through
    /// it. Each assertion is a separate way the guard could stop guarding.
    fn assert_the_probe_is_guarded(name: &str, listed: &str, packages: &str) {
        // The single discriminating check on the escaping: after substitution
        // no `$$` may survive. Every `$$` here is an escape for the remote
        // shell, so one left doubled is one the shell would read as its own PID.
        assert!(
            !listed.contains("$$"),
            "{name}: an unresolved `$$` reached the remote shell: {listed}"
        );
        // Without the definition, `mtui_probe_ok …` is "command not found"
        // (127 on stderr) and the script carries on to emit an empty list at
        // exit 0 — the bug it exists to close.
        let guard_at = listed
            .find("mtui_probe_ok() {")
            .unwrap_or_else(|| panic!("{name}: the probe guard is not defined at all: {listed}"));
        let first_call = listed
            .find("mtui_probe_ok \"")
            .unwrap_or_else(|| panic!("{name}: nothing ever calls the probe guard: {listed}"));
        assert!(
            guard_at < first_call,
            "{name}: the guard must be defined before any probe runs: {listed}"
        );
        // `0` is the only status accepted without a word in the transcript.
        assert!(
            listed.contains("  case \"$2\" in\n  0) return 0 ;;\n"),
            "{name}: the guard must accept `0` outright: {listed}"
        );
        // `104` — `zypper search`'s empty result table. Dropping this arm turns
        // every host in a mixed group that carries none of the packages into a
        // failed rollback.
        assert!(
            listed.contains("  104)\n    printf 'mtui: note: %s exited 104,"),
            "{name}: `104` must stay the legitimate no-op: {listed}"
        );
        // `106` — any skipped repository, which `init_repos()` cannot name.
        // Dropping this arm aborts the recovery on refhosts carrying one stale
        // repo alongside working ones.
        assert!(
            listed.contains("  106)\n    printf 'mtui: note: %s exited 106,"),
            "{name}: `106` must be noted, not refused: {listed}"
        );
        // Both notes ride the stream `parse_downgrade_versions` reads, and are
        // safe only because neither carries the separator it splits on. That
        // constraint lives two modules away, so the script names it where the
        // notes are written; `the_probe_notes_never_parse_as_versions` pins the
        // property itself.
        assert!(
            listed.contains("parse_downgrade_versions"),
            "{name}: the notes must name the parser whose stream they share: {listed}"
        );
        // Every other status marks and exits with the probe's own status. The
        // marker is split across the format string and its argument so the
        // script's own text does not contain it.
        assert!(
            listed.contains(
                "  printf 'mtui: could not %s\\n' \"determine what to downgrade: $1 exited $2\"\n  exit \"$2\"\n}\n"
            ),
            "{name}: an unrecognised probe status must mark and exit: {listed}"
        );
        assert!(
            !listed.contains("mtui: could not determine what to downgrade"),
            "{name}: the script text must not contain the marker it prints, or a \
             transcript echoing the command would read as a probe failure: {listed}"
        );
        // The list is produced in four steps, as one contiguous block, because
        // each step is load-bearing:
        //
        // * the probe runs **alone** in a command substitution, so the
        //   assignment takes zypper's own status (POSIX XCU 2.9.1). Piped
        //   straight into the filter, as it was, the status is the pipeline's —
        //   the *last* stage's — and awk exits 0 on empty input, so zypper's
        //   was not observable at all;
        // * the guard reads that status on the very next line, before anything
        //   else can clobber `$?`;
        // * only then do the filters run, fed by the builtin `printf` rather
        //   than `echo`/`xargs`, so no `ARG_MAX` limit sits between the probe
        //   and the filter;
        // * and awk's own status is guarded in turn — it is the last stage of
        //   that pipeline, so `$?` on the next line is already awk's.
        assert!(
            listed.contains(&format!(
                "mtui_rows=$(zypper -n se -s --match-exact -t package {packages})\n\
                 mtui_probe_ok \"zypper -n se\" $?\n\
                 mtui_versions=$(printf '%s\\n' \"$mtui_rows\" | grep -v \"(System\" | \
                 grep ^[iv] | sed \"s, ,,g\" | awk -F \"|\" '{{ print $2,\"=\",$4 }}')\n\
                 mtui_probe_ok \"awk\" $?\n"
            )),
            "{name}: the version list must be captured, guarded, filtered, guarded: {listed}"
        );
        // Discriminating: the block above proves the guarded form is present,
        // not that an unguarded one is gone. A template carrying both would
        // satisfy it while the second, unguarded capture silently won.
        assert_eq!(
            listed.matches("awk -F \"|\"").count(),
            1,
            "{name}: exactly one awk may filter the rows: {listed}"
        );
        // And the captured list must actually be emitted, as the last thing the
        // script does: capturing the rows and forgetting to re-emit them would
        // hand the flow empty stdout at exit 0 — this fix reintroducing its own
        // bug.
        assert!(
            listed.ends_with("printf '%s\\n' \"$mtui_versions\"\n"),
            "{name}: the script must end by emitting the version list: {listed}"
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

    /// Executes the *rendered* `LIST_COMMAND` under a real `/bin/sh`, with a
    /// stub `zypper` first on `PATH`.
    ///
    /// The subject here is shell semantics — *which command's status the script
    /// reports* — and no mock can express it: `MockConnection` keys on the whole
    /// command string and scripts one exit code per command, so "the probe
    /// failed but the last stage of the pipeline succeeded" is a state it cannot
    /// produce. That state is issue #451 in one sentence, which is why these
    /// run a shell.
    ///
    /// The stub appends to a sentinel file on every invocation, so a case whose
    /// headline assertion is a *negative* ("no version line was emitted") can
    /// also show the script reached the stub at all — otherwise a `PATH` mistake
    /// would make every assertion here vacuous.
    #[cfg(unix)]
    mod rendered_script {
        use std::ffi::OsString;
        use std::fs;
        use std::os::unix::fs::PermissionsExt;
        use std::path::Path;
        use std::process::Command;

        use super::*;
        use crate::reports::update_flow::parse_downgrade_versions;

        /// The start of the line a failed probe prints.
        ///
        /// Matched at the **start of a line**, never anywhere in the stream: the
        /// script's own text does not carry this sentence (it is split across
        /// `printf`'s format string and its argument), but nothing stops a host
        /// echoing it mid-line — a `set -x` trace, a log line quoting an earlier
        /// run. Only a line that begins with it is mtui's own verdict.
        const PROBE_MARKER: &str = "mtui: could not determine what to downgrade";

        /// A `zypper -n se -s` row for an **installed** package, in the real
        /// table's shape: status, name, type, version, arch, repository.
        ///
        /// The filter chain turns it into the `pkg-a = 1.0-1` line
        /// `parse_downgrade_versions` splits on `" = "`.
        const INSTALLED_ROW: &str = "i | pkg-a | package | 1.0-1 | x86_64 | test-repo";

        /// A table every row of which the filter chain drops: one whose
        /// repository is `(System Packages)` — an installed package no
        /// repository offers, so there is no released version to go back to,
        /// dropped by `grep -v "(System"` — and one for a package that is not
        /// installed (blank status column), dropped by `grep ^[iv]`.
        const FILTERED_ROWS: &str = "i | pkg-a | package | 1.0-1 | x86_64 | (System Packages)\n  \
                                     | pkg-a | package | 2.0-1 | x86_64 | test-repo";

        /// What `zypper se` prints on the run that exits `104`.
        const NO_MATCH_STDOUT: &str = "No matching items found.";

        /// The stub `zypper`. Every call the probe makes is
        /// `zypper -n se …`, so `$2` discriminates — and the `esac` fallthrough
        /// keeps any future subcommand from silently exiting non-zero.
        ///
        /// Every arm records its invocation, which is the liveness signal the
        /// negative assertions rest on.
        const ZYPPER_STUB: &str = r#"#!/bin/sh
printf '%s\n' "$2" >> "$MTUI_STUB_DIR/probe.ran"
case "$2" in
se)
    if [ -n "$MTUI_STUB_SE_ROWS" ]; then printf '%s\n' "$MTUI_STUB_SE_ROWS"; fi
    exit "$MTUI_STUB_SE_EXIT"
    ;;
esac
exit 0
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
                // Keep the inherited PATH behind the stubs: the template also
                // calls the real `grep`, `sed` and `awk`.
                let mut path = dir.path().as_os_str().to_owned();
                if let Some(inherited) = std::env::var_os("PATH") {
                    path.push(":");
                    path.push(inherited);
                }
                Self { dir, path }
            }

            /// Every `zypper` subcommand the stub saw, in order.
            fn probe_invocations(&self) -> Vec<String> {
                fs::read_to_string(self.dir.path().join("probe.ran"))
                    .unwrap_or_default()
                    .lines()
                    .map(ToOwned::to_owned)
                    .collect()
            }
        }

        fn write_exe(dir: &Path, name: &str, body: &str) {
            let p = dir.join(name);
            fs::write(&p, body).expect("write stub");
            fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).expect("chmod stub");
        }

        /// The two downgraders that share `LIST_COMMAND`.
        ///
        /// Both are run through every case: the constant is shared today, and
        /// pinning both is what notices a future divergence that guards only one.
        fn downgraders() -> Vec<(&'static str, ActionCommands)> {
            vec![
                (
                    "zypper",
                    downgrader("15", false).expect("zypper downgrader"),
                ),
                (
                    "slmicro",
                    downgrader("slmicro", true).expect("slmicro downgrader"),
                ),
            ]
        }

        /// The knobs one scripted run is driven with. [`Default`] is the wholly
        /// healthy probe: one installed row, everything exits `0`.
        #[derive(Clone, Copy)]
        struct Knobs {
            /// `zypper -n se`'s status — the probe whose output *is* the version
            /// list.
            se_exit: i32,
            /// awk's status. Non-zero shadows `awk` on `PATH` with a stub that
            /// consumes stdin and exits with it — a broken or missing awk, the
            /// other way the list comes back empty with nothing visibly wrong.
            awk_exit: i32,
            /// The `zypper -n se -s` table, one row per line.
            rows: &'static str,
        }

        impl Default for Knobs {
            fn default() -> Self {
                Self {
                    se_exit: 0,
                    awk_exit: 0,
                    rows: INSTALLED_ROW,
                }
            }
        }

        /// What one scripted run produced.
        struct Ran {
            /// The script's own exit status — what the flow records as
            /// `Target::lastexit` and `dead_probes` filters on.
            code: i32,
            /// The script's stdout — what the flow parses into the version map,
            /// and what `show_log` shows the operator.
            stdout: String,
        }

        impl Ran {
            /// The probe-failure marker line, if the script printed one at the
            /// start of a line.
            fn marker(&self) -> Option<&str> {
                self.stdout
                    .lines()
                    .find(|l| l.starts_with(PROBE_MARKER))
                    .map(str::trim_end)
            }

            /// The `name = version` lines the flow would parse
            /// (`parse_downgrade_versions` splits on `" = "`).
            fn versions(&self) -> Vec<&str> {
                self.stdout.lines().filter(|l| l.contains(" = ")).collect()
            }
        }

        /// Renders `cmds`'s list command and runs it under `/bin/sh -c`.
        ///
        /// A non-zero `awk_exit` shadows the real `awk` for this run only; the
        /// stub dir is already first on `PATH`, and `Stubs` is per-case, so
        /// nothing leaks between runs and no serialisation is needed.
        fn run_script(cmds: &ActionCommands, stubs: &Stubs, knobs: Knobs) -> Ran {
            if knobs.awk_exit != 0 {
                write_exe(
                    stubs.dir.path(),
                    "awk",
                    &format!("#!/bin/sh\ncat > /dev/null\nexit {}\n", knobs.awk_exit),
                );
            }
            let vars: HashMap<&str, &str> = [("packages", "pkg-a")].into_iter().collect();
            let rendered = cmds
                .render_list_command(&vars)
                .expect("safe substitute never fails")
                .expect("both downgraders carry a list command");
            let out = Command::new("/bin/sh")
                .arg("-c")
                .arg(&rendered)
                .env("PATH", &stubs.path)
                .env("MTUI_STUB_DIR", stubs.dir.path())
                .env("MTUI_STUB_SE_EXIT", knobs.se_exit.to_string())
                .env("MTUI_STUB_SE_ROWS", knobs.rows)
                .output()
                .expect("run the rendered list command under /bin/sh");
            Ran {
                code: out
                    .status
                    .code()
                    .unwrap_or_else(|| panic!("script died on a signal: {:?}", out.status)),
                stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            }
        }

        #[test]
        fn a_failed_se_probe_fails_the_script_and_emits_no_versions() {
            // Issue #451. `zypper -n se -s` is the probe whose output *is* the
            // downgrade version list. When it fails — `7` on a locked ZYpp
            // stack, `6` with no repositories, an unreadable rpmdb, zypper
            // missing entirely — the list comes back empty. As a pipeline the
            // probe's status was not merely discarded but unobservable: the
            // pipeline reports its last stage's status, that stage is awk, and
            // awk exits `0` on empty input. So the flow recorded exit `0`, the
            // host never entered `dead_probes`, no downgrade command was built
            // for it, and the rollback reported success having reverted nothing.
            for (name, cmds) in downgraders() {
                let stubs = Stubs::new();
                let ran = run_script(
                    &cmds,
                    &stubs,
                    Knobs {
                        se_exit: 7,
                        ..Knobs::default()
                    },
                );
                // Liveness first: the negative below is also what an empty
                // `PATH` produces.
                assert!(
                    stubs.probe_invocations().iter().any(|c| c == "se"),
                    "{name}: the script never reached the stub zypper at all: {:?}",
                    stubs.probe_invocations()
                );
                assert_eq!(
                    ran.marker(),
                    Some(format!("{PROBE_MARKER}: zypper -n se exited 7").as_str()),
                    "{name}: the marker must start a line and name the probe and its \
                     status: {}",
                    ran.stdout
                );
                assert!(
                    ran.versions().is_empty(),
                    "{name}: a failed probe must emit no version line: {:?}",
                    ran.versions()
                );
                // Pass-through, not a sentinel: a POSIX `exit N` truncates to
                // `0..=255`, so any value of the template's own choosing would
                // sit inside zypper's space. The marker carries the meaning; the
                // status stays honest — and non-zero is what `dead_probes`
                // filters on.
                assert_eq!(
                    ran.code, 7,
                    "{name}: the script must exit with the probe's own status"
                );
            }
        }

        #[test]
        fn only_zero_104_and_106_are_accepted_by_the_downgrade_probe() {
            // The whole vocabulary, not a sample: zypper's informational band
            // (`100`-`103`, `105`-`107`), the band's boundaries (`99`, `108`),
            // the package-not-found neighbours (`4`, `5`, `8`) and plain errors.
            // A code that becomes acceptable on this probe has to be argued for
            // here, individually.
            //
            // `107` (`ZYPPER_EXIT_INF_RPM_SCRIPT_FAILED`) is refused despite
            // being a success for a *transaction*: that argument is "the
            // transaction committed", a claim about a command that installs
            // something. `se` installs nothing, so a `107` from it is not a
            // partial success but a status this probe cannot account for.
            for code in [
                1, 2, 3, 4, 5, 6, 7, 8, 99, 100, 101, 102, 103, 105, 107, 108, 255,
            ] {
                for (name, cmds) in downgraders() {
                    let stubs = Stubs::new();
                    let ran = run_script(
                        &cmds,
                        &stubs,
                        Knobs {
                            se_exit: code,
                            ..Knobs::default()
                        },
                    );
                    assert_eq!(
                        ran.marker(),
                        Some(format!("{PROBE_MARKER}: zypper -n se exited {code}").as_str()),
                        "{name}: probe exit {code} must be refused: {}",
                        ran.stdout
                    );
                    assert!(
                        ran.versions().is_empty(),
                        "{name}: probe exit {code} must emit no version line: {:?}",
                        ran.versions()
                    );
                    assert_eq!(
                        ran.code, code,
                        "{name}: probe exit {code} must be reported verbatim"
                    );
                }
            }
            // And the three accepted codes, or the loop above proves only that
            // the script always fails.
            //
            // `104` is `ZYPPER_EXIT_INF_CAP_NOT_FOUND`: `zypper se` sets it when
            // the result table is empty, which is the legitimate "this host
            // carries none of these packages" — routine in a mixed group, and it
            // must stay the no-op it has always been.
            //
            // `106` is `ZYPPER_EXIT_INF_REPO_SKIPPED`, raised by `init_repos()`
            // for *any* skipped repository without saying which. A refhost
            // routinely carries one stale or unreachable repo alongside working
            // ones; refusing `106` would abort the recovery on hosts that would
            // have rolled back correctly — and here the rollback is the repair
            // for an update that already failed, so the fleet would pay for it.
            for (name, cmds) in downgraders() {
                for code in [0, 104, 106] {
                    let stubs = Stubs::new();
                    let ran = run_script(
                        &cmds,
                        &stubs,
                        Knobs {
                            se_exit: code,
                            ..Knobs::default()
                        },
                    );
                    assert_eq!(
                        ran.marker(),
                        None,
                        "{name}: probe exit {code} must be accepted: {}",
                        ran.stdout
                    );
                    assert_eq!(
                        ran.code, 0,
                        "{name}: probe exit {code} must not fail the probe: {}",
                        ran.stdout
                    );
                }
            }
        }

        #[test]
        fn a_broken_awk_fails_the_downgrade_probe() {
            // awk is the stage that actually *produces* the version list, and
            // the one guarding `zypper se` alone does not cover: an awk that
            // exits `2`, or is missing from `PATH` (`127`), yields an empty list
            // at status `0` — #451's verdict verbatim, reached through a
            // different command. Its status is free to read, because `$?` after
            // the filter pipeline is already awk's.
            for (name, cmds) in downgraders() {
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
                    stubs.probe_invocations().iter().any(|c| c == "se"),
                    "{name}: the script never reached the stub zypper at all: {:?}",
                    stubs.probe_invocations()
                );
                assert_eq!(
                    ran.marker(),
                    Some(format!("{PROBE_MARKER}: awk exited 2").as_str()),
                    "{name}: the marker must name awk: {}",
                    ran.stdout
                );
                assert_eq!(ran.code, 2, "{name}: awk's own status is reported");
            }
        }

        #[test]
        fn a_healthy_probe_emits_the_parsable_version_list() {
            // The positive contract the whole flow rests on: one `name = version`
            // line per installed package, in the shape
            // `parse_downgrade_versions` splits on. Capturing the rows into a
            // variable and forgetting to re-emit them would give the flow empty
            // stdout at exit `0` — the restructure's own way of reintroducing
            // exactly the bug it fixes.
            for (name, cmds) in downgraders() {
                let stubs = Stubs::new();
                let ran = run_script(&cmds, &stubs, Knobs::default());
                assert_eq!(
                    ran.versions(),
                    vec!["pkg-a = 1.0-1"],
                    "{name}: the probe must emit the parsable version list: {}",
                    ran.stdout
                );
                assert_eq!(ran.marker(), None, "{name}: {}", ran.stdout);
                assert_eq!(ran.code, 0, "{name}: a healthy probe succeeds");
            }
        }

        #[test]
        fn a_host_with_none_of_the_packages_stays_a_noop() {
            // `zypper search` sets `ZYPPER_EXIT_INF_CAP_NOT_FOUND` (`104`) when
            // its result table is empty. In a mixed group that is simply a host
            // carrying none of the update's packages — there is nothing to roll
            // back on it, and refusing `104` would turn every such host into a
            // failed rollback.
            for (name, cmds) in downgraders() {
                let stubs = Stubs::new();
                let ran = run_script(
                    &cmds,
                    &stubs,
                    Knobs {
                        se_exit: 104,
                        rows: NO_MATCH_STDOUT,
                        ..Knobs::default()
                    },
                );
                assert!(
                    stubs.probe_invocations().iter().any(|c| c == "se"),
                    "{name}: the script never reached the stub zypper at all: {:?}",
                    stubs.probe_invocations()
                );
                assert_eq!(
                    ran.marker(),
                    None,
                    "{name}: a host carrying none of the packages is not a probe \
                     failure: {}",
                    ran.stdout
                );
                assert!(
                    ran.versions().is_empty(),
                    "{name}: no package matched, so no version line: {:?}",
                    ran.versions()
                );
                assert_eq!(ran.code, 0, "{name}: and the no-op must not fail");
            }
        }

        #[test]
        fn the_probe_notes_never_parse_as_versions() {
            // Everything this template prints goes to **stdout**, and stdout is
            // what `parse_downgrade_versions` reads back as the version list —
            // it takes any line carrying the separator as one `name`/`version`
            // row. So the three notes are safe only for as long as none of them
            // contains it, and a reword ("... exited 104, no package = no
            // rollback") would quietly add a package the flow then tries to
            // downgrade. That coupling is between a shell string here and a
            // parser two modules away, which is why it is pinned with the real
            // parser rather than a local re-implementation of its rule.
            //
            // Each case is the real script's real stdout, so the note text is
            // never copied into this test — a reword is caught because it is
            // *parsed*, not because it stopped matching a literal.
            for (name, cmds) in downgraders() {
                for (case, knobs, expected) in [
                    // A refused status: only the failure marker on stdout.
                    (
                        "a failed probe",
                        Knobs {
                            se_exit: 7,
                            rows: "",
                            ..Knobs::default()
                        },
                        Vec::new(),
                    ),
                    // `104`: the note, and nothing else.
                    (
                        "the 104 note",
                        Knobs {
                            se_exit: 104,
                            rows: NO_MATCH_STDOUT,
                            ..Knobs::default()
                        },
                        Vec::new(),
                    ),
                    // `106`: the note *alongside* a real version line, which is
                    // the realistic shape (a repo was skipped, the survivors
                    // still answered). Discriminating in a way the note-only
                    // cases are not: a note that parsed would show up as a
                    // second entry rather than as the difference between empty
                    // and non-empty.
                    (
                        "the 106 note beside a real row",
                        Knobs {
                            se_exit: 106,
                            ..Knobs::default()
                        },
                        vec![("pkg-a".to_owned(), "1.0-1".to_owned())],
                    ),
                ] {
                    let stubs = Stubs::new();
                    let ran = run_script(&cmds, &stubs, knobs);
                    // Anti-vacuity: without a note on stdout every assertion
                    // below holds for the wrong reason. Matched on the prefix
                    // the script prints, not on the wording it is free to
                    // change.
                    assert!(
                        ran.stdout
                            .lines()
                            .any(|l| l.starts_with("mtui: note: ") || l.starts_with(PROBE_MARKER)),
                        "{name}/{case}: the script printed no note at all, so nothing is \
                         being tested: {}",
                        ran.stdout
                    );
                    let parsed = parse_downgrade_versions(&ran.stdout);
                    let mut got: Vec<(String, String)> = parsed.into_iter().collect();
                    got.sort();
                    assert_eq!(
                        got, expected,
                        "{name}/{case}: the notes must contribute no package to the version \
                         map: {}",
                        ran.stdout
                    );
                }
            }
        }

        #[test]
        fn rows_filtered_to_nothing_is_a_noop() {
            // The probe answers `0` with a perfectly good table whose every row
            // the filter chain drops: `(System Packages)` (installed, but no
            // repository offers it — nothing to go back to) and a not-installed
            // row (blank status column). This pins the chain surviving the move
            // into a `printf`-fed command substitution — a quoting mistake there
            // would either break the filtering or make awk fail, and this is the
            // case a probe guard is most likely to get wrong, since "the filter
            // matched nothing" and "the filter could not run" reach the same
            // empty variable.
            for (name, cmds) in downgraders() {
                let stubs = Stubs::new();
                let ran = run_script(
                    &cmds,
                    &stubs,
                    Knobs {
                        rows: FILTERED_ROWS,
                        ..Knobs::default()
                    },
                );
                assert!(
                    stubs.probe_invocations().iter().any(|c| c == "se"),
                    "{name}: the script never reached the stub zypper at all: {:?}",
                    stubs.probe_invocations()
                );
                assert_eq!(
                    ran.marker(),
                    None,
                    "{name}: rows filtered to nothing is not a probe failure: {}",
                    ran.stdout
                );
                assert!(
                    ran.versions().is_empty(),
                    "{name}: every row is filtered, so no version line: {:?}",
                    ran.versions()
                );
                assert_eq!(ran.code, 0, "{name}: and it must not fail");
            }
        }
    }
}
