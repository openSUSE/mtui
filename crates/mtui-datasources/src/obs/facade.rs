//! The `OSC(config, rrid)` never-raise review seam (native OBS API, no `osc`).
//!
//! Ported from upstream `mtui/data_sources/oscqam.py`'s `OSC` class. This is the
//! final cutover of the native OBS backend: the [`Osc`] seam binds the resolved
//! [`Config`] and target [`RequestReviewID`] to the five QAM operations
//! ([`crate::obs::qam`]), reading credentials from the user's `~/.oscrc`
//! ([`read_credentials`]) and authenticating with SSH signature auth
//! ([`ObsSignatureAuth`]). It replaces the historical shell-out to the external
//! `osc qam` plugin (the deleted `oscqam` subprocess wrapper).
//!
//! ## Never-raise contract
//!
//! Upstream folds *every* failure — reading oscrc, loading the key, building the
//! session, the authenticated calls, XML parsing — into a logged `False`,
//! because callers (`apicall` / `approve`) invoke the seam bare with no guard of
//! their own. This port keeps the same **no-panic** contract but returns the
//! typed [`ObsError`] instead of a bare bool (the workspace's typed-`Result`
//! house rule): the seam never `panic!`s / `unwrap`s / `expect`s — every
//! transport/config/parse fault becomes an `Err(ObsError)` the caller logs.
//! The escape hatches upstream PR#323 hardened are covered:
//!
//! * a non-PEM key file → [`ObsError::Config`] from the auth layer (never a
//!   panic), surfaced when the first authenticated call is challenged;
//! * `expanduser()` with no home (a headless container) → the oscrc reader
//!   leaves `~` in place rather than panicking, and any resulting read failure
//!   is a typed [`ObsError::Config`];
//! * a lone surrogate in the request body from MCP JSON input → impossible to
//!   reach this layer: Rust's `String`/`&str` cannot hold a lone surrogate, and
//!   `serde_json` rejects one at the MCP JSON boundary (see the boundary test),
//!   so encoding the body to bytes cannot panic.

use std::sync::Arc;
use std::time::Duration;

use mtui_config::Config;
use mtui_types::RequestReviewID;

use crate::http::{VerifyPolicy, resolve_verify};
use crate::obs::auth::ObsSignatureAuth;
use crate::obs::client::ObsClient;
use crate::obs::errors::ObsError;
use crate::obs::oscrc::read_credentials;
use crate::obs::qam;

/// A resolved, authenticated OBS client plus the acting user, produced by the
/// facade's credential/transport build step.
type Built = (ObsClient, String);

/// The credential-reading + client-building seam.
///
/// The production path ([`build_client`]) reads `~/.oscrc` and attaches SSH
/// signature auth; tests inject a closure that returns an already-built
/// wiremock-backed [`ObsClient`] (happy path) or an [`Err`] (never-raise
/// escape-hatch paths) without touching the real oscrc or a real agent.
type ClientFactory = Arc<dyn Fn(&Config) -> Result<Built, ObsError> + Send + Sync>;

/// The native OBS review backend (approve / assign / unassign / comment /
/// reject).
///
/// Construct with [`Osc::new`]; construction cannot fail (mirroring upstream
/// `OSC.__init__`). Each operation reads credentials, builds an authenticated
/// client, and runs the corresponding [`crate::obs::qam`] op — folding any
/// failure into a logged `Err(ObsError)` rather than panicking.
#[derive(Clone)]
pub struct Osc {
    config: Config,
    rrid: RequestReviewID,
    factory: ClientFactory,
}

impl Osc {
    /// Build an [`Osc`] seam for `config` and the target `rrid`.
    ///
    /// Construction cannot fail; the credential/transport build is deferred to
    /// each operation (and folded into its never-raise result).
    #[must_use]
    pub fn new(config: Config, rrid: RequestReviewID) -> Self {
        Self {
            config,
            rrid,
            factory: Arc::new(build_client),
        }
    }

    /// Build an [`Osc`] seam with an explicit client factory (the test seam).
    ///
    /// Lets tests supply an already-built (e.g. wiremock-backed) client, or an
    /// error, without reading the real `~/.oscrc` or contacting a real
    /// ssh-agent.
    #[must_use]
    pub fn with_factory(config: Config, rrid: RequestReviewID, factory: ClientFactory) -> Self {
        Self {
            config,
            rrid,
            factory,
        }
    }

    /// Run a native OBS operation, folding any failure into a logged `Err`.
    ///
    /// Everything fallible — reading oscrc, building the client, the
    /// authenticated calls, XML parsing — happens inside here, because callers
    /// invoke the seam methods bare. Nothing in this path panics: `read_credentials`
    /// / `ObsClient::new` / the ops all return typed [`ObsError`], and the future
    /// produced by `op` is awaited without any `unwrap`/`expect`.
    async fn run<F, Fut>(&self, op: F) -> Result<(), ObsError>
    where
        F: FnOnce(ObsClient, String) -> Fut,
        Fut: std::future::Future<Output = Result<(), ObsError>>,
    {
        let result = async {
            let (client, user) = (self.factory)(&self.config)?;
            op(client, user).await
        }
        .await;
        if let Err(e) = &result {
            tracing::error!("OBS operation on {} failed: {e}", self.rrid);
        }
        result
    }

    /// Approve the review for the acting user (group-approve is refused).
    ///
    /// # Errors
    ///
    /// Returns [`ObsError`] on any credential, transport, parse, or
    /// workflow-precondition failure (the failure is also logged).
    pub async fn approve(&self, groups: &[String]) -> Result<(), ObsError> {
        let cfg = self.config.clone();
        let rrid = self.rrid.clone();
        let groups = groups.to_vec();
        self.run(move |client, user| async move {
            qam::approve(
                &client,
                &cfg.reports_url,
                &cfg.fancy_reports_url,
                &cfg.ssl_verify,
                &rrid,
                &user,
                &groups,
            )
            .await
        })
        .await
    }

    /// Assign the review to the acting user for the resolved group(s).
    ///
    /// # Errors
    ///
    /// Returns [`ObsError`] on any credential, transport, parse, or
    /// workflow-precondition failure (the failure is also logged).
    pub async fn assign(&self, groups: &[String]) -> Result<(), ObsError> {
        let cfg = self.config.clone();
        let rrid = self.rrid.clone();
        let groups = groups.to_vec();
        self.run(move |client, user| async move {
            qam::assign(
                &client,
                &cfg.reports_url,
                &cfg.ssl_verify,
                &rrid,
                &user,
                &groups,
            )
            .await
        })
        .await
    }

    /// Revert the acting user's assignment for the resolved (or explicit)
    /// group(s).
    ///
    /// # Errors
    ///
    /// Returns [`ObsError`] on any credential, transport, parse, or
    /// workflow-precondition failure (the failure is also logged).
    pub async fn unassign(&self, groups: &[String]) -> Result<(), ObsError> {
        let rrid = self.rrid.clone();
        let groups = groups.to_vec();
        self.run(
            move |client, user| async move { qam::unassign(&client, &rrid, &user, &groups).await },
        )
        .await
    }

    /// Add a (raw, unprefixed) comment to the review.
    ///
    /// # Errors
    ///
    /// Returns [`ObsError`] on an empty comment, or any credential/transport
    /// failure (the failure is also logged).
    pub async fn comment(&self, comment: &str) -> Result<(), ObsError> {
        let rrid = self.rrid.clone();
        let text = comment.to_owned();
        self.run(move |client, _user| async move { qam::comment(&client, &rrid, &text).await })
            .await
    }

    /// Decline the review for the acting user, recording the reject reason.
    ///
    /// # Errors
    ///
    /// Returns [`ObsError`] on any credential, transport, parse, or
    /// workflow-precondition failure (the failure is also logged).
    pub async fn reject(
        &self,
        groups: &[String],
        reason: &str,
        message: &str,
    ) -> Result<(), ObsError> {
        let cfg = self.config.clone();
        let rrid = self.rrid.clone();
        let groups = groups.to_vec();
        let reason = reason.to_owned();
        let message = message.to_owned();
        self.run(move |client, user| async move {
            qam::reject(
                &client,
                &cfg.reports_url,
                &cfg.fancy_reports_url,
                &cfg.ssl_verify,
                &rrid,
                &user,
                &groups,
                &reason,
                &message,
            )
            .await
        })
        .await
    }
}

/// The production client factory: read `~/.oscrc` and attach SSH signature auth.
///
/// Reads the credentials for `obs_api_url` from `obs_conffile` (empty =
/// `~/.oscrc`), builds an [`ObsClient`] against `obs_api_url` with the coarse
/// `obs_request_timeout` budget and the resolved TLS posture, and injects an
/// [`ObsSignatureAuth`] signer for the acting user's key.
fn build_client(config: &Config) -> Result<Built, ObsError> {
    let credentials = read_credentials(&config.obs_api_url, &config.obs_conffile)?;
    let verify: VerifyPolicy = resolve_verify(
        VerifyPolicy::Default(true),
        Some(VerifyPolicy::from_config(&config.ssl_verify)),
    );
    let auth = ObsSignatureAuth::new(
        credentials.user.clone(),
        credentials.sshkey_path.clone(),
        credentials.sshkey_fingerprint.clone(),
    );
    let client = ObsClient::new(
        &config.obs_api_url,
        Duration::from_secs(config.obs_request_timeout),
        verify,
        Arc::new(auth),
    )?;
    Ok((client, credentials.user))
}
