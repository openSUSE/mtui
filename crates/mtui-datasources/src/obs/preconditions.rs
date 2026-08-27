//! `qam.suse.de` testreport preconditions for the native QAM ops.
//!
//! A plain HTTPS GET of the machine-readable testreport log — **no OBS auth**,
//! this is the public reports host, not the OBS API — applying the same guards
//! the `osc qam` plugin does: [`assign`](crate::obs::qam::assign) needs only a
//! 200, while [`approve`](crate::obs::qam::approve) /
//! [`reject`](crate::obs::qam::reject) also require `SUMMARY: PASSED` /
//! `SUMMARY: FAILED` plus, for reject, a non-empty `comment:`. The caller skips
//! it for PI/SLFO requests, which carry no maintenance testreport.

use std::sync::LazyLock;

use mtui_config::SslVerify;
use regex::Regex;

use mtui_types::RequestReviewID;

use crate::error::HttpError;
use crate::http::{HttpClient, MAX_API_BODY, VerifyPolicy, read_body_capped, sanitize_url};

/// Captures the whole trimmed `SUMMARY:` value, not just the first token, so a
/// trailing qualifier ("PASSED with notes") reads as UNKNOWN rather than
/// approving on the first word.
static SUMMARY_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?m)^SUMMARY:\s*(.+?)\s*$").expect("static SUMMARY regex"));

/// Capture the `comment:` value.
static COMMENT_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?m)^comment:\s*(.*)$").expect("static comment regex"));

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
    reports_url: &str,
    ssl_verify: &SslVerify,
    rrid: &RequestReviewID,
) -> Option<String> {
    let url = log_url(reports_url, rrid);
    // The reports URL may carry credentials; never log them verbatim.
    let safe_url = sanitize_url(&url);
    let client = match HttpClient::new(VerifyPolicy::from_config(ssl_verify)) {
        Ok(client) => client,
        Err(e) => {
            tracing::error!("could not build testreport HTTP client for {safe_url}: {e}");
            return None;
        }
    };
    let response = match client.inner().get(&url).send().await {
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

/// The upper-cased `SUMMARY:` value of a testreport log (else `UNKNOWN`).
///
/// The WHOLE trimmed value is upper-cased, so "PASSED with notes" becomes
/// `PASSED WITH NOTES` — not exactly `PASSED`.
#[must_use]
pub(crate) fn summary(log: &str) -> String {
    SUMMARY_RE
        .captures(log)
        .and_then(|c| c.get(1))
        .map_or_else(|| "UNKNOWN".to_owned(), |m| m.as_str().to_uppercase())
}

/// The `comment:` value of a testreport log (empty when absent).
#[must_use]
pub(crate) fn comment(log: &str) -> String {
    COMMENT_RE
        .captures(log)
        .and_then(|c| c.get(1))
        .map_or_else(String::new, |m| m.as_str().trim().to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summary_captures_whole_value_not_first_token() {
        assert_eq!(summary("SUMMARY: PASSED\n"), "PASSED");
        assert_eq!(summary("SUMMARY: PASSED with notes\n"), "PASSED WITH NOTES");
    }

    #[test]
    fn summary_unknown_when_absent() {
        assert_eq!(summary("no summary here\n"), "UNKNOWN");
    }

    #[test]
    fn comment_extracts_trimmed_value() {
        assert_eq!(comment("SUMMARY: FAILED\ncomment: broken\n"), "broken");
    }

    #[test]
    fn comment_empty_when_absent() {
        assert_eq!(comment("SUMMARY: FAILED\n"), "");
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
            fetch_testreport_log(&server.uri(), &SslVerify::Enabled, &rrid)
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
            fetch_testreport_log("http://127.0.0.1:1/nope", &SslVerify::Enabled, &rrid)
                .await
                .is_none()
        );
    }
}
