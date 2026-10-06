//! `qam.suse.de` testreport preconditions for the native QAM ops.
//!
//! [`assign`](crate::obs::qam::assign) needs only a 200 on the public text log
//! (a plain HTTPS GET, **no OBS auth** — this is the reports host, not the OBS
//! API). [`approve`](crate::obs::qam::approve) /
//! [`reject`](crate::obs::qam::reject) read the tester's `verdict` and
//! `comment` from the report document on TeReGen v2 instead ([`report_verdict`]):
//! mtui no longer writes the text log, and a document upload does not re-render
//! it. The caller skips both for PI/SLFO requests, which carry no maintenance
//! testreport.

use mtui_types::RequestReviewID;
use mtui_types::report_document::Verdict;

use crate::error::HttpError;
use crate::http::{HttpClient, MAX_API_BODY, read_body_capped, sanitize_url};
use crate::teregen::document::{DocumentFetch, TeregenV2};

/// The machine-readable testreport log URL:
/// `reports_url.rstrip('/') + "/" + rrid + "/log"`.
fn log_url(reports_url: &str, rrid: &RequestReviewID) -> String {
    format!("{}/{rrid}/log", reports_url.trim_end_matches('/'))
}

/// GET the testreport log; `None` when absent (404), unreachable, or any other
/// non-2xx status.
///
/// Best-effort by design: a transport failure or a non-404 error status is
/// logged at ERROR and folded to `None`, so a flaky reports host degrades to "no
/// testreport" rather than aborting the operation. Uses a status-preserving GET
/// (`HttpClient::inner`) rather than
/// [`HttpClient::get_bytes`](crate::http::HttpClient::get_bytes), which raises
/// on non-2xx and so cannot tell a 404 from a 200.
pub(crate) async fn fetch_testreport_log(
    http: &HttpClient,
    reports_url: &str,
    rrid: &RequestReviewID,
) -> Option<String> {
    let url = log_url(reports_url, rrid);
    // The reports URL may carry credentials; never log them verbatim.
    let safe_url = sanitize_url(&url);
    let response = match http.inner().get(&url).send().await {
        Ok(response) => response,
        Err(e) => {
            // Convert first: a raw `reqwest::Error` would append the unsafe
            // URL right next to the sanitized one (#431).
            tracing::error!(
                "could not fetch testreport {safe_url}: {}",
                HttpError::from(e)
            );
            return None;
        }
    };
    let status = response.status();
    if status == reqwest::StatusCode::NOT_FOUND {
        return None;
    }
    if !status.is_success() {
        tracing::error!("testreport {safe_url} returned {}", status.as_u16());
        return None;
    }
    match read_body_capped(response, MAX_API_BODY).await {
        Ok(bytes) => Some(String::from_utf8_lossy(&bytes).into_owned()),
        Err(e) => {
            tracing::error!("could not read testreport body {safe_url}: {e}");
            None
        }
    }
}

/// The report document's `verdict` and top-level `comment`, fetched fresh.
///
/// # Errors
///
/// The cause, ready to embed in a refusal: no document, a stale one, or the
/// transport/status failure.
pub(crate) async fn report_verdict(
    v2: &TeregenV2,
    rrid: &RequestReviewID,
) -> Result<(Option<Verdict>, Option<String>), String> {
    match v2.fetch_document(&rrid.to_string(), None).await {
        Ok(DocumentFetch::Fresh { document, .. }) => {
            Ok((document.verdict.0, document.comment.0.clone()))
        }
        Ok(DocumentFetch::NotModified) => {
            Err("the server answered 304 to an unconditional request".to_owned())
        }
        Err(e) => Err(e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::VerifyPolicy;

    fn http() -> HttpClient {
        HttpClient::new(VerifyPolicy::Default(true)).unwrap()
    }

    // The 404 -> None path is covered end-to-end by the qam integration test
    // `assign_refused_when_no_testreport`; these cover the other best-effort
    // arms directly.
    #[tokio::test]
    async fn fetch_testreport_log_none_on_server_error() {
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
        let rrid = RequestReviewID::parse("SUSE:Maintenance:1:56789").unwrap();
        assert!(
            fetch_testreport_log(&http(), &server.uri(), &rrid)
                .await
                .is_none()
        );
    }

    #[tokio::test]
    async fn fetch_testreport_log_none_on_connection_error() {
        // A reserved-but-unroutable base URL: the transport-layer failure must
        // fold to None rather than propagate.
        let rrid = RequestReviewID::parse("SUSE:Maintenance:1:56789").unwrap();
        assert!(
            fetch_testreport_log(&http(), "http://127.0.0.1:1/nope", &rrid)
                .await
                .is_none()
        );
    }
}
