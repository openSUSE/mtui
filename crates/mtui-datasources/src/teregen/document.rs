//! The v2 read/write client for TeReGen's JSON report document
//! (`qam.suse.de/api/v2`), an alternative source for the typed
//! `ReportDocument` model alongside the SVN `metadata.json` pair.
//!
//! Reads are anonymous: [`fetch_document`](TeregenV2::fetch_document) sends no
//! `Authorization` header — reads are anonymous on the live server, and
//! sending a possibly-stale bearer can only turn a working 200 into a 401.
//! `If-None-Match` is set whenever the caller has a previous ETag, so an
//! unchanged document costs a `304` instead of a re-parse.
//!
//! Writes require auth: [`upload_document`](TeregenV2::upload_document) is the
//! only call needing a [`TeregenAuth`] (via [`TeregenV2::with_auth`]), and it
//! never sends an unconditional update — [`Precondition::Match`] carries the
//! `If-Match` etag on every update, and [`Precondition::Create`] (no header)
//! is offered only to backfill a `log`-only id (P5-D2). mtui never sends
//! `If-Match: *`: it asserts existence, not identity, which would permit
//! exactly the lost update the precondition exists to prevent.

use mtui_config::Config;
use mtui_types::report_document::{DocumentError, ReportDocument, parse_and_warn_on_dropped_keys};
use reqwest::StatusCode;
use reqwest::header::{CONTENT_TYPE, ETAG, IF_MATCH, IF_NONE_MATCH};
use thiserror::Error;

use crate::error::HttpError;
use crate::http::{
    HTTP_TIMEOUT, HttpClient, MAX_API_BODY, VerifyPolicy, read_body_capped, resolve_verify,
};
use crate::teregen::auth::{TeregenAuth, TeregenAuthError};

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

/// The precondition governing a [`TeregenV2::upload_document`] write (P5-D2).
///
/// Named, not an `Option`, so an update cannot accidentally go out
/// unconditional: every update to an existing document is
/// [`Match`](Self::Match); [`Create`](Self::Create) sends no `If-Match` and is
/// valid only to backfill a `log`-only id that has no `report.json` yet.
/// `If-Match: *` is deliberately not offered — it asserts existence, not
/// identity, so it would permit exactly the lost update this type exists to
/// prevent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Precondition {
    /// `If-Match: <etag>`, sent byte-for-byte as received from a prior read —
    /// no parsing, no reconstruction.
    Match(String),
    /// No `If-Match` header. Only correct against an id that has neither a
    /// `report.json` nor produced one yet.
    Create,
}

/// The outcome of a successful [`TeregenV2::upload_document`] call.
///
/// Both fields come from the server's own `202` response, never from the
/// request mtui sent (P5-D4): the `202` body is teregen's own re-read of what
/// it just stored, and the `ETag` is recomputed over that canonical decode —
/// keeping mtui's local bytes as the record would drift from the value the
/// next `If-Match` is compared against.
#[derive(Debug, Clone, PartialEq)]
pub struct UploadOutcome {
    /// The stored document, as re-read and returned by the server.
    pub document: Box<ReportDocument>,
    /// The fresh `ETag` the server minted for the just-stored document.
    pub etag: Option<String>,
}

/// Errors from [`TeregenV2::upload_document`] — one variant per distinct
/// refusal a caller should message differently (P5-D5). A separate enum from
/// [`TeregenV2Error`]: the read and write refusal sets overlap only partly.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum TeregenV2WriteError {
    /// [`TeregenV2::upload_document`] was called before
    /// [`TeregenV2::with_auth`] attached a [`TeregenAuth`]. No request is
    /// sent.
    #[error("upload_document requires an attached TeregenAuth (see TeregenV2::with_auth)")]
    NotConfigured,

    /// Any failure from the auth layer itself: a `401` from
    /// `/reports/{id}` after one re-mint attempt (the common case), or a
    /// failure minting the token in the first place (rate limit, signing, the
    /// on-disk store).
    #[error(transparent)]
    Auth(#[from] TeregenAuthError),

    /// `400 {"error":"invalid id"}` — the server rejects the id itself.
    #[error("the server rejects this id: {detail}")]
    RejectedId {
        /// The server's own error message, verbatim.
        detail: String,
    },

    /// `400 {"error":"id mismatch"}` — the document body's `id` disagreed
    /// with the path id. Pre-flight (P5-D3) checks this locally before any
    /// request is sent; reaching the server with a mismatch is an mtui bug.
    #[error(
        "the document's id does not match the target id — this is an mtui bug \
         (pre-flight should have caught it)"
    )]
    IdMismatch,

    /// `404 {"error":"not found"}` — this id has neither a `report.json` nor
    /// a `log`; `PUT` cannot create a report from nothing.
    #[error("this id has neither a document nor a log — run `regenerate` first")]
    NeverGenerated,

    /// `412 {"error":"precondition failed"}` — the report changed under you.
    /// **Never retried**: the caller must reload before writing again.
    #[error("the report changed under you; reload before writing again")]
    PreconditionFailed {
        /// The server's current `ETag`, when it sent one in the response.
        server_etag: Option<String>,
    },

    /// `413` — the serialized body exceeded [`MAX_API_BODY`]. Pre-flight
    /// (P5-D3) checks this locally; reaching the server is unexpected.
    #[error("the document exceeds the {MAX_API_BODY}-byte limit")]
    TooLarge,

    /// `422 {"error":"invalid document","pointers":[…]}` — schema validation
    /// failed. The server checks this *before* the precondition (server step
    /// 5 precedes step 7), so this can mask a genuine `412`: an mtui bug that
    /// emits an invalid document never learns whether someone else also
    /// edited the report.
    #[error("the document failed schema validation: {}", pointers.join(", "))]
    Invalid {
        /// RFC 6901 pointers naming the offending locations, verbatim.
        pointers: Vec<String>,
    },

    /// `428 {"error":"precondition required"}` — an unconditional update
    /// against an existing document. mtui never sends one (P5-D2); reaching
    /// this is an mtui bug.
    #[error("the server required a precondition that mtui did not send — this is an mtui bug")]
    PreconditionRequired,

    /// `503`, body `{"error":"generating"}` — a client mistake: don't write
    /// to a report while it is being generated.
    #[error("the server is generating this document right now")]
    Generating,

    /// `503`, body `{"error":"busy"}` — an SVN commit for a previous upload
    /// holds the guard. Transient; a short backoff and retry is reasonable.
    #[error("the server is busy committing a previous upload")]
    Busy,

    /// The `202` response body did not parse as a [`ReportDocument`] —
    /// unexpected, since it is the server's own re-read of what it just
    /// validated and stored.
    #[error("the server's response failed to parse: {0}")]
    ResponseInvalid(#[source] DocumentError),

    /// The response body exceeded [`MAX_API_BODY`].
    #[error("response body exceeds the {MAX_API_BODY}-byte limit")]
    BodyTooLarge,

    /// A transport failure or an unmodelled non-2xx status — including an
    /// unrecognised `503` body, which falls back to the conservative,
    /// non-retrying [`Generating`](Self::Generating) reading instead of here.
    #[error("teregen v2 write failed: {0}")]
    Transport(String),
}

/// Read/write v2 client for TeReGen's JSON report document API.
#[derive(Debug)]
pub struct TeregenV2 {
    base: String,
    http: HttpClient,
    auth: Option<TeregenAuth>,
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
            auth: None,
        }
    }

    /// Attach a [`TeregenAuth`], required by
    /// [`upload_document`](Self::upload_document). Reads stay anonymous
    /// regardless — [`fetch_document`](Self::fetch_document) never consults
    /// this field.
    #[must_use]
    pub fn with_auth(mut self, auth: TeregenAuth) -> Self {
        self.auth = Some(auth);
        self
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

    /// `PUT /reports/{rrid}`: upload `document`, adopting the server's `202`
    /// response as the new authoritative document and `ETag` (P5-D4).
    ///
    /// Pre-flight (P5-D3), before any request is built: `document.id` must
    /// equal `rrid` (the server's own `400 id mismatch` check), and the
    /// serialized body must be at most [`MAX_API_BODY`] (the server's `413`) —
    /// both fail locally instead of costing a round trip.
    ///
    /// # Errors
    ///
    /// See [`TeregenV2WriteError`] for the distinct refusal each status maps
    /// to. Returns [`TeregenV2WriteError::NotConfigured`], with no request
    /// sent, if [`with_auth`](Self::with_auth) was never called.
    pub async fn upload_document(
        &self,
        rrid: &str,
        document: &ReportDocument,
        precondition: &Precondition,
    ) -> Result<UploadOutcome, TeregenV2WriteError> {
        let Some(auth) = &self.auth else {
            return Err(TeregenV2WriteError::NotConfigured);
        };
        if document.id != rrid {
            return Err(TeregenV2WriteError::IdMismatch);
        }
        let body = serde_json::to_string(document).map_err(|e| {
            TeregenV2WriteError::Transport(format!("failed to serialize the document: {e}"))
        })?;
        if body.len() > MAX_API_BODY {
            return Err(TeregenV2WriteError::TooLarge);
        }

        let url = format!("{}/reports/{rrid}", self.base);
        let if_match = match precondition {
            Precondition::Match(etag) => Some(etag.clone()),
            Precondition::Create => None,
        };
        let response = auth
            .authenticated_request_with(reqwest::Method::PUT, &url, move |b| {
                let b = b
                    .header(CONTENT_TYPE, "application/json")
                    .body(body.clone());
                match &if_match {
                    Some(etag) => b.header(IF_MATCH, etag.clone()),
                    None => b,
                }
            })
            .await?;

        match response.status().as_u16() {
            202 => {
                let etag_header = response
                    .headers()
                    .get(ETAG)
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_owned);
                let bytes =
                    read_body_capped(response, MAX_API_BODY)
                        .await
                        .map_err(|e| match e {
                            HttpError::BodyTooLarge { .. } => TeregenV2WriteError::BodyTooLarge,
                            other => TeregenV2WriteError::Transport(other.to_string()),
                        })?;
                let raw = String::from_utf8(bytes).map_err(|e| {
                    TeregenV2WriteError::Transport(format!("response was not valid UTF-8: {e}"))
                })?;
                let document = parse_and_warn_on_dropped_keys(&raw)
                    .map_err(TeregenV2WriteError::ResponseInvalid)?;
                Ok(UploadOutcome {
                    document: Box::new(document),
                    etag: etag_header,
                })
            }
            400 => {
                let detail = read_body_capped(response, MAX_API_BODY)
                    .await
                    .ok()
                    .map(|bytes| error_detail(&bytes))
                    .unwrap_or_default();
                if detail == "id mismatch" {
                    Err(TeregenV2WriteError::IdMismatch)
                } else {
                    Err(TeregenV2WriteError::RejectedId { detail })
                }
            }
            404 => Err(TeregenV2WriteError::NeverGenerated),
            412 => {
                let server_etag = response
                    .headers()
                    .get(ETAG)
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_owned);
                Err(TeregenV2WriteError::PreconditionFailed { server_etag })
            }
            413 => Err(TeregenV2WriteError::TooLarge),
            422 => {
                let bytes = read_body_capped(response, MAX_API_BODY)
                    .await
                    .unwrap_or_default();
                let pointers = serde_json::from_slice::<serde_json::Value>(&bytes)
                    .ok()
                    .and_then(|v| v.get("pointers").and_then(|p| p.as_array().cloned()))
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|v| v.as_str().map(str::to_owned))
                            .collect()
                    })
                    .unwrap_or_default();
                Err(TeregenV2WriteError::Invalid { pointers })
            }
            428 => Err(TeregenV2WriteError::PreconditionRequired),
            503 => {
                let bytes = read_body_capped(response, MAX_API_BODY)
                    .await
                    .unwrap_or_default();
                let error_field = serde_json::from_slice::<serde_json::Value>(&bytes)
                    .ok()
                    .and_then(|v| v.get("error").and_then(|e| e.as_str().map(str::to_owned)));
                match error_field.as_deref() {
                    Some("busy") => Err(TeregenV2WriteError::Busy),
                    // Conservative fallback (taxonomy): an unrecognised body
                    // reads as still-generating, never as safe-to-retry.
                    _ => Err(TeregenV2WriteError::Generating),
                }
            }
            other => Err(TeregenV2WriteError::Transport(format!(
                "unexpected status {other}"
            ))),
        }
    }

    /// `PUT /reports/{rrid}/artifacts/{name}`: upload one artifact file
    /// (`application/octet-stream`), replacing any existing artifact of the
    /// same name.
    ///
    /// Pre-flight, before any request: `name` must match the server's
    /// `^[A-Za-z0-9_.-]+$` pattern and must not start with `.`, and
    /// `bytes.len()` must be at most [`MAX_API_BODY`] — both fail locally
    /// instead of costing a round trip.
    ///
    /// # Errors
    ///
    /// See [`ArtifactUploadError`]. Returns
    /// [`ArtifactUploadError::NotConfigured`], with no request sent, if
    /// [`with_auth`](Self::with_auth) was never called.
    pub async fn upload_artifact(
        &self,
        rrid: &str,
        name: &str,
        bytes: Vec<u8>,
    ) -> Result<ArtifactStored, ArtifactUploadError> {
        let Some(auth) = &self.auth else {
            return Err(ArtifactUploadError::NotConfigured);
        };
        if !is_valid_artifact_name(name) {
            return Err(ArtifactUploadError::InvalidName {
                name: name.to_owned(),
            });
        }
        if bytes.len() > MAX_API_BODY {
            return Err(ArtifactUploadError::TooLarge {
                name: name.to_owned(),
            });
        }

        let url = format!(
            "{}/reports/{rrid}/artifacts/{}",
            self.base,
            urlencoding::encode(name)
        );
        let response = auth
            .authenticated_request_with(reqwest::Method::PUT, &url, move |b| {
                b.header(CONTENT_TYPE, "application/octet-stream")
                    .body(bytes.clone())
            })
            .await?;

        match response.status().as_u16() {
            201 => Ok(ArtifactStored::Created),
            200 => Ok(ArtifactStored::Replaced),
            400 => {
                let detail = read_body_capped(response, MAX_API_BODY)
                    .await
                    .ok()
                    .map(|bytes| error_detail(&bytes))
                    .unwrap_or_default();
                Err(ArtifactUploadError::RejectedId { detail })
            }
            413 => Err(ArtifactUploadError::TooLarge {
                name: name.to_owned(),
            }),
            422 => Err(ArtifactUploadError::InvalidName {
                name: name.to_owned(),
            }),
            503 => {
                let bytes = read_body_capped(response, MAX_API_BODY)
                    .await
                    .unwrap_or_default();
                let error_field = serde_json::from_slice::<serde_json::Value>(&bytes)
                    .ok()
                    .and_then(|v| v.get("error").and_then(|e| e.as_str().map(str::to_owned)));
                match error_field.as_deref() {
                    Some("busy") => Err(ArtifactUploadError::Busy),
                    // Conservative fallback, same taxonomy as
                    // `upload_document`: an unrecognised body reads as
                    // still-generating, never as safe-to-retry.
                    _ => Err(ArtifactUploadError::Generating),
                }
            }
            other => Err(ArtifactUploadError::Transport(format!(
                "unexpected status {other}"
            ))),
        }
    }
}

/// The outcome of a successful [`TeregenV2::upload_artifact`] call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArtifactStored {
    /// `201`: no artifact existed under this name yet.
    Created,
    /// `200`: an existing artifact was replaced.
    Replaced,
}

/// Errors from [`TeregenV2::upload_artifact`] — a separate enum from
/// [`TeregenV2WriteError`], whose `413`/`422` wording is document-specific;
/// stretching it to cover a per-file artifact refusal would blur the two.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ArtifactUploadError {
    /// [`TeregenV2::upload_artifact`] was called before [`TeregenV2::with_auth`]
    /// attached a [`TeregenAuth`]. No request is sent.
    #[error("upload_artifact requires an attached TeregenAuth (see TeregenV2::with_auth)")]
    NotConfigured,

    /// Any failure from the auth layer itself (see
    /// [`TeregenV2WriteError::Auth`]).
    #[error(transparent)]
    Auth(#[from] TeregenAuthError),

    /// `400 {"error":"invalid id"}` — the server rejects the report id itself.
    #[error("the server rejects this id: {detail}")]
    RejectedId {
        /// The server's own error message, verbatim.
        detail: String,
    },

    /// The artifact name does not match the server's `^[A-Za-z0-9_.-]+$`
    /// pattern, or starts with `.` (`Api/V2/Report.pm`). Checked locally
    /// before any request when mtui names the artifact itself; also the
    /// server's own `422 invalid artifact name` refusal.
    #[error("invalid artifact name: {name}")]
    InvalidName {
        /// The offending name, verbatim.
        name: String,
    },

    /// The artifact body exceeds [`MAX_API_BODY`] (pre-flight, or the
    /// server's own `413`).
    #[error("artifact {name} exceeds the {MAX_API_BODY}-byte limit")]
    TooLarge {
        /// The artifact's name.
        name: String,
    },

    /// `503`, body `{"error":"generating"}` — the document is being generated
    /// right now.
    #[error("the server is generating this document right now")]
    Generating,

    /// `503`, body `{"error":"busy"}` — a previous upload's SVN commit holds
    /// the per-id guard (every write takes `minion->guard($id, 60)`).
    #[error("the server is busy committing a previous upload")]
    Busy,

    /// A transport failure or an unmodelled non-2xx status.
    #[error("teregen v2 artifact upload failed: {0}")]
    Transport(String),
}

/// `true` when `name` matches the server's `^[A-Za-z0-9_.-]+$` pattern and
/// does not start with `.` (`Api/V2/Report.pm`'s artifact-name rule). Public
/// so a caller assembling artifacts locally (`mtui-testreport`'s
/// `collect_artifacts`) can refuse a bad name before ever building a request,
/// using the exact same predicate [`TeregenV2::upload_artifact`] checks.
#[must_use]
pub fn is_valid_artifact_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('.')
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'))
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
