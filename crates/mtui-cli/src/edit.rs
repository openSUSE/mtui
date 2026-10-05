//! The `edit` REPL command: the report-document editor, or `$EDITOR` on an
//! explicit file.
//!
//! Spawning `$EDITOR` (default `vim`) inherits the process stdio, so the child
//! needs the controlling terminal only the `mtui` binary owns. `mtui-core`'s
//! `edit` is therefore a headless-error stub, mirroring `shell`, and the REPL
//! intercepts the `edit` line before dispatch ([`is_edit_line`]) to spawn the
//! editor here. [`run_edit`] returns a typed [`anyhow::Result`] rather than
//! logging and swallowing; the REPL renders any failure in red.

use std::path::PathBuf;
use std::process::Command;

use clap::Arg;
use mtui_core::{CommandError, Session};

/// Peeks a REPL input line: if its first token is the `edit` command, returns
/// its argv (everything after the command word); otherwise `None`.
///
/// The pure seam the REPL routes `edit` through to reach the local `$EDITOR`
/// spawn instead of the headless engine, kept off the reedline boundary so it is
/// unit-testable (mirroring [`crate::shell::is_shell_line`]).
#[must_use]
pub(crate) fn is_edit_line(line: &str) -> Option<Vec<String>> {
    let tokens = shlex::split(line)?;
    let (name, argv) = tokens.split_first()?;
    (name == "edit").then(|| argv.to_vec())
}

/// What `edit` opens.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum EditTarget {
    /// The full-screen editor over the active report's document.
    Form,
    /// A file in `$EDITOR`.
    File(PathBuf),
}

/// Routes `edit`: an explicit `filename` is a file, and with none the document
/// editor over the active report's document.
///
/// # Errors
///
/// [`CommandError::NoDocument`] when no `filename` is given and the active
/// report holds no document.
pub(crate) fn edit_target(
    session: &Session,
    filename: Option<&String>,
) -> anyhow::Result<EditTarget> {
    if let Some(name) = filename {
        return Ok(EditTarget::File(PathBuf::from(name)));
    }
    let has_document = session.templates.active_rrid().is_some_and(|rrid| {
        session
            .with_report(rrid, |report| report.base().document.is_some())
            .unwrap_or(false)
    });
    if has_document {
        Ok(EditTarget::Form)
    } else {
        Err(CommandError::NoDocument.into())
    }
}

/// Runs the `edit` command: parse the optional `filename`, then either open the
/// document editor or spawn `$EDITOR` (default `vim`) on the file with
/// inherited stdio.
///
/// `$EDITOR` reaches `Command::new` unsplit, so `$EDITOR="code -w"` is one
/// program name — deliberate, not an oversight.
///
/// # Errors
///
/// Returns an error on an argument-parse failure, no filename with no report
/// document loaded, a spawn failure, or a non-zero editor exit.
pub(crate) fn run_edit(session: &mut Session, argv: &[String]) -> anyhow::Result<()> {
    let parser = clap::Command::new("edit").no_binary_name(true).arg(
        Arg::new("filename")
            .num_args(0..=1)
            .value_name("FILENAME")
            .help("File to edit (defaults to the report document)"),
    );
    let matches = parser
        .try_get_matches_from(argv)
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    let path = match edit_target(session, matches.get_one::<String>("filename"))? {
        EditTarget::Form => return crate::tui_edit::edit_document(session),
        EditTarget::File(path) => path,
    };

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

    /// A headless session with a captured (sunk) display and nothing loaded.
    fn empty_session() -> Session {
        let display = CommandPromptDisplay::with_sink(Box::new(std::io::sink()), ColorMode::Never);
        Session::with_display(Config::default(), true, display)
    }

    /// An executable stub that records its argv (one per line) to `record` and
    /// exits with `code`; its path is used as `$EDITOR`.
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
        assert_eq!(is_edit_line("edit \"unbalanced"), None);
    }

    /// A session whose active report holds `document` (or none) and, when
    /// given, a template `path`.
    fn session_with_report(document: bool, path: Option<&str>) -> Session {
        use mtui_testreport::{ObsReport, TestReport};
        use mtui_types::RequestReviewID;
        use mtui_types::report_document::ReportDocument;

        const RRID: &str = "SUSE:Maintenance:1:1";
        let mut session = empty_session();
        let mut report = ObsReport::new(session.config.clone());
        report.base_mut().rrid = Some(RequestReviewID::parse(RRID).unwrap());
        report.base_mut().path = path.map(PathBuf::from);
        if document {
            report.base_mut().document = Some(
                include_str!("../../mtui-types/tests/fixtures/document/maintenance_obs.json")
                    .parse::<ReportDocument>()
                    .unwrap(),
            );
        }
        session.templates.add(Box::new(report));
        session.templates.set_active(RRID);
        let _ = session.activate(RRID);
        session
    }

    #[test]
    fn a_loaded_document_with_no_argument_opens_the_editor() {
        let session = session_with_report(true, Some("/tmp/x/log"));

        assert_eq!(edit_target(&session, None).unwrap(), EditTarget::Form);
    }

    #[test]
    fn a_report_without_a_document_refuses() {
        let session = session_with_report(false, Some("/tmp/x/log"));

        let err = edit_target(&session, None).unwrap_err();

        assert!(
            matches!(err.downcast_ref(), Some(CommandError::NoDocument)),
            "{err:?}"
        );
    }

    #[test]
    fn an_explicit_file_wins_over_a_loaded_document() {
        let session = session_with_report(true, Some("/tmp/x/log"));
        let arg = "notes.txt".to_owned();

        assert_eq!(
            edit_target(&session, Some(&arg)).unwrap(),
            EditTarget::File(PathBuf::from("notes.txt"))
        );
    }

    #[test]
    fn nothing_loaded_and_no_argument_is_still_an_error() {
        let session = empty_session();

        let err = edit_target(&session, None).unwrap_err();

        assert!(err.to_string().contains("no report document loaded"));
    }

    #[test]
    fn run_edit_no_document_and_no_arg_errors() {
        let mut session = empty_session();
        let err = run_edit(&mut session, &[]).unwrap_err();
        assert!(err.to_string().contains("no report document loaded"));
    }

    #[cfg(unix)]
    #[test]
    // Edition-2024 env mutation is `unsafe`; the crate's single `env` serial
    // domain makes it exclusive.
    #[serial_test::serial(env)]
    #[allow(unsafe_code)]
    fn run_edit_uses_editor_env_and_path() {
        let dir = tempfile::tempdir().unwrap();
        let record = dir.path().join("argv.log");
        let stub = editor_stub(dir.path(), &record, 0);

        let mut session = empty_session();
        // SAFETY: `#[serial(env)]` is the crate's one exclusion domain for the
        // process-global environment, so no other test reads, writes or
        // *inherits* it — this one spawns `$EDITOR` — while this runs.
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
    // See `run_edit_uses_editor_env_and_path`.
    #[serial_test::serial(env)]
    #[allow(unsafe_code)]
    fn run_edit_nonzero_exit_errors() {
        let dir = tempfile::tempdir().unwrap();
        let record = dir.path().join("argv.log");
        let stub = editor_stub(dir.path(), &record, 3);

        let mut session = empty_session();
        // SAFETY: as above — `#[serial(env)]`, and this test spawns `$EDITOR`,
        // which inherits the environment.
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
