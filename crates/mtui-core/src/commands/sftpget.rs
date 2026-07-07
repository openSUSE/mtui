//! The `get` command (SFTP download).

use std::path::PathBuf;

use async_trait::async_trait;
use clap::{Arg, ArgMatches};

use crate::command::{Command, Scope};
use crate::error::{CommandError, CommandResult};
use crate::session::Session;

/// Downloads a remote file from every enabled reference host.
///
/// Ports upstream `mtui.commands.sftpcmd.SFTPGet` (`metadata.perform_get`).
/// Files are saved under `{report_wd}/downloads/` with the hostname appended as
/// a file extension (the per-host suffixing is applied by
/// [`HostsGroup::sftp_get`](mtui_hosts)). Only enabled hosts are contacted.
pub struct SftpGet;

#[async_trait]
impl Command for SftpGet {
    fn name(&self) -> &'static str {
        "get"
    }

    fn scope(&self) -> Scope {
        Scope::Fanout
    }

    fn configure(&self, cmd: clap::Command) -> clap::Command {
        cmd.arg(
            Arg::new("filename")
                .required(true)
                .value_parser(clap::value_parser!(PathBuf))
                .help("file to download from the target hosts"),
        )
    }

    async fn call(&self, session: &mut Session, args: &ArgMatches) -> CommandResult {
        let remote = args
            .get_one::<PathBuf>("filename")
            .expect("filename is required")
            .clone();

        // Local target: {report_wd}/downloads/<name> (upstream `perform_get`).
        let wd = session
            .metadata()
            .base()
            .report_wd()
            .map_err(|e| CommandError::Other(format!("no report loaded: {e}")))?;
        let name = remote
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| CommandError::Other("invalid remote file name".to_owned()))?;
        let local = wd.join("downloads").join(name);
        if let Some(parent) = local.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| CommandError::Other(format!("{}: {e}", parent.display())))?;
        }

        let remote_str = remote
            .to_str()
            .ok_or_else(|| CommandError::Other("invalid remote path".to_owned()))?;
        let targets = session.targets_mut();
        targets.sftp_get(remote_str, &local).await;
        tracing::info!(remote = %remote.display(), "downloaded");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::testkit::{matches, session_with_hosts};

    #[test]
    fn name_and_fanout_scope() {
        assert_eq!(SftpGet.name(), "get");
        assert_eq!(SftpGet.scope(), Scope::Fanout);
    }

    #[tokio::test]
    async fn invalid_remote_without_filename_errors() {
        // A root-only remote path has no file name component → clear error
        // before any transfer is attempted.
        let (mut session, _buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "ok");
        let args = matches(&SftpGet, &["/"]);
        let err = SftpGet.call(&mut session, &args).await.unwrap_err();
        assert!(matches!(err, CommandError::Other(_)));
    }

    #[tokio::test]
    async fn no_report_path_errors() {
        // The fixture report carries no checkout path → report_wd errors →
        // surfaced clearly rather than attempting a transfer.
        let (mut session, _buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "ok");
        let args = matches(&SftpGet, &["/remote/file.log"]);
        let err = SftpGet.call(&mut session, &args).await.unwrap_err();
        assert!(matches!(err, CommandError::Other(_)));
    }
}
