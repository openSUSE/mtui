//! `cargo xtask teregen-put` — a dev-only probe for
//! `TeregenV2::upload_document`. Nothing in either shipped binary calls the
//! write client yet (P5-D1/P5-D9); this is its only caller besides the test
//! suite.
//!
//! Default behaviour is the honest round trip: `GET` the document, then
//! re-`PUT` the same bytes with the `ETag` just received. `--file` supplies a
//! different body (to backfill a `log`-only id with `--create`, or to drive a
//! deliberate `422`); `--if-match`/`--create` override the precondition the
//! round trip would otherwise derive from the `GET`. `--dry-run` prints the
//! request instead of sending it (the bearer is always masked).

use std::path::PathBuf;
use std::str::FromStr;

use anyhow::{Context, Result, bail};
use mtui_datasources::obs::auth::RusshAgent;
use mtui_datasources::obs::oscrc;
use mtui_datasources::teregen::{DocumentFetch, Precondition, TeregenAuth, TeregenV2};
use mtui_datasources::{HttpClient, VerifyPolicy};
use mtui_types::report_document::ReportDocument;

/// The default teregen v2 base — the same value `corpus_survey`/`teregen_login` use.
pub const DEFAULT_V2_BASE: &str = crate::corpus_survey::DEFAULT_V2_BASE;

/// Parsed `cargo xtask teregen-put` arguments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PutArgs {
    /// The report id to write (`--id`, required).
    pub id: String,
    /// The teregen v2 base URL.
    pub base: String,
    /// Override the principal sent to teregen (default: the oscrc user).
    pub user: Option<String>,
    /// Upload this file's contents instead of the just-fetched document.
    pub file: Option<PathBuf>,
    /// Send `If-Match: <etag>` instead of the etag from the round-trip `GET`.
    pub if_match: Option<String>,
    /// Send no `If-Match` at all (mutually exclusive with `--if-match`) — the
    /// deliberate-negative/backfill probe; production mtui never does this
    /// (P5-D2).
    pub create: bool,
    /// Print the request instead of sending it.
    pub dry_run: bool,
}

impl PutArgs {
    /// Parse `--id <RRID> [--base <URL>] [--user <NAME>] [--file <PATH>]
    /// [--if-match <ETAG>|--create] [--dry-run]` (already past `teregen-put`).
    ///
    /// # Errors
    ///
    /// Returns an error for an unknown flag, a value-taking flag missing its
    /// value, or a missing `--id`.
    pub fn parse(args: impl Iterator<Item = String>) -> Result<Self> {
        let mut id = None;
        let mut base = DEFAULT_V2_BASE.to_owned();
        let mut user = None;
        let mut file = None;
        let mut if_match = None;
        let mut create = false;
        let mut dry_run = false;
        let mut args = args;
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--id" => id = Some(args.next().context("--id needs a value")?),
                "--base" => base = args.next().context("--base needs a value")?,
                "--user" => user = Some(args.next().context("--user needs a value")?),
                "--file" => {
                    file = Some(PathBuf::from(args.next().context("--file needs a value")?))
                }
                "--if-match" => if_match = Some(args.next().context("--if-match needs a value")?),
                "--create" => create = true,
                "--dry-run" => dry_run = true,
                other => bail!("unknown teregen-put flag: {other}"),
            }
        }
        if if_match.is_some() && create {
            bail!("--if-match and --create are mutually exclusive");
        }
        Ok(Self {
            id: id.context("teregen-put requires --id <RRID>")?,
            base,
            user,
            file,
            if_match,
            create,
            dry_run,
        })
    }
}

/// Run `cargo xtask teregen-put`.
///
/// # Errors
///
/// Returns an error if oscrc credentials cannot be read, the document to
/// upload cannot be resolved (neither a successful `GET` nor `--file`), or the
/// upload itself fails (including every
/// [`TeregenV2WriteError`](mtui_datasources::teregen::TeregenV2WriteError)
/// variant).
pub async fn run(args: &PutArgs, verify: VerifyPolicy) -> Result<()> {
    let config = mtui_config::Config::load(None);
    let creds = oscrc::read_credentials(&config.obs_api_url)
        .context("reading oscrc SSH-signature credentials")?;
    let principal = args.user.clone().unwrap_or_else(|| creds.user.clone());

    println!("principal:    {principal}");
    println!("base:         {}", args.base);
    println!("id:           {}", args.id);

    let read_http = HttpClient::new(verify.clone()).context("building the HTTP client")?;
    let read_client = TeregenV2::with_client(read_http, &args.base);

    let (document, fetched_etag) = match &args.file {
        Some(file) => {
            let raw = std::fs::read_to_string(file)
                .with_context(|| format!("reading {}", file.display()))?;
            let doc = ReportDocument::from_str(&raw)
                .with_context(|| format!("parsing {} as a ReportDocument", file.display()))?;
            (doc, None)
        }
        None => match read_client.fetch_document(&args.id, None).await {
            Ok(DocumentFetch::Fresh { document, etag, .. }) => (*document, etag),
            Ok(DocumentFetch::NotModified) => {
                unreachable!("no etag was sent, so a 304 cannot come back")
            }
            Err(e) => bail!(
                "GET /reports/{} failed: {e} (pass --file to supply a document — \
                 e.g. to backfill a log-only id with --create)",
                args.id
            ),
        },
    };

    let precondition = if args.create {
        Precondition::Create
    } else if let Some(etag) = &args.if_match {
        Precondition::Match(etag.clone())
    } else {
        let etag = fetched_etag
            .context("the GET returned no ETag, and neither --if-match nor --create was given")?;
        Precondition::Match(etag)
    };

    if args.dry_run {
        let body = serde_json::to_string(&document).context("serializing the document")?;
        println!("PUT {}/reports/{}", args.base, args.id);
        println!("Authorization: Bearer ***");
        println!("Content-Type: application/json");
        match &precondition {
            Precondition::Match(etag) => println!("If-Match: {etag}"),
            Precondition::Create => println!("(no If-Match)"),
        }
        println!("body:         {} bytes", body.len());
        return Ok(());
    }

    let write_http = HttpClient::new(verify).context("building the HTTP client")?;
    let auth = TeregenAuth::<RusshAgent>::new(
        args.base.clone(),
        principal,
        creds.sshkey_path,
        creds.sshkey_fingerprint,
        write_http.clone(),
    );
    let write_client = TeregenV2::with_client(write_http, &args.base).with_auth(auth);

    let outcome = write_client
        .upload_document(&args.id, &document, &precondition)
        .await
        .context("upload_document failed")?;

    println!("status:       202");
    println!(
        "etag:         {}",
        outcome.etag.as_deref().unwrap_or("<none>")
    );
    println!(
        "durability:   stored in teregen and immediately readable; the SVN commit is a \
         Minion job delayed ~600s with three attempts and no guarantee past the third \
         (T8) — this PUT alone does not guarantee the change reached SVN."
    );
    println!(
        "log/status:   this write does not re-render `log` or refresh `status:{}` \
         (T4, issue #22) — the legacy log and HTML views can go stale until T4 lands.",
        args.id
    );

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requires_id() {
        assert!(PutArgs::parse(std::iter::empty()).is_err());
    }

    #[test]
    fn defaults_match_the_documented_flags() {
        let raw = ["--id", "SUSE:Maintenance:1:2"].map(str::to_owned);
        let args = PutArgs::parse(raw.into_iter()).unwrap();
        assert_eq!(args.id, "SUSE:Maintenance:1:2");
        assert_eq!(args.base, DEFAULT_V2_BASE);
        assert_eq!(args.user, None);
        assert_eq!(args.file, None);
        assert_eq!(args.if_match, None);
        assert!(!args.create);
        assert!(!args.dry_run);
    }

    #[test]
    fn parses_every_flag() {
        let raw = [
            "--id",
            "SUSE:Maintenance:1:2",
            "--base",
            "https://staging.example/api/v2",
            "--user",
            "bob",
            "--file",
            "doc.json",
            "--dry-run",
        ]
        .map(str::to_owned);
        let args = PutArgs::parse(raw.into_iter()).unwrap();
        assert_eq!(args.base, "https://staging.example/api/v2");
        assert_eq!(args.user.as_deref(), Some("bob"));
        assert_eq!(args.file, Some(PathBuf::from("doc.json")));
        assert!(args.dry_run);
    }

    #[test]
    fn if_match_and_create_are_mutually_exclusive() {
        let raw = ["--id", "x", "--if-match", "\"a\"", "--create"].map(str::to_owned);
        assert!(PutArgs::parse(raw.into_iter()).is_err());
    }

    #[test]
    fn create_flag_is_parsed() {
        let raw = ["--id", "x", "--create"].map(str::to_owned);
        let args = PutArgs::parse(raw.into_iter()).unwrap();
        assert!(args.create);
    }

    #[test]
    fn unknown_flag_is_rejected() {
        let raw = ["--id", "x", "--bogus"].map(str::to_owned);
        assert!(PutArgs::parse(raw.into_iter()).is_err());
    }

    #[test]
    fn value_flag_missing_its_value_is_rejected() {
        let raw = ["--id", "x", "--base"].map(str::to_owned);
        assert!(PutArgs::parse(raw.into_iter()).is_err());
    }
}
