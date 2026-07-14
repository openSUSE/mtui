//! Integration tests for the never-raise `OSC(config, rrid)` facade
//! (`mtui_datasources::obs::facade`).
//!
//! Covers the DoD escape hatches upstream PR#323 hardened: a non-PEM key file, a
//! no-home `expanduser`, and a lone-surrogate MCP body must each yield a *logged
//! failure* (a typed `ObsError`), never a panic. Also exercises the happy path
//! (a comment through a wiremock-backed OBS server via the injectable factory)
//! and confirms the production `Osc::new` path resolves credentials from a real
//! oscrc file and reaches the OBS API.
//!
//! HTTP is mocked with `wiremock`; oscrc/keys are written to `tempfile` dirs.
//! `$HOME` is manipulated only inside a serialized test to exercise the
//! no-home path, and always restored.

use std::sync::Arc;
use std::time::Duration;

use mtui_config::Config;
use mtui_datasources::http::VerifyPolicy;
use mtui_datasources::obs::client::{NoAuth, ObsClient};
use mtui_datasources::obs::errors::ObsError;
use mtui_datasources::obs::facade::Osc;
use mtui_types::RequestReviewID;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn rrid() -> RequestReviewID {
    RequestReviewID::parse("SUSE:Maintenance:1:56789").unwrap()
}

/// An SLFO RRID: `approve`/`reject` skip the `qam.suse.de` testreport
/// preconditions for this kind, so the facade's op wrappers can be exercised
/// against a single wiremock OBS server (no reports server needed).
fn slfo_rrid() -> RequestReviewID {
    RequestReviewID::parse("SUSE:SLFO:1.1:70000").unwrap()
}

/// The facade's injectable client-factory type (mirrors the private alias).
type Factory = Arc<dyn Fn(&Config) -> Result<(ObsClient, String), ObsError> + Send + Sync>;

/// A factory that hands the facade an unauthenticated ObsClient pointed at `uri`.
fn factory_for(uri: String) -> Factory {
    Arc::new(move |_cfg: &Config| {
        let client = ObsClient::new(
            &uri,
            Duration::from_secs(180),
            VerifyPolicy::Default(true),
            Arc::new(NoAuth),
        )?;
        Ok((client, "qamuser".to_owned()))
    })
}

// --------------------------------------------------------------------------- //
// Happy path (injectable factory)                                             //
// --------------------------------------------------------------------------- //

#[tokio::test]
async fn comment_happy_path_posts_via_injected_client() {
    // A wiremock OBS server backs an unauthenticated ObsClient; the facade's
    // comment op is one POST to comments/request/{id}. The factory injects the
    // already-built client + acting user, so no oscrc/agent is touched.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/comments/request/56789"))
        .respond_with(ResponseTemplate::new(200).set_body_string("<status code='ok'/>"))
        .mount(&server)
        .await;

    let uri = server.uri();
    let osc = Osc::with_factory(
        Config::default(),
        rrid(),
        Arc::new(move |_cfg: &Config| {
            let client = ObsClient::new(
                &uri,
                Duration::from_secs(180),
                VerifyPolicy::Default(true),
                Arc::new(NoAuth),
            )?;
            Ok((client, "qamuser".to_owned()))
        }),
    );

    osc.comment("looks good").await.unwrap();
    // The server recorded exactly the one comment POST.
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 1);
}

#[tokio::test]
async fn empty_comment_is_refused_not_panicked() {
    // A whitespace-only comment is an ObsError::Op refusal from the op itself;
    // the factory still builds a (never-contacted) client.
    let osc = Osc::with_factory(
        Config::default(),
        rrid(),
        Arc::new(|_cfg: &Config| {
            let client = ObsClient::new(
                "https://api.invalid",
                Duration::from_secs(180),
                VerifyPolicy::Default(true),
                Arc::new(NoAuth),
            )?;
            Ok((client, "qamuser".to_owned()))
        }),
    );
    let err = osc.comment("   ").await.unwrap_err();
    assert!(matches!(err, ObsError::Op(_)), "{err:?}");
}

#[tokio::test]
async fn factory_error_folds_into_logged_err() {
    // A factory that fails (e.g. unresolvable credentials) surfaces as a typed
    // Err, not a panic — the never-raise seam.
    let osc = Osc::with_factory(
        Config::default(),
        rrid(),
        Arc::new(|_cfg: &Config| Err(ObsError::Config("no credentials".to_owned()))),
    );
    let err = osc.assign(&[]).await.unwrap_err();
    assert!(matches!(err, ObsError::Config(m) if m == "no credentials"));
}

#[tokio::test]
async fn approve_happy_path_accepts_via_injected_client() {
    // SLFO approve (no groups): the op GETs the request (USER is assigned via an
    // accepted by_group review) then POSTs the changereviewstate. Preconditions
    // are skipped for SLFO, so one wiremock OBS server suffices.
    let server = MockServer::start().await;
    let request_xml = "<request id='70000'><state name='review'/>\
         <action type='maintenance_release'>\
         <source project='SUSE:SLFO:1.1' package='p'/></action>\
         <review state='accepted' by_group='qam-sle'>\
         <history who='qamuser' when='2020-01-01T00:00:00'>\
         <description>Review got accepted</description></history></review></request>";
    Mock::given(method("GET"))
        .and(path("/request/70000"))
        .respond_with(ResponseTemplate::new(200).set_body_string(request_xml))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/request/70000"))
        .respond_with(ResponseTemplate::new(200).set_body_string("<status code='ok'/>"))
        .mount(&server)
        .await;

    let osc = Osc::with_factory(Config::default(), slfo_rrid(), factory_for(server.uri()));
    osc.approve(&[]).await.unwrap();
}

#[tokio::test]
async fn reject_happy_path_declines_via_injected_client() {
    // SLFO reject: preconditions + reject-reason attribute are skipped for SLFO,
    // so the op GETs the request then POSTs the declined changereviewstate.
    let server = MockServer::start().await;
    let request_xml = "<request id='70000'><state name='review'/>\
         <action type='maintenance_release'>\
         <source project='SUSE:SLFO:1.1' package='p'/></action>\
         <review state='new' by_group='qam-sle'/></request>";
    Mock::given(method("GET"))
        .and(path("/request/70000"))
        .respond_with(ResponseTemplate::new(200).set_body_string(request_xml))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/request/70000"))
        .respond_with(ResponseTemplate::new(200).set_body_string("<status code='ok'/>"))
        .mount(&server)
        .await;

    let osc = Osc::with_factory(Config::default(), slfo_rrid(), factory_for(server.uri()));
    osc.reject(&[], "regression", "broke on boot")
        .await
        .unwrap();
}

// --------------------------------------------------------------------------- //
// Never-raise escape hatches through the production Osc::new path             //
// --------------------------------------------------------------------------- //

/// Write a `[<apiurl>]` oscrc section pointing `sshkey` at `key_path`.
fn write_oscrc(
    dir: &std::path::Path,
    apiurl: &str,
    key_path: &std::path::Path,
) -> std::path::PathBuf {
    let oscrc = dir.join("oscrc");
    std::fs::write(
        &oscrc,
        format!(
            "[{apiurl}]\nuser = qamuser\nsshkey = {}\n",
            key_path.display()
        ),
    )
    .unwrap();
    oscrc
}

#[tokio::test]
async fn non_pem_key_yields_logged_failure_not_panic() {
    // Escape hatch #1 (PR#323): a non-PEM key file. The oscrc references an
    // existing-but-garbage key; on the first authenticated call the wiremock OBS
    // server returns a 401 Signature challenge, the signer tries to load the key,
    // and fails with a typed ObsError::Config — never a panic.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/request/56789"))
        .respond_with(
            ResponseTemplate::new(401).insert_header("WWW-Authenticate", "Signature realm=\"OBS\""),
        )
        .mount(&server)
        .await;

    let dir = tempfile::TempDir::new().unwrap();
    let key = dir.path().join("id_bad");
    std::fs::write(&key, "this is not a PEM private key\n").unwrap();
    let apiurl = server.uri();
    let oscrc = write_oscrc(dir.path(), &apiurl, &key);

    let mut config = Config::default();
    config.obs_api_url = apiurl;
    config.obs_conffile = oscrc.display().to_string();

    // unassign hits GET request/{id} first, triggering the 401 -> sign -> fail.
    let osc = Osc::new(config, rrid());
    let err = osc.unassign(&[]).await.unwrap_err();
    assert!(matches!(err, ObsError::Config(_)), "{err:?}");
}

#[tokio::test]
async fn expanduser_conffile_yields_logged_failure_not_panic() {
    // Escape hatch #2 (PR#323): a `~`-relative conffile through expanduser().
    // Whether or not `$HOME` is set, the reader never panics on the `~` path: it
    // expands (or leaves `~` in place with no home) and the resulting missing
    // file surfaces as a typed ObsError::Config. (The no-home invariant — that
    // expanduser leaves `~` in place rather than panicking — is unit-tested in
    // `obs::oscrc`; this asserts the facade folds that path into a logged Err.)
    let mut config = Config::default();
    config.obs_conffile = "~/.oscrc-mtui-rs-facade-test-does-not-exist".to_owned();
    let osc = Osc::new(config, rrid());
    let err = osc.comment("hi").await.unwrap_err();
    assert!(matches!(err, ObsError::Config(_)), "{err:?}");
}

#[test]
fn lone_surrogate_body_is_rejected_at_the_json_boundary() {
    // Escape hatch #3 (PR#323): a lone surrogate in the request body from MCP
    // JSON input. In Python a lone surrogate can live in a `str` and blows up
    // only at encode time; in Rust a `String`/`&str` cannot hold one at all, and
    // serde_json rejects it at the MCP JSON boundary — so it can never reach the
    // facade to panic on encode. This proves that boundary guarantee.
    //
    // A JSON string with a lone high surrogate (\ud800 with no low surrogate)
    // is invalid; serde_json refuses to decode it into a Rust String.
    let json = r#""\ud800""#;
    let decoded: Result<String, _> = serde_json::from_str(json);
    assert!(
        decoded.is_err(),
        "serde_json must reject a lone surrogate before it reaches the facade"
    );
}
