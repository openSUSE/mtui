//! `qam.suse.de` testreport preconditions for the native QAM ops.
//!
//! Ported from upstream `mtui/data_sources/obs/preconditions.py`. A plain HTTPS
//! GET of the machine-readable testreport log (**no OBS auth** — this is the
//! public reports host, not the OBS API), applying the same guards the `osc qam`
//! plugin does: [`assign`](crate::obs::qam::assign) only needs the log to EXIST
//! (a 200); [`approve`](crate::obs::qam::approve) / [`reject`](crate::obs::qam::reject)
//! additionally require `SUMMARY: PASSED` / `SUMMARY: FAILED` plus a non-empty
//! `comment:` for reject. Skipped by the caller for PI/SLFO requests, which
//! carry no maintenance testreport.

use std::sync::LazyLock;

use mtui_config::SslVerify;
use regex::Regex;

use mtui_types::RequestReviewID;

use crate::http::{HttpClient, MAX_API_BODY, VerifyPolicy, read_body_capped, sanitize_url};

/// Capture the whole trimmed `SUMMARY:` value, not just the first token, so a
/// trailing qualifier ("PASSED with notes") reads as UNKNOWN — matching the
/// plugin's whole-value compare rather than approving/rejecting on the first
/// word. Mirrors upstream `_SUMMARY_RE` (`^SUMMARY:\s*(.+?)\s*$`, MULTILINE).
static SUMMARY_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?m)^SUMMARY:\s*(.+?)\s*$").expect("static SUMMARY regex"));

/// Capture the `comment:` value. Mirrors upstream `_COMMENT_RE`
/// (`^comment:\s*(.*)$`, MULTILINE).
static COMMENT_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?m)^comment:\s*(.*)$").expect("static comment regex"));

/// The machine-readable testreport log URL, mirroring upstream `_log_url`
/// (`reports_url.rstrip('/') + "/" + rrid + "/log"`).
fn log_url(reports_url: &str, rrid: &RequestReviewID) -> String {
    format!("{}/{rrid}/log", reports_url.trim_end_matches('/'))
}

/// GET the testreport log; `None` when absent (404), unreachable, or any other
/// non-2xx status.
///
/// Best-effort by design (mirrors upstream `fetch_testreport_log`): a transport
/// failure or a non-404 error status is logged at ERROR and folded to `None`, so
/// a flaky reports host degrades to "no testreport" rather than aborting the
/// operation. Uses a status-preserving GET (`HttpClient::inner`) rather than
/// [`HttpClient::get_bytes`](crate::http::HttpClient::get_bytes), which raises on
/// non-2xx and so cannot tell a 404 apart from a 200.
pub async fn fetch_testreport_log(
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
            tracing::error!("could not fetch testreport {safe_url}: {e}");
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
/// Mirrors upstream `summary`: the WHOLE trimmed captured value is upper-cased,
/// so "PASSED with notes" becomes `PASSED WITH NOTES` (i.e. not exactly
/// `PASSED`).
#[must_use]
pub fn summary(log: &str) -> String {
    SUMMARY_RE
        .captures(log)
        .and_then(|c| c.get(1))
        .map_or_else(|| "UNKNOWN".to_owned(), |m| m.as_str().to_uppercase())
}

/// The `comment:` value of a testreport log (empty when absent).
///
/// Mirrors upstream `comment`.
#[must_use]
pub fn comment(log: &str) -> String {
    COMMENT_RE
        .captures(log)
        .and_then(|c| c.get(1))
        .map_or_else(String::new, |m| m.as_str().trim().to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    // Ported from upstream test_obs_qam.py::test_summary_captures_whole_value_not_first_token.
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
    // arms (non-404 status and an unreachable host) directly.
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
        // A reserved-but-unroutable base URL: the GET fails at the transport
        // layer and is folded to None rather than propagating.
        let rrid = RequestReviewID::parse("SUSE:Maintenance:1:56789").unwrap();
        assert!(
            fetch_testreport_log("http://127.0.0.1:1/nope", &SslVerify::Enabled, &rrid)
                .await
                .is_none()
        );
    }
}
