//! Slack Web API connector for the `request_review` workflow.
//!
//! Posts a review request for a loaded update into a configured channel, then
//! reads back the reactions and threaded replies reviewers leave on it. The
//! surface is deliberately small — post, react, reply, identity — because the
//! review *policy* (which reaction counts as an ack, which reviewer is allowed
//! to ack) belongs to the command, not the transport.
//!
//! Two Slack-specific behaviours shape the code here and are worth stating
//! up front, because neither matches the other connectors in this crate:
//!
//! * **Application errors arrive as HTTP 200.** A refused call still returns
//!   `200 OK`, carrying `{"ok": false, "error": "channel_not_found"}`. Checking
//!   the status is therefore *not* enough — [`Slack::request`] inspects the
//!   `ok` field on every response and converts a false one into
//!   [`SlackError::Api`].
//! * **Rate limiting is routine, not exceptional.** Tier-3 methods allow
//!   roughly 50 requests a minute, and a watch loop polling several templates
//!   will meet a `429` eventually. That is modelled as its own
//!   [`SlackError::RateLimited`] variant so a caller can back off and keep
//!   watching instead of counting throttling as a failure.
//!
//! Like the other connectors, the constructor pair splits config-reading
//! ([`Slack::new`]) from the injectable seam ([`Slack::with_client`]) so tests
//! can aim the client at a `wiremock` server.

use mtui_config::Config;
use reqwest::Method;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::error::SlackError;
use crate::http::{
    HttpClient, MAX_API_BODY, VerifyPolicy, is_ssl_verification_error, read_body_capped,
    resolve_verify, sanitize_url, ssl_verification_hint,
};

/// Maximum `conversations.replies` pages walked before giving up.
///
/// A review thread is a handful of messages; a cursor that keeps yielding
/// pages past this many means either a pathological thread or a server-side
/// loop, and either way the watch should not spin forever on one poll.
const MAX_REPLY_PAGES: usize = 10;

/// Reaction names that count as an approval ack, after skin-tone stripping.
const ACK_REACTIONS: &[&str] = &["+1", "thumbsup", "white_check_mark", "heavy_check_mark"];

/// Reaction names that count as a rejection, after skin-tone stripping.
const NACK_REACTIONS: &[&str] = &["-1", "thumbsdown", "x", "no_entry"];

/// A message this client posted, identified the way Slack wants it referenced.
///
/// The `channel` is Slack's **canonical** channel ID from the post response,
/// not whatever was configured. Those differ whenever the config names a
/// channel by `#name`, and every later `reactions.get` / `conversations.replies`
/// call must use the canonical form, so it is what gets persisted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PostedMessage {
    /// The canonical channel ID Slack resolved the post to.
    pub channel: String,
    /// The message timestamp, which doubles as its ID within the channel.
    pub ts: String,
}

/// A reaction on a message, as returned by `reactions.get`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reaction {
    /// The emoji name, with any `::skin-tone-N` suffix already stripped.
    pub name: String,
    /// User IDs that added this reaction.
    pub users: Vec<String>,
}

/// A threaded reply to a posted message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reply {
    /// The user ID that wrote the reply.
    pub user: String,
    /// The message body.
    pub text: String,
    /// The reply's own timestamp, used to track which replies were seen.
    pub ts: String,
}

/// Strip a trailing `::skin-tone-N` modifier from a Slack reaction name.
///
/// Slack reports `+1::skin-tone-4` as a distinct reaction from `+1`, so a
/// literal comparison would silently ignore an ack from anyone who has set a
/// default skin tone. Normalising at the transport edge means every consumer
/// compares against the base name and cannot forget this.
#[must_use]
pub fn normalize_reaction(name: &str) -> &str {
    match name.split_once("::skin-tone-") {
        Some((base, _)) => base,
        None => name,
    }
}

/// Whether `name` is a reaction that acknowledges/approves a review request.
#[must_use]
pub fn is_ack_reaction(name: &str) -> bool {
    ACK_REACTIONS.contains(&normalize_reaction(name))
}

/// Whether `name` is a reaction that rejects a review request.
#[must_use]
pub fn is_nack_reaction(name: &str) -> bool {
    NACK_REACTIONS.contains(&normalize_reaction(name))
}

/// Whether `host` is a loopback address, so a plain-HTTP mock server is allowed
/// to receive the token in tests.
fn is_loopback(host: &str) -> bool {
    host == "localhost"
        || host == "127.0.0.1"
        || host == "::1"
        || host == "[::1]"
        || host.starts_with("127.")
}

/// Whether `url` may carry the bearer token: `https`, or `http` to loopback.
///
/// Unlike the Gitea connector — whose PR URL comes from attacker-influenceable
/// checked-out metadata and so needs a full origin comparison — the Slack base
/// is only ever read from the user's own config. The remaining risk is a
/// misconfiguration that would put the token on the wire in clear text, which
/// this refuses outright.
fn is_token_safe_url(url: &str) -> bool {
    let Some((scheme, rest)) = url.split_once("://") else {
        return false;
    };
    // Userinfo in the authority would mean credentials in the URL itself.
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..end];
    if authority.contains('@') {
        return false;
    }
    let host = authority
        .rsplit_once(':')
        .map_or(authority, |(h, _)| h)
        .trim_start_matches('[')
        .trim_end_matches(']');
    match scheme {
        "https" => true,
        "http" => is_loopback(host),
        _ => false,
    }
}

/// Percent-encode `params` into a query string, without a leading `?`.
///
/// reqwest's `query` feature is disabled workspace-wide, so query strings are
/// hand-rolled here exactly as the TeReGen connector does.
fn build_query(params: &[(&str, &str)]) -> String {
    params
        .iter()
        .map(|(k, v)| format!("{}={}", urlencoding::encode(k), urlencoding::encode(v)))
        .collect::<Vec<_>>()
        .join("&")
}

/// The shared envelope every Slack Web API method returns.
#[derive(Debug, Deserialize)]
struct SlackEnvelope {
    ok: bool,
    error: Option<String>,
}

/// Slack Web API client for the review-request workflow.
#[derive(Debug, Clone)]
pub struct Slack {
    http: HttpClient,
    token: String,
    /// API base such as `https://slack.com/api`, never with a trailing slash.
    base: String,
}

impl Slack {
    /// Build a client from the session configuration.
    ///
    /// The integration is opt-in, so the common case is that this refuses: an
    /// mtui with no `[slack]` configuration gets [`SlackError::Disabled`], not
    /// an authentication failure several calls later.
    ///
    /// # Errors
    ///
    /// [`SlackError::Disabled`] when `[slack] enabled` is unset or `false`,
    /// [`SlackError::MissingToken`] when no token is configured, and
    /// [`SlackError::UntrustedOrigin`] when the configured API base would put
    /// the token on an unencrypted wire.
    pub fn new(config: &Config) -> Result<Self, SlackError> {
        if !config.slack_enabled {
            return Err(SlackError::Disabled);
        }
        if config.slack_token.is_empty() {
            return Err(SlackError::MissingToken);
        }
        let verify: VerifyPolicy = resolve_verify(
            VerifyPolicy::Default(true),
            Some(VerifyPolicy::from_config(&config.ssl_verify)),
        );
        let http = HttpClient::new(verify)?;
        Self::with_client(http, config.slack_token.clone(), &config.slack_api_url)
    }

    /// Build a client over an existing [`HttpClient`] against `api_base`.
    ///
    /// The injectable seam the other connectors use: tests point `api_base` at
    /// a `wiremock` server, which is permitted because it is loopback.
    ///
    /// # Errors
    ///
    /// [`SlackError::UntrustedOrigin`] when `api_base` is not `https` (or
    /// `http` to loopback), or embeds userinfo.
    pub fn with_client(
        http: HttpClient,
        token: String,
        api_base: &str,
    ) -> Result<Self, SlackError> {
        let base = api_base.trim_end_matches('/').to_owned();
        if !is_token_safe_url(&base) {
            return Err(SlackError::UntrustedOrigin(sanitize_url(&base)));
        }
        Ok(Self { http, token, base })
    }

    /// The configured API base.
    #[must_use]
    pub fn base(&self) -> &str {
        &self.base
    }

    /// Issue a Slack Web API call and return its decoded envelope.
    ///
    /// Folds the three failure shapes — transport, HTTP status, and the
    /// `ok: false` application error that arrives as HTTP 200 — into typed
    /// errors, and lifts `429` into [`SlackError::RateLimited`] before any of
    /// them so a caller can back off rather than fail.
    async fn request(
        &self,
        method: Method,
        api_method: &str,
        query: &[(&str, &str)],
        body: Option<Value>,
    ) -> Result<Value, SlackError> {
        let mut url = format!("{}/{api_method}", self.base);
        if !query.is_empty() {
            url.push('?');
            url.push_str(&build_query(query));
        }

        tracing::debug!("Requesting {method} on {}", sanitize_url(&url));
        let mut builder = self
            .http
            .inner()
            .request(method.clone(), &url)
            .header("Authorization", format!("Bearer {}", self.token))
            .header("Accept", "application/json");
        if let Some(json) = &body {
            builder = builder.json(json);
        }

        let response = match builder.send().await {
            Ok(r) => r,
            Err(e) => {
                if is_ssl_verification_error(&e) {
                    tracing::error!("{}", ssl_verification_hint(None));
                    tracing::debug!("Slack TLS error detail: {e}");
                } else {
                    tracing::warn!("API call to Slack failed: {e}");
                }
                return Err(SlackError::FailedCall(format!(
                    "{method} - {}",
                    sanitize_url(&url)
                )));
            }
        };

        let status = response.status();
        if status.as_u16() == 429 {
            // Slack sends the backoff in seconds; a malformed or absent header
            // leaves the decision to the caller's own default.
            let retry_after = response
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.trim().parse::<u64>().ok());
            tracing::debug!(retry_after, "Slack rate limited the request");
            return Err(SlackError::RateLimited { retry_after });
        }
        if !status.is_success() {
            tracing::warn!(
                "API call to {} failed with status code: {status}",
                sanitize_url(&url)
            );
            return Err(SlackError::FailedCall(format!(
                "{method} - {} returned status {}",
                sanitize_url(&url),
                status.as_u16()
            )));
        }

        let bytes = read_body_capped(response, MAX_API_BODY)
            .await
            .map_err(|e| {
                SlackError::FailedCall(format!("{method} - {}: {e}", sanitize_url(&url)))
            })?;
        let value: Value = serde_json::from_slice(&bytes).map_err(|e| {
            SlackError::FailedCall(format!("{method} - {}: {e}", sanitize_url(&url)))
        })?;

        // A 200 proves only that Slack answered; `ok` is what says it agreed.
        let envelope: SlackEnvelope = serde_json::from_value(value.clone()).map_err(|e| {
            SlackError::FailedCall(format!("{method} - {}: {e}", sanitize_url(&url)))
        })?;
        if !envelope.ok {
            return Err(SlackError::Api(
                envelope.error.unwrap_or_else(|| "unknown".to_owned()),
            ));
        }
        Ok(value)
    }

    /// Verify the token and return the bot's own user ID.
    ///
    /// Called before posting so a bad token is reported as such, up front,
    /// instead of surfacing as a confusing failure on the post itself. The
    /// returned ID also lets the caller ignore the bot's own reactions.
    ///
    /// # Errors
    ///
    /// Any [`SlackError`]; [`SlackError::Api`] carries codes such as
    /// `invalid_auth` or `account_inactive`.
    pub async fn auth_test(&self) -> Result<String, SlackError> {
        let data = self.request(Method::POST, "auth.test", &[], None).await?;
        data.get("user_id")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| SlackError::FailedCall("auth.test - missing user_id".to_owned()))
    }

    /// Post `text` to `channel` and return its canonical identifiers.
    ///
    /// # Errors
    ///
    /// Any [`SlackError`]; [`SlackError::Api`] carries codes such as
    /// `channel_not_found` or `not_in_channel`.
    pub async fn post_message(
        &self,
        channel: &str,
        text: &str,
    ) -> Result<PostedMessage, SlackError> {
        tracing::info!("Posting a review request to Slack");
        let data = self
            .request(
                Method::POST,
                "chat.postMessage",
                &[],
                Some(json!({ "channel": channel, "text": text })),
            )
            .await?;
        // Persist what Slack echoes back, not what was asked for: a `#name`
        // channel resolves to an ID here, and only the ID works for the
        // reaction/reply reads that follow.
        let canonical = data
            .get("channel")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| {
                SlackError::FailedCall("chat.postMessage - missing channel".to_owned())
            })?;
        let ts = data
            .get("ts")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| SlackError::FailedCall("chat.postMessage - missing ts".to_owned()))?;
        Ok(PostedMessage {
            channel: canonical,
            ts,
        })
    }

    /// Read the reactions currently on the message identified by `channel`/`ts`.
    ///
    /// Reaction names come back skin-tone-normalised. An absent `reactions`
    /// field means nobody has reacted yet, which is an empty list rather than
    /// an error.
    ///
    /// # Errors
    ///
    /// Any [`SlackError`]; [`SlackError::Api`] carries `message_not_found`
    /// when the message was deleted underneath the watch.
    pub async fn reactions(&self, channel: &str, ts: &str) -> Result<Vec<Reaction>, SlackError> {
        let data = self
            .request(
                Method::GET,
                "reactions.get",
                &[("channel", channel), ("timestamp", ts)],
                None,
            )
            .await?;
        let Some(items) = data
            .get("message")
            .and_then(|m| m.get("reactions"))
            .and_then(Value::as_array)
        else {
            return Ok(Vec::new());
        };
        Ok(items
            .iter()
            .filter_map(|r| {
                let name = r.get("name").and_then(Value::as_str)?;
                let users = r
                    .get("users")
                    .and_then(Value::as_array)
                    .map(|us| {
                        us.iter()
                            .filter_map(Value::as_str)
                            .map(str::to_owned)
                            .collect()
                    })
                    .unwrap_or_default();
                Some(Reaction {
                    name: normalize_reaction(name).to_owned(),
                    users,
                })
            })
            .collect())
    }

    /// Read the threaded replies to the message identified by `channel`/`ts`.
    ///
    /// Follows Slack's cursor pagination, bounded by [`MAX_REPLY_PAGES`]. The
    /// parent message is excluded: Slack returns it as the first element of
    /// the first page, and it is the request itself, not a reply to it.
    ///
    /// # Errors
    ///
    /// Any [`SlackError`]; [`SlackError::Api`] carries `thread_not_found`.
    pub async fn replies(&self, channel: &str, ts: &str) -> Result<Vec<Reply>, SlackError> {
        let mut out: Vec<Reply> = Vec::new();
        let mut cursor: Option<String> = None;

        for page in 0..MAX_REPLY_PAGES {
            let mut query: Vec<(&str, &str)> = vec![("channel", channel), ("ts", ts)];
            if let Some(c) = &cursor {
                query.push(("cursor", c.as_str()));
            }
            let data = self
                .request(Method::GET, "conversations.replies", &query, None)
                .await?;

            if let Some(messages) = data.get("messages").and_then(Value::as_array) {
                for m in messages {
                    let Some(mts) = m.get("ts").and_then(Value::as_str) else {
                        continue;
                    };
                    // The parent is the review request itself, not a reply.
                    if mts == ts {
                        continue;
                    }
                    out.push(Reply {
                        user: m
                            .get("user")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_owned(),
                        text: m
                            .get("text")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_owned(),
                        ts: mts.to_owned(),
                    });
                }
            }

            cursor = data
                .get("response_metadata")
                .and_then(|m| m.get("next_cursor"))
                .and_then(Value::as_str)
                .filter(|c| !c.is_empty())
                .map(str::to_owned);
            if cursor.is_none() {
                return Ok(out);
            }
            if page + 1 == MAX_REPLY_PAGES {
                tracing::warn!(
                    pages = MAX_REPLY_PAGES,
                    "Slack thread has more replies than the page cap; ignoring the rest"
                );
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn client(base: &str) -> Result<Slack, SlackError> {
        let http = HttpClient::new(VerifyPolicy::Default(true)).expect("client builds");
        Slack::with_client(http, "xoxb-test".to_owned(), base)
    }

    #[test]
    fn normalize_reaction_strips_skin_tone() {
        assert_eq!(normalize_reaction("+1::skin-tone-4"), "+1");
        assert_eq!(normalize_reaction("thumbsup::skin-tone-2"), "thumbsup");
        // A bare name is returned untouched.
        assert_eq!(normalize_reaction("+1"), "+1");
    }

    #[test]
    fn ack_and_nack_recognise_skin_toned_variants() {
        // The whole point of normalising: a toned thumbs-up still acks.
        assert!(is_ack_reaction("+1::skin-tone-6"));
        assert!(is_ack_reaction("white_check_mark"));
        assert!(!is_ack_reaction("eyes"));

        assert!(is_nack_reaction("-1::skin-tone-3"));
        assert!(is_nack_reaction("x"));
        assert!(!is_nack_reaction("+1"));

        // Ack and nack sets must stay disjoint, or a message could be both.
        for a in ACK_REACTIONS {
            assert!(!is_nack_reaction(a), "{a} is in both sets");
        }
    }

    #[test]
    fn with_client_accepts_https_and_loopback_http() {
        assert!(client("https://slack.com/api").is_ok());
        // wiremock serves plain HTTP on loopback; tests depend on this.
        assert!(client("http://127.0.0.1:8080/api").is_ok());
        assert!(client("http://localhost:9999").is_ok());
    }

    #[test]
    fn with_client_refuses_to_put_the_token_in_clear_text() {
        // Plain HTTP to a non-loopback host would leak the bearer token.
        let err = client("http://slack.example.com/api").unwrap_err();
        assert!(matches!(err, SlackError::UntrustedOrigin(_)), "{err:?}");
        // Userinfo means credentials in the URL itself.
        assert!(matches!(
            client("https://user:pw@slack.com/api").unwrap_err(),
            SlackError::UntrustedOrigin(_)
        ));
        // A non-HTTP scheme is not a Slack API base.
        assert!(matches!(
            client("ftp://slack.com/api").unwrap_err(),
            SlackError::UntrustedOrigin(_)
        ));
    }

    #[test]
    fn untrusted_origin_error_never_echoes_credentials() {
        let err = client("https://user:hunter2@slack.com/api").unwrap_err();
        let msg = err.to_string();
        assert!(!msg.contains("hunter2"), "leaked a credential: {msg}");
    }

    #[test]
    fn with_client_trims_trailing_slash() {
        let c = client("https://slack.com/api/").expect("builds");
        assert_eq!(c.base(), "https://slack.com/api");
    }

    #[test]
    fn new_refuses_when_disabled_or_tokenless() {
        let mut config = Config::default();

        // The out-of-the-box state: nothing configured, so nothing is posted.
        assert!(matches!(
            Slack::new(&config).unwrap_err(),
            SlackError::Disabled
        ));

        // Disabled wins even with a token present, so the reason is
        // unambiguous when a site has switched the feature off on purpose.
        config.slack_token = "xoxb-test".to_owned();
        assert!(matches!(
            Slack::new(&config).unwrap_err(),
            SlackError::Disabled
        ));

        // Enabled but tokenless is the out-of-the-box state.
        config.slack_enabled = true;
        config.slack_token = String::new();
        assert!(matches!(
            Slack::new(&config).unwrap_err(),
            SlackError::MissingToken
        ));
    }

    #[test]
    fn build_query_percent_encodes() {
        assert_eq!(build_query(&[("channel", "C1")]), "channel=C1");
        // A `#name` channel and a `.`-bearing ts must survive encoding.
        assert_eq!(
            build_query(&[("channel", "#qam review"), ("ts", "1700000000.000100")]),
            "channel=%23qam%20review&ts=1700000000.000100"
        );
    }

    #[test]
    fn rate_limited_message_reports_the_backoff() {
        let with = SlackError::RateLimited {
            retry_after: Some(30),
        };
        assert!(with.to_string().contains("retry after 30s"), "{with}");
        // No header means no clause, rather than a misleading "0s".
        let without = SlackError::RateLimited { retry_after: None };
        assert!(!without.to_string().contains("retry after"), "{without}");
    }
}
