//! The v2 read client for TeReGen's JSON report document
//! (`qam.suse.de/api/v2`), an alternative source for the typed
//! `ReportDocument` model alongside the SVN `metadata.json` pair.
//!
//! Reads only: [`fetch_document`](TeregenV2::fetch_document) sends no
//! `Authorization` header — reads are anonymous on the live server, and
//! sending a possibly-stale bearer can only turn a working 200 into a 401.
//! `If-None-Match` is set whenever the caller has a previous ETag, so an
//! unchanged document costs a `304` instead of a re-parse.

use mtui_config::Config;
use mtui_types::report_document::{DocumentError, ReportDocument, parse_and_warn_on_dropped_keys};
use reqwest::StatusCode;
use reqwest::header::{ETAG, IF_NONE_MATCH};
use thiserror::Error;

use crate::error::HttpError;
use crate::http::{
    HTTP_TIMEOUT, HttpClient, MAX_API_BODY, VerifyPolicy, read_body_capped, resolve_verify,
};

/// The outcome of a successful [`TeregenV2::fetch_document`] call.
#[derive(Debug, Clone, PartialEq)]
pub enum DocumentFetch {
    /// A fresh (or first-seen) document. `raw` is the exact response body,
    /// kept alongside the parsed form for callers that want to hash it or
    /// commit it verbatim as a fixture.
    Fresh {
        /// The parsed document. Boxed: `ReportDocument` is large relative to
        /// the unit `NotModified` variant, and this call sits on a hot,
        /// per-load path.
        document: Box<ReportDocument>,
        /// The raw JSON body the document was parsed from.
        raw: String,
        /// The response's `ETag` header, when the server sent one.
        etag: Option<String>,
    },
    /// The caller's `etag` matched (`304 Not Modified`): the previously-seen
    /// document is still current.
    NotModified,
}

/// Errors from [`TeregenV2::fetch_document`] — one variant per distinct
/// refusal a caller should message differently. Never constructed with
/// a raw response body or URL: see the field docs.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum TeregenV2Error {
    /// `404`: no document has been generated for this id yet.
    #[error("no document yet — run `regenerate` first")]
    NotFound,
    /// `409`: the server holds a document but flags it stale (source moved on
    /// since it was generated).
    #[error("the server's document is stale — run `regenerate`")]
    Stale,
    /// `503`: the document is being generated right now.
    #[error("the server is still generating this document")]
    Generating,
    /// `400`: the server rejects the id itself (`GET /updates` can
    /// advertise an id both APIs reject).
    #[error("the server rejects this id: {detail}")]
    RejectedId {
        /// The server's own error message, verbatim.
        detail: String,
    },
    /// The response was a `200`/`304` but the body did not parse as a
    /// [`ReportDocument`].
    #[error("the document failed to parse: {0}")]
    Invalid(#[source] DocumentError),
    /// A transport failure or an unmodelled non-2xx status. Never carries a
    /// URL: `e` is always converted through [`HttpError`] first (#431).
    #[error("teregen v2 request failed: {0}")]
    Transport(String),
    /// The response body exceeded [`MAX_API_BODY`].
    #[error("response body exceeds the {MAX_API_BODY}-byte limit")]
    BodyTooLarge,
}

/// Best-known-error-message length before it is truncated in
/// [`TeregenV2Error::RejectedId`] — the server's own text, never mtui's
/// hostile-input surface, but still capped so a misbehaving proxy cannot dump
/// an arbitrary amount of text into a `CommandError`.
const MAX_DETAIL_LEN: usize = 2048;

/// Read-only v2 client for TeReGen's JSON report document API.
#[derive(Debug, Clone)]
pub struct TeregenV2 {
    base: String,
    http: HttpClient,
}

impl TeregenV2 {
    /// Build a client targeting `apiurl`, deriving the TLS posture from
    /// `config.ssl_verify` — mirrors [`crate::teregen::TeReGen::new`].
    ///
    /// # Errors
    ///
    /// Returns [`HttpError`] if the shared HTTP client cannot be built (e.g. a
    /// configured CA bundle cannot be read).
    pub fn new(config: &Config, apiurl: &str) -> crate::Result<Self> {
        let verify: VerifyPolicy = resolve_verify(
            VerifyPolicy::Default(true),
            Some(VerifyPolicy::from_config(&config.ssl_verify)),
        );
        let http = HttpClient::new(verify)?;
        Ok(Self::with_client(http, apiurl))
    }

    /// Build a client from an already-constructed [`HttpClient`], bypassing
    /// [`Config`] — the composition-root / test seam.
    #[must_use]
    pub fn with_client(http: HttpClient, apiurl: &str) -> Self {
        Self {
            base: apiurl.trim_end_matches('/').to_string(),
            http,
        }
    }

    /// `GET /reports/{rrid}`, with `If-None-Match: etag` when `etag` is
    /// `Some`. Sends no `Authorization` header (reads are anonymous).
    ///
    /// `rrid` is placed in the path verbatim (unencoded): a dotted SLFO id
    /// (`SUSE:SLFO:1.2:*`) must round-trip with its colons intact, matching
    /// how [`crate::teregen::TeReGen`] builds its own report paths.
    ///
    /// # Errors
    ///
    /// See [`TeregenV2Error`] for the distinct refusal each status maps to.
    pub async fn fetch_document(
        &self,
        rrid: &str,
        etag: Option<&str>,
    ) -> Result<DocumentFetch, TeregenV2Error> {
        let url = format!("{}/reports/{rrid}", self.base);
        let mut request = self.http.inner().get(&url).timeout(HTTP_TIMEOUT.1);
        if let Some(etag) = etag {
            request = request.header(IF_NONE_MATCH, etag);
        }

        let response = request.send().await.map_err(|e| {
            let e = HttpError::from(e);
            tracing::debug!("TeReGen v2 GET reports/{rrid} failed: {e}");
            TeregenV2Error::Transport(e.to_string())
        })?;

        match response.status() {
            StatusCode::NOT_MODIFIED => return Ok(DocumentFetch::NotModified),
            StatusCode::NOT_FOUND => return Err(TeregenV2Error::NotFound),
            StatusCode::CONFLICT => return Err(TeregenV2Error::Stale),
            StatusCode::SERVICE_UNAVAILABLE => return Err(TeregenV2Error::Generating),
            StatusCode::BAD_REQUEST => {
                let detail = read_body_capped(response, MAX_API_BODY)
                    .await
                    .ok()
                    .map(|bytes| error_detail(&bytes))
                    .unwrap_or_default();
                return Err(TeregenV2Error::RejectedId { detail });
            }
            _ => {}
        }

        let response = response.error_for_status().map_err(|e| {
            let e = HttpError::from(e);
            tracing::debug!("TeReGen v2 GET reports/{rrid} failed: {e}");
            TeregenV2Error::Transport(e.to_string())
        })?;
        let etag_header = response
            .headers()
            .get(ETAG)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        let bytes = read_body_capped(response, MAX_API_BODY)
            .await
            .map_err(|e| match e {
                HttpError::BodyTooLarge { .. } => TeregenV2Error::BodyTooLarge,
                other => TeregenV2Error::Transport(other.to_string()),
            })?;
        let raw = String::from_utf8(bytes)
            .map_err(|e| TeregenV2Error::Transport(format!("response was not valid UTF-8: {e}")))?;
        let document = parse_and_warn_on_dropped_keys(&raw).map_err(TeregenV2Error::Invalid)?;

        Ok(DocumentFetch::Fresh {
            document: Box::new(document),
            raw,
            etag: etag_header,
        })
    }
}

/// Extract a `{"error": "..."}` body's message, falling back to the raw body
/// (lossily decoded) when it is not that exact shape. Truncated to
/// [`MAX_DETAIL_LEN`].
fn error_detail(bytes: &[u8]) -> String {
    let detail = serde_json::from_slice::<serde_json::Value>(bytes)
        .ok()
        .and_then(|v| v.get("error").and_then(|e| e.as_str()).map(str::to_owned))
        .unwrap_or_else(|| String::from_utf8_lossy(bytes).into_owned());
    if detail.len() > MAX_DETAIL_LEN {
        detail[..MAX_DETAIL_LEN].to_owned()
    } else {
        detail
    }
}
