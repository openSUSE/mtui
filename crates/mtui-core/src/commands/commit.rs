//! The `commit` command: commits the testing template working copy to SVN, or
//! uploads it to teregen's v2 API when the loaded report came from a v2
//! document.

use async_trait::async_trait;
use clap::{Arg, ArgAction, ArgMatches};
use mtui_datasources::teregen::{ArtifactStored, TeregenV2};
use mtui_testreport::{
    TokioSvnRunner, collect_artifacts, detect_system, svn_commit_testreport, system_info,
    upload_current,
};

use super::apicall::teregen_v2_writer;
use super::support::{complete_with_templates, stale_hash_gate};
use crate::command::{Command, Scope};
use crate::error::{CommandError, CommandResult};
use crate::session::Session;

/// Commits the testing template working copy, persisting the final template
/// after testing. Requires a loaded report.
///
/// A report loaded from a v2 document goes through `commit_document`
/// instead of `svn`: it uploads the document (conditional on its stored
/// `ETag`) and its local artifacts through teregen's write API. Every other
/// report still runs `svn ci` — without `-m/--msg` a message is generated
/// from the local system info (the export footer, via
/// `system_info(..., prefix="committed from")`), so the commit never opens
/// `svn`'s editor. `-m/--msg` on the document path is accepted but ignored
/// (teregen writes its own commit message) and noted as such.
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
        Some("Commits the testing template working copy to SVN.")
    }

    fn scope(&self) -> Scope {
        Scope::Explicit
    }

    fn configure(&self, cmd: clap::Command) -> clap::Command {
        cmd.arg(
            Arg::new("msg")
                .short('m')
                .long("msg")
                .action(ArgAction::Append)
                .num_args(1..)
                .value_name("MSG")
                .help("commit message"),
        )
        .arg(
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
        complete_with_templates(
            session,
            &[&["-m", "--msg"], &["--allow-stale"]],
            Vec::new(),
            line,
            text,
        )
    }

    async fn call(&self, session: &mut Session, args: &ArgMatches) -> CommandResult {
        stale_hash_gate(session, args.get_flag("allow_stale"))?;

        let msg_tokens: Vec<String> = args
            .try_get_many::<String>("msg")
            .ok()
            .flatten()
            .map(|it| it.cloned().collect::<Vec<_>>())
            .unwrap_or_default();

        // A report loaded from a v2 document goes through teregen's write
        // API instead of `svn ci`; a report loaded from SVN is unaffected.
        if session.metadata().base().document.is_some() {
            let client = teregen_v2_writer(session)?;
            return commit_document(session, &client, !msg_tokens.is_empty()).await;
        }

        let checkout = session
            .metadata()
            .base()
            .report_wd()
            .map_err(|e| CommandError::Other(format!("no report working directory: {e}")))?;
        let install_logs = session.config.install_logs.clone();

        let msg: Vec<String> = if msg_tokens.is_empty() {
            let (distro, verid, kernel) = detect_system();
            let default = system_info(
                &distro,
                &verid,
                &kernel,
                &session.config.session_user,
                "committed from",
            )
            .trim_end()
            .to_owned();
            vec!["-m".to_owned(), default]
        } else {
            vec!["-m".to_owned(), format!("\"{}\"", msg_tokens.join(" "))]
        };

        let runner = TokioSvnRunner;
        svn_commit_testreport(&runner, &checkout, &install_logs, &msg)
            .await
            .map_err(|e| CommandError::Other(format!("committing template failed: {e}")))?;
        session.display.println(&format!(
            "testreport committed: {}",
            session.metadata().fancy_report_url()
        ));
        Ok(())
    }
}

/// The document-path half of [`Commit::call`]: upload the loaded document
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
async fn commit_document(
    session: &mut Session,
    client: &TeregenV2,
    msg_given: bool,
) -> CommandResult {
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
    if msg_given {
        session.display.println(
            "note: --msg ignored on the document path (teregen writes its own SVN message)",
        );
    }
    session
        .display
        .println("note: the legacy log view may lag (teregen T4)");

    if failed > 0 {
        return Err(CommandError::Other(format!(
            "{failed} of {total} artifacts failed; re-run commit to retry"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::str::FromStr;

    use mtui_datasources::teregen::{TeregenAuth, TokenStore};
    use mtui_datasources::{HttpClient, VerifyPolicy};
    use mtui_types::report_document::ReportDocument;
    use wiremock::matchers::{method, path as wpath};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::commands::testkit::{empty_session, matches, session_with_hosts};

    #[test]
    fn complete_offers_msg_flag_and_templates_no_hosts() {
        let (session, _buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "ok");
        let out = Commit.complete(&session, "", "commit ");
        assert!(
            out.contains(&"-m".to_owned()) && out.contains(&"--msg".to_owned()),
            "{out:?}"
        );
        assert!(out.contains(&"SUSE:Maintenance:1:1".to_owned()), "{out:?}");
        assert!(!out.contains(&"h1".to_owned()), "{out:?}");
    }

    #[test]
    fn name_and_fanout_scope() {
        assert_eq!(Commit.name(), "commit");
        assert_eq!(Commit.scope(), Scope::Explicit);
    }

    #[tokio::test]
    async fn no_report_errors_before_shelling_out() {
        let (mut session, _buf) = empty_session();
        let args = matches(&Commit, &[]);
        let err = Commit.call(&mut session, &args).await.unwrap_err();
        assert!(matches!(err, CommandError::Other(_)));
    }

    /// A template that loaded with a stale Gitea hash (`load_template
    /// --force-continue`) refuses `commit` unless `--allow-stale` is given —
    /// checked before any `svn` shell-out, same as `no_report_errors_before_shelling_out`.
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

    /// A successful commit must print the report URL, so the MCP result is never
    /// empty.
    #[tokio::test]
    async fn success_prints_committed_url_to_display() {
        if std::process::Command::new("svn")
            .arg("--version")
            .output()
            .is_err()
        {
            return; // svn not installed in this environment
        }
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let wc = tmp.path().join("wc");
        assert!(
            std::process::Command::new("svnadmin")
                .args(["create", repo.to_str().unwrap()])
                .status()
                .unwrap()
                .success()
        );
        let repo_url = format!("file://{}", repo.display());
        assert!(
            std::process::Command::new("svn")
                .args(["checkout", &repo_url, wc.to_str().unwrap()])
                .status()
                .unwrap()
                .success()
        );

        // The commit runs `svn add --force install_logs`.
        std::fs::create_dir_all(wc.join("install_logs")).unwrap();

        let (mut session, buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "ok");
        session.metadata_mut().base_mut().path = Some(wc.join("metadata.json"));

        let args = matches(&Commit, &["-m", "test commit"]);
        Commit.call(&mut session, &args).await.unwrap();

        let out = buf.contents();
        assert!(out.contains("testreport committed:"), "{out:?}");
    }

    /// `--allow-stale` permits committing a template that loaded with a stale
    /// Gitea hash, past the gate `refuses_stale_template_without_allow_stale` pins.
    #[tokio::test]
    async fn allow_stale_permits_commit_of_a_stale_template() {
        if std::process::Command::new("svn")
            .arg("--version")
            .output()
            .is_err()
        {
            return; // svn not installed in this environment
        }
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let wc = tmp.path().join("wc");
        assert!(
            std::process::Command::new("svnadmin")
                .args(["create", repo.to_str().unwrap()])
                .status()
                .unwrap()
                .success()
        );
        let repo_url = format!("file://{}", repo.display());
        assert!(
            std::process::Command::new("svn")
                .args(["checkout", &repo_url, wc.to_str().unwrap()])
                .status()
                .unwrap()
                .success()
        );
        std::fs::create_dir_all(wc.join("install_logs")).unwrap();

        let (mut session, buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "ok");
        session.metadata_mut().base_mut().path = Some(wc.join("metadata.json"));
        session.metadata_mut().base_mut().stale_hash_warning =
            Some("template hash mismatch (stale checkout)".to_owned());

        let args = matches(&Commit, &["-m", "test commit", "--allow-stale"]);
        Commit.call(&mut session, &args).await.unwrap();

        assert!(buf.contents().contains("testreport committed:"));
    }

    // --- document path (teregen v2) ---

    const NONCE: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    const PRINCIPAL: &str = "alice";

    fn fixture(name: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/obs")
            .join(name)
    }

    fn minimal_document(id: &str) -> ReportDocument {
        let raw = format!(
            r#"{{
                "schema_version": "1.0", "id": "{id}", "kind": "pi",
                "workflow": "obs", "generated_at": "2026-01-01T00:00:00Z",
                "verdict": null, "comment": null,
                "people": {{"testers": [], "reviewer": {{"name": null}}}},
                "update": {{"packager": "p", "source_packages": ["a"], "origin": {{}},
                           "products": [{{"name": "n", "version": "v", "archs": ["x86_64"]}}],
                           "patches": [{{"id": "1", "title": "t"}}]}},
                "install": {{"repository": "http://x/", "targets": [{{
                    "product": "n", "version": "v", "arch": "x86_64",
                    "repository": "http://x/r", "binaries": {{"a": "1-1.x86_64"}}
                }}], "test_platforms": []}},
                "issues": {{}}, "testing": {{}}
            }}"#
        );
        ReportDocument::from_str(&raw).expect("fixture document parses")
    }

    fn auth_for(server: &MockServer, store_path: PathBuf) -> TeregenAuth {
        TeregenAuth::new(
            server.uri(),
            PRINCIPAL.to_owned(),
            Some(fixture("id_ed25519")),
            None,
            HttpClient::new(VerifyPolicy::Default(true)).expect("client builds"),
        )
        .with_store(Some(TokenStore::at(store_path)))
    }

    fn store_path() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let store_file = dir.path().join("teregen-token.json");
        (dir, store_file)
    }

    fn teregen_v2_client(server: &MockServer, store_file: PathBuf) -> TeregenV2 {
        let http = HttpClient::new(VerifyPolicy::Default(false)).unwrap();
        TeregenV2::with_client(http, &server.uri()).with_auth(auth_for(server, store_file))
    }

    async fn mount_auth_success(server: &MockServer) {
        Mock::given(method("POST"))
            .and(wpath("/auth/ssh/challenge"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"nonce": NONCE})),
            )
            .mount(server)
            .await;
        Mock::given(method("POST"))
            .and(wpath("/auth/ssh/verify"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "token": "a".repeat(64),
            })))
            .mount(server)
            .await;
    }

    /// Sets a loaded report's `path` to a fresh, empty working directory, so
    /// `report_wd()`/`collect_artifacts` resolve without touching SVN.
    fn set_bare_report_wd(session: &mut Session) -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        session.metadata_mut().base_mut().path = Some(tmp.path().join("metadata.json"));
        tmp
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
        commit_document(&mut session, &client, false).await.unwrap();

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
        let err = commit_document(&mut session, &client, false)
            .await
            .unwrap_err();
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
        let err = commit_document(&mut session, &client, false)
            .await
            .unwrap_err();
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
        let err = commit_document(&mut session, &client, false)
            .await
            .unwrap_err();
        assert!(matches!(err, CommandError::Other(_)));

        let requests = server.received_requests().await.unwrap();
        assert!(
            requests.is_empty(),
            "no etag must send nothing: {requests:?}"
        );
    }

    /// `-m/--msg` is accepted but noted as ignored on the document path.
    #[tokio::test]
    async fn document_path_notes_ignored_msg() {
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

        let (mut session, buf) = session_with_hosts(doc_id, &["h1"], "ok");
        let _tmp = set_bare_report_wd(&mut session);
        session.metadata_mut().base_mut().document = Some(doc);
        session.metadata_mut().base_mut().document_etag = Some("\"x\"".to_owned());

        let client = teregen_v2_client(&server, store_file);
        commit_document(&mut session, &client, true).await.unwrap();

        assert!(
            buf.contents().contains("--msg ignored"),
            "{}",
            buf.contents()
        );
    }

    /// Proves `Commit::call` actually routes a document-loaded report to the
    /// teregen builder instead of `svn` — using a broken `$OSC_CONFIG` so the
    /// call fails immediately at credential resolution rather than reaching
    /// the network (the real builder must never be exercised against the
    /// ambient environment in a test).
    #[tokio::test]
    #[serial_test::serial(osc_config_env)]
    // `set_var`/`remove_var` are `unsafe` in edition 2024; `#[serial]` makes the
    // mutation of the process-global `$OSC_CONFIG` exclusive.
    #[allow(unsafe_code)]
    async fn document_loaded_dispatches_to_teregen_builder_not_svn() {
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
