//! Action command tables: the per-`(release, transactional)` command templates
//! for install / uninstall / update / prepare / downgrade.
//!
//! Each action's templates are keyed by `(release, transactional)` and resolved
//! through `substitute`, preserving `$$` escaping and `${}` bracing (see
//! [`template`] docs). The value type is [`ActionCommands`]; not every action
//! uses every field — `installed_only` is prepare-only, `list_command`
//! downgrade-only, `reboot` transactional-only — so absent templates are `None`.
//!
//! [`template`]: crate::update_workflow::template

pub(crate) mod downgrade;
pub(crate) mod install;
pub(crate) mod prepare;
pub(crate) mod uninstall;
pub(crate) mod update;

use std::collections::HashMap;

use crate::update_workflow::template::{TemplateError, safe_substitute, substitute};

/// Whether a template is rendered with strict `substitute` or lenient
/// `safe_substitute` semantics.
///
/// `install` / `uninstall` / `prepare` use [`Strict`](SubstMode::Strict);
/// `update` / `downgrade` use [`Safe`](SubstMode::Safe) because their templates
/// embed shell/awk `$`-tokens that must survive unresolved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubstMode {
    /// Raise on a missing key / malformed placeholder.
    Strict,
    /// Leave a missing key / malformed placeholder verbatim.
    Safe,
}

impl SubstMode {
    /// Renders `template` with `vars` under this mode.
    fn render(self, template: &str, vars: &HashMap<&str, &str>) -> Result<String, TemplateError> {
        match self {
            SubstMode::Strict => substitute(template, vars),
            SubstMode::Safe => Ok(safe_substitute(template, vars)),
        }
    }
}

/// The command templates for one resolved action.
///
/// `command` is always present; the remaining fields are used only by specific
/// actions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActionCommands {
    /// The primary command template, always present.
    command: String,
    /// The transactional reboot template; present only for transactional
    /// (`slmicro`) entries.
    reboot: Option<String>,
    /// The "only if already installed" variant; present only for `prepare`
    /// actions.
    installed_only: Option<String>,
    /// The package-listing helper; present only for `downgrade` actions.
    list_command: Option<String>,
    /// The substitution mode for this action's templates.
    mode: SubstMode,
}

impl ActionCommands {
    /// A strict command-only action set (no reboot / installed_only /
    /// list_command).
    #[must_use]
    fn command_only(command: impl Into<String>) -> Self {
        Self {
            command: command.into(),
            reboot: None,
            installed_only: None,
            list_command: None,
            mode: SubstMode::Strict,
        }
    }

    /// A strict command + reboot action set (the transactional shape).
    #[must_use]
    fn with_reboot(command: impl Into<String>, reboot: impl Into<String>) -> Self {
        Self {
            command: command.into(),
            reboot: Some(reboot.into()),
            installed_only: None,
            list_command: None,
            mode: SubstMode::Strict,
        }
    }

    /// Overrides the substitution [`mode`](Self::mode) (builder-style).
    #[must_use]
    fn with_mode(mut self, mode: SubstMode) -> Self {
        self.mode = mode;
        self
    }

    /// Renders [`command`](Self::command) with `vars` substituted, honouring the
    /// action's [`mode`](Self::mode).
    ///
    /// # Errors
    ///
    /// In [`Strict`](SubstMode::Strict) mode, propagates [`TemplateError`] from
    /// the underlying substitution (missing key or malformed placeholder).
    pub(crate) fn render_command(
        &self,
        vars: &HashMap<&str, &str>,
    ) -> Result<String, TemplateError> {
        self.mode.render(&self.command, vars)
    }

    /// Renders [`reboot`](Self::reboot) if present. The template has no
    /// placeholders, so it is rendered strictly against an empty map.
    ///
    /// # Errors
    ///
    /// Propagates [`TemplateError`]; never expected, for the reason above.
    pub(crate) fn render_reboot(&self) -> Result<Option<String>, TemplateError> {
        match &self.reboot {
            Some(t) => Ok(Some(substitute(t, &HashMap::new())?)),
            None => Ok(None),
        }
    }

    /// The raw, unrendered [`command`](Self::command) template.
    ///
    /// Needed by the [`PlanProvider`](mtui_hosts::PlanProvider) adapter, whose
    /// [`Doer`](mtui_hosts::Doer) substitutes `$packages` itself — the package
    /// list is only known inside `mtui-hosts`, at
    /// [`Operation::collect`](mtui_hosts::Operation::collect) time. Prefer
    /// [`render_command`](Self::render_command) everywhere else.
    pub(crate) fn command_template(&self) -> &str {
        &self.command
    }

    /// The raw, unrendered [`reboot`](Self::reboot) template, if any. See
    /// [`command_template`](Self::command_template).
    pub(crate) fn reboot_template(&self) -> Option<&str> {
        self.reboot.as_deref()
    }

    /// Renders [`installed_only`](Self::installed_only) with `vars` if present,
    /// honouring the action's [`mode`](Self::mode).
    ///
    /// # Errors
    ///
    /// In strict mode, propagates [`TemplateError`] from the underlying
    /// substitution.
    pub(crate) fn render_installed_only(
        &self,
        vars: &HashMap<&str, &str>,
    ) -> Result<Option<String>, TemplateError> {
        match &self.installed_only {
            Some(t) => Ok(Some(self.mode.render(t, vars)?)),
            None => Ok(None),
        }
    }

    /// Renders [`list_command`](Self::list_command) with `vars` if present,
    /// honouring the action's [`mode`](Self::mode).
    ///
    /// # Errors
    ///
    /// In strict mode, propagates [`TemplateError`] from the underlying
    /// substitution.
    pub(crate) fn render_list_command(
        &self,
        vars: &HashMap<&str, &str>,
    ) -> Result<Option<String>, TemplateError> {
        match &self.list_command {
            Some(t) => Ok(Some(self.mode.render(t, vars)?)),
            None => Ok(None),
        }
    }
}

/// Shared scaffolding for the `rendered_script` harnesses in `downgrade` and
/// `update`, which write stub executables onto `PATH` and run a rendered
/// template under `/bin/sh`.
#[cfg(all(test, unix))]
pub(crate) mod sh_harness {
    use std::ffi::OsStr;
    use std::fs;
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};
    use std::process::{Command, Output};
    use std::time::Duration;

    /// Writes `body` to `dir/name` as an executable stub, via a temp file
    /// `fchmod`ed before the rename so `dir/name` never names a partial or
    /// not-yet-executable file. This does not address #483 — `ETXTBSY` is
    /// refused against the *inode*, which the rename carries along; see
    /// [`sh_output`].
    pub(crate) fn write_exe(dir: &Path, name: &str, body: &str) {
        let mut tmp = tempfile::NamedTempFile::new_in(dir).expect("create temp stub");
        tmp.write_all(body.as_bytes()).expect("write stub");
        tmp.as_file()
            .set_permissions(fs::Permissions::from_mode(0o755))
            .expect("chmod stub");
        tmp.into_temp_path()
            .persist(dir.join(name))
            .expect("persist stub");
    }

    /// Runs `cmd`, retrying once if the shell reports `126` — how an `ETXTBSY`
    /// exec surfaces (#483).
    ///
    /// A sibling test thread's `Command::spawn` forks a child holding a
    /// duplicated write descriptor on a stub written here, and the exec is
    /// refused until it dies, so no ordering on the write side can prevent it.
    /// Retrying on `126` is safe because no case drives it: every exit knob is a
    /// fixed list, the widest being `update`'s probe vocabulary (`1..=8`,
    /// `99..=108`, `255`). **No case may add a `126`.**
    ///
    /// The retry clears `MTUI_STUB_DIR`'s `.ran` sentinels first, since cases
    /// count invocations, and returns the second run even on another `126`:
    /// that `Output` agrees with the files the case then reads, and a genuine
    /// "found but not executable" still surfaces as `126`.
    ///
    /// **Mitigation, not a fix.** An exec whose status the template discards
    /// leaves the script's status untouched, so no retry fires and a case
    /// asserting on that stub's invocation fails unrescued — in `ZYPPER_UPDATE`:
    /// `zypper -n lr`, `zypper -n refresh`, both `zypper -n patches | grep`
    /// transcript lines, and the `zypper -n rr` cleanup loop. Rejected: a module
    /// `RwLock` would close the race outright (the only forkers are the harness
    /// runs themselves) but serialise every run in the crate's lib suite, while
    /// the retry costs nothing until a `126`.
    pub(crate) fn sh_output(cmd: &mut Command) -> Output {
        let first = cmd.output().expect("run under /bin/sh");
        if first.status.code() != Some(126) {
            return first;
        }
        clear_sentinels(&stub_dir(cmd));
        // The window is one forked child's fork-to-exec gap; outwaiting it is
        // what makes a single retry worth having.
        std::thread::sleep(Duration::from_millis(50));
        cmd.output().expect("run under /bin/sh")
    }

    /// The stub directory `cmd` is configured with, read back off the command
    /// so it cannot disagree with the one the stubs were written into.
    fn stub_dir(cmd: &Command) -> PathBuf {
        cmd.get_envs()
            .find_map(|(k, v)| (k == OsStr::new("MTUI_STUB_DIR")).then_some(v))
            .flatten()
            .expect("the harness command sets MTUI_STUB_DIR")
            .into()
    }

    /// Best-effort removal of `dir`'s `.ran` sentinel files.
    fn clear_sentinels(dir: &Path) {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            if entry.path().extension().is_some_and(|e| e == "ran") {
                let _ = fs::remove_file(entry.path());
            }
        }
    }

    mod tests {
        use super::*;

        /// `sh -c` against `dir`, through [`sh_output`].
        fn run(dir: &Path, script: &str) -> Output {
            let mut cmd = Command::new("/bin/sh");
            cmd.arg("-c").arg(script).env("MTUI_STUB_DIR", dir);
            sh_output(&mut cmd)
        }

        #[test]
        fn a_126_is_retried_once_on_a_cleared_sentinel() {
            // The #483 shape, injected deterministically. Both runs append to a
            // `.ran` sentinel, so the surviving line count also pins the clear —
            // without it the retry doubles every invocation count the harness
            // cases assert on.
            let dir = tempfile::tempdir().expect("tempdir");
            let out = run(
                dir.path(),
                r#"printf 'x\n' >> "$MTUI_STUB_DIR/probe.ran"
if [ -e "$MTUI_STUB_DIR/attempted" ]; then exit 0; fi
: > "$MTUI_STUB_DIR/attempted"
exit 126
"#,
            );
            assert_eq!(
                out.status.code(),
                Some(0),
                "the second run's status must be the one returned"
            );
            let ran = fs::read_to_string(dir.path().join("probe.ran")).expect("probe.ran");
            assert_eq!(
                ran.lines().count(),
                1,
                "the retry must start from cleared sentinels: {ran:?}"
            );
        }

        #[test]
        fn a_second_126_surfaces_the_retry_and_stops_there() {
            // 126 is ambiguous — a real "not executable" reports it too — so a
            // run failing twice must still report 126 and must not be retried a
            // third time. `attempts` is not a `.ran` sentinel, so it survives
            // the clear and counts the runs; the echoed count pins that the
            // retry's stdout is what comes back, matching the sentinels the case
            // reads afterwards.
            let dir = tempfile::tempdir().expect("tempdir");
            let out = run(
                dir.path(),
                r#"printf 'x\n' >> "$MTUI_STUB_DIR/attempts"
wc -l < "$MTUI_STUB_DIR/attempts" | tr -d ' '
exit 126
"#,
            );
            assert_eq!(
                out.status.code(),
                Some(126),
                "a real 126 must not be masked"
            );
            assert_eq!(
                String::from_utf8_lossy(&out.stdout).trim(),
                "2",
                "the retry's output must be the one returned"
            );
            let attempts = fs::read_to_string(dir.path().join("attempts")).expect("attempts");
            assert_eq!(
                attempts.lines().count(),
                2,
                "exactly one retry: {attempts:?}"
            );
        }

        #[test]
        fn a_non_126_failure_is_not_retried() {
            // Every other failing status is a case's own subject, and rerunning
            // one would double its sentinels for nothing.
            let dir = tempfile::tempdir().expect("tempdir");
            let out = run(
                dir.path(),
                r#"printf 'x\n' >> "$MTUI_STUB_DIR/attempts"; exit 7"#,
            );
            assert_eq!(out.status.code(), Some(7));
            let attempts = fs::read_to_string(dir.path().join("attempts")).expect("attempts");
            assert_eq!(attempts.lines().count(), 1, "no rerun: {attempts:?}");
        }

        #[test]
        fn write_exe_publishes_the_body_at_0o755_without_writing_the_live_name() {
            // The three hazards the temp file removes: wrong mode, partial
            // content, and a write seen through the name a concurrent exec
            // resolves. `witness` is a second link to the first stub's inode —
            // `fs::write` onto the final name would show through it.
            let dir = tempfile::tempdir().expect("tempdir");
            let stub = dir.path().join("probe");
            let first = "#!/bin/sh\nexit 1\n";
            write_exe(dir.path(), "probe", first);
            let witness = dir.path().join("witness");
            fs::hard_link(&stub, &witness).expect("hard link");

            let body = "#!/bin/sh\nexit 3\n";
            write_exe(dir.path(), "probe", body);

            assert_eq!(fs::read_to_string(&stub).expect("read stub"), body);
            assert_eq!(
                fs::metadata(&stub).expect("stat stub").permissions().mode() & 0o777,
                0o755,
                "the shell execs the stub by name"
            );
            assert_eq!(
                fs::read_to_string(&witness).expect("read witness"),
                first,
                "the live name must never be written through"
            );
        }
    }
}
