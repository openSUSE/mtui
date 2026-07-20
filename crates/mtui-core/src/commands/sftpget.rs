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

    fn about(&self) -> Option<&'static str> {
        Some("Downloads a remote file from every enabled reference host.")
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

        // Per-host download outcomes: `sftp_get` suffixes the local path with the
        // hostname (`{local}.{host}`), so report that concrete path and fail if
        // any enabled host's transfer errored — an MCP call must never silently
        // "succeed" on a download that never happened. `None` (disabled /
        // not-attempted) hosts are skipped.
        let local_label = local.display().to_string();
        let mut outcomes: Vec<(String, Result<(), String>)> = Vec::new();
        for host in targets.names() {
            let Some(outcome) = targets
                .get(&host)
                .and_then(mtui_hosts::Target::last_download)
            else {
                continue;
            };
            outcomes.push((host, outcome.clone()));
        }

        let mut failed: Vec<String> = Vec::new();
        for (host, outcome) in &outcomes {
            match outcome {
                Ok(()) => {
                    session
                        .display
                        .println(&format!("{local_label}.{host} <- {host}: ok"));
                    tracing::info!(remote = %remote.display(), host = %host, "downloaded");
                }
                Err(reason) => {
                    session.display.println(&format!(
                        "{local_label}.{host} <- {host}: FAILED ({reason})"
                    ));
                    failed.push(host.clone());
                }
            }
        }

        if failed.is_empty() {
            Ok(())
        } else {
            Err(CommandError::Other(format!(
                "download failed on: {}",
                failed.join(", ")
            )))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::testkit::{matches, session_with_download_outcomes, session_with_hosts};

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

    #[tokio::test]
    async fn all_hosts_succeed_prints_ok_summary() {
        let dir = tempfile::tempdir().unwrap();
        let (mut session, buf) = session_with_download_outcomes(
            "SUSE:Maintenance:1:1",
            &[("h1", true), ("h2", true)],
            dir.path(),
        );
        let args = matches(&SftpGet, &["/remote/file.log"]);
        SftpGet.call(&mut session, &args).await.unwrap();
        let out = buf.contents();
        assert!(out.contains("<- h1: ok"), "{out}");
        assert!(out.contains("<- h2: ok"), "{out}");
        assert!(out.contains("file.log.h1"), "suffixed path: {out}");
        assert!(!out.contains("FAILED"), "{out}");
    }

    #[tokio::test]
    async fn one_host_fails_errors_and_reports_both() {
        let dir = tempfile::tempdir().unwrap();
        let (mut session, buf) = session_with_download_outcomes(
            "SUSE:Maintenance:1:1",
            &[("h1", true), ("h2", false)],
            dir.path(),
        );
        let args = matches(&SftpGet, &["/remote/file.log"]);
        let err = SftpGet.call(&mut session, &args).await.unwrap_err();
        match err {
            CommandError::Other(msg) => {
                assert!(msg.contains("h2"), "{msg}");
                assert!(!msg.contains("h1"), "only h2 failed: {msg}");
            }
            other => panic!("expected Other, got {other:?}"),
        }
        let out = buf.contents();
        assert!(out.contains("<- h1: ok"), "{out}");
        assert!(out.contains("<- h2: FAILED"), "{out}");
    }
}
