//! `cargo xtask teregen-login` — mint (or reuse a cached) teregen v2 bearer
//! token from the operator's own oscrc SSH key, and probe it against a live
//! authenticated endpoint.
//!
//! Never prints the token: only the resolved principal, the key locator, the
//! cache-hit/minted verdict, the store path/age, and the probe result.

use anyhow::{Context, Result, bail};
use mtui_datasources::obs::auth::RusshAgent;
use mtui_datasources::obs::oscrc;
use mtui_datasources::teregen::{DEFAULT_NAMESPACE, TeregenAuth, TokenStore};
use mtui_datasources::{HttpClient, VerifyPolicy};

/// The default teregen v2 base — the same value `corpus_survey` uses.
pub const DEFAULT_V2_BASE: &str = crate::corpus_survey::DEFAULT_V2_BASE;

/// Parsed `cargo xtask teregen-login` arguments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoginArgs {
    /// The teregen v2 base URL.
    pub base: String,
    /// Override the principal sent to teregen (default: the oscrc user).
    pub user: Option<String>,
    /// The SSHSIG namespace (default [`DEFAULT_NAMESPACE`]) — override to
    /// `--namespace wrong-namespace` to run the deliberate-negative probe.
    pub namespace: String,
    /// `false` disables the on-disk token cache entirely.
    pub store: bool,
    /// `false` skips the authenticated `GET /schema` probe after minting.
    pub probe: bool,
}

impl LoginArgs {
    /// Parse `--base <URL> --user <NAME> --namespace <NS> --no-store
    /// --no-probe` from an argument iterator (already past `teregen-login`).
    ///
    /// # Errors
    ///
    /// Returns an error for an unknown flag or a value-taking flag missing its
    /// value.
    pub fn parse(args: impl Iterator<Item = String>) -> Result<Self> {
        let mut base = DEFAULT_V2_BASE.to_owned();
        let mut user = None;
        let mut namespace = DEFAULT_NAMESPACE.to_owned();
        let mut store = true;
        let mut probe = true;
        let mut args = args;
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--base" => base = args.next().context("--base needs a value")?,
                "--user" => user = Some(args.next().context("--user needs a value")?),
                "--namespace" => namespace = args.next().context("--namespace needs a value")?,
                "--no-store" => store = false,
                "--no-probe" => probe = false,
                other => bail!("unknown teregen-login flag: {other}"),
            }
        }
        Ok(Self {
            base,
            user,
            namespace,
            store,
            probe,
        })
    }
}

/// The elapsed time since `minted_at` (an RFC3339 timestamp), rendered as
/// whole seconds. `None` if `minted_at` cannot be parsed.
fn describe_age(minted_at: &str) -> Option<String> {
    let minted = chrono::DateTime::parse_from_rfc3339(minted_at).ok()?;
    let now_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs();
    let elapsed = i64::try_from(now_secs).unwrap_or(i64::MAX) - minted.timestamp();
    Some(format!("{elapsed}s ago"))
}

/// Run `cargo xtask teregen-login`: resolve the oscrc identity, mint/reuse a
/// token, and (unless `--no-probe`) exercise it against `GET /schema`.
///
/// # Errors
///
/// Returns an error if oscrc credentials cannot be read, the HTTP client
/// cannot be built, or minting the token fails (including the deliberate
/// `--namespace wrong-namespace` negative case, which fails here with a `401`
/// — the probe step is never reached).
pub async fn run(args: &LoginArgs, verify: VerifyPolicy) -> Result<()> {
    let config = mtui_config::Config::load(None);
    let creds = oscrc::read_credentials(&config.obs_api_url)
        .context("reading oscrc SSH-signature credentials")?;
    let principal = args.user.clone().unwrap_or_else(|| creds.user.clone());

    println!("principal:    {principal}");
    match (&creds.sshkey_path, &creds.sshkey_fingerprint) {
        (Some(path), _) => println!("key:          {} (file)", path.display()),
        (None, Some(fingerprint)) => println!("key:          {fingerprint} (ssh-agent)"),
        (None, None) => bail!(
            "oscrc [{}] has neither sshkey_path nor sshkey_fingerprint",
            config.obs_api_url
        ),
    }
    println!("base:         {}", args.base);
    println!("namespace:    {}", args.namespace);

    let http = HttpClient::new(verify).context("building the HTTP client")?;
    let store = if args.store { TokenStore::new() } else { None };
    if args.store && store.is_none() {
        println!("store:        disabled (could not resolve the XDG data dir)");
    }
    let cache_hit_before = store
        .as_ref()
        .and_then(|s| s.load(&args.base, &principal))
        .is_some();

    let auth = TeregenAuth::<RusshAgent>::new(
        args.base.clone(),
        principal.clone(),
        creds.sshkey_path,
        creds.sshkey_fingerprint,
        http,
    )
    .with_namespace(args.namespace.clone())
    .with_store(store.clone());

    auth.token()
        .await
        .context("minting/loading the teregen token")?;

    println!(
        "token:        {}",
        if cache_hit_before {
            "from cache"
        } else {
            "minted"
        }
    );

    if let Some(store) = &store {
        println!("store:        {}", store.path().display());
        if let Some(cached) = store.load(&args.base, &principal) {
            match describe_age(&cached.minted_at) {
                Some(age) => println!("minted_at:    {} ({age})", cached.minted_at),
                None => println!("minted_at:    {}", cached.minted_at),
            }
        }
    }

    if args.probe {
        let schema_url = format!("{}/schema", args.base);
        match auth
            .authenticated_request(reqwest::Method::GET, &schema_url)
            .await
        {
            Ok(response) => println!("probe:        GET /schema -> {}", response.status()),
            Err(e) => println!("probe:        GET /schema -> error: {e}"),
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_the_documented_flags() {
        let args = LoginArgs::parse(std::iter::empty()).unwrap();
        assert_eq!(args.base, DEFAULT_V2_BASE);
        assert_eq!(args.user, None);
        assert_eq!(args.namespace, DEFAULT_NAMESPACE);
        assert!(args.store);
        assert!(args.probe);
    }

    #[test]
    fn parses_every_flag() {
        let raw = [
            "--base",
            "https://staging.example/api/v2",
            "--user",
            "bob",
            "--namespace",
            "wrong-namespace",
            "--no-store",
            "--no-probe",
        ]
        .map(str::to_owned);
        let args = LoginArgs::parse(raw.into_iter()).unwrap();
        assert_eq!(args.base, "https://staging.example/api/v2");
        assert_eq!(args.user.as_deref(), Some("bob"));
        assert_eq!(args.namespace, "wrong-namespace");
        assert!(!args.store);
        assert!(!args.probe);
    }

    #[test]
    fn unknown_flag_is_rejected() {
        let raw = ["--bogus"].map(str::to_owned);
        assert!(LoginArgs::parse(raw.into_iter()).is_err());
    }

    #[test]
    fn value_flag_missing_its_value_is_rejected() {
        let raw = ["--base"].map(str::to_owned);
        assert!(LoginArgs::parse(raw.into_iter()).is_err());
    }

    #[test]
    fn describe_age_reports_elapsed_seconds() {
        let past = chrono::DateTime::from_timestamp(0, 0).unwrap().to_rfc3339();
        let age = describe_age(&past).expect("parses");
        assert!(age.ends_with("s ago"), "got {age}");
    }

    #[test]
    fn describe_age_none_for_garbage() {
        assert_eq!(describe_age("not a timestamp"), None);
    }
}
