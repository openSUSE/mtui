//! A client for managing Gitea pull requests via a comment-based workflow.
//!
//! Ported from upstream `mtui/data_sources/gitea.py`. The [`Gitea`] client
//! assigns, unassigns, approves, and rejects a PR by posting specially
//! formatted comments; there is no dedicated state field. It derives the
//! current state by replaying the PR's comment history:
//!
//! * **assignment** — magic marker comments
//!   (`<MTUI: PR - UV assigned to user: X - group: Y >` and its unassign twin)
//!   are replayed as a state machine scoped to the review group, "last marker
//!   wins" (see [`Gitea::assignee_from_comments`]);
//! * **decision** — a `@<group>-review: LGTM|approve|decline` comment records a
//!   decision, but a *re-requested* review (e.g. after a rebuild) supersedes a
//!   stale decision, so [`Gitea`] only treats the PR as decided when a decision
//!   comment exists **and** the group is not currently a requested reviewer.
//!
//! Deviations from upstream, all faithful to behaviour:
//!
//! * The Python `method` typing-shim enum is dropped in favour of
//!   [`reqwest::Method`].
//! * Comment timestamps are parsed with `chrono` (RFC3339 with offset), matching
//!   Python's `datetime.fromisoformat`, and comments sort by that timestamp.
//! * TLS posture is fixed when the shared [`HttpClient`] is built (reqwest
//!   fixes it per-client), so [`Gitea::new`] resolves the verify policy up front
//!   via [`resolve_verify`] exactly as upstream did per-request.

use std::cmp::Ordering;

use chrono::{DateTime, FixedOffset};
use mtui_config::Config;
use mtui_types::Assignment;
use regex::Regex;
use reqwest::Method;
use serde_json::json;

use crate::error::GiteaError;
use crate::http::{
    HttpClient, MAX_API_BODY, VerifyPolicy, is_ssl_verification_error, read_body_capped,
    resolve_verify, sanitize_url, ssl_verification_hint,
};

/// The default review group a [`Gitea`] client operates on behalf of, matching
/// upstream `Gitea.__init__(..., group="qam-sle")`.
const DEFAULT_GROUP: &str = "qam-sle";

/// Template for an assignment marker comment (`user`, `group`).
const ASSIGN_TEMPLATE: &str = "<MTUI: PR - UV assigned to user: {user} - group: {group} >";
/// Template for an unassignment marker comment (`user`, `group`).
const UNASSIGN_TEMPLATE: &str = "<MTUI: PR - UV unassigned user: {user} - group: {group} >";

/// Format an assignment marker comment body for `user`/`group`.
#[must_use]
pub fn assign_marker(user: &str, group: &str) -> String {
    ASSIGN_TEMPLATE
        .replace("{user}", user)
        .replace("{group}", group)
}

/// Format an unassignment marker comment body for `user`/`group`.
#[must_use]
fn unassign_marker(user: &str, group: &str) -> String {
    UNASSIGN_TEMPLATE
        .replace("{user}", user)
        .replace("{group}", group)
}

/// Extract the host (authority without any port) from an `scheme://host[:port]/…`
/// URL, for the TLS-failure hint. Returns `None` if the shape is unexpected.
fn host_of(url: &str) -> Option<String> {
    let rest = url.split_once("://")?.1;
    let authority = rest.split(['/', '?', '#']).next()?;
    let authority = authority.rsplit('@').next().unwrap_or(authority);
    let host = authority.split(':').next()?;
    (!host.is_empty()).then(|| host.to_string())
}

/// A URL origin: scheme (lower-cased), host (lower-cased), and effective port.
///
/// The security anchor for [`Gitea`]: the token is attached only to a request
/// whose origin equals the client's configured trusted origin. Comparison is
/// exact on all three components, with the scheme's default port filled in so
/// `https://h` and `https://h:443` compare equal.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Origin {
    scheme: String,
    host: String,
    port: u16,
}

/// Parse an `scheme://[userinfo@]host[:port]` URL into an [`Origin`], rejecting
/// any URL that carries userinfo (`user:pass@host`) — such a URL could smuggle
/// a credential and its host is easy to misread, so it is never trusted.
///
/// Returns `None` for a non-URL, an empty host, a non-numeric/out-of-range
/// port, a scheme without a known default port, or any userinfo present.
fn parse_origin(url: &str) -> Option<Origin> {
    let (scheme, rest) = url.split_once("://")?;
    let scheme = scheme.to_ascii_lowercase();
    if scheme.is_empty() {
        return None;
    }
    let authority = rest.split(['/', '?', '#']).next()?;
    if authority.is_empty() {
        return None;
    }
    // Reject any userinfo: an `@` in the authority means `user[:pass]@host`.
    if authority.contains('@') {
        return None;
    }
    let default_port = match scheme.as_str() {
        "https" => 443,
        "http" => 80,
        _ => return None,
    };
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) => (h, p.parse::<u16>().ok()?),
        None => (authority, default_port),
    };
    if host.is_empty() {
        return None;
    }
    Some(Origin {
        scheme,
        host: host.to_ascii_lowercase(),
        port,
    })
}

/// Whether `host` is a loopback address, for which a plaintext `http` origin is
/// acceptable (a test/mock server, never a real exfiltration target).
fn is_loopback(host: &str) -> bool {
    host == "localhost"
        || host == "127.0.0.1"
        || host == "::1"
        || host == "[::1]"
        || host.starts_with("127.")
}

/// Whether the origin `o` is allowed to carry the token: it must be `https`,
/// unless the host is loopback (a test/mock server).
fn scheme_ok(o: &Origin) -> bool {
    o.scheme == "https" || (o.scheme == "http" && is_loopback(&o.host))
}

/// Whether `url` carries no userinfo, has an acceptable scheme, and shares the
/// exact origin (scheme/host/port) of `trusted`. The single predicate guarding
/// token attachment; a `None` origin (non-URL, userinfo, bad port) is never
/// trusted.
fn is_trusted(url: &str, trusted: &Origin) -> bool {
    parse_origin(url).is_some_and(|o| scheme_ok(&o) && &o == trusted)
}

/// Parse the operator-configured trusted Gitea origin (`config.gitea_url`),
/// requiring a usable origin the token may be sent to (`https`, or plaintext
/// `http` only for a loopback test server).
///
/// # Errors
///
/// [`GiteaError::UntrustedOrigin`] (carrying the sanitised URL) if the value is
/// empty, not a URL, not `https` (and not loopback `http`), carries userinfo, or
/// has a bad port — so the client can never be built with a trust anchor that
/// would silently accept a plaintext or credential-bearing endpoint.
fn parse_trusted_origin(gitea_url: &str) -> Result<Origin, GiteaError> {
    match parse_origin(gitea_url) {
        Some(o) if scheme_ok(&o) => Ok(o),
        _ => Err(GiteaError::UntrustedOrigin(sanitize_url(gitea_url))),
    }
}

/// Whether any comment records a decision for `group`
/// (`@<group>-review: LGTM|approve[d]|decline[d]`).
///
/// Pure over an already-fetched comment snapshot. The "does it *still* stand"
/// question (a pending re-request supersedes a stale decision) is answered by
/// the caller via [`Gitea::has_review`]; this only reports that a decision
/// marker exists.
#[must_use]
fn decision_present(comments: &[Comment], group: &str) -> bool {
    let done = Regex::new(&format!(
        r"^@{}-review: (LGTM|approved?|declined?)",
        regex::escape(group)
    ))
    .expect("decision regex is valid");
    comments.iter().any(|c| done.is_match(&c.body))
}

/// A Gitea comment, sortable by its `updated_at` timestamp.
///
/// Mirrors upstream `Comment`: ordering and equality are **by date** (upstream
/// used `@total_ordering` with a date-based `__eq__`), so [`sort`](slice::sort)
/// over a comment slice reproduces the chronological replay order the state
/// machine depends on.
#[derive(Debug, Clone)]
pub struct Comment {
    /// The comment body.
    body: String,
    /// The comment's `updated_at` timestamp.
    date: DateTime<FixedOffset>,
}

impl Comment {
    /// Build a comment, parsing an RFC3339 `updated_at` timestamp.
    ///
    /// # Errors
    ///
    /// Returns [`GiteaError::FailedCall`] if `updated_at` is not a parseable
    /// RFC3339 timestamp (folded into the fetch failure surface, matching
    /// upstream where a malformed comment payload aborts the API call).
    fn parse(body: String, updated_at: &str) -> Result<Self, GiteaError> {
        let date = DateTime::parse_from_rfc3339(updated_at).map_err(|e| {
            GiteaError::FailedCall(format!("unparseable comment timestamp {updated_at:?}: {e}"))
        })?;
        Ok(Self { body, date })
    }
}

impl PartialEq for Comment {
    fn eq(&self, other: &Self) -> bool {
        self.date == other.date
    }
}
impl Eq for Comment {}
impl PartialOrd for Comment {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for Comment {
    fn cmp(&self, other: &Self) -> Ordering {
        self.date.cmp(&other.date)
    }
}

/// The raw JSON shape of a Gitea issue comment (the fields we consume).
#[derive(serde::Deserialize)]
struct RawComment {
    body: String,
    updated_at: String,
}

/// A client for managing a Gitea Pull Request via a comment-based workflow.
///
/// Provides methods to assign, unassign, approve, and reject a PR by posting
/// specially formatted comments; the current state is derived by parsing the
/// PR's whole comment history. Built once per PR and reused.
#[derive(Debug, Clone)]
pub struct Gitea {
    http: HttpClient,
    token: String,
    /// The session user this client acts as by default.
    user: String,
    /// The review group this client operates on behalf of.
    group: String,
    /// The PR API URL (`.../pulls/<n>`).
    pr: String,
    /// The PR issue-comments API URL (`.../issues/<n>/comments`).
    prissues: String,
    /// The only origin the [`token`](Self::token) may be sent to. Every request
    /// URL is checked against this before the `Authorization` header is
    /// attached; a mismatch is [`GiteaError::UntrustedOrigin`].
    trusted_origin: Origin,
    assign_re: Regex,
    unassign_re: Regex,
}

impl Gitea {
    /// Build a Gitea client for the PR at `giteaprapi` (a REST API URL).
    ///
    /// Mirrors upstream `Gitea.__init__`: reads the token/session user/TLS
    /// posture from `config`, defaults the group to [`DEFAULT_GROUP`], and
    /// derives the issue-comments endpoint from the PR URL.
    ///
    /// # Errors
    ///
    /// Returns [`GiteaError::MissingToken`] if `config.gitea_token` is empty,
    /// [`GiteaError::UntrustedOrigin`] if `config.gitea_url` is not a usable
    /// `https` origin, or [`GiteaError::Http`] if the shared HTTP client cannot
    /// be built (e.g. a configured CA bundle cannot be read).
    pub fn new(config: &Config, giteaprapi: &str, group: Option<&str>) -> Result<Self, GiteaError> {
        if config.gitea_token.is_empty() {
            return Err(GiteaError::MissingToken);
        }
        let verify: VerifyPolicy = resolve_verify(
            VerifyPolicy::Default(true),
            Some(VerifyPolicy::from_config(&config.ssl_verify)),
        );
        let http = HttpClient::new(verify)?;
        Self::with_client(
            http,
            config.gitea_token.clone(),
            config.session_user.clone(),
            giteaprapi,
            &config.gitea_url,
            group,
        )
    }

    /// Build a client from an already-constructed [`HttpClient`] and explicit
    /// credentials, bypassing [`Config`].
    ///
    /// The composition-root / test seam: it lets a caller inject a client whose
    /// TLS posture (or base host, under `wiremock`) is already fixed. The token
    /// is trusted as non-empty here — [`new`](Self::new) is the guarded entry.
    ///
    /// `trusted_gitea_url` is the operator-configured origin (`config.gitea_url`)
    /// the token may be sent to; requests to any other origin are refused.
    ///
    /// # Errors
    ///
    /// Returns [`GiteaError::UntrustedOrigin`] if `trusted_gitea_url` is not a
    /// usable `https` origin (bad URL, userinfo present, non-https, or a
    /// non-numeric/out-of-range port).
    pub fn with_client(
        http: HttpClient,
        token: String,
        user: String,
        giteaprapi: &str,
        trusted_gitea_url: &str,
        group: Option<&str>,
    ) -> Result<Self, GiteaError> {
        let trusted_origin = parse_trusted_origin(trusted_gitea_url)?;
        // `.../pulls/<n>` -> `.../issues/<n>/comments`, matching upstream's
        // `giteaprapi.replace("pulls", "issues") + "/comments"`.
        let prissues = format!("{}/comments", giteaprapi.replace("pulls", "issues"));
        Ok(Self {
            http,
            token,
            user,
            group: group.unwrap_or(DEFAULT_GROUP).to_string(),
            pr: giteaprapi.to_string(),
            prissues,
            trusted_origin,
            assign_re: Regex::new(
                r"^<MTUI: PR - UV assigned to user: (?P<user>.*) - group: (?P<group>.*) >",
            )
            .expect("static assign regex is valid"),
            unassign_re: Regex::new(
                r"^<MTUI: PR - UV unassigned user: (?P<user>.*) - group: (?P<group>.*) >",
            )
            .expect("static unassign regex is valid"),
        })
    }

    /// A private wrapper for a request to the Gitea API, returning the decoded
    /// JSON body (or [`serde_json::Value::Null`] for `204 No Content`).
    ///
    /// Folds every failure onto [`GiteaError::FailedCall`], surfacing a concise
    /// actionable hint at ERROR for a TLS certificate failure (detail at DEBUG)
    /// rather than a raw transport error — mirroring upstream `__request`.
    async fn request(
        &self,
        method: Method,
        url: &str,
        body: Option<serde_json::Value>,
    ) -> Result<serde_json::Value, GiteaError> {
        // Never attach the token to a URL that is not the configured trusted
        // origin. Metadata (`gitea_pr_api`) is attacker-influenceable, so a
        // hostile PR URL — or a non-https/userinfo-bearing one — must not
        // receive the credential. reqwest additionally strips the Authorization
        // header on any *cross-origin* redirect, so a same-origin request can
        // never leak the token to another host.
        if !is_trusted(url, &self.trusted_origin) {
            tracing::warn!(
                "Refusing to send Gitea token to untrusted URL {}",
                sanitize_url(url)
            );
            return Err(GiteaError::UntrustedOrigin(sanitize_url(url)));
        }
        tracing::debug!("Requesting {method} on {}", sanitize_url(url));
        let mut builder = self
            .http
            .inner()
            .request(method.clone(), url)
            .header("Authorization", format!("token {}", self.token))
            .header("Accept", "application/json");
        if let Some(json) = &body {
            builder = builder.json(json);
        }

        let response = match builder.send().await {
            Ok(r) => r,
            Err(e) => {
                if is_ssl_verification_error(&e) {
                    let host = host_of(url);
                    tracing::error!("{}", ssl_verification_hint(host.as_deref()));
                    tracing::debug!("Gitea TLS error detail: {e}");
                } else {
                    tracing::warn!("API call to Gitea failed: {e}");
                }
                return Err(GiteaError::FailedCall(format!(
                    "{method} - {}",
                    sanitize_url(url)
                )));
            }
        };

        let status = response.status();
        if !status.is_success() {
            tracing::warn!(
                "API call to {} failed with status code: {status}",
                sanitize_url(url)
            );
            // Best-effort: surface a more specific message from the body.
            if let Ok(bytes) = read_body_capped(response, MAX_API_BODY).await
                && let Ok(v) = serde_json::from_slice::<serde_json::Value>(&bytes)
                && let Some(msg) = v.get("message").and_then(serde_json::Value::as_str)
            {
                tracing::debug!("Gitea error message: {msg}");
            }
            return Err(GiteaError::FailedCall(format!(
                "{method} - {} returned status {}",
                sanitize_url(url),
                status.as_u16()
            )));
        }

        // 204 No Content: no body to decode.
        if status.as_u16() == 204 {
            return Ok(serde_json::Value::Null);
        }
        let bytes = read_body_capped(response, MAX_API_BODY)
            .await
            .map_err(|e| {
                GiteaError::FailedCall(format!("{method} - {}: {e}", sanitize_url(url)))
            })?;
        serde_json::from_slice::<serde_json::Value>(&bytes)
            .map_err(|e| GiteaError::FailedCall(format!("{method} - {}: {e}", sanitize_url(url))))
    }

    /// Fetch and deserialise all comments on the pull request.
    async fn get_all_comments(&self) -> Result<Vec<Comment>, GiteaError> {
        let value = self.request(Method::GET, &self.prissues, None).await?;
        let raw: Vec<RawComment> = serde_json::from_value(value)
            .map_err(|e| GiteaError::FailedCall(format!("decoding comments: {e}")))?;
        raw.into_iter()
            .map(|c| Comment::parse(c.body, &c.updated_at))
            .collect()
    }

    /// Replay assign/unassign markers over `comments` (assumed chronologically
    /// sorted) and return the current assignee for `group`, or `None`.
    ///
    /// The last valid assignment or unassignment marker for the group wins; a
    /// marker for another group is ignored. Public + static so the state
    /// machine can be tested without any HTTP.
    #[must_use]
    fn assignee_from_comments(&self, comments: &[Comment], group: &str) -> Option<String> {
        let mut assignee: Option<String> = None;
        for c in comments {
            if let Some(m) = self.assign_re.captures(&c.body) {
                if &m["group"] == group {
                    assignee = Some(m["user"].to_string());
                }
            } else if let Some(m) = self.unassign_re.captures(&c.body)
                && &m["group"] == group
            {
                assignee = None;
            }
        }
        assignee
    }

    /// Fetch the PR comments once and return them chronologically sorted.
    ///
    /// The single comment snapshot a write operation derives *all* of its state
    /// from — assignment ([`assign_state`](Self::assign_state)) and decision
    /// presence ([`decision_present`]) — so one op issues one comments GET
    /// instead of refetching per helper.
    async fn load_sorted_comments(&self) -> Result<Vec<Comment>, GiteaError> {
        let mut comments = self.get_all_comments().await?;
        comments.sort();
        Ok(comments)
    }

    /// Return the current assignee for this PR's group, or `None`.
    ///
    /// Reloads the comments and replays the assign/unassign markers. `None`
    /// means the group is unassigned (no marker, or the last marker is an
    /// unassignment).
    ///
    /// # Errors
    ///
    /// Returns [`GiteaError::FailedCall`] if the comments cannot be fetched.
    pub async fn assignee(&self) -> Result<Option<String>, GiteaError> {
        let comments = self.load_sorted_comments().await?;
        Ok(self.assignee_from_comments(&comments, &self.group))
    }

    /// Derive the assignment state for `check_user` from an already-fetched
    /// comment snapshot (pure; no I/O).
    fn assign_state(&self, comments: &[Comment], check_user: &str) -> Assignment {
        match self.assignee_from_comments(comments, &self.group) {
            None => Assignment::Unassigned,
            Some(a) if a == check_user => Assignment::AssignedUser,
            Some(_) => Assignment::AssignedOther,
        }
    }

    /// Whether the group's review is currently requested on the PR.
    ///
    /// Issues one GET on the PR endpoint. A write op calls this at most once,
    /// and only when a decision comment exists (see [`decision_present`]).
    async fn has_review(&self) -> Result<bool, GiteaError> {
        let pr = self.request(Method::GET, &self.pr, None).await?;
        let wanted = format!("{}-review", self.group);
        Ok(pr
            .get("requested_reviewers")
            .and_then(serde_json::Value::as_array)
            .is_some_and(|revs| {
                revs.iter().any(|d| {
                    d.get("login").and_then(serde_json::Value::as_str) == Some(wanted.as_str())
                })
            }))
    }

    /// Whether the group's review is decided *and still stands*, derived from an
    /// already-fetched comment snapshot.
    ///
    /// A decision comment (`@<group>-review: LGTM|approve|decline`) records a
    /// decision, but the history is append-only: a re-requested review after a
    /// rebuild supersedes a stale decision. So this only reports "done" when a
    /// decision comment exists **and** the group is not currently a requested
    /// reviewer — the latter checked lazily (one PR GET) so no decision means no
    /// PR fetch at all.
    async fn is_done_from(&self, comments: &[Comment]) -> Result<bool, GiteaError> {
        if !decision_present(comments, &self.group) {
            return Ok(false);
        }
        // A decision exists, but a pending re-request supersedes it.
        Ok(!self.has_review().await?)
    }

    /// Approve the PR by posting an LGTM comment.
    ///
    /// # Errors
    ///
    /// [`GiteaError::AssignInvalid`] if the PR is not assigned to the acting
    /// user; [`GiteaError::NoReview`] if it was already approved/rejected.
    pub async fn approve(&self, other: Option<&str>) -> Result<(), GiteaError> {
        let a_user = other.unwrap_or(&self.user);
        let comments = self.load_sorted_comments().await?;
        let state = self.assign_state(&comments, a_user);
        if state != Assignment::AssignedUser {
            return Err(GiteaError::AssignInvalid {
                state,
                user: a_user.to_string(),
            });
        }
        if self.is_done_from(&comments).await? {
            return Err(GiteaError::NoReview(
                "PR was already approved/rejected".to_string(),
            ));
        }
        tracing::info!("Approving PR as {a_user} for group {}", self.group);
        let msg = format!("@{}-review: LGTM", self.group);
        self.request(Method::POST, &self.prissues, Some(json!({ "body": msg })))
            .await?;
        Ok(())
    }

    /// Reject the PR by posting a decline comment.
    ///
    /// `reason` and `message` are appended on their own lines when non-empty.
    ///
    /// # Errors
    ///
    /// [`GiteaError::AssignInvalid`] if the PR is not assigned to the acting
    /// user; [`GiteaError::NoReview`] if it was already approved/rejected.
    pub async fn reject(
        &self,
        reason: &str,
        other: Option<&str>,
        message: &str,
    ) -> Result<(), GiteaError> {
        let a_user = other.unwrap_or(&self.user);
        let comments = self.load_sorted_comments().await?;
        let state = self.assign_state(&comments, a_user);
        if state != Assignment::AssignedUser {
            return Err(GiteaError::AssignInvalid {
                state,
                user: a_user.to_string(),
            });
        }
        if self.is_done_from(&comments).await? {
            return Err(GiteaError::NoReview(
                "PR was already approved/rejected".to_string(),
            ));
        }
        tracing::info!("Rejecting PR as {a_user} for group {}", self.group);
        let mut msg = format!("@{}-review: decline", self.group);
        if !reason.is_empty() {
            msg.push_str(&format!("\nReason: {reason}"));
        }
        if !message.is_empty() {
            msg.push_str(&format!("\n{message}"));
        }
        self.request(Method::POST, &self.prissues, Some(json!({ "body": msg })))
            .await?;
        Ok(())
    }

    /// Assign the PR to a user by posting an assignment marker comment.
    ///
    /// `force` skips the "review requested" and "already assigned" guards (e.g.
    /// to re-assign a PR held by a different user); an approved/rejected PR is
    /// still refused.
    ///
    /// # Errors
    ///
    /// [`GiteaError::NoReview`] if no review is requested (and not `force`), or
    /// the PR was already decided; [`GiteaError::AssignInvalid`] if the PR is
    /// not unassigned (and not `force`).
    pub async fn assign(&self, other: Option<&str>, force: bool) -> Result<(), GiteaError> {
        let a_user = other.unwrap_or(&self.user);
        if !force && !self.has_review().await? {
            return Err(GiteaError::NoReview(format!(
                "There is no review for {}-review",
                self.group
            )));
        }
        let comments = self.load_sorted_comments().await?;
        if self.is_done_from(&comments).await? {
            return Err(GiteaError::NoReview(
                "PR was already approved/rejected".to_string(),
            ));
        }
        if !force {
            let state = self.assign_state(&comments, a_user);
            if state != Assignment::Unassigned {
                return Err(GiteaError::AssignInvalid {
                    state,
                    user: a_user.to_string(),
                });
            }
        }
        tracing::info!("Assigning PR to {a_user} for group {}", self.group);
        let msg = assign_marker(a_user, &self.group);
        self.request(Method::POST, &self.prissues, Some(json!({ "body": msg })))
            .await?;
        Ok(())
    }

    /// Unassign a user from the PR by posting an unassignment marker comment.
    ///
    /// # Errors
    ///
    /// [`GiteaError::AssignInvalid`] if the PR is not assigned to the user.
    pub async fn unassign(&self, other: Option<&str>) -> Result<(), GiteaError> {
        let a_user = other.unwrap_or(&self.user);
        let comments = self.load_sorted_comments().await?;
        let state = self.assign_state(&comments, a_user);
        if state != Assignment::AssignedUser {
            return Err(GiteaError::AssignInvalid {
                state,
                user: a_user.to_string(),
            });
        }
        tracing::info!("Unassigning user {a_user} for group {}", self.group);
        let msg = unassign_marker(a_user, &self.group);
        self.request(Method::POST, &self.prissues, Some(json!({ "body": msg })))
            .await?;
        Ok(())
    }

    /// Post a generic comment to the pull request.
    ///
    /// # Errors
    ///
    /// [`GiteaError::FailedCall`] if the API call fails.
    pub async fn comment(&self, body: &str) -> Result<(), GiteaError> {
        tracing::info!("Posting a comment to Gitea PR");
        self.request(Method::POST, &self.prissues, Some(json!({ "body": body })))
            .await?;
        Ok(())
    }

    /// Return the PR's HEAD commit SHA.
    ///
    /// # Errors
    ///
    /// [`GiteaError::FailedCall`] if the API call fails or the shape is missing
    /// the `head.sha` field.
    pub async fn get_hash(&self) -> Result<String, GiteaError> {
        let data = self.request(Method::GET, &self.pr, None).await?;
        data.get("head")
            .and_then(|h| h.get("sha"))
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| GiteaError::FailedCall(format!("{} - missing head.sha", self.pr)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dummy() -> Gitea {
        let http = HttpClient::new(VerifyPolicy::Default(true)).unwrap();
        Gitea::with_client(
            http,
            "tok".to_string(),
            "testuser".to_string(),
            "https://gitea.example.com/api/v1/repos/owner/repo/pulls/1",
            "https://gitea.example.com",
            None,
        )
        .unwrap()
    }

    fn comment(body: &str, date: &str) -> Comment {
        Comment::parse(body.to_string(), date).unwrap()
    }

    // --- Comment ordering / equality ---

    #[test]
    fn comment_orders_and_equals_by_date() {
        let c1 = comment("first", "2024-01-01T00:00:00+00:00");
        let c2 = comment("second", "2024-01-02T00:00:00+00:00");
        assert!(c1 < c2);
        assert!(c2 > c1);
        // Equal dates -> equal comments even with different bodies.
        let a = comment("a", "2024-01-01T00:00:00+00:00");
        let b = comment("b", "2024-01-01T00:00:00+00:00");
        assert_eq!(a, b);
    }

    #[test]
    fn comments_sort_chronologically() {
        let mut cs = [
            comment("first", "2024-01-03T00:00:00+00:00"),
            comment("second", "2024-01-01T00:00:00+00:00"),
            comment("third", "2024-01-02T00:00:00+00:00"),
        ];
        cs.sort();
        assert_eq!(cs[0].body, "second");
        assert_eq!(cs[1].body, "third");
        assert_eq!(cs[2].body, "first");
    }

    #[test]
    fn comment_parse_rejects_bad_timestamp() {
        let err = Comment::parse("b".to_string(), "not-a-date").unwrap_err();
        assert!(matches!(err, GiteaError::FailedCall(_)));
    }

    // --- assignee_from_comments state machine ---

    #[test]
    fn parser_last_marker_wins() {
        let g = dummy();
        let comments = [
            comment(
                &assign_marker("alice", "qam-sle"),
                "2024-01-01T00:00:00+00:00",
            ),
            comment(
                &assign_marker("bob", "qam-sle"),
                "2024-01-02T00:00:00+00:00",
            ),
        ];
        assert_eq!(
            g.assignee_from_comments(&comments, "qam-sle"),
            Some("bob".to_string())
        );
    }

    #[test]
    fn parser_unassign_clears() {
        let g = dummy();
        let comments = [
            comment(
                &assign_marker("alice", "qam-sle"),
                "2024-01-01T00:00:00+00:00",
            ),
            comment(
                &unassign_marker("alice", "qam-sle"),
                "2024-01-02T00:00:00+00:00",
            ),
        ];
        assert_eq!(g.assignee_from_comments(&comments, "qam-sle"), None);
    }

    #[test]
    fn parser_is_group_scoped() {
        let g = dummy();
        let comments = [comment(
            &assign_marker("bob", "qam-openqa"),
            "2024-01-01T00:00:00+00:00",
        )];
        assert_eq!(g.assignee_from_comments(&comments, "qam-sle"), None);
        assert_eq!(
            g.assignee_from_comments(&comments, "qam-openqa"),
            Some("bob".to_string())
        );
    }

    // --- decision_present golden decision vectors ---

    #[test]
    fn decision_present_matches_all_decision_forms() {
        for body in [
            "@qam-sle-review: LGTM",
            "@qam-sle-review: approve",
            "@qam-sle-review: approved",
            "@qam-sle-review: decline",
            "@qam-sle-review: declined",
        ] {
            let comments = [comment(body, "2024-01-01T00:00:00+00:00")];
            assert!(decision_present(&comments, "qam-sle"), "{body}");
        }
    }

    #[test]
    fn decision_present_false_without_decision() {
        // No comments, a non-decision comment, and a chat mention that is not a
        // start-anchored decision all read as "no decision".
        assert!(!decision_present(&[], "qam-sle"));
        let plain = [comment("just a comment", "2024-01-01T00:00:00+00:00")];
        assert!(!decision_present(&plain, "qam-sle"));
        let midline = [comment(
            "ping @qam-sle-review: LGTM",
            "2024-01-01T00:00:00+00:00",
        )];
        assert!(!decision_present(&midline, "qam-sle"));
    }

    #[test]
    fn decision_present_is_group_scoped() {
        let comments = [comment(
            "@qam-openqa-review: LGTM",
            "2024-01-01T00:00:00+00:00",
        )];
        assert!(!decision_present(&comments, "qam-sle"));
        assert!(decision_present(&comments, "qam-openqa"));
    }

    #[test]
    fn assign_state_classifies_from_snapshot() {
        let g = dummy();
        let assigned = [comment(
            &assign_marker("testuser", "qam-sle"),
            "2024-01-01T00:00:00+00:00",
        )];
        assert_eq!(
            g.assign_state(&assigned, "testuser"),
            Assignment::AssignedUser
        );
        assert_eq!(
            g.assign_state(&assigned, "someoneelse"),
            Assignment::AssignedOther
        );
        assert_eq!(g.assign_state(&[], "testuser"), Assignment::Unassigned);
    }

    #[test]
    fn default_group_and_url_derivation() {
        let g = dummy();
        assert_eq!(g.group, DEFAULT_GROUP);
        assert_eq!(g.user, "testuser");
        assert_eq!(
            g.pr,
            "https://gitea.example.com/api/v1/repos/owner/repo/pulls/1"
        );
        assert_eq!(
            g.prissues,
            "https://gitea.example.com/api/v1/repos/owner/repo/issues/1/comments"
        );
    }

    #[test]
    fn new_refuses_empty_token() {
        let cfg = Config::default(); // gitea_token defaults empty
        let err = Gitea::new(
            &cfg,
            "https://gitea.example.com/api/v1/repos/owner/repo/pulls/1",
            None,
        )
        .unwrap_err();
        assert!(matches!(err, GiteaError::MissingToken));
    }

    #[test]
    fn new_refuses_untrusted_gitea_url() {
        let mut cfg = Config::default();
        cfg.gitea_token = "tok".to_string();
        // A non-https trusted origin (and non-loopback) is refused up front.
        cfg.gitea_url = "http://gitea.example.com".to_string();
        let err = Gitea::new(
            &cfg,
            "https://gitea.example.com/api/v1/repos/owner/repo/pulls/1",
            None,
        )
        .unwrap_err();
        assert!(matches!(err, GiteaError::UntrustedOrigin(_)));
    }

    #[test]
    fn default_config_trusts_src_suse_de() {
        // The shipped default is https://src.suse.de, so a matching PR URL builds.
        let mut cfg = Config::default();
        cfg.gitea_token = "tok".to_string();
        assert_eq!(cfg.gitea_url, "https://src.suse.de");
        let g = Gitea::new(&cfg, "https://src.suse.de/api/v1/repos/o/r/pulls/1", None).unwrap();
        assert!(g.pr.starts_with("https://src.suse.de/"));
    }

    // --- origin parsing / trust predicate ---

    #[test]
    fn parse_origin_fills_default_port_and_lowercases() {
        let a = parse_origin("https://Gitea.Example.com/x").unwrap();
        assert_eq!(a.scheme, "https");
        assert_eq!(a.host, "gitea.example.com");
        assert_eq!(a.port, 443);
        let b = parse_origin("https://gitea.example.com:443/y").unwrap();
        assert_eq!(a, b, "explicit default port equals implicit");
    }

    #[test]
    fn parse_origin_rejects_userinfo_and_bad_shapes() {
        for bad in [
            "https://user:pass@gitea.example.com/x", // userinfo
            "https://user@gitea.example.com/x",      // userinfo (no pass)
            "ftp://gitea.example.com/x",             // unknown scheme
            "https://:443/x",                        // empty host
            "https://gitea.example.com:notaport/x",  // bad port
            "not a url",                             // no scheme sep
            "://gitea.example.com",                  // empty scheme
        ] {
            assert!(parse_origin(bad).is_none(), "should reject: {bad}");
        }
    }

    #[test]
    fn is_trusted_matches_only_exact_https_origin() {
        let trusted = parse_origin("https://src.suse.de").unwrap();
        // Exact https origin (any path) is trusted.
        assert!(is_trusted(
            "https://src.suse.de/api/v1/repos/o/r/pulls/1",
            &trusted
        ));
        assert!(is_trusted("https://SRC.SUSE.DE/x", &trusted));
        // Everything hostile is refused.
        for bad in [
            "http://src.suse.de/x",            // plaintext, non-loopback
            "https://evil.example.com/x",      // foreign host
            "https://src.suse.de:8443/x",      // foreign port
            "https://user:pass@src.suse.de/x", // userinfo
            "https://src.suse.de.evil.com/x",  // suffix trick
        ] {
            assert!(!is_trusted(bad, &trusted), "should refuse: {bad}");
        }
    }

    #[test]
    fn loopback_http_is_trusted_for_mock_servers() {
        let trusted = parse_origin("http://127.0.0.1:8080").unwrap();
        assert!(is_trusted("http://127.0.0.1:8080/api/x", &trusted));
        // A non-loopback http trusted origin can't even be parsed as trusted.
        assert!(parse_trusted_origin("http://example.com").is_err());
        assert!(parse_trusted_origin("http://127.0.0.1:9000").is_ok());
    }
}
