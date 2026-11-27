//! The test-report construction lifecycle (`make_testreport`).
//!
//! Selects the report class by RRID kind (`tr_factory`), loads it from the
//! teregen v2 document, and applies workflow selection + the
//! deferred-autoconnect flag for the auto/kernel update kinds.
//!
//! The host connect belongs to the composition root
//! (`mtui-core::Session::load_update`), which owns the arbiter wiring, so
//! `make_testreport` only records the intent via
//! [`TestReportBase::autoconnect_pending`](crate::testreport::TestReportBase::autoconnect_pending).
//! The QEM Dashboard / auto-openQA enrichment does run here, for the `-a` kind,
//! and autoconnect fires only on its downgrade-to-[`Workflow::Manual`] path.

use mtui_config::options::Config;
use mtui_datasources::qem_dashboard::dashboard_openqa::DashboardAutoOpenQA;
use mtui_datasources::qem_dashboard::incident::QemIncident;
use mtui_datasources::{TeReGen, VerifyPolicy};
use mtui_hosts::Prompter;
use mtui_types::enums::RequestKind;
use mtui_types::{UpdateID, Workflow};
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use crate::reports::{NullReport, ObsReport, PiReport, SlReport};
use crate::testreport::{HashCheck, TestReport, TestReportBase};

/// Which update kind produced the report — selects the workflow and whether
/// autoconnect defaults on.
///
/// Orthogonal to the RRID kind, which selects the concrete `TestReport` class
/// (`tr_factory`); this is the kind the operator named on the command line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdateKind {
    /// An automatic OBS update (`load_template -a`). Workflow starts
    /// [`Workflow::Auto`]; autoconnect defaults **on**.
    Auto,
    /// A kernel/live-patch update (`load_template -k`). Workflow is
    /// [`Workflow::Kernel`]; autoconnect defaults **off**.
    Kernel,
}

impl UpdateKind {
    /// The workflow this update kind starts in.
    #[must_use]
    fn workflow(self) -> Workflow {
        match self {
            Self::Auto => Workflow::Auto,
            Self::Kernel => Workflow::Kernel,
        }
    }
}

/// Selects the concrete [`TestReport`] implementation for an RRID kind.
///
/// SLFO → [`SlReport`], PI → [`PiReport`], everything else (Maintenance) →
/// [`ObsReport`].
#[must_use]
fn tr_factory(update: &UpdateID, config: Config) -> Box<dyn TestReport + Send + Sync> {
    match update.id.kind {
        RequestKind::Slfo => Box::new(SlReport::new(config)),
        RequestKind::Pi => Box::new(PiReport::new(config)),
        RequestKind::Maintenance => Box::new(ObsReport::new(config)),
    }
}

/// What to do with a stale template hash.
enum StaleHash {
    /// TeReGen regenerated a fresh, verified report.
    Regenerated(Box<dyn TestReport + Send + Sync>),
    /// Force-continue: the caller keeps its existing stale report.
    ForceContinue,
    /// Abandon the load (the caller substitutes a [`NullReport`]).
    Abandon,
}

/// The caller's side of a load: whether it can be prompted, the
/// non-interactive stale-hash answer, and the token that stops its waits.
/// The default is a headless load that declines a stale hash.
#[derive(Default)]
pub struct LoadContext<'a> {
    /// The session is the interactive REPL.
    pub is_repl: bool,
    /// The REPL's prompter; `Some` only in interactive mode.
    pub prompter: Option<&'a Prompter>,
    /// The non-interactive answer to the stale-hash "force continue" question.
    pub force_continue: bool,
    /// Stops the load's TeReGen waits.
    pub cancel: CancellationToken,
}

/// A load that [`LoadContext::cancel`] stopped while it waited for TeReGen,
/// by what it was waiting for; nothing was loaded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum LoadCancelled {
    /// The wait for the report document.
    #[error("stopped waiting for TeReGen's document; nothing was loaded")]
    Document,
    /// The wait for a stale template's regeneration, which TeReGen keeps running.
    #[error(
        "stopped waiting for TeReGen to regenerate the stale template; nothing was \
         loaded, and the requested regeneration continues on the server"
    )]
    Regeneration,
}

/// Why [`load_via_document`] did not load the report.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
enum LoadError {
    #[error(transparent)]
    Cancelled(#[from] LoadCancelled),
    #[error("{0}")]
    Failed(String),
}

impl From<String> for LoadError {
    fn from(reason: String) -> Self {
        Self::Failed(reason)
    }
}

impl LoadError {
    /// The failure's reason, or the cancel as the `Err` to propagate.
    fn into_failure(self) -> Result<String, LoadCancelled> {
        match self {
            Self::Cancelled(cancelled) => Err(cancelled),
            Self::Failed(reason) => Ok(reason),
        }
    }
}

/// A [`NullReport`] carrying the reason its load failed, so
/// `Session::load_update` can surface *why* rather than "could not load".
fn null_with_error(config: Config, reason: String) -> NullReport {
    let mut report = NullReport::new(config);
    report.base_mut().load_error = Some(reason);
    report
}

/// Switches an auto load to [`Workflow::Manual`], arming the deferred autoconnect
/// when the caller asked for one. The composition root wires the arbiter first,
/// so `refhosts_from_tp` draws one host per slot.
fn downgrade_to_manual(base: &mut TestReportBase, autoconnect: bool) {
    base.workflow = Workflow::Manual;
    if autoconnect {
        base.autoconnect_pending = true;
    }
}

/// Builds and populates a [`TestReport`] for `update`.
///
/// 1. Selects the report class by RRID kind (`tr_factory`).
/// 2. Loads the report from the teregen v2 document; nothing is checked out,
///    and `template_dir/<rrid>/` is only a scratch directory.
/// 3. On a load failure returns a [`NullReport`], so the caller can add a
///    benign inactive template rather than propagate an error.
/// 4. Verifies the Gitea token + template hash ([`TestReport::check_hash`]): a
///    missing token or failed call abandons the load; a stale hash goes to the
///    TeReGen regenerate / force-continue handling.
/// 5. Sets the workflow from `kind`. The `-a` (auto) kind builds the
///    [`QemIncident`] and runs [`DashboardAutoOpenQA`]; with no install jobs
///    (or an unreachable dashboard) the workflow is **downgraded to
///    [`Workflow::Manual`]**.
///
/// The deferred connect (via
/// [`TestReportBase::autoconnect_pending`](crate::testreport::TestReportBase::autoconnect_pending),
/// honoured by the composition root *after* wiring the host arbiter) is armed
/// **only** when `autoconnect` is `true` **and** the auto load downgraded to
/// `MANUAL` — never on the auto happy path, never for the kernel kind.
///
/// `ctx.force_continue` is the non-interactive escape hatch for a stale hash
/// (`handle_stale_hash`): when `is_repl && prompter.is_some()`, the REPL's own
/// "Force continue loading template ?" prompt governs and this is ignored;
/// otherwise it takes the prompt's place. `false` abandons the load.
///
/// # Errors
///
/// [`LoadCancelled`] when `ctx.cancel` fires while the load waits for TeReGen
/// (the busy-document poll or a stale-template regeneration). Every other
/// failure is a [`NullReport`], as above.
pub async fn make_testreport(
    update: &UpdateID,
    config: Config,
    kind: UpdateKind,
    autoconnect: bool,
    ctx: LoadContext<'_>,
) -> Result<Box<dyn TestReport + Send + Sync>, LoadCancelled> {
    let template_dir = config.template_dir.clone();
    let max_parallel = config.max_parallel as usize;
    let mut report = tr_factory(update, config);

    let rrid_dir = template_dir.join(update.id.to_string());
    let trpath = rrid_dir.join("log");

    let checkout_config = report.base().config.clone();
    let rrid = update.id.clone();

    // The document is the source of the model; nothing is checked out — only
    // the scratch directory is created.
    if let Err(e) = load_via_document(
        &mut report,
        &rrid,
        &checkout_config,
        &trpath,
        None,
        &ctx.cancel,
    )
    .await
    {
        let reason = e.into_failure()?;
        info!("TestReport isn't loaded");
        return Ok(Box::new(null_with_error(checkout_config, reason)));
    }

    // `read` is sync and `check_hash` async, so the Gitea token + template-hash
    // verification fires here, right after a successful read.
    match report.check_hash().await {
        HashCheck::Ok => {}
        HashCheck::MissingToken => {
            let msg = "Gitea API token is not configured. Pass -g/--gitea_token, \
                 set GITEA_TOKEN in your environment, or add a [gitea] token \
                 entry to ~/.mtuirc.";
            error!("{msg}");
            return Ok(Box::new(null_with_error(checkout_config, msg.to_owned())));
        }
        HashCheck::Failed(e) => {
            error!("Gitea API call failed");
            info!(error = %e, "TestReport isn't loaded");
            return Ok(Box::new(null_with_error(
                checkout_config,
                format!("Gitea API call failed: {e}"),
            )));
        }
        HashCheck::Mismatch { .. } => {
            let prev_etag = report.base().document_etag.clone();
            match handle_stale_hash(
                update,
                &checkout_config,
                &trpath,
                &ctx,
                prev_etag.as_deref(),
            )
            .await?
            {
                StaleHash::Regenerated(fresh) => report = fresh,
                StaleHash::ForceContinue => {
                    // The (stale) `report` is kept as-is; flag it so a
                    // non-interactive caller can surface the fact too.
                    report.base_mut().stale_hash_warning = Some(
                        "template hash mismatch (stale checkout); loaded as-is \
                         via force-continue"
                            .to_owned(),
                    );
                }
                StaleHash::Abandon => {
                    return Ok(Box::new(null_with_error(
                        checkout_config,
                        "template hash mismatch (stale checkout); regeneration \
                         declined or unavailable"
                            .to_owned(),
                    )));
                }
            }
        }
    }

    report.base_mut().workflow = kind.workflow();

    if kind == UpdateKind::Auto {
        // Snapshot before the awaits: no `&report` borrow may cross `.await`.
        let dashboard_api = report.base().config.qem_dashboard_api.clone();
        let openqa_instance = report.base().config.openqa_instance.clone();
        let policy = VerifyPolicy::from_config(&report.base().config.ssl_verify);
        let source = report.update_source();
        let pr_url = report.base().giteapr.clone();

        match QemIncident::new(
            rrid.clone(),
            dashboard_api,
            policy,
            source,
            pr_url.as_deref(),
        )
        .await
        {
            Ok(incident) => {
                info!("Getting data from QEM Dashboard");
                let mut auto = DashboardAutoOpenQA::new(
                    openqa_instance,
                    &incident,
                    rrid.clone(),
                    max_parallel,
                );
                // Best-effort at load: a failed fetch folds to "no results" (→
                // manual) rather than aborting the load; the interactive
                // `set_workflow`/`reload_openqa` surface it as `Err` instead.
                if let Err(e) = auto.run().await {
                    warn!(error = %e, "QEM Dashboard fetch failed; treating as no results");
                }
                let no_results = auto.results.is_none();
                report.base_mut().openqa.auto = Some(auto);

                if no_results {
                    warn!("No install jobs or install jobs failed");
                    info!("Switch mode to manual");
                    downgrade_to_manual(report.base_mut(), autoconnect);
                }
            }
            Err(e) => {
                // No dashboard client at all: same best-effort downgrade.
                warn!(error = %e, "QEM Dashboard unavailable; switching mode to manual");
                downgrade_to_manual(report.base_mut(), autoconnect);
            }
        }
    }

    // The session is the single source of truth for REPL-vs-headless: the group
    // is built headless and reconciled here, once, never toggled afterwards.
    report.base_mut().targets.set_is_repl(ctx.is_repl);
    report.base_mut().targets.set_max_parallel(max_parallel);

    Ok(report)
}

/// Handles a stale template hash: log, offer TeReGen regeneration, then the
/// manual force-continue fallback.
///
/// Returns the [`StaleHash`] outcome, or [`LoadCancelled`] when the
/// regeneration wait was cancelled.
///
/// `prompter` is `Some` only in interactive mode, so every prompt is gated on
/// `is_repl && prompter.is_some()` and otherwise takes the non-interactive
/// answer — **except** the force-continue question, whose non-interactive
/// default is `ctx.force_continue`. It reaches exactly the outcome the REPL's
/// "y" answer does ([`StaleHash::ForceContinue`]) and nothing else. The
/// `regenerate` question stays interactive-only.
async fn handle_stale_hash(
    update: &UpdateID,
    config: &Config,
    trpath: &std::path::Path,
    ctx: &LoadContext<'_>,
    prev_etag: Option<&str>,
) -> Result<StaleHash, LoadCancelled> {
    let rrid = update.id.clone();
    error!("Invalid Gitea hash");
    warn!("TestReport hash differs from the Gitea PR; the template is stale");

    let regenerate = match (ctx.is_repl, ctx.prompter) {
        (true, Some(p)) => {
            p.confirm("Regenerate the template now via TeReGen? [y/N]: ", false)
                .await
        }
        _ => false,
    };

    if regenerate {
        if let Some(fresh) =
            regenerate_via_teregen(update, config, trpath, prev_etag, &ctx.cancel).await?
        {
            return Ok(StaleHash::Regenerated(fresh));
        }
        warn!("Regeneration failed; falling back to manual handling");
    } else {
        info!(
            "TestReport can be regenerated here: https://qam.suse.de/reports/{}/log",
            rrid
        );
    }

    // Manual fallback: non-interactive falls through to `ctx.force_continue`.
    let force_continue = match (ctx.is_repl, ctx.prompter) {
        (true, Some(p)) => {
            p.confirm("Force continue loading template ? [y/N]: ", false)
                .await
        }
        _ => ctx.force_continue,
    };
    if force_continue {
        warn!("Template is loaded, but hash differs");
        return Ok(StaleHash::ForceContinue);
    }

    // Declined: abandon the load; the scratch directory stays.
    Ok(StaleHash::Abandon)
}

/// Regenerates a stale template via TeReGen, then reloads it by re-fetching the
/// v2 document (passing `prev_etag`, so a `304` is reported as "unchanged"
/// rather than misread as success).
///
/// Returns the freshly loaded, hash-verified report, or `None` so the caller
/// falls back to the manual force/decline handling. Any TeReGen failure, reload
/// failure, or *still*-failing hash is a `None`. A `cancel` that stops the job
/// wait or the reload is [`LoadCancelled`].
async fn regenerate_via_teregen(
    update: &UpdateID,
    config: &Config,
    trpath: &std::path::Path,
    prev_etag: Option<&str>,
    cancel: &CancellationToken,
) -> Result<Option<Box<dyn TestReport + Send + Sync>>, LoadCancelled> {
    let rrid = update.id.clone();
    info!("Waiting for the template to be regenerated ...");

    let teregen = match TeReGen::new(config, &config.teregen_api) {
        Ok(t) => t,
        Err(e) => {
            error!("TeReGen unreachable; cannot regenerate");
            info!(error = %e, "could not build TeReGen client");
            return Ok(None);
        }
    };
    let mut stopped = false;
    let outcome = teregen
        .regenerate_and_wait(&rrid.to_string(), true, false, || {
            stopped |= cancel.is_cancelled();
            stopped
        })
        .await;

    if outcome.unreachable {
        error!("TeReGen unreachable; cannot regenerate");
        return Ok(None);
    }
    if let Some(err) = &outcome.error {
        error!("Regeneration refused: {err}");
        return Ok(None);
    }
    if let Some(failed) = &outcome.failed {
        error!("Regeneration request failed: {failed}");
        return Ok(None);
    }
    info!("Regeneration job {:?} enqueued for {}", outcome.job, rrid);

    if cancelled_job_wait(&outcome, stopped) {
        return Err(LoadCancelled::Regeneration);
    }
    if !outcome.ok {
        let detail = outcome
            .minion_error
            .as_deref()
            .map(|e| format!(": {e}"))
            .unwrap_or_default();
        error!(
            "Regeneration did not finish (state={}){detail}",
            outcome.state.as_deref().unwrap_or("unknown")
        );
        return Ok(None);
    }

    // A still-failing hash on the fresh template is a reload failure.
    let mut fresh = tr_factory(update, config.clone());

    if let Err(e) = load_via_document(&mut fresh, &rrid, config, trpath, prev_etag, cancel).await {
        let reason = e.into_failure()?;
        error!("Reload after regeneration failed: {reason}");
        return Ok(None);
    }

    match fresh.check_hash().await {
        HashCheck::Ok => Ok(Some(fresh)),
        other => {
            error!("Reload after regeneration failed: hash still not verified ({other:?})");
            Ok(None)
        }
    }
}

/// Whether a job wait that `stopped` saw a cancel in ended as cancelled: only
/// while the job was unfinished, so a job that failed is reported as the failure.
fn cancelled_job_wait(outcome: &mtui_datasources::RegenOutcome, stopped: bool) -> bool {
    !outcome.ok && stopped && outcome.state.as_deref() != Some("failed")
}

// Mirrors `TeReGen::wait_for_template`'s poll budget: a `generating`
// refusal is bounded, not immediate.
const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5);
const POLL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(600);

/// Calls `fetch` again every [`POLL_INTERVAL`] while TeReGen answers that it is
/// generating or busy, until [`POLL_TIMEOUT`] has passed or `cancel` fires.
async fn fetch_while_busy<T, F, Fut>(
    cancel: &CancellationToken,
    mut fetch: F,
) -> Result<Result<T, mtui_datasources::teregen::TeregenV2Error>, LoadCancelled>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, mtui_datasources::teregen::TeregenV2Error>>,
{
    use mtui_datasources::teregen::TeregenV2Error;

    let deadline = tokio::time::Instant::now() + POLL_TIMEOUT;
    loop {
        match fetch().await {
            Err(TeregenV2Error::Generating | TeregenV2Error::Busy)
                if tokio::time::Instant::now() < deadline =>
            {
                tokio::select! {
                    biased;
                    () = cancel.cancelled() => return Err(LoadCancelled::Document),
                    () = tokio::time::sleep(POLL_INTERVAL) => {}
                }
            }
            other => return Ok(other),
        }
    }
}

/// Loads a report from the teregen v2 JSON document.
///
/// Fetches `config.teregen_api_v2`'s document for `rrid`, conditional on
/// `prev_etag` (the initial load passes `None`; [`regenerate_via_teregen`]
/// passes the superseded report's etag). Every non-`Fresh` outcome maps to a
/// distinct, actionable message, never a fallback, which would mask the corpus
/// gap this path exists to surface. A `503 generating` response is retried on
/// the poll budget [`TeReGen::wait_for_template`] uses (5s / 600s) before
/// refusing. On success, applies the document (see
/// [`crate::ingest::apply_document`]), creates the scratch directory, and
/// derives `update_repos`. A `cancel` during that poll is
/// [`LoadError::Cancelled`].
async fn load_via_document(
    report: &mut Box<dyn TestReport + Send + Sync>,
    rrid: &mtui_types::RequestReviewID,
    config: &Config,
    trpath: &std::path::Path,
    prev_etag: Option<&str>,
    cancel: &CancellationToken,
) -> Result<(), LoadError> {
    use mtui_datasources::teregen::{DocumentFetch, TeregenV2};

    let client = TeregenV2::new(config, &config.teregen_api_v2)
        .map_err(|e| format!("building the teregen v2 client failed: {e}"))?;
    let rrid_str = rrid.to_string();

    let fetch = fetch_while_busy(cancel, || client.fetch_document(&rrid_str, prev_etag)).await?;

    let document = match fetch {
        Ok(DocumentFetch::Fresh { document, etag, .. }) => {
            report.base_mut().document_etag = etag;
            document
        }
        Ok(DocumentFetch::NotModified) => {
            return Err("the server's document is unchanged since the last load"
                .to_owned()
                .into());
        }
        Err(e) => return Err(document_fetch_message(e).into()),
    };

    report.base_mut().schema_drift = match client.fetch_schema().await {
        Ok(live) => {
            let drift = mtui_types::report_document::schema_drift(&live);
            if let Some(drift) = &drift {
                warn!(%drift, "teregen's report schema differs from this mtui's; writes are refused");
            }
            drift
        }
        Err(e) => {
            warn!(error = %e, "could not check teregen's report schema");
            None
        }
    };

    crate::ingest::check_document(&document)?;
    crate::ingest::apply_document(report.base_mut(), &document);
    report.base_mut().document = Some(*document);

    if let Some(dir) = trpath.parent() {
        tokio::fs::create_dir_all(dir).await.map_err(|e| {
            format!(
                "could not create the report directory {}: {e}",
                dir.display()
            )
        })?;
    }
    report.base_mut().path = Some(trpath.to_path_buf());
    let repos = report.update_repos_parser();
    report.base_mut().update_repos = repos;
    Ok(())
}

/// Renders a [`TeregenV2Error`](mtui_datasources::teregen::TeregenV2Error) as
/// a distinct refusal text per status, naming the remedy, and for
/// [`Invalid`](mtui_datasources::teregen::TeregenV2Error::Invalid) the
/// `DocumentError` pointer verbatim rather than wrapped in extra prose.
fn document_fetch_message(e: mtui_datasources::teregen::TeregenV2Error) -> String {
    use mtui_datasources::teregen::TeregenV2Error;
    match e {
        TeregenV2Error::NotFound => "no document yet — run `regenerate` first".to_owned(),
        TeregenV2Error::Stale => "the server's document is stale — run `regenerate`".to_owned(),
        TeregenV2Error::Generating => "the server is still generating this document".to_owned(),
        TeregenV2Error::Busy => {
            "the server is committing an earlier upload; retry shortly".to_owned()
        }
        TeregenV2Error::RejectedId { detail } => format!("the server rejects this id: {detail}"),
        TeregenV2Error::Invalid(doc_err) => doc_err.to_string(),
        other => other.to_string(),
    }
}

/// `load_via_document`'s refusal-message mapping and backoff.
///
/// Colocated here because it needs the private
/// `load_via_document`/`document_fetch_message` seam.
#[cfg(test)]
mod ingest_tests {
    use mtui_datasources::teregen::TeregenV2Error;
    use mtui_types::RequestReviewID;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::reports::ObsReport;

    const RRID_STR: &str = "SUSE:Maintenance:1:2";

    fn rrid() -> RequestReviewID {
        RequestReviewID::parse(RRID_STR).unwrap()
    }

    fn config_for(server: &MockServer) -> Config {
        let mut c = Config::default();
        c.teregen_api_v2 = server.uri();
        c
    }

    fn obs_report(config: &Config) -> Box<dyn TestReport + Send + Sync> {
        Box::new(ObsReport::new(config.clone()))
    }

    async fn mount_document(server: &MockServer) {
        Mock::given(method("GET"))
            .and(path(format!("/reports/{RRID_STR}")))
            .respond_with(ResponseTemplate::new(200).set_body_string(minimal_document(RRID_STR)))
            .mount(server)
            .await;
    }

    fn minimal_document(id: &str) -> String {
        format!(
            r#"{{
                "schema_version": "1.0", "id": "{id}", "kind": "maintenance",
                "workflow": "obs", "generated_at": "2026-01-01T00:00:00Z",
                "verdict": null, "comment": null,
                "people": {{"testers": [], "reviewer": {{"name": null}}}},
                "update": {{"packager": "p", "source_packages": ["a"], "origin": {{}},
                           "products": [{{"name": "n", "version": "v", "archs": ["x86_64"]}}]}},
                "install": {{"repository": "http://x/", "targets": [{{
                    "product": "n", "version": "v", "arch": "x86_64",
                    "repository": "http://x/r", "binaries": {{"a": "1-1.x86_64"}}
                }}], "test_platforms": []}},
                "issues": {{}}, "testing": {{}}
            }}"#
        )
    }

    #[test]
    fn document_fetch_message_matches_p3_d5_texts() {
        assert_eq!(
            document_fetch_message(TeregenV2Error::NotFound),
            "no document yet — run `regenerate` first"
        );
        assert_eq!(
            document_fetch_message(TeregenV2Error::Stale),
            "the server's document is stale — run `regenerate`"
        );
        assert_eq!(
            document_fetch_message(TeregenV2Error::Generating),
            "the server is still generating this document"
        );
        assert_eq!(
            document_fetch_message(TeregenV2Error::Busy),
            "the server is committing an earlier upload; retry shortly"
        );
        assert_eq!(
            document_fetch_message(TeregenV2Error::RejectedId {
                detail: "bad id".to_owned()
            }),
            "the server rejects this id: bad id"
        );

        // `Invalid` must render the `DocumentError` pointer verbatim, not
        // wrapped in `TeregenV2Error::Invalid`'s own "the document failed to
        // parse: ..." prose.
        let doc_err = "{}"
            .parse::<mtui_types::report_document::ReportDocument>()
            .unwrap_err();
        let pointer = doc_err.pointer.clone();
        let msg = document_fetch_message(TeregenV2Error::Invalid(doc_err));
        assert!(msg.starts_with(&pointer), "{msg}");
        assert!(!msg.starts_with("the document failed to parse"), "{msg}");
    }

    /// A server that never stops generating, or never finishes committing, is
    /// given up on after ten minutes of polling, with its own refusal.
    #[tokio::test(start_paused = true)]
    async fn the_busy_poll_gives_up_after_ten_minutes() {
        for busy in [TeregenV2Error::Generating, TeregenV2Error::Busy] {
            let calls = std::cell::Cell::new(0u32);
            let start = tokio::time::Instant::now();
            let fetch = || {
                calls.set(calls.get() + 1);
                let answer = match busy {
                    TeregenV2Error::Generating => TeregenV2Error::Generating,
                    _ => TeregenV2Error::Busy,
                };
                async move { Err::<(), _>(answer) }
            };
            let never = CancellationToken::new();
            let out = tokio::time::timeout(
                std::time::Duration::from_secs(3600),
                fetch_while_busy(&never, fetch),
            )
            .await
            .expect("the poll must end on its own")
            .expect("nothing cancelled the poll");
            assert_eq!(
                std::mem::discriminant(&out.unwrap_err()),
                std::mem::discriminant(&busy)
            );
            assert_eq!(start.elapsed(), std::time::Duration::from_secs(600));
            assert_eq!(calls.get(), 121);
        }
    }

    /// A cancel during the busy poll ends it at once, not after the pending
    /// sleep or the ten-minute budget.
    #[tokio::test(start_paused = true)]
    async fn a_cancel_stops_the_busy_poll_mid_sleep() {
        let cancel = CancellationToken::new();
        let calls = std::cell::Cell::new(0u32);
        let start = tokio::time::Instant::now();
        let fetch = || {
            calls.set(calls.get() + 1);
            async { Err::<(), _>(TeregenV2Error::Generating) }
        };
        let canceller = async {
            tokio::time::sleep(std::time::Duration::from_secs(7)).await;
            cancel.cancel();
        };

        let (out, ()) = tokio::join!(fetch_while_busy(&cancel, fetch), canceller);

        assert_eq!(out.unwrap_err(), LoadCancelled::Document);
        assert_eq!(start.elapsed(), std::time::Duration::from_secs(7));
        assert_eq!(calls.get(), 2);
    }

    /// An answer already fetched is returned as it is, cancel or not.
    #[tokio::test]
    async fn a_cancelled_poll_still_returns_the_answer_in_hand() {
        let cancel = CancellationToken::new();
        cancel.cancel();

        let out = fetch_while_busy(&cancel, || async { Err::<(), _>(TeregenV2Error::NotFound) })
            .await
            .expect("a real answer outranks the cancel");

        assert!(matches!(out, Err(TeregenV2Error::NotFound)));
    }

    #[tokio::test]
    async fn load_via_document_404_maps_to_no_document_yet() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(format!("/reports/{RRID_STR}")))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;

        let config = config_for(&server);
        let mut report = obs_report(&config);
        let err = load_via_document(
            &mut report,
            &rrid(),
            &config,
            std::path::Path::new("/nonexistent/log"),
            None,
            &CancellationToken::new(),
        )
        .await
        .unwrap_err();
        assert_eq!(
            err,
            LoadError::Failed("no document yet — run `regenerate` first".to_owned())
        );
    }

    #[tokio::test]
    async fn load_via_document_invalid_json_names_the_pointer() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(format!("/reports/{RRID_STR}")))
            .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"id": "x"}"#))
            .mount(&server)
            .await;

        let config = config_for(&server);
        let mut report = obs_report(&config);
        let err = load_via_document(
            &mut report,
            &rrid(),
            &config,
            std::path::Path::new("/nonexistent/log"),
            None,
            &CancellationToken::new(),
        )
        .await
        .unwrap_err();
        assert!(
            matches!(&err, LoadError::Failed(m) if m.starts_with('/')),
            "{err:?}"
        );
    }

    /// The bounded backoff: a `503` followed by a `200` must be
    /// retried, not refused immediately. Real-time bound by one
    /// `POLL_INTERVAL` sleep (~5s) — the single test in this file that pays
    /// that cost.
    #[tokio::test]
    async fn load_via_document_retries_past_a_single_generating_response() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(format!("/reports/{RRID_STR}")))
            .respond_with(ResponseTemplate::new(503))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        mount_document(&server).await;

        let dir = tempfile::tempdir().unwrap();
        let trpath = dir.path().join("log");

        let config = config_for(&server);
        let mut report = obs_report(&config);
        load_via_document(
            &mut report,
            &rrid(),
            &config,
            &trpath,
            None,
            &CancellationToken::new(),
        )
        .await
        .unwrap();

        assert_eq!(report.base().document.as_ref().unwrap().id, RRID_STR);
        assert_eq!(report.base().path.as_deref(), Some(trpath.as_path()));
    }

    /// The same backoff for the commit guard's `503 busy`: it is transient too.
    #[tokio::test]
    async fn load_via_document_retries_past_a_single_busy_response() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(format!("/reports/{RRID_STR}")))
            .respond_with(
                ResponseTemplate::new(503).set_body_json(serde_json::json!({"error": "busy"})),
            )
            .up_to_n_times(1)
            .expect(1)
            .mount(&server)
            .await;
        mount_document(&server).await;

        let dir = tempfile::tempdir().unwrap();
        let trpath = dir.path().join("log");
        let config = config_for(&server);
        let mut report = obs_report(&config);
        load_via_document(
            &mut report,
            &rrid(),
            &config,
            &trpath,
            None,
            &CancellationToken::new(),
        )
        .await
        .unwrap();

        assert_eq!(report.base().document.as_ref().unwrap().id, RRID_STR);
    }

    #[tokio::test]
    async fn document_load_creates_the_dir_and_never_checks_out() {
        let server = MockServer::start().await;
        mount_document(&server).await;

        let dir = tempfile::tempdir().unwrap();
        let rrid_dir = dir.path().join(RRID_STR);
        let trpath = rrid_dir.join("log");

        let config = config_for(&server);
        let mut report = obs_report(&config);
        load_via_document(
            &mut report,
            &rrid(),
            &config,
            &trpath,
            None,
            &CancellationToken::new(),
        )
        .await
        .unwrap();

        assert!(rrid_dir.is_dir());
        assert!(!trpath.exists());
        assert_eq!(report.base().path.as_deref(), Some(trpath.as_path()));
    }

    /// Loads `RRID_STR` against a server whose `/schema` answers `schema`, and
    /// returns the loaded report's `schema_drift`.
    async fn drift_after_loading(schema: ResponseTemplate) -> Option<String> {
        let server = MockServer::start().await;
        mount_document(&server).await;
        Mock::given(method("GET"))
            .and(path("/schema"))
            .respond_with(schema)
            .mount(&server)
            .await;
        let dir = tempfile::tempdir().unwrap();
        let config = config_for(&server);
        let mut report = obs_report(&config);

        load_via_document(
            &mut report,
            &rrid(),
            &config,
            &dir.path().join("log"),
            None,
            &CancellationToken::new(),
        )
        .await
        .expect("the load never fails because of the schema");
        report.base().schema_drift.clone()
    }

    #[tokio::test]
    async fn a_drifted_schema_loads_but_is_recorded() {
        let mut live: serde_json::Value =
            serde_json::from_str(mtui_types::report_document::SCHEMA_JSON).unwrap();
        live["title"] = serde_json::json!("changed upstream");

        let drift = drift_after_loading(ResponseTemplate::new(200).set_body_json(live)).await;

        assert!(drift.is_some_and(|d| d.starts_with("/title")));
    }

    #[tokio::test]
    async fn a_matching_schema_records_no_drift() {
        let drift = drift_after_loading(
            ResponseTemplate::new(200).set_body_string(mtui_types::report_document::SCHEMA_JSON),
        )
        .await;

        assert_eq!(drift, None);
    }

    #[tokio::test]
    async fn an_unreadable_schema_does_not_fail_the_load() {
        assert_eq!(drift_after_loading(ResponseTemplate::new(500)).await, None);
    }

    /// Mounts the v1 regenerate write path: `POST .../regenerate` accepted,
    /// `GET .../status` immediately `finished`.
    async fn mount_v1_regenerate_finished(server: &MockServer) {
        Mock::given(method("POST"))
            .and(path(format!("/reports/{RRID_STR}/regenerate")))
            .respond_with(ResponseTemplate::new(202).set_body_json(serde_json::json!({"job": 1})))
            .mount(server)
            .await;
        Mock::given(method("GET"))
            .and(path(format!("/reports/{RRID_STR}/status")))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"minion_state": "finished"})),
            )
            .mount(server)
            .await;
    }

    /// Step 7: after a successful regenerate, the reload goes through the v2
    /// document, not a re-checkout + re-read.
    #[tokio::test]
    async fn regenerate_via_teregen_reloads_from_the_fresh_document() {
        let server = MockServer::start().await;
        mount_v1_regenerate_finished(&server).await;
        mount_document(&server).await;

        let dir = tempfile::tempdir().unwrap();
        let rrid_dir = dir.path().join(RRID_STR);
        std::fs::create_dir_all(&rrid_dir).unwrap();
        let trpath = rrid_dir.join("log");
        std::fs::write(&trpath, "unused").unwrap();

        let mut config = config_for(&server);
        config.teregen_api = server.uri();
        let update = mtui_types::UpdateID::parse(RRID_STR).unwrap();

        let fresh =
            regenerate_via_teregen(&update, &config, &trpath, None, &CancellationToken::new())
                .await
                .unwrap()
                .expect("regenerate + reload succeeds");
        assert_eq!(fresh.base().document.as_ref().unwrap().id, RRID_STR);
    }

    /// Step 7: a `304` against the previous etag after a regenerate means the
    /// document did not actually change — reported as a reload failure
    /// (falls back to manual handling), not misread as success.
    #[tokio::test]
    async fn regenerate_via_teregen_reports_still_stale_on_304() {
        let server = MockServer::start().await;
        mount_v1_regenerate_finished(&server).await;
        Mock::given(method("GET"))
            .and(path(format!("/reports/{RRID_STR}")))
            .and(wiremock::matchers::header("if-none-match", "\"prev\""))
            .respond_with(ResponseTemplate::new(304))
            .mount(&server)
            .await;

        let dir = tempfile::tempdir().unwrap();
        let rrid_dir = dir.path().join(RRID_STR);
        let trpath = rrid_dir.join("log");

        let mut config = config_for(&server);
        config.teregen_api = server.uri();
        let update = mtui_types::UpdateID::parse(RRID_STR).unwrap();

        let fresh = regenerate_via_teregen(
            &update,
            &config,
            &trpath,
            Some("\"prev\""),
            &CancellationToken::new(),
        )
        .await;
        assert!(matches!(fresh, Ok(None)));
    }

    /// A cancel during the regeneration job wait stops it as cancelled, rather
    /// than waiting out the job or falling back to the manual handling.
    #[tokio::test]
    async fn regenerate_via_teregen_stops_on_a_cancel_during_the_job_wait() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(format!("/reports/{RRID_STR}/regenerate")))
            .respond_with(ResponseTemplate::new(202).set_body_json(serde_json::json!({"job": 1})))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path(format!("/reports/{RRID_STR}/status")))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"minion_state": "active"})),
            )
            .mount(&server)
            .await;

        let dir = tempfile::tempdir().unwrap();
        let mut config = config_for(&server);
        config.teregen_api = server.uri();
        let update = mtui_types::UpdateID::parse(RRID_STR).unwrap();
        let cancel = CancellationToken::new();
        cancel.cancel();

        let out = tokio::time::timeout(
            std::time::Duration::from_secs(30),
            regenerate_via_teregen(&update, &config, &dir.path().join("log"), None, &cancel),
        )
        .await
        .expect("the cancel must end the job wait");

        assert!(matches!(out, Err(LoadCancelled::Regeneration)));
    }

    /// A cancel while the reload after a finished job waits out a generating
    /// document is cancelled, not a reload failure.
    #[tokio::test]
    async fn regenerate_via_teregen_stops_on_a_cancel_during_the_reload() {
        let server = MockServer::start().await;
        mount_v1_regenerate_finished(&server).await;
        Mock::given(method("GET"))
            .and(path(format!("/reports/{RRID_STR}")))
            .respond_with(ResponseTemplate::new(503))
            .mount(&server)
            .await;

        let dir = tempfile::tempdir().unwrap();
        let mut config = config_for(&server);
        config.teregen_api = server.uri();
        let update = mtui_types::UpdateID::parse(RRID_STR).unwrap();
        let cancel = CancellationToken::new();
        cancel.cancel();

        let out = tokio::time::timeout(
            std::time::Duration::from_secs(30),
            regenerate_via_teregen(&update, &config, &dir.path().join("log"), None, &cancel),
        )
        .await
        .expect("the cancel must end the reload's busy poll");

        assert!(matches!(out, Err(LoadCancelled::Document)));
    }

    /// Answers `active` and cancels `0` as it does, so the cancel follows the
    /// poll it answers by construction.
    struct CancelOnAnswer(CancellationToken);

    impl wiremock::Respond for CancelOnAnswer {
        fn respond(&self, _: &wiremock::Request) -> ResponseTemplate {
            self.0.cancel();
            ResponseTemplate::new(200).set_body_json(serde_json::json!({"minion_state": "active"}))
        }
    }

    /// A cancel that arrives during the job wait, after a first poll found the
    /// job running, stops it as cancelled at that poll.
    #[tokio::test]
    async fn regenerate_via_teregen_stops_on_a_cancel_arriving_mid_wait() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(format!("/reports/{RRID_STR}/regenerate")))
            .respond_with(ResponseTemplate::new(202).set_body_json(serde_json::json!({"job": 1})))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path(format!("/reports/{RRID_STR}/status")))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"minion_state": "active"})),
            )
            .up_to_n_times(1)
            .mount(&server)
            .await;
        let cancel = CancellationToken::new();
        Mock::given(method("GET"))
            .and(path(format!("/reports/{RRID_STR}/status")))
            .respond_with(CancelOnAnswer(cancel.clone()))
            .mount(&server)
            .await;
        let dir = tempfile::tempdir().unwrap();
        let mut config = config_for(&server);
        config.teregen_api = server.uri();
        let update = mtui_types::UpdateID::parse(RRID_STR).unwrap();

        let out = tokio::time::timeout(
            std::time::Duration::from_secs(30),
            regenerate_via_teregen(&update, &config, &dir.path().join("log"), None, &cancel),
        )
        .await
        .expect("the cancel must end the job wait");

        assert!(
            matches!(out, Err(LoadCancelled::Regeneration)),
            "{:?}",
            out.err()
        );
        let polls = server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .filter(|r| r.url.path().ends_with("/status"))
            .count();
        assert_eq!(polls, 2);
    }

    /// A job that failed after the wait saw a cancel is reported as the
    /// failure; only an unfinished job is cancelled.
    #[test]
    fn a_failed_job_outranks_a_cancel_seen_during_the_wait() {
        let ended = |state: &str| mtui_datasources::RegenOutcome {
            state: Some(state.to_owned()),
            ..Default::default()
        };

        assert!(!cancelled_job_wait(&ended("failed"), true));
        assert!(cancelled_job_wait(&ended("active"), true));
        assert!(!cancelled_job_wait(&ended("active"), false));
    }
}
