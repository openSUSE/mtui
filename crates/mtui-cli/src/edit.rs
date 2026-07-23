//! The `edit` REPL command and its `$EDITOR` spawn.
//!
//! Ports upstream `mtui.commands.edit.Edit`. Spawning `$EDITOR` (default `vim`)
//! on the file inherits the process stdio, so the child needs the controlling
//! terminal — which only the `mtui` binary owns. `mtui-core`'s `edit` command is
//! therefore a headless-error stub (mirroring `shell`); the REPL **intercepts**
//! the `edit` line before dispatch (see [`is_edit_line`]) and spawns the editor
//! here, where the local TTY is available. A host library / the headless MCP
//! engine never runs this path.
//!
//! Deviation from upstream: upstream `edit.py` wraps the spawn in a bare
//! `except` that logs and swallows every failure. Here [`run_edit`] returns a
//! typed [`anyhow::Result`] and the REPL renders any failure in red (the same
//! path `run_shell` uses), matching the project's "typed `Result` over
//! log-and-swallow" preference.

use std::path::PathBuf;
use std::process::Command;

use clap::Arg;
use mtui_core::Session;

/// Peeks a REPL input line: if its first token is the `edit` command, returns
/// its argv (everything after the command word); otherwise `None`.
///
/// The pure seam the REPL uses to route `edit` to the local `$EDITOR` spawn
/// instead of the headless engine (kept off the reedline boundary so it is
/// unit-testable, mirroring [`crate::shell::is_shell_line`]).
#[must_use]
pub(crate) fn is_edit_line(line: &str) -> Option<Vec<String>> {
    let tokens = shlex::split(line)?;
    let (name, argv) = tokens.split_first()?;
    (name == "edit").then(|| argv.to_vec())
}

/// Resolves the edit target: the explicit `filename` argument, or — when none is
/// given — the active report's template path.
///
/// Mirrors upstream `edit.py`: `self.args.filename or self._template()`, where
/// `_template` is `@requires_update` (errors when nothing is loaded). Returns
/// the same "not loaded" message the engine's `require_update` would.
fn resolve_path(session: &Session, filename: Option<&String>) -> anyhow::Result<PathBuf> {
    if let Some(name) = filename {
        return Ok(PathBuf::from(name));
    }
    let meta = session.metadata();
    if !meta.is_loaded() {
        anyhow::bail!("Metadata not loaded, please use load_template first");
    }
    meta.base()
        .path
        .clone()
        .ok_or_else(|| anyhow::anyhow!("Metadata not loaded, please use load_template first"))
}

/// Runs the `edit` command: parse the optional `filename`, resolve the path,
/// then spawn `$EDITOR` (default `vim`) on it with inherited stdio.
///
/// Upstream passes a two-element argv (`[editor, path]`) with no shell-word
/// splitting; this mirrors that exactly (`Command::new(editor).arg(path)`), so
/// `$EDITOR="code -w"` is treated as a single program name (a deliberate parity
/// choice, not word-split).
///
/// # Errors
///
/// Returns an error on an argument-parse failure (clap usage), an unresolved
/// default path (no template loaded), a spawn failure (`$EDITOR` not found), or
/// a non-zero editor exit. The REPL renders it; nothing is swallowed.
pub(crate) fn run_edit(session: &mut Session, argv: &[String]) -> anyhow::Result<()> {
    let parser = clap::Command::new("edit").no_binary_name(true).arg(
        Arg::new("filename")
            .num_args(0..=1)
            .value_name("FILENAME")
            .help("File to edit (defaults to the active template)"),
    );
    let matches = parser
        .try_get_matches_from(argv)
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    let path = resolve_path(session, matches.get_one::<String>("filename"))?;

    let editor = std::env::var("EDITOR").unwrap_or_else(|_| "vim".to_owned());
    tracing::debug!(editor, path = %path.display(), "spawning editor");

    let status = Command::new(&editor)
        .arg(&path)
        .status()
        .map_err(|e| anyhow::anyhow!("failed to run {editor}: {e}"))?;

    if !status.success() {
        anyhow::bail!("{editor} exited with {status}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use mtui_config::Config;
    use mtui_core::{ColorMode, CommandPromptDisplay, Session};
    use std::sync::Mutex;

    /// Serializes the two tests that mutate the process-global `$EDITOR`, so they
    /// never race under the parallel test harness.
    static EDITOR_ENV: Mutex<()> = Mutex::new(());

    /// A headless session with a captured (sunk) display and nothing loaded.
    fn empty_session() -> Session {
        let display = CommandPromptDisplay::with_sink(Box::new(std::io::sink()), ColorMode::Never);
        Session::with_display(Config::default(), true, display)
    }

    /// Writes an executable stub script that records its argv (one per line) to
    /// `record` and exits with `code`. Returns its path (used as `$EDITOR`).
    #[cfg(unix)]
    fn editor_stub(dir: &std::path::Path, record: &std::path::Path, code: i32) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let script = dir.join("fake-editor.sh");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\nfor a in \"$@\"; do echo \"$a\" >> \"{}\"; done\nexit {code}\n",
                record.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        script
    }

    #[test]
    fn is_edit_line_matches_only_edit() {
        assert_eq!(is_edit_line("edit"), Some(vec![]));
        assert_eq!(
            is_edit_line("edit foo.txt"),
            Some(vec!["foo.txt".to_owned()])
        );
        assert_eq!(is_edit_line("run uname -a"), None);
        assert_eq!(is_edit_line(""), None);
        // Unbalanced quote → no split.
        assert_eq!(is_edit_line("edit \"unbalanced"), None);
    }

    #[test]
    fn resolve_path_uses_explicit_filename() {
        let session = empty_session();
        let arg = "some/file.txt".to_owned();
        let p = resolve_path(&session, Some(&arg)).unwrap();
        assert_eq!(p, PathBuf::from("some/file.txt"));
    }

    #[test]
    fn resolve_path_no_arg_no_template_errors() {
        let session = empty_session();
        let err = resolve_path(&session, None).unwrap_err();
        assert!(err.to_string().contains("Metadata not loaded"));
    }

    #[test]
    fn run_edit_no_template_and_no_arg_errors() {
        let mut session = empty_session();
        let err = run_edit(&mut session, &[]).unwrap_err();
        assert!(err.to_string().contains("Metadata not loaded"));
    }

    #[cfg(unix)]
    #[test]
    // `std::env::set_var`/`remove_var` are `unsafe` in edition 2024; the
    // EDITOR_ENV mutex makes the mutation exclusive within these tests.
    #[allow(unsafe_code)]
    fn run_edit_uses_editor_env_and_path() {
        let dir = tempfile::tempdir().unwrap();
        let record = dir.path().join("argv.log");
        let stub = editor_stub(dir.path(), &record, 0);

        let mut session = empty_session();
        let _env = EDITOR_ENV.lock().unwrap_or_else(|e| e.into_inner());
        // SAFETY: the EDITOR_ENV mutex serializes every $EDITOR reader/writer in
        // this module's tests, so no concurrent env access occurs.
        unsafe {
            std::env::set_var("EDITOR", &stub);
        }
        let target = dir.path().join("payload.txt");
        run_edit(&mut session, &[target.display().to_string()]).unwrap();
        unsafe {
            std::env::remove_var("EDITOR");
        }

        let logged = std::fs::read_to_string(&record).unwrap();
        assert_eq!(logged.trim(), target.display().to_string());
    }

    #[cfg(unix)]
    #[test]
    // See `run_edit_uses_editor_env_and_path`: edition-2024 env mutation is
    // `unsafe`, serialized by EDITOR_ENV.
    #[allow(unsafe_code)]
    fn run_edit_nonzero_exit_errors() {
        let dir = tempfile::tempdir().unwrap();
        let record = dir.path().join("argv.log");
        let stub = editor_stub(dir.path(), &record, 3);

        let mut session = empty_session();
        let _env = EDITOR_ENV.lock().unwrap_or_else(|e| e.into_inner());
        // SAFETY: the EDITOR_ENV mutex serializes every $EDITOR access here.
        unsafe {
            std::env::set_var("EDITOR", &stub);
        }
        let err = run_edit(&mut session, &["whatever.txt".to_owned()]).unwrap_err();
        unsafe {
            std::env::remove_var("EDITOR");
        }
        assert!(err.to_string().contains("exited with"));
    }
}
