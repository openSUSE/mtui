//! The `checkout` command (refreshes the loaded report document from teregen).

use async_trait::async_trait;
use clap::{Arg, ArgAction, ArgMatches};
use mtui_datasources::teregen::TeregenV2;
use mtui_testreport::{Refreshed, refresh_document};

use super::support::{complete_with_templates, document_edits_guard, require_document};
use crate::command::{Command, Scope};
use crate::error::{CommandError, CommandResult};
use crate::session::Session;

/// Re-fetches the loaded report document from teregen (conditional on its
/// `ETag`), adopting it when it changed. That replaces the local document, so it
/// is refused before any I/O while it holds edits no `commit` has uploaded,
/// unless `--discard-authored` is passed.
pub struct Checkout;

#[async_trait]
impl Command for Checkout {
    fn name(&self) -> &'static str {
        "checkout"
    }

    fn about(&self) -> Option<&'static str> {
        Some("Refreshes the loaded report document from teregen.")
    }

    fn scope(&self) -> Scope {
        Scope::Fanout
    }

    fn configure(&self, cmd: clap::Command) -> clap::Command {
        cmd.arg(
            Arg::new("discard_authored")
                .long("discard-authored")
                .action(ArgAction::SetTrue)
                .help(
                    "refresh the report document even though it holds edits that were \
                     never committed",
                ),
        )
    }

    fn complete(&self, session: &Session, text: &str, line: &str) -> Vec<String> {
        complete_with_templates(session, &[&["--discard-authored"]], Vec::new(), line, text)
    }

    async fn call(&self, session: &mut Session, args: &ArgMatches) -> CommandResult {
        let discard_authored = args.get_flag("discard_authored");
        let loaded = session.metadata().rrid().map(|r| r.to_string());
        if let Some(rrid) = &loaded {
            document_edits_guard(session, rrid, discard_authored)?;
        }

        require_document(session)?;
        let http = session
            .http_client()
            .map_err(|e| CommandError::Other(format!("could not build TeReGen client: {e}")))?;
        let client = TeregenV2::with_client(http, &session.config.teregen_api_v2);
        refresh_step(session, &client, discard_authored).await
    }
}

/// Re-fetches the active report's document through `client` and reports the
/// outcome. `force` ignores the stored `ETag`: local edits are being discarded,
/// so the server's copy must replace them even when it has not changed.
async fn refresh_step(
    session: &mut Session,
    client: &TeregenV2,
    force: bool,
) -> Result<(), CommandError> {
    let outcome = refresh_document(session.metadata_mut(), client, force)
        .await
        .map_err(|e| CommandError::Other(e.to_string()))?;
    let line = match outcome {
        Refreshed::Unchanged => format!(
            "document unchanged (etag {})",
            etag_text(session.metadata().base().document_etag.as_deref())
        ),
        Refreshed::Updated { etag } => {
            format!("document refreshed (etag {})", etag_text(etag.as_deref()))
        }
    };
    session.display.println(&line);
    Ok(())
}

fn etag_text(etag: Option<&str>) -> &str {
    etag.unwrap_or("none")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::testkit::{Buffer, empty_session, matches, session_with_hosts};
    use mtui_types::report_document::ReportDocument;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[test]
    fn name_and_fanout_scope() {
        assert_eq!(Checkout.name(), "checkout");
        assert_eq!(Checkout.scope(), Scope::Fanout);
    }

    #[test]
    fn complete_offers_template_flags_and_rrids_but_no_hosts() {
        let (session, _buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "linux");
        let out = Checkout.complete(&session, "", "checkout ");
        assert!(out.contains(&"-T".to_owned()), "{out:?}");
        assert!(out.contains(&"--all-templates".to_owned()), "{out:?}");
        assert!(out.contains(&"SUSE:Maintenance:1:1".to_owned()), "{out:?}");
        // No host names for a template-scoped command.
        assert!(!out.contains(&"h1".to_owned()), "{out:?}");
    }

    #[tokio::test]
    async fn no_report_is_refused() {
        let (mut session, _buf) = empty_session();
        let args = matches(&Checkout, &[]);
        let err = Checkout.call(&mut session, &args).await.unwrap_err();
        assert!(matches!(err, CommandError::NoDocument), "{err:?}");
    }

    const DOC_ID: &str = "SUSE:PI:16.0:1";
    const OLD_ETAG: &str = "\"etag-old\"";
    const NEW_ETAG: &str = "\"etag-new\"";

    fn document(comment: Option<&str>) -> ReportDocument {
        let mut doc: ReportDocument =
            include_str!("../../../mtui-types/tests/fixtures/document/pi.json")
                .parse()
                .expect("fixture parses");
        doc.comment = mtui_types::report_document::Req(comment.map(str::to_owned));
        doc
    }

    /// A session whose active report carries `document` (stored under
    /// [`OLD_ETAG`]) and talks to `server`, as on the document path.
    fn document_session(server: &MockServer, dirty: bool) -> (Session, Buffer) {
        let (mut session, buf) = session_with_hosts(DOC_ID, &["h1"], "ok");
        session.config.teregen_api_v2 = server.uri();
        let base = session.metadata_mut().base_mut();
        base.document = Some(document(Some("local")));
        base.document_etag = Some(OLD_ETAG.to_owned());
        base.document_dirty = dirty;
        (session, buf)
    }

    fn client(server: &MockServer) -> TeregenV2 {
        let http =
            mtui_datasources::HttpClient::new(mtui_datasources::VerifyPolicy::Default(false))
                .unwrap();
        TeregenV2::with_client(http, &server.uri())
    }

    async fn mount_document(server: &MockServer, response: ResponseTemplate) {
        Mock::given(method("GET"))
            .and(path(format!("/reports/{DOC_ID}")))
            .respond_with(response)
            .mount(server)
            .await;
    }

    #[tokio::test]
    async fn refresh_step_reports_an_unchanged_document() {
        let server = MockServer::start().await;
        mount_document(&server, ResponseTemplate::new(304)).await;
        let (mut session, buf) = document_session(&server, false);

        refresh_step(&mut session, &client(&server), false)
            .await
            .unwrap();

        assert!(
            buf.contents()
                .contains(&format!("document unchanged (etag {OLD_ETAG})")),
            "{:?}",
            buf.contents()
        );
    }

    #[tokio::test]
    async fn refresh_step_adopts_a_newer_document() {
        let server = MockServer::start().await;
        let body = serde_json::to_string(&document(Some("server"))).unwrap();
        mount_document(
            &server,
            ResponseTemplate::new(200)
                .set_body_string(body)
                .insert_header("etag", NEW_ETAG),
        )
        .await;
        let (mut session, buf) = document_session(&server, false);

        refresh_step(&mut session, &client(&server), false)
            .await
            .unwrap();

        assert!(
            buf.contents()
                .contains(&format!("document refreshed (etag {NEW_ETAG})")),
            "{:?}",
            buf.contents()
        );
        let base = session.metadata().base();
        assert_eq!(base.document, Some(document(Some("server"))));
        assert_eq!(base.document_etag.as_deref(), Some(NEW_ETAG));
    }

    #[tokio::test]
    async fn refresh_step_surfaces_a_refusal_as_an_error() {
        let server = MockServer::start().await;
        mount_document(&server, ResponseTemplate::new(409)).await;
        let (mut session, _buf) = document_session(&server, false);

        let err = refresh_step(&mut session, &client(&server), false)
            .await
            .unwrap_err();

        assert!(
            matches!(&err, CommandError::Other(m) if m.contains("stale")),
            "{err:?}"
        );
        assert_eq!(
            session.metadata().base().document,
            Some(document(Some("local")))
        );
    }

    #[tokio::test]
    async fn a_dirty_document_is_refused_before_any_request() {
        let server = MockServer::start().await;
        let (mut session, _buf) = document_session(&server, true);

        let args = matches(&Checkout, &[]);
        let err = Checkout.call(&mut session, &args).await.unwrap_err();

        assert!(
            matches!(&err, CommandError::Other(m) if m.contains("--discard-authored")),
            "{err:?}"
        );
        assert!(server.received_requests().await.unwrap().is_empty());
        assert!(session.metadata().base().document_dirty);
    }

    /// `--discard-authored` through the whole command: the guard is lifted and
    /// the document is re-fetched unconditionally, so the server's copy
    /// replaces the local edits even if its `ETag` is unchanged.
    #[tokio::test]
    async fn discard_authored_refreshes_a_dirty_document() {
        let server = MockServer::start().await;
        let body = serde_json::to_string(&document(Some("server"))).unwrap();
        mount_document(
            &server,
            ResponseTemplate::new(200)
                .set_body_string(body)
                .insert_header("etag", NEW_ETAG),
        )
        .await;
        let (mut session, buf) = document_session(&server, true);

        let args = matches(&Checkout, &["--discard-authored"]);
        Checkout.call(&mut session, &args).await.unwrap();

        assert!(
            buf.contents().contains("document refreshed"),
            "{:?}",
            buf.contents()
        );
        let base = session.metadata().base();
        assert!(!base.document_dirty);
        assert_eq!(base.document, Some(document(Some("server"))));
        let requests = server.received_requests().await.unwrap();
        assert!(requests[0].headers.get("if-none-match").is_none());
    }

    /// There is no working copy: the command only refreshes the document.
    #[tokio::test]
    async fn a_document_report_refreshes_the_document_only() {
        let server = MockServer::start().await;
        let body = serde_json::to_string(&document(Some("server"))).unwrap();
        mount_document(
            &server,
            ResponseTemplate::new(200)
                .set_body_string(body)
                .insert_header("etag", NEW_ETAG),
        )
        .await;
        let (mut session, buf) = document_session(&server, false);
        let tmp = tempfile::tempdir().unwrap();
        session.metadata_mut().base_mut().path = Some(tmp.path().join("log"));

        let args = matches(&Checkout, &[]);
        Checkout.call(&mut session, &args).await.unwrap();

        let out = buf.contents();
        assert!(out.contains("document refreshed"), "{out:?}");
        assert_eq!(std::fs::read_dir(tmp.path()).unwrap().count(), 0);
    }
}
