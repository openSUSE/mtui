//! The `export` command (writes the gathered update data to the template).

use std::path::PathBuf;

use async_trait::async_trait;
use clap::{Arg, ArgAction, ArgMatches};
use mtui_datasources::{HttpClient, VerifyPolicy, resolve_verify};
use mtui_testreport::{
    AutoExport, DenyOverwrite, ExportContext, FileList, KernelExport, ManualExport, ManualHost,
};
use mtui_types::Workflow;

use super::support::{add_hosts_arg, require_update, select_names, template_completion};
use crate::command::{Command, Scope};
use crate::error::{CommandError, CommandResult};
use crate::session::Session;

/// Exports the gathered update data to the testing template.
///
/// Ports upstream `mtui.commands.export.Export`. Picks the exporter by the
/// report's [`Workflow`] and writes the pre/post package versions and update log
/// into the template (or `filename` when given). Requires a loaded report.
///
/// ## openQA enrichment (graceful Manual)
///
/// Upstream additionally folds openQA results into the export via
/// `metadata.openqa`. That openQA state holder is not yet on the Rust metadata
/// (deferred to mtui-rs-0pe/plt/zs4), so the exporters here run with empty
/// openQA inputs: `Auto`/`Kernel` still render their full local template, and
/// `Manual` degrades gracefully — it writes the per-host install logs it can
/// gather and logs a warning that openQA enrichment is unavailable, rather than
/// failing. The enrichment leg is tracked as a follow-up.
pub struct Export;

#[async_trait]
impl Command for Export {
    fn name(&self) -> &'static str {
        "export"
    }

    fn scope(&self) -> Scope {
        Scope::Fanout
    }

    fn configure(&self, cmd: clap::Command) -> clap::Command {
        add_hosts_arg(cmd)
            .arg(
                Arg::new("force")
                    .short('f')
                    .long("force")
                    .action(ArgAction::SetTrue)
                    .help(
                        "force overwrite existing template and re-download openQA \
                         results present in the log",
                    ),
            )
            .arg(
                Arg::new("filename")
                    .value_name("FILENAME")
                    .help("output template file name (defaults to the loaded template)"),
            )
    }

    async fn call(&self, session: &mut Session, args: &ArgMatches) -> CommandResult {
        let rrid = require_update(session)?;
        let workflow = session.metadata().workflow();
        let force = args.get_flag("force");

        // Output path: explicit `filename`, else the loaded report's own path.
        let filename: PathBuf = match args.get_one::<String>("filename") {
            Some(f) => PathBuf::from(f),
            None => session
                .metadata()
                .base()
                .path
                .clone()
                .ok_or_else(|| CommandError::Other("no report path to export to".to_owned()))?,
        };

        let text = FileList::load(&filename).map_err(|e| {
            CommandError::Other(format!("could not read template {filename:?}: {e}"))
        })?;
        let ctx = ExportContext::new(session.config.clone(), text.lines(), force, rrid);

        let template: Vec<String> = match workflow {
            Workflow::Auto => {
                let http = build_http(session)?;
                AutoExport::new(ctx, None, None)
                    .run(&http, &DenyOverwrite)
                    .await
            }
            Workflow::Kernel => {
                let http = build_http(session)?;
                KernelExport::new(ctx, Vec::new(), None).run(&http).await
            }
            Workflow::Manual => {
                // openQA enrichment (auto/overview) is unavailable until the
                // openQA state holder lands; degrade to the local install logs.
                tracing::warn!(
                    "manual export: openQA results are not yet wired into metadata; \
                     exporting local install logs only"
                );
                let hosts = select_names(session.targets(), args, false)
                    .map_err(|e| CommandError::Other(e.to_string()))?;
                let results = manual_hosts(session, &hosts);
                ManualExport::new(ctx, results, None, None).run(&hosts, &DenyOverwrite)
            }
        };

        let mut out = FileList::from_lines(&filename, template);
        out.write().map_err(|e| {
            CommandError::Other(format!("could not write template {filename:?}: {e}"))
        })?;
        tracing::info!(path = %filename.display(), "template exported");
        Ok(())
    }

    fn complete(&self, session: &Session, text: &str, _line: &str) -> Vec<String> {
        template_completion(session, text)
    }
}

/// Builds a verifying HTTP client from the session config (log-download seam).
fn build_http(session: &Session) -> Result<HttpClient, CommandError> {
    let verify = resolve_verify(
        VerifyPolicy::Default(true),
        Some(VerifyPolicy::from_config(&session.config.ssl_verify)),
    );
    HttpClient::new(verify)
        .map_err(|e| CommandError::Other(format!("could not build HTTP client: {e}")))
}

/// Builds the decoupled [`ManualHost`] views the manual exporter reads, from the
/// named connected targets (upstream reads the live `Target`s directly).
fn manual_hosts(session: &Session, hosts: &[String]) -> Vec<ManualHost> {
    hosts
        .iter()
        .filter_map(|name| session.targets().get(name))
        .map(|t| ManualHost {
            hostname: t.hostname().to_owned(),
            system: t.system().to_string(),
            packages: t.packages().to_vec(),
            hostlog: t.out().clone(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::testkit::{empty_session, matches, session_with_hosts};

    #[test]
    fn name_and_fanout_scope() {
        assert_eq!(Export.name(), "export");
        assert_eq!(Export.scope(), Scope::Fanout);
    }

    #[tokio::test]
    async fn no_report_errors_before_any_io() {
        let (mut session, _buf) = empty_session();
        let args = matches(&Export, &[]);
        let err = Export.call(&mut session, &args).await.unwrap_err();
        assert!(matches!(err, CommandError::Other(_)));
    }

    #[tokio::test]
    async fn auto_writes_template_to_explicit_filename() {
        let (mut session, _buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "ok");
        session.templates.active_mut().base_mut().workflow = Workflow::Auto;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("template.txt");
        std::fs::write(&path, "source code change review:\n").unwrap();

        let args = matches(&Export, &["-f", path.to_str().unwrap()]);
        Export.call(&mut session, &args).await.unwrap();

        let written = std::fs::read_to_string(&path).unwrap();
        // The auto exporter appends the system-info footer to any template.
        assert!(written.contains("## export MTUI:"));
    }

    #[tokio::test]
    async fn kernel_writes_template_to_explicit_filename() {
        let (mut session, _buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "ok");
        session.templates.active_mut().base_mut().workflow = Workflow::Kernel;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("template.txt");
        std::fs::write(&path, "regression tests:\n").unwrap();

        let args = matches(&Export, &["-f", path.to_str().unwrap()]);
        Export.call(&mut session, &args).await.unwrap();

        let written = std::fs::read_to_string(&path).unwrap();
        assert!(written.contains("## export MTUI:"));
    }

    #[tokio::test]
    async fn manual_degrades_gracefully_without_openqa() {
        let (mut session, _buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "ok");
        session.templates.active_mut().base_mut().workflow = Workflow::Manual;
        let dir = tempfile::tempdir().unwrap();
        // The manual exporter writes per-host install logs under
        // `template_dir/<rrid>/install_logs`; keep that inside the tempdir so the
        // test never pollutes the working tree.
        session.config.template_dir = dir.path().to_path_buf();
        let path = dir.path().join("template.txt");
        std::fs::write(&path, "source code change review:\n").unwrap();

        let args = matches(&Export, &["-f", path.to_str().unwrap()]);
        // Must not panic / hard-fail even though the openQA holder is absent.
        Export.call(&mut session, &args).await.unwrap();

        let written = std::fs::read_to_string(&path).unwrap();
        assert!(written.contains("## export MTUI:"));
    }

    #[tokio::test]
    async fn missing_file_errors_cleanly() {
        let (mut session, _buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "ok");
        session.templates.active_mut().base_mut().workflow = Workflow::Auto;
        let args = matches(&Export, &["-f", "/nonexistent/dir/nope.txt"]);
        let err = Export.call(&mut session, &args).await.unwrap_err();
        assert!(matches!(err, CommandError::Other(_)));
    }
}
