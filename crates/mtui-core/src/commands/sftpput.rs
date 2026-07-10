//! The `put` command (SFTP upload).

use std::path::PathBuf;

use async_trait::async_trait;
use clap::{Arg, ArgMatches};

use super::support::complete_choices_filelist;
use crate::command::{Command, Scope};
use crate::error::{CommandError, CommandResult};
use crate::session::Session;

/// Uploads a local file (or directory tree) to every enabled reference host.
///
/// Ports upstream `mtui.commands.sftpcmd.SFTPPut`. Files are placed under the
/// report's remote working directory (`metadata.target_wd(<name>)`); a directory
/// argument is walked and each contained file uploaded. Only enabled hosts are
/// contacted. Shell-glob expansion of the argument is a Phase-6 REPL concern;
/// here the argument is a concrete path (file or directory).
pub struct SftpPut;

/// Recursively collects the regular files under `path` (a single file returns
/// itself). Mirrors upstream's `os.walk` traversal.
fn collect_files(path: &std::path::Path) -> std::io::Result<Vec<PathBuf>> {
    if path.is_file() {
        return Ok(vec![path.to_path_buf()]);
    }
    let mut out = Vec::new();
    if path.is_dir() {
        for entry in std::fs::read_dir(path)? {
            let entry = entry?;
            out.extend(collect_files(&entry.path())?);
        }
    }
    Ok(out)
}

#[async_trait]
impl Command for SftpPut {
    fn name(&self) -> &'static str {
        "put"
    }

    fn about(&self) -> Option<&'static str> {
        Some("Uploads a local file (or directory tree) to every enabled reference host.")
    }

    fn scope(&self) -> Scope {
        Scope::Fanout
    }

    fn configure(&self, cmd: clap::Command) -> clap::Command {
        cmd.arg(
            Arg::new("filename")
                .required(true)
                .value_parser(clap::value_parser!(PathBuf))
                .help("file (or directory) to upload to all hosts"),
        )
    }

    fn complete(&self, session: &Session, text: &str, line: &str) -> Vec<String> {
        // Upstream `complete_choices_filelist(template_completion(state), …)`:
        // file paths merged with the template synonym groups + loaded RRIDs.
        complete_choices_filelist(
            &[&["-T", "--template"], &["--all-templates"]],
            session.templates.rrids(),
            line,
            text,
        )
    }

    async fn call(&self, session: &mut Session, args: &ArgMatches) -> CommandResult {
        let filename = args
            .get_one::<PathBuf>("filename")
            .expect("filename is required")
            .clone();

        let files = collect_files(&filename)
            .map_err(|e| CommandError::Other(format!("{}: {e}", filename.display())))?;
        if files.is_empty() {
            return Err(CommandError::Other(format!(
                "File {} not found",
                filename.display()
            )));
        }

        for file in &files {
            let name = file
                .file_name()
                .and_then(|n| n.to_str())
                .ok_or_else(|| CommandError::Other("invalid file name".to_owned()))?;
            let remote = session.metadata().target_wd(&[name]);

            let targets = session.targets_mut();
            targets.sftp_put(file, &remote).await;
            tracing::info!(local = %file.display(), remote = %remote.display(), "uploaded");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::testkit::{matches, session_with_hosts};

    #[test]
    fn name_and_fanout_scope() {
        assert_eq!(SftpPut.name(), "put");
        assert_eq!(SftpPut.scope(), Scope::Fanout);
    }

    #[test]
    fn complete_offers_files_and_template_flags() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("payload.bin"), "x").unwrap();
        let (session, _buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "linux");

        // Empty tail → template synonym flags + loaded RRID (files need a path).
        let flags = SftpPut.complete(&session, "", "put ");
        assert!(flags.contains(&"-T".to_owned()), "{flags:?}");
        assert!(
            flags.contains(&"SUSE:Maintenance:1:1".to_owned()),
            "{flags:?}"
        );

        // A path prefix surfaces matching files.
        let path = format!("{}/pay", dir.path().display());
        let files = SftpPut.complete(&session, &path, &format!("put {path}"));
        assert!(
            files.iter().any(|c| c.ends_with("payload.bin")),
            "{files:?}"
        );
    }

    #[test]
    fn collect_files_single_file() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("a.txt");
        std::fs::write(&f, "x").unwrap();
        assert_eq!(collect_files(&f).unwrap(), vec![f]);
    }

    #[test]
    fn collect_files_walks_dir() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "x").unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("sub/b.txt"), "y").unwrap();
        let mut got: Vec<String> = collect_files(dir.path())
            .unwrap()
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        got.sort();
        assert_eq!(got, vec!["a.txt", "b.txt"]);
    }

    #[tokio::test]
    async fn missing_file_errors() {
        let (mut session, _buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "ok");
        let args = matches(&SftpPut, &["/no/such/file"]);
        let err = SftpPut.call(&mut session, &args).await.unwrap_err();
        assert!(matches!(err, CommandError::Other(_)));
    }

    #[tokio::test]
    async fn uploads_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("payload.txt");
        std::fs::write(&f, "data").unwrap();
        let (mut session, _buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "ok");
        let args = matches(&SftpPut, &[f.to_str().unwrap()]);
        SftpPut.call(&mut session, &args).await.unwrap();
    }
}
