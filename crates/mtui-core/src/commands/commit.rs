//! The `commit` command: uploads the report document and its artifacts to
//! teregen's v2 API.

use async_trait::async_trait;
use clap::{Arg, ArgAction, ArgMatches};
use mtui_datasources::teregen::{ArtifactStored, TeregenV2};
use mtui_testreport::{collect_artifacts, upload_current};
use mtui_types::report_document::ReportDocument;

use super::apicall::teregen_v2_writer;
use super::support::{complete_with_templates, require_document, stale_hash_gate};
use crate::command::{Command, Scope};
use crate::error::{CommandError, CommandResult};
use crate::session::Session;

/// Stores the report document and its local artifacts in teregen: uploads the
/// document (conditional on its stored `ETag`) and every artifact
/// [`collect_artifacts`] finds through teregen's write API. Requires a loaded
/// report document.
///
/// Refuses on a template that loaded with a stale Gitea hash
/// (`load_template --force-continue`) unless `--allow-stale` is given —
/// `stale_hash_gate`.
pub struct Commit;

#[async_trait]
impl Command for Commit {
    fn name(&self) -> &'static str {
        "commit"
    }

    fn about(&self) -> Option<&'static str> {
        Some("Stores the report document and artifacts in teregen.")
    }

    fn scope(&self) -> Scope {
        Scope::Explicit
    }

    fn configure(&self, cmd: clap::Command) -> clap::Command {
        cmd.arg(
            Arg::new("allow_stale")
                .long("allow-stale")
                .action(ArgAction::SetTrue)
                .help(
                    "Commit a template that loaded with a stale Gitea hash \
                     (load_template --force-continue); refused otherwise.",
                ),
        )
    }

    fn complete(&self, session: &Session, text: &str, line: &str) -> Vec<String> {
        complete_with_templates(session, &[&["--allow-stale"]], Vec::new(), line, text)
    }

    async fn call(&self, session: &mut Session, args: &ArgMatches) -> CommandResult {
        stale_hash_gate(session, args.get_flag("allow_stale"))?;

        require_document(session)?;
        let client = teregen_v2_writer(session)?;
        commit_document(session, &client).await
    }
}

/// The body of [`Commit::call`]: upload the loaded document
/// (conditional on its stored `ETag`) plus every artifact
/// [`collect_artifacts`] finds, then report the outcome. Split out so tests
/// can inject `client` instead of hitting the real teregen v2 API.
///
/// A document failure aborts before any artifact is sent
/// ([`CommitUploadError::Document`](mtui_testreport::CommitUploadError::Document));
/// an artifact failure does not — every artifact is attempted and reported,
/// and the command fails only afterwards, naming how many failed. Re-running
/// `commit` is the recovery path: every upload is replace-by-name, so it is
/// idempotent.
async fn commit_document(session: &mut Session, client: &TeregenV2) -> CommandResult {
    let summary = upload_and_report(session, client).await?;
    session
        .display
        .println("note: the legacy log view may lag (teregen T4)");

    if summary.failed > 0 {
        return Err(CommandError::Other(format!(
            "{} of {} artifacts failed; re-run commit to retry",
            summary.failed, summary.total
        )));
    }
    Ok(())
}

/// How many artifacts an [`upload_and_report`] sent and how many of those failed.
pub(crate) struct UploadSummary {
    pub total: usize,
    pub failed: usize,
}

/// Uploads the loaded document and every local artifact, printing the
/// "document stored", per-artifact and "skipped" lines. A document failure is
/// an `Err`, and so is a report schema that drifted from this build's, refused
/// before anything is sent; an artifact failure is only counted, so every
/// artifact is still attempted.
pub(crate) async fn upload_and_report(
    session: &mut Session,
    client: &TeregenV2,
) -> Result<UploadSummary, CommandError> {
    if let Some(drift) = &session.metadata().base().schema_drift {
        return Err(CommandError::Other(format!(
            "teregen's report schema differs from this mtui's ({drift}); upgrade mtui before writing"
        )));
    }
    let report_wd = session
        .metadata()
        .base()
        .report_wd()
        .map_err(|e| CommandError::Other(format!("no report working directory: {e}")))?;
    let install_logs = session.config.install_logs.clone();

    let collected = collect_artifacts(&report_wd, &install_logs)
        .map_err(|e| CommandError::Other(format!("collecting artifacts failed: {e}")))?;

    let base = session.metadata_mut().base_mut();
    let report = upload_current(base, client, collected.files)
        .await
        .map_err(|e| CommandError::Other(format!("uploading to teregen failed: {e}")))?;

    session.display.println(&format!(
        "document stored in teregen (etag {}); SVN mirror pending — teregen commits it itself \
         within ~10 min",
        report.etag.as_deref().unwrap_or("<none>")
    ));

    let total = report.artifacts.len();
    let mut failed = 0usize;
    for (name, result) in &report.artifacts {
        match result {
            Ok(ArtifactStored::Created) => {
                session.display.println(&format!("artifact {name}: new"));
            }
            Ok(ArtifactStored::Replaced) => {
                session
                    .display
                    .println(&format!("artifact {name}: replaced"));
            }
            Err(e) => {
                failed += 1;
                session
                    .display
                    .println(&format!("artifact {name}: FAILED: {e}"));
            }
        }
    }
    for path in &collected.skipped {
        session
            .display
            .println(&format!("skipped {} (not a regular file)", path.display()));
    }
    Ok(UploadSummary { total, failed })
}

/// Why [`record_onto_document`] did not finish.
pub(crate) enum RecordError {
    /// Nothing was stored: the document edit was rolled back.
    Upload(CommandError),
    /// The document was stored, but this many artifacts were not.
    ArtifactsFailed(usize),
}

/// Applies `edit` to the loaded document, marks `touched` as authored, then
/// uploads the document and the artifacts as `commit` does.
///
/// On an [`Upload`](RecordError::Upload) error the document and its dirty flag
/// are restored, so a refused action leaves nothing half-done.
pub(crate) async fn record_onto_document(
    session: &mut Session,
    client: &TeregenV2,
    touched: &[&str],
    edit: impl FnOnce(&mut ReportDocument),
) -> Result<(), RecordError> {
    let base = session.metadata_mut().base_mut();
    let saved = base.document.clone();
    let was_dirty = base.document_dirty;
    let Some(document) = base.document.as_mut() else {
        return Err(RecordError::Upload(CommandError::Other(
            "no report document is loaded".to_owned(),
        )));
    };
    edit(document);
    base.mark_document_authored(touched);

    match upload_and_report(session, client).await {
        Ok(summary) if summary.failed == 0 => Ok(()),
        Ok(summary) => Err(RecordError::ArtifactsFailed(summary.failed)),
        Err(e) => {
            let base = session.metadata_mut().base_mut();
            base.document = saved;
            base.document_dirty = was_dirty;
            Err(RecordError::Upload(e))
        }
    }
}

#[cfg(test)]
mod tests {
    use wiremock::matchers::{method, path as wpath};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::commands::testkit::teregen::{
        minimal_document, mount_auth_success, set_bare_report_wd, store_path, teregen_v2_client,
    };
    use crate::commands::testkit::{empty_session, matches, session_with_hosts};

    #[test]
    fn complete_offers_allow_stale_and_templates_no_hosts() {
        let (session, _buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "ok");
        let out = Commit.complete(&session, "", "commit ");
        assert!(out.contains(&"--allow-stale".to_owned()), "{out:?}");
        assert!(!out.contains(&"--msg".to_owned()), "{out:?}");
        assert!(out.contains(&"SUSE:Maintenance:1:1".to_owned()), "{out:?}");
        assert!(!out.contains(&"h1".to_owned()), "{out:?}");
    }

    #[test]
    fn name_and_fanout_scope() {
        assert_eq!(Commit.name(), "commit");
        assert_eq!(Commit.scope(), Scope::Explicit);
    }

    #[tokio::test]
    async fn no_report_is_refused() {
        let (mut session, _buf) = empty_session();
        let args = matches(&Commit, &[]);
        let err = Commit.call(&mut session, &args).await.unwrap_err();
        assert!(matches!(err, CommandError::NoDocument), "{err:?}");
    }

    /// A template that loaded with a stale Gitea hash (`load_template
    /// --force-continue`) refuses `commit` unless `--allow-stale` is given —
    /// checked before the document is looked at.
    #[tokio::test]
    async fn refuses_stale_template_without_allow_stale() {
        let (mut session, _buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "ok");
        session.metadata_mut().base_mut().stale_hash_warning =
            Some("template hash mismatch (stale checkout)".to_owned());

        let args = matches(&Commit, &[]);
        let err = Commit.call(&mut session, &args).await.unwrap_err();
        assert!(
            matches!(&err, CommandError::Other(m) if m.contains("--allow-stale")),
            "{err:?}"
        );
    }

    #[tokio::test]
    async fn document_path_happy_prints_stored_and_artifact_lines() {
        let server = MockServer::start().await;
        let (_dir, store_file) = store_path();
        mount_auth_success(&server).await;
        let doc_id = "SUSE:Maintenance:1:1";
        let doc = minimal_document(doc_id);
        Mock::given(method("PUT"))
            .and(wpath(format!("/reports/{doc_id}")))
            .respond_with(
                ResponseTemplate::new(202)
                    .set_body_string(serde_json::to_string(&doc).unwrap())
                    .insert_header("etag", "\"fresh\""),
            )
            .mount(&server)
            .await;
        Mock::given(method("PUT"))
            .and(wpath(format!("/reports/{doc_id}/artifacts/h1.log")))
            .respond_with(ResponseTemplate::new(201))
            .mount(&server)
            .await;

        let (mut session, buf) = session_with_hosts(doc_id, &["h1"], "ok");
        let tmp = set_bare_report_wd(&mut session);
        std::fs::create_dir_all(tmp.path().join("install_logs")).unwrap();
        std::fs::write(tmp.path().join("install_logs/h1.log"), b"log").unwrap();
        session.metadata_mut().base_mut().document = Some(doc);
        session.metadata_mut().base_mut().document_etag = Some("\"stale\"".to_owned());

        let client = teregen_v2_client(&server, store_file);
        commit_document(&mut session, &client).await.unwrap();

        let out = buf.contents();
        assert!(
            out.contains("document stored in teregen (etag \"fresh\")"),
            "{out}"
        );
        assert!(out.contains("artifact h1.log: new"), "{out}");
        assert!(out.contains("legacy log view may lag"), "{out}");
    }

    #[tokio::test]
    async fn document_path_412_errors_and_leaves_document_unchanged() {
        let server = MockServer::start().await;
        let (_dir, store_file) = store_path();
        mount_auth_success(&server).await;
        let doc_id = "SUSE:Maintenance:1:1";
        Mock::given(method("PUT"))
            .and(wpath(format!("/reports/{doc_id}")))
            .respond_with(
                ResponseTemplate::new(412)
                    .set_body_json(serde_json::json!({"error": "precondition failed"})),
            )
            .mount(&server)
            .await;

        let (mut session, _buf) = session_with_hosts(doc_id, &["h1"], "ok");
        let _tmp = set_bare_report_wd(&mut session);
        session.metadata_mut().base_mut().document = Some(minimal_document(doc_id));
        session.metadata_mut().base_mut().document_etag = Some("\"stale\"".to_owned());
        let before_doc = session.metadata().base().document.clone();
        let before_etag = session.metadata().base().document_etag.clone();

        let client = teregen_v2_client(&server, store_file);
        let err = commit_document(&mut session, &client).await.unwrap_err();
        assert!(matches!(err, CommandError::Other(_)));
        assert_eq!(session.metadata().base().document, before_doc);
        assert_eq!(session.metadata().base().document_etag, before_etag);

        let requests = server.received_requests().await.unwrap();
        let artifact_puts = requests
            .iter()
            .filter(|r| r.url.path().contains("/artifacts/"))
            .count();
        assert_eq!(artifact_puts, 0, "a 412 must send no artifacts");
    }

    /// A schema that drifted from this build's refuses every write: no request
    /// of any kind reaches teregen. Mutation caught: removing the guard sends
    /// the document PUT.
    #[tokio::test]
    async fn document_path_schema_drift_sends_nothing() {
        let server = MockServer::start().await;
        let (_dir, store_file) = store_path();
        let doc_id = "SUSE:Maintenance:1:1";
        let (mut session, _buf) = session_with_hosts(doc_id, &["h1"], "ok");
        let _tmp = set_bare_report_wd(&mut session);
        let base = session.metadata_mut().base_mut();
        base.document = Some(minimal_document(doc_id));
        base.document_etag = Some("\"x\"".to_owned());
        base.schema_drift = Some("/properties/kind: \"a\" != \"b\"".to_owned());

        let client = teregen_v2_client(&server, store_file);
        let err = commit_document(&mut session, &client).await.unwrap_err();

        assert!(
            matches!(&err, CommandError::Other(m)
                if m.contains("upgrade mtui") && m.contains("/properties/kind")),
            "{err:?}"
        );
        assert!(server.received_requests().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn document_path_artifact_failure_errors_naming_the_count() {
        let server = MockServer::start().await;
        let (_dir, store_file) = store_path();
        mount_auth_success(&server).await;
        let doc_id = "SUSE:Maintenance:1:1";
        let doc = minimal_document(doc_id);
        Mock::given(method("PUT"))
            .and(wpath(format!("/reports/{doc_id}")))
            .respond_with(
                ResponseTemplate::new(202).set_body_string(serde_json::to_string(&doc).unwrap()),
            )
            .mount(&server)
            .await;
        Mock::given(method("PUT"))
            .and(wpath(format!("/reports/{doc_id}/artifacts/h1.log")))
            .respond_with(ResponseTemplate::new(413))
            .mount(&server)
            .await;

        let (mut session, buf) = session_with_hosts(doc_id, &["h1"], "ok");
        let tmp = set_bare_report_wd(&mut session);
        std::fs::create_dir_all(tmp.path().join("install_logs")).unwrap();
        std::fs::write(tmp.path().join("install_logs/h1.log"), b"log").unwrap();
        session.metadata_mut().base_mut().document = Some(doc);
        session.metadata_mut().base_mut().document_etag = Some("\"x\"".to_owned());

        let client = teregen_v2_client(&server, store_file);
        let err = commit_document(&mut session, &client).await.unwrap_err();
        assert!(
            matches!(&err, CommandError::Other(m) if m.contains("1 of 1 artifacts failed")),
            "{err:?}"
        );

        let out = buf.contents();
        assert!(out.contains("document stored in teregen"), "{out}");
        assert!(out.contains("artifact h1.log: FAILED"), "{out}");
    }

    #[tokio::test]
    async fn document_path_no_etag_sends_zero_requests() {
        let server = MockServer::start().await;
        let (_dir, store_file) = store_path();
        // Deliberately no mocks mounted: any request is a hard failure.
        let doc_id = "SUSE:Maintenance:1:1";
        let (mut session, _buf) = session_with_hosts(doc_id, &["h1"], "ok");
        let _tmp = set_bare_report_wd(&mut session);
        session.metadata_mut().base_mut().document = Some(minimal_document(doc_id));
        // document_etag stays unset.

        let client = teregen_v2_client(&server, store_file);
        let err = commit_document(&mut session, &client).await.unwrap_err();
        assert!(matches!(err, CommandError::Other(_)));

        let requests = server.received_requests().await.unwrap();
        assert!(
            requests.is_empty(),
            "no etag must send nothing: {requests:?}"
        );
    }

    /// Proves `Commit::call` actually routes a document-loaded report to the
    /// teregen builder — using a broken `$OSC_CONFIG` so the
    /// call fails immediately at credential resolution rather than reaching
    /// the network (the real builder must never be exercised against the
    /// ambient environment in a test).
    #[tokio::test]
    #[serial_test::serial(osc_config_env)]
    // `set_var`/`remove_var` are `unsafe` in edition 2024; `#[serial]` makes the
    // mutation of the process-global `$OSC_CONFIG` exclusive.
    #[allow(unsafe_code)]
    async fn document_loaded_dispatches_to_teregen_builder() {
        let doc_id = "SUSE:Maintenance:1:1";
        let (mut session, _buf) = session_with_hosts(doc_id, &["h1"], "ok");
        session.metadata_mut().base_mut().document = Some(minimal_document(doc_id));
        session.metadata_mut().base_mut().document_etag = Some("\"x\"".to_owned());

        let args = matches(&Commit, &[]);
        // SAFETY: inside the `#[serial(osc_config_env)]` critical section.
        unsafe { std::env::set_var("OSC_CONFIG", "/nonexistent/oscrc-for-tests") };
        let res = Commit.call(&mut session, &args).await;
        // SAFETY: still inside that critical section.
        unsafe { std::env::remove_var("OSC_CONFIG") };

        let err = res.unwrap_err();
        assert!(
            matches!(&err, CommandError::Other(m) if m.contains("could not read oscrc credentials")),
            "{err:?}"
        );
    }
}
