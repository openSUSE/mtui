//! The `OSC(config, rrid)` never-raise review seam (native OBS API, no `osc`).
//!
//! The [`Osc`] seam binds the resolved [`Config`] and target
//! [`RequestReviewID`] to the five QAM operations ([`crate::obs::qam`]), reading
//! credentials from the user's oscrc — located like `osc` (`$OSC_CONFIG` →
//! `$XDG_CONFIG_HOME/osc/oscrc` → `~/.oscrc`) ([`read_credentials`]) — and
//! authenticating with SSH signature auth ([`ObsSignatureAuth`]).
//!
//! ## Never-raise contract
//!
//! Callers (`apicall` / `approve`) invoke the seam bare with no guard of their
//! own, so it never `panic!`s / `unwrap`s / `expect`s: every failure — reading
//! oscrc, loading the key, building the session, the authenticated calls, XML
//! parsing — becomes an `Err(ObsError)` the caller logs. The escape hatches are
//! covered:
//!
//! * a non-PEM key file → [`ObsError::Config`] from the auth layer, surfaced
//!   when the first authenticated call is challenged;
//! * no home directory (a headless container) → the oscrc reader leaves `~` in
//!   place rather than panicking, and a resulting read failure is a typed
//!   [`ObsError::Config`];
//! * a lone surrogate in an MCP-supplied request body cannot reach this layer at
//!   all: `String`/`&str` cannot hold one and `serde_json` rejects it at the MCP
//!   boundary (see the boundary test), so encoding the body cannot panic.

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
/// The production path ([`build_client`]) reads the osc-located oscrc and
/// attaches SSH signature auth; tests inject a closure returning a
/// wiremock-backed [`ObsClient`] or an [`Err`], touching neither the real oscrc
/// nor a real agent.
type ClientFactory = Arc<dyn Fn(&Config) -> Result<Built, ObsError> + Send + Sync>;

/// The native OBS review backend (approve / assign / unassign / comment /
/// reject).
///
/// Construction ([`Osc::new`]) cannot fail. Each operation reads credentials,
/// builds an authenticated client and runs the corresponding
/// [`crate::obs::qam`] op, folding any failure into a logged `Err(ObsError)`.
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
    /// Lets tests supply an already-built (wiremock-backed) client, or an error,
    /// without reading the real `~/.oscrc` or contacting a real ssh-agent.
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
    /// authenticated calls, XML parsing — happens in here, because callers
    /// invoke the seam methods bare. Nothing in this path panics.
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

/// The production client factory: read oscrc and attach SSH signature auth.
///
/// Reads the credentials for `obs_api_url` from the oscrc located like `osc`
/// (`$OSC_CONFIG` → `$XDG_CONFIG_HOME/osc/oscrc` → `~/.oscrc`), builds an
/// [`ObsClient`] against `obs_api_url` with the coarse `obs_request_timeout`
/// budget and the resolved TLS posture, and injects an [`ObsSignatureAuth`]
/// signer for the acting user's key.
fn build_client(config: &Config) -> Result<Built, ObsError> {
    let credentials = read_credentials(&config.obs_api_url)?;
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
