//! The test-report construction lifecycle (`make_testreport`).
//!
//! Selects the report class by RRID kind (`tr_factory`), runs the checkout +
//! read cycle, and applies workflow selection + the deferred-autoconnect flag
//! for the auto/kernel update kinds.
//!
//! This crate stays free of the host-connect layer: the connect belongs to the
//! composition root (`mtui-core::Session::load_update`), which owns the arbiter
//! wiring and the refhosts-from-testplatform resolution, so `make_testreport`
//! only records the intent via
//! [`TestReportBase::autoconnect_pending`](crate::testreport::TestReportBase::autoconnect_pending).
//! The QEM Dashboard / auto-openQA enrichment does run here, for the `-a` kind,
//! and autoconnect fires only on its downgrade-to-[`Workflow::Manual`] path.

use mtui_config::options::Config;
use mtui_datasources::qem_dashboard::dashboard_openqa::DashboardAutoOpenQA;
use mtui_datasources::qem_dashboard::incident::QemIncident;
use mtui_datasources::{TeReGen, VerifyPolicy, resolve_verify};
use mtui_hosts::Prompter;
use mtui_types::enums::RequestKind;
use mtui_types::{UpdateID, Workflow};
use tracing::{error, info, warn};

#[cfg(not(feature = "api-ingest"))]
use crate::checkout::ReadOutcome;
use crate::checkout::TokioSvnRunner;
use crate::reports::{NullReport, ObsReport, PiReport, SlReport};
#[cfg(not(feature = "api-ingest"))]
use crate::testreport::ReadError;
use crate::testreport::{HashCheck, TestReport};

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

/// A [`NullReport`] carrying the reason its load failed, so
/// `Session::load_update` can surface *why* rather than "could not load".
fn null_with_error(config: Config, reason: String) -> NullReport {
    let mut report = NullReport::new(config);
    report.base_mut().load_error = Some(reason);
    report
}

/// Builds and populates a [`TestReport`] for `update`.
///
/// 1. Selects the report class by RRID kind (`tr_factory`).
/// 2. Reads `template_dir/<rrid>/log`; a missing template triggers a `svn`
///    checkout and one retry.
/// 3. On a load failure returns a [`NullReport`], so the caller can add a
///    benign inactive template rather than propagate an error.
/// 4. Verifies the Gitea token + template hash ([`TestReport::check_hash`]): a
///    missing token or failed call abandons the load; a stale hash goes to the
///    TeReGen regenerate / force-continue / delete-checkout handling.
/// 5. Sets the workflow from `kind`. The `-a` (auto) kind builds the
///    [`QemIncident`] and runs [`DashboardAutoOpenQA`]; with no install jobs
///    (or an unreachable dashboard) the workflow is **downgraded to
///    [`Workflow::Manual`]**.
///
/// `autoconnect` is the caller's explicit choice, but the deferred connect (via
/// [`TestReportBase::autoconnect_pending`](crate::testreport::TestReportBase::autoconnect_pending),
/// honoured by the composition root *after* wiring the host arbiter) is armed
/// **only** when it is `true` **and** the auto load downgraded to `MANUAL` —
/// never on the auto happy path, never for the kernel kind.
///
/// `force_continue` is the non-interactive escape hatch for a stale hash
/// (`handle_stale_hash`): when `is_repl && prompter.is_some()`, the REPL's
/// own "Force continue loading template ?" prompt governs and this argument
/// is ignored; otherwise it takes the prompt's place. `false` reproduces the
/// pre-existing non-interactive behaviour (abandon the load) exactly.
pub async fn make_testreport(
    update: &UpdateID,
    config: Config,
    kind: UpdateKind,
    autoconnect: bool,
    is_repl: bool,
    prompter: Option<&Prompter>,
    force_continue: bool,
) -> Box<dyn TestReport + Send + Sync> {
    let template_dir = config.template_dir.clone();
    let svn_path = config.svn_path.clone();
    let max_parallel = config.max_parallel as usize;
    let mut report = tr_factory(update, config);

    let rrid_dir = template_dir.join(update.id.to_string());
    let trpath = rrid_dir.join("log");

    #[cfg(not(feature = "api-ingest"))]
    let runner = TokioSvnRunner;
    let checkout_config = report.base().config.clone();
    let rrid = update.id.clone();

    // Inlined rather than routed through `checkout_and_read`: the `read` step
    // must mutate `report`, which clashes with the borrows the closures need.
    #[cfg(not(feature = "api-ingest"))]
    let loaded: Result<(), String> = match to_outcome(report.read(&trpath)) {
        ReadOutcome::Ok => Ok(()),
        ReadOutcome::Io(e) if !e.is_not_found() => {
            // A non-ENOENT read error is not a "needs checkout" signal.
            info!("{e}");
            Err(format!("reading {}: {e}", trpath.display()))
        }
        ReadOutcome::Io(_missing) => {
            match crate::checkout::testreport_svn_checkout(
                &runner,
                &checkout_config,
                &svn_path,
                &rrid,
            )
            .await
            {
                Ok(()) => match to_outcome(report.read(&trpath)) {
                    ReadOutcome::Ok => Ok(()),
                    ReadOutcome::Io(e) => {
                        info!("{e}");
                        Err(format!("reading {} after checkout: {e}", trpath.display()))
                    }
                },
                Err(e) => {
                    info!("{e}");
                    Err(format!("svn checkout of {rrid} failed: {e}"))
                }
            }
        }
    };

    // The v2 read path: the document replaces `TestReport::read`'s
    // log-scraping + `metadata.json` parse as the source of the model, but
    // the SVN checkout still runs — `report_wd()`, `export`, `commit`,
    // `showdiff` and `ObsReport::update_repos_parser`'s `project.xml` read
    // all still need the scratch directory.
    #[cfg(feature = "api-ingest")]
    let loaded: Result<(), String> = load_via_document(
        &mut report,
        &rrid,
        &checkout_config,
        &svn_path,
        &trpath,
        None,
    )
    .await;

    if let Err(reason) = loaded {
        info!("TestReport isn't loaded");
        return Box::new(null_with_error(checkout_config, reason));
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
            return Box::new(null_with_error(checkout_config, msg.to_owned()));
        }
        HashCheck::Failed(e) => {
            error!("Gitea API call failed");
            info!(error = %e, "TestReport isn't loaded");
            return Box::new(null_with_error(
                checkout_config,
                format!("Gitea API call failed: {e}"),
            ));
        }
        HashCheck::Mismatch { .. } => {
            let prev_etag = report.base().document_etag.clone();
            match handle_stale_hash(
                update,
                &checkout_config,
                &svn_path,
                &rrid_dir,
                &trpath,
                is_repl,
                prompter,
                force_continue,
                prev_etag.as_deref(),
            )
            .await
            {
                Some(regenerated) => {
                    if let Some(fresh) = regenerated {
                        report = fresh;
                    } else {
                        // Force-continue kept the (stale) `report` as-is; flag it
                        // so a non-interactive caller can surface the fact too.
                        report.base_mut().stale_hash_warning = Some(
                            "template hash mismatch (stale checkout); loaded as-is \
                             via force-continue"
                                .to_owned(),
                        );
                    }
                }
                None => {
                    return Box::new(null_with_error(
                        checkout_config,
                        "template hash mismatch (stale checkout); regeneration \
                         declined or unavailable"
                            .to_owned(),
                    ));
                }
            }
        }
    }

    report.base_mut().workflow = kind.workflow();

    if kind == UpdateKind::Auto {
        // Snapshot before the awaits: no `&report` borrow may cross `.await`.
        let dashboard_api = report.base().config.qem_dashboard_api.clone();
        let openqa_instance = report.base().config.openqa_instance.clone();
        let max_parallel = report.base().config.max_parallel as usize;
        let policy = resolve_verify(
            VerifyPolicy::Default(true),
            Some(VerifyPolicy::from_config(&report.base().config.ssl_verify)),
        );
        let source = report.update_source();

        match QemIncident::new(rrid.clone(), dashboard_api, policy, source).await {
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
                    report.base_mut().workflow = Workflow::Manual;
                    if autoconnect {
                        // The composition root wires the arbiter first, so
                        // refhosts_from_tp draws one host per slot.
                        report.base_mut().autoconnect_pending = true;
                    }
                }
            }
            Err(e) => {
                // No dashboard client at all: same best-effort downgrade.
                warn!(error = %e, "QEM Dashboard unavailable; switching mode to manual");
                report.base_mut().workflow = Workflow::Manual;
                if autoconnect {
                    report.base_mut().autoconnect_pending = true;
                }
            }
        }
    }

    // The session is the single source of truth for REPL-vs-headless: the group
    // is built headless and reconciled here, once, never toggled afterwards.
    report.base_mut().targets.set_is_repl(is_repl);
    report.base_mut().targets.set_max_parallel(max_parallel);

    report
}

/// Handles a stale template hash: log, offer TeReGen regeneration, then the
/// manual force-continue / delete-checkout fallback.
///
/// * `Some(Some(fresh))` — TeReGen regenerated a fresh, verified report;
/// * `Some(None)` — force-continue; the caller keeps its existing stale report;
/// * `None` — abandon the load (the caller substitutes a [`NullReport`]).
///
/// `prompter` is `Some` only in interactive mode, so every prompt is gated on
/// `is_repl && prompter.is_some()` and otherwise takes the non-interactive
/// answer — **except** the force-continue question, whose non-interactive
/// default is `force_continue_arg` rather than a hard-coded `false` (#517).
/// It reaches exactly the outcome the REPL's own "y" answer does — `Some(None)`
/// — and does nothing else: no regeneration, no re-checkout, no write. The
/// `regenerate` question is unaffected and stays interactive-only.
#[allow(clippy::too_many_arguments)]
async fn handle_stale_hash(
    update: &UpdateID,
    config: &Config,
    svn_path: &str,
    rrid_dir: &std::path::Path,
    trpath: &std::path::Path,
    is_repl: bool,
    prompter: Option<&Prompter>,
    force_continue_arg: bool,
    prev_etag: Option<&str>,
) -> Option<Option<Box<dyn TestReport + Send + Sync>>> {
    let rrid = update.id.clone();
    error!("Invalid Gitea hash");
    warn!("TestReport hash differs from the Gitea PR; the template is stale");

    let regenerate = match (is_repl, prompter) {
        (true, Some(p)) => {
            p.confirm("Regenerate the template now via TeReGen? [y/N]: ", false)
                .await
        }
        _ => false,
    };

    if regenerate {
        if let Some(fresh) =
            regenerate_via_teregen(update, config, svn_path, rrid_dir, trpath, prev_etag).await
        {
            return Some(Some(fresh));
        }
        warn!("Regeneration failed; falling back to manual handling");
    } else {
        info!(
            "TestReport can be regenerated here: https://qam.suse.de/reports/{}/log",
            rrid
        );
    }

    // Manual fallback: non-interactive falls through to `force_continue_arg`.
    let force_continue = match (is_repl, prompter) {
        (true, Some(p)) => {
            p.confirm("Force continue loading template ? [y/N]: ", false)
                .await
        }
        _ => force_continue_arg,
    };
    if force_continue {
        warn!("Template is loaded, but hash differs");
        // Keep the caller's existing (stale) report.
        return Some(None);
    }

    // Declined: optionally delete the stale checkout, then abandon the load.
    let delete = match (is_repl, prompter) {
        (true, Some(p)) if rrid_dir.exists() => {
            p.confirm(
                &format!(
                    "Delete checked out template {}? [Y/n]: ",
                    rrid_dir.display()
                ),
                true,
            )
            .await
        }
        _ => false,
    };
    if delete {
        let _ = tokio::fs::remove_dir_all(rrid_dir).await;
        info!("Removed checked out template {}", rrid_dir.display());
    }
    None
}

/// Regenerates a stale template via TeReGen, then reloads it — under
/// `--features api-ingest`, by re-fetching the v2 document (passing
/// `prev_etag`, so a `304` means the regenerate did not actually change the
/// document and is reported as such rather than misread as success); by
/// re-checking-out and re-reading otherwise.
///
/// Returns the freshly loaded, hash-verified report on success, or `None` so the
/// caller falls back to the manual force/decline handling. Any TeReGen failure,
/// reload failure, or a *still*-failing hash on the fresh template is a
/// `None` (logged as "Reload after regeneration failed").
async fn regenerate_via_teregen(
    update: &UpdateID,
    config: &Config,
    svn_path: &str,
    rrid_dir: &std::path::Path,
    trpath: &std::path::Path,
    prev_etag: Option<&str>,
) -> Option<Box<dyn TestReport + Send + Sync>> {
    let rrid = update.id.clone();
    info!("Waiting for the template to be regenerated ...");

    let teregen = match TeReGen::new(config, &config.teregen_api) {
        Ok(t) => t,
        Err(e) => {
            error!("TeReGen unreachable; cannot regenerate");
            info!(error = %e, "could not build TeReGen client");
            return None;
        }
    };
    let outcome = teregen
        .regenerate_and_wait(&rrid.to_string(), true, false, || false)
        .await;

    if outcome.unreachable {
        error!("TeReGen unreachable; cannot regenerate");
        return None;
    }
    if let Some(err) = &outcome.error {
        error!("Regeneration refused: {err}");
        return None;
    }
    info!("Regeneration job {:?} enqueued for {}", outcome.job, rrid);

    // The job was accepted: it is now safe to drop the stale local checkout.
    // Under `--features api-ingest` the reload below re-fetches the document
    // instead of re-checking-out, and `load_via_document` already skips the
    // checkout when a working copy exists — deleting it here would defeat
    // that and force a needless `svn co` for a directory the model no longer
    // reads content from.
    #[cfg(feature = "api-ingest")]
    let _ = rrid_dir;
    #[cfg(not(feature = "api-ingest"))]
    if rrid_dir.exists() {
        let _ = tokio::fs::remove_dir_all(rrid_dir).await;
        info!("Removed stale checked out template {}", rrid_dir.display());
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
        return None;
    }

    // A still-failing hash on the fresh template is a reload failure.
    let mut fresh = tr_factory(update, config.clone());

    #[cfg(feature = "api-ingest")]
    if let Err(e) = load_via_document(&mut fresh, &rrid, config, svn_path, trpath, prev_etag).await
    {
        error!("Reload after regeneration failed: {e}");
        return None;
    }
    #[cfg(not(feature = "api-ingest"))]
    {
        let _ = prev_etag;
        let runner = TokioSvnRunner;
        if let Err(e) =
            crate::checkout::testreport_svn_checkout(&runner, config, svn_path, &rrid).await
        {
            error!("Reload after regeneration failed: {e}");
            return None;
        }
        if let Err(e) = fresh.read(trpath) {
            error!("Reload after regeneration failed: {e}");
            return None;
        }
    }

    match fresh.check_hash().await {
        HashCheck::Ok => Some(fresh),
        other => {
            error!("Reload after regeneration failed: hash still not verified ({other:?})");
            None
        }
    }
}

/// Loads a report from the teregen v2 JSON document instead of the SVN
/// `metadata.json`/`log` pair.
///
/// Fetches `config.teregen_api_v2`'s document for `rrid`, conditional on
/// `prev_etag` (the initial load passes `None`; [`regenerate_via_teregen`]
/// passes the just-superseded report's etag, so a `304` after a regenerate
/// job is reported as "unchanged" rather than misread as fresh). Every
/// non-`Fresh` outcome maps to a distinct, actionable message rather than
/// falling back to SVN — a fallback would mask exactly the corpus gap this
/// path exists to surface. A `503 generating` response is retried on
/// the same bounded poll budget [`TeReGen::wait_for_template`] uses (5s /
/// 600s) before refusing with the same message. On success, applies the
/// document (see [`crate::ingest::apply_document`]), then runs the checkout
/// only if no working copy exists yet, and finally derives `update_repos`
/// exactly as [`TestReport::read`] does. `ReducedMetadataParser`/
/// `JSONParser`/`patchinfo_titles` are never invoked on this path.
#[cfg(feature = "api-ingest")]
async fn load_via_document(
    report: &mut Box<dyn TestReport + Send + Sync>,
    rrid: &mtui_types::RequestReviewID,
    config: &Config,
    svn_path: &str,
    trpath: &std::path::Path,
    prev_etag: Option<&str>,
) -> Result<(), String> {
    use mtui_datasources::teregen::{DocumentFetch, TeregenV2, TeregenV2Error};

    // Mirrors `TeReGen::wait_for_template`'s poll budget: a `generating`
    // refusal is bounded, not immediate.
    const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5);
    const POLL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(600);

    let client = TeregenV2::new(config, &config.teregen_api_v2)
        .map_err(|e| format!("building the teregen v2 client failed: {e}"))?;
    let rrid_str = rrid.to_string();

    let deadline = tokio::time::Instant::now() + POLL_TIMEOUT;
    let fetch = loop {
        match client.fetch_document(&rrid_str, prev_etag).await {
            Err(TeregenV2Error::Generating) if tokio::time::Instant::now() < deadline => {
                tokio::time::sleep(POLL_INTERVAL).await;
            }
            other => break other,
        }
    };

    let document = match fetch {
        Ok(DocumentFetch::Fresh { document, etag, .. }) => {
            report.base_mut().document_etag = etag;
            document
        }
        Ok(DocumentFetch::NotModified) => {
            return Err("the server's document is unchanged since the last load".to_owned());
        }
        Err(e) => return Err(document_fetch_message(e)),
    };

    crate::ingest::apply_document(report.base_mut(), &document);
    report.base_mut().document = Some(*document);

    if tokio::fs::metadata(trpath).await.is_err() {
        let runner = TokioSvnRunner;
        crate::checkout::testreport_svn_checkout(&runner, config, svn_path, rrid)
            .await
            .map_err(|e| format!("svn checkout of {rrid} failed: {e}"))?;
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
#[cfg(feature = "api-ingest")]
fn document_fetch_message(e: mtui_datasources::teregen::TeregenV2Error) -> String {
    use mtui_datasources::teregen::TeregenV2Error;
    match e {
        TeregenV2Error::NotFound => "no document yet — run `regenerate` first".to_owned(),
        TeregenV2Error::Stale => "the server's document is stale — run `regenerate`".to_owned(),
        TeregenV2Error::Generating => "the server is still generating this document".to_owned(),
        TeregenV2Error::RejectedId { detail } => format!("the server rejects this id: {detail}"),
        TeregenV2Error::Invalid(doc_err) => doc_err.to_string(),
        other => other.to_string(),
    }
}

/// Maps a [`TestReport::read`] result to the checkout seam's [`ReadOutcome`].
///
/// A present-but-unparseable `metadata.json` becomes a **non-ENOENT** read error
/// so the seam does not loop into a (pointless) checkout for it.
#[cfg(not(feature = "api-ingest"))]
fn to_outcome(res: Result<(), ReadError>) -> ReadOutcome {
    match res {
        Ok(()) => ReadOutcome::Ok,
        Err(ReadError::Template(e)) => ReadOutcome::Io(e),
        Err(_) => ReadOutcome::Io(crate::checkout::TemplateIoError::from_io(
            &std::io::Error::other("metadata.json present but could not be parsed"),
        )),
    }
}

/// `load_via_document`'s refusal-message mapping and backoff.
///
/// Colocated here rather than in `tests/lifecycle.rs` for two reasons: this
/// needs the private `load_via_document`/`document_fetch_message` seam, and
/// that integration suite is SVN-`make_testreport`-only — `tests/it.rs` excludes
/// it entirely under `--features api-ingest`, since the SVN branch it exercises
/// does not exist under the feature.
#[cfg(all(test, feature = "api-ingest"))]
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

    #[tokio::test]
    async fn load_via_document_404_maps_to_no_document_yet() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(format!("/reports/{RRID_STR}")))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;

        let config = config_for(&server);
        let mut report: Box<dyn TestReport + Send + Sync> =
            Box::new(ObsReport::new(config.clone()));
        let err = load_via_document(
            &mut report,
            &rrid(),
            &config,
            "svn+ssh://unused",
            std::path::Path::new("/nonexistent/log"),
            None,
        )
        .await
        .unwrap_err();
        assert_eq!(err, "no document yet — run `regenerate` first");
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
        let mut report: Box<dyn TestReport + Send + Sync> =
            Box::new(ObsReport::new(config.clone()));
        let err = load_via_document(
            &mut report,
            &rrid(),
            &config,
            "svn+ssh://unused",
            std::path::Path::new("/nonexistent/log"),
            None,
        )
        .await
        .unwrap_err();
        assert!(err.starts_with('/'), "{err}");
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
        Mock::given(method("GET"))
            .and(path(format!("/reports/{RRID_STR}")))
            .respond_with(ResponseTemplate::new(200).set_body_string(minimal_document(RRID_STR)))
            .mount(&server)
            .await;

        let dir = tempfile::tempdir().unwrap();
        let trpath = dir.path().join("log");
        // Pre-create the checkout so the metadata-existence check skips the
        // (real, offline-unreachable) `svn co` this test does not mock.
        std::fs::write(&trpath, "unused").unwrap();

        let config = config_for(&server);
        let mut report: Box<dyn TestReport + Send + Sync> =
            Box::new(ObsReport::new(config.clone()));
        load_via_document(
            &mut report,
            &rrid(),
            &config,
            "svn+ssh://unused",
            &trpath,
            None,
        )
        .await
        .unwrap();

        assert_eq!(report.base().document.as_ref().unwrap().id, RRID_STR);
        assert_eq!(report.base().path.as_deref(), Some(trpath.as_path()));
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
        Mock::given(method("GET"))
            .and(path(format!("/reports/{RRID_STR}")))
            .respond_with(ResponseTemplate::new(200).set_body_string(minimal_document(RRID_STR)))
            .mount(&server)
            .await;

        let dir = tempfile::tempdir().unwrap();
        let rrid_dir = dir.path().join(RRID_STR);
        std::fs::create_dir_all(&rrid_dir).unwrap();
        let trpath = rrid_dir.join("log");
        std::fs::write(&trpath, "unused").unwrap();

        let mut config = config_for(&server);
        config.teregen_api = server.uri();
        let update = mtui_types::UpdateID::parse(RRID_STR).unwrap();

        let fresh = regenerate_via_teregen(
            &update,
            &config,
            "svn+ssh://unused",
            &rrid_dir,
            &trpath,
            None,
        )
        .await
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
            "svn+ssh://unused",
            &rrid_dir,
            &trpath,
            Some("\"prev\""),
        )
        .await;
        assert!(fresh.is_none());
    }
}
