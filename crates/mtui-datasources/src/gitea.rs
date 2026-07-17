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
pub const DEFAULT_GROUP: &str = "qam-sle";

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
pub fn unassign_marker(user: &str, group: &str) -> String {
    UNASSIGN_TEMPLATE
        .replace("{user}", user)
        .replace("{group}", group)
}

/// Convert a Gitea PR *web* URL to its REST *API* URL.
///
/// `https://<host>/<owner>/<repo>/pulls/<n>` becomes
/// `https://<host>/api/v1/repos/<owner>/<repo>/pulls/<n>` — the form the
/// [`Gitea`] constructor expects. The SLFO update feed only carries the web
/// form (an update's `external_url`), so callers that build a client straight
/// from the feed need this conversion.
///
/// # Errors
///
/// Returns [`GiteaError::InvalidPrUrl`] if `web_url` is not a recognisable
/// Gitea PR URL (mirroring upstream's `ValueError`).
pub fn pr_api_url(web_url: &str) -> Result<String, GiteaError> {
    let invalid = || GiteaError::InvalidPrUrl(web_url.to_string());
    // Split `scheme://authority/path...`. A bare non-URL (no `://`) is invalid.
    let (scheme, rest) = web_url.split_once("://").ok_or_else(invalid)?;
    if scheme.is_empty() {
        return Err(invalid());
    }
    let (authority, path) = match rest.split_once('/') {
        Some((a, p)) => (a, p),
        None => (rest, ""),
    };
    if authority.is_empty() {
        return Err(invalid());
    }
    // Drop any query/fragment, then take the non-empty path segments.
    let path = path.split(['?', '#']).next().unwrap_or("");
    let parts: Vec<&str> = path
        .trim_matches('/')
        .split('/')
        .filter(|s| !s.is_empty())
        .collect();
    // Need owner/repo/pulls/number, and the second-to-last segment must be
    // "pulls" — the exact guard upstream applies.
    if parts.len() < 4 || parts[parts.len() - 2] != "pulls" {
        return Err(invalid());
    }
    let tail = &parts[parts.len() - 4..];
    let (owner, repo, number) = (tail[0], tail[1], tail[3]);
    Ok(format!(
        "{scheme}://{authority}/api/v1/repos/{owner}/{repo}/pulls/{number}"
    ))
}

/// Extract the host (authority without any port) from an `scheme://host[:port]/…`
/// URL, for the TLS-failure hint. Returns `None` if the shape is unexpected.
fn host_of(url: &str) -> Option<String> {
    let rest = url.split_once("://")?.1;
    let authority = rest.split(['/', '?', '#']).next()?;
    let host = authority.split(':').next()?;
    (!host.is_empty()).then(|| host.to_string())
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
/// machine depends on. The `serial` (comment id) is retained for diagnostics.
#[derive(Debug, Clone)]
pub struct Comment {
    /// The Gitea comment id.
    pub serial: i64,
    /// The comment body.
    pub body: String,
    /// The comment's `updated_at` timestamp.
    pub date: DateTime<FixedOffset>,
}

impl Comment {
    /// Build a comment, parsing an RFC3339 `updated_at` timestamp.
    ///
    /// # Errors
    ///
    /// Returns [`GiteaError::FailedCall`] if `updated_at` is not a parseable
    /// RFC3339 timestamp (folded into the fetch failure surface, matching
    /// upstream where a malformed comment payload aborts the API call).
    pub fn parse(serial: i64, body: String, updated_at: &str) -> Result<Self, GiteaError> {
        let date = DateTime::parse_from_rfc3339(updated_at).map_err(|e| {
            GiteaError::FailedCall(format!("unparseable comment timestamp {updated_at:?}: {e}"))
        })?;
        Ok(Self { serial, body, date })
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
    id: i64,
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
    /// or [`GiteaError::Http`] if the shared HTTP client cannot be built (e.g. a
    /// configured CA bundle cannot be read).
    pub fn new(config: &Config, giteaprapi: &str, group: Option<&str>) -> Result<Self, GiteaError> {
        if config.gitea_token.is_empty() {
            return Err(GiteaError::MissingToken);
        }
        let verify: VerifyPolicy = resolve_verify(
            VerifyPolicy::Default(true),
            Some(VerifyPolicy::from_config(&config.ssl_verify)),
        );
        let http = HttpClient::new(verify)?;
        Ok(Self::with_client(
            http,
            config.gitea_token.clone(),
            config.session_user.clone(),
            giteaprapi,
            group,
        ))
    }

    /// Build a client from an already-constructed [`HttpClient`] and explicit
    /// credentials, bypassing [`Config`].
    ///
    /// The composition-root / test seam: it lets a caller inject a client whose
    /// TLS posture (or base host, under `wiremock`) is already fixed. The token
    /// is trusted as non-empty here — [`new`](Self::new) is the guarded entry.
    #[must_use]
    pub fn with_client(
        http: HttpClient,
        token: String,
        user: String,
        giteaprapi: &str,
        group: Option<&str>,
    ) -> Self {
        // `.../pulls/<n>` -> `.../issues/<n>/comments`, matching upstream's
        // `giteaprapi.replace("pulls", "issues") + "/comments"`.
        let prissues = format!("{}/comments", giteaprapi.replace("pulls", "issues"));
        Self {
            http,
            token,
            user,
            group: group.unwrap_or(DEFAULT_GROUP).to_string(),
            pr: giteaprapi.to_string(),
            prissues,
            assign_re: Regex::new(
                r"^<MTUI: PR - UV assigned to user: (?P<user>.*) - group: (?P<group>.*) >",
            )
            .expect("static assign regex is valid"),
            unassign_re: Regex::new(
                r"^<MTUI: PR - UV unassigned user: (?P<user>.*) - group: (?P<group>.*) >",
            )
            .expect("static unassign regex is valid"),
        }
    }

    /// The PR API URL this client targets.
    #[must_use]
    pub fn pr_url(&self) -> &str {
        &self.pr
    }

    /// The review group this client operates on behalf of.
    #[must_use]
    pub fn group(&self) -> &str {
        &self.group
    }

    /// The session user this client acts as by default.
    #[must_use]
    pub fn user(&self) -> &str {
        &self.user
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
            .map(|c| Comment::parse(c.id, c.body, &c.updated_at))
            .collect()
    }

    /// Replay assign/unassign markers over `comments` (assumed chronologically
    /// sorted) and return the current assignee for `group`, or `None`.
    ///
    /// The last valid assignment or unassignment marker for the group wins; a
    /// marker for another group is ignored. Public + static so the state
    /// machine can be tested without any HTTP.
    #[must_use]
    pub fn assignee_from_comments(&self, comments: &[Comment], group: &str) -> Option<String> {
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
            None,
        )
    }

    fn comment(serial: i64, body: &str, date: &str) -> Comment {
        Comment::parse(serial, body.to_string(), date).unwrap()
    }

    // --- pr_api_url ---

    #[test]
    fn pr_api_url_converts_web_to_api() {
        assert_eq!(
            pr_api_url("https://src.example.de/products/SLFO/pulls/4919").unwrap(),
            "https://src.example.de/api/v1/repos/products/SLFO/pulls/4919"
        );
    }

    #[test]
    fn pr_api_url_trailing_slash_ok() {
        assert_eq!(
            pr_api_url("https://h.example/owner/repo/pulls/1/").unwrap(),
            "https://h.example/api/v1/repos/owner/repo/pulls/1"
        );
    }

    #[test]
    fn pr_api_url_preserves_port() {
        assert_eq!(
            pr_api_url("https://h.example:3000/owner/repo/pulls/7").unwrap(),
            "https://h.example:3000/api/v1/repos/owner/repo/pulls/7"
        );
    }

    #[test]
    fn pr_api_url_rejects_non_pr_urls() {
        for bad in [
            "https://h.example/owner/repo/issues/1", // not a pulls URL
            "https://h.example/owner/repo",          // too short
            "not a url",
        ] {
            let err = pr_api_url(bad).unwrap_err();
            assert!(matches!(err, GiteaError::InvalidPrUrl(_)), "{bad}");
            assert!(err.to_string().contains("not a Gitea PR URL"));
        }
    }

    // --- Comment ordering / equality ---

    #[test]
    fn comment_orders_and_equals_by_date() {
        let c1 = comment(1, "first", "2024-01-01T00:00:00+00:00");
        let c2 = comment(2, "second", "2024-01-02T00:00:00+00:00");
        assert!(c1 < c2);
        assert!(c2 > c1);
        // Equal dates -> equal comments even with different serials/bodies.
        let a = comment(1, "a", "2024-01-01T00:00:00+00:00");
        let b = comment(2, "b", "2024-01-01T00:00:00+00:00");
        assert_eq!(a, b);
    }

    #[test]
    fn comments_sort_chronologically() {
        let mut cs = [
            comment(1, "first", "2024-01-03T00:00:00+00:00"),
            comment(2, "second", "2024-01-01T00:00:00+00:00"),
            comment(3, "third", "2024-01-02T00:00:00+00:00"),
        ];
        cs.sort();
        assert_eq!(cs[0].serial, 2);
        assert_eq!(cs[1].serial, 3);
        assert_eq!(cs[2].serial, 1);
    }

    #[test]
    fn comment_parse_rejects_bad_timestamp() {
        let err = Comment::parse(1, "b".to_string(), "not-a-date").unwrap_err();
        assert!(matches!(err, GiteaError::FailedCall(_)));
    }

    // --- assignee_from_comments state machine ---

    #[test]
    fn parser_last_marker_wins() {
        let g = dummy();
        let comments = [
            comment(
                1,
                &assign_marker("alice", "qam-sle"),
                "2024-01-01T00:00:00+00:00",
            ),
            comment(
                2,
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
                1,
                &assign_marker("alice", "qam-sle"),
                "2024-01-01T00:00:00+00:00",
            ),
            comment(
                2,
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
            1,
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
            let comments = [comment(1, body, "2024-01-01T00:00:00+00:00")];
            assert!(decision_present(&comments, "qam-sle"), "{body}");
        }
    }

    #[test]
    fn decision_present_false_without_decision() {
        // No comments, a non-decision comment, and a chat mention that is not a
        // start-anchored decision all read as "no decision".
        assert!(!decision_present(&[], "qam-sle"));
        let plain = [comment(1, "just a comment", "2024-01-01T00:00:00+00:00")];
        assert!(!decision_present(&plain, "qam-sle"));
        let midline = [comment(
            1,
            "ping @qam-sle-review: LGTM",
            "2024-01-01T00:00:00+00:00",
        )];
        assert!(!decision_present(&midline, "qam-sle"));
    }

    #[test]
    fn decision_present_is_group_scoped() {
        let comments = [comment(
            1,
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
            1,
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
        assert_eq!(g.group(), DEFAULT_GROUP);
        assert_eq!(g.user(), "testuser");
        assert_eq!(
            g.pr_url(),
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
}
