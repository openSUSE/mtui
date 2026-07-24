//! The test-report construction lifecycle (`make_testreport`).
//!
//! Ports the report-construction slice of upstream `mtui/types/updateid.py`:
//! `UpdateID.tr_factory` (report class by RRID kind), `_checkout` (checkout +
//! read), and `AutoOBSUpdateID.make_testreport` / `KernelOBSUpdateID.make_testreport`
//! (workflow selection + deferred-autoconnect flag).
//!
//! The Rust split keeps this crate free of the host-connect layer:
//! * The actual host connect is **deferred to the caller** (the composition
//!   root, `mtui-core::Session::load_update`), which owns the arbiter wiring and
//!   the refhosts-from-testplatform resolution. `make_testreport` only records
//!   the intent via [`TestReportBase::autoconnect_pending`].
//! * The QEM Dashboard / auto-openQA enrichment runs inside `make_testreport`
//!   for the `-a` (auto) kind, mirroring upstream `AutoOBSUpdateID.make_testreport`:
//!   it builds the [`QemIncident`], runs [`DashboardAutoOpenQA`], and — when the
//!   auto result has no install jobs (or they failed) — **downgrades the workflow
//!   to [`Workflow::Manual`]** and defers the reference-host connect (upstream
//!   only autoconnects on that manual-downgrade path).

use mtui_config::options::Config;
use mtui_datasources::qem_dashboard::dashboard_openqa::DashboardAutoOpenQA;
use mtui_datasources::qem_dashboard::incident::QemIncident;
use mtui_datasources::{TeReGen, VerifyPolicy, resolve_verify};
use mtui_hosts::Prompter;
use mtui_types::enums::RequestKind;
use mtui_types::{UpdateID, Workflow};
use tracing::{error, info, warn};

use crate::checkout::{ReadOutcome, TokioSvnRunner};
use crate::reports::{NullReport, ObsReport, PiReport, SlReport};
use crate::testreport::{HashCheck, ReadError, TestReport};

/// Which upstream `UpdateID` subclass produced the report — selects the workflow
/// and whether autoconnect defaults on.
///
/// Ports the distinction between `AutoOBSUpdateID` (`-a`) and `KernelOBSUpdateID`
/// (`-k`): the concrete `TestReport` class is chosen by RRID kind
/// ([`tr_factory`]), but the *workflow* and *autoconnect default* come from the
/// update kind the operator named on the command line.
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
    /// The workflow this update kind starts in (upstream `tr.workflow = …`).
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
/// Port of upstream `UpdateID.tr_factory`: SLFO → [`SlReport`], PI →
/// [`PiReport`], everything else (Maintenance) → [`ObsReport`].
#[must_use]
fn tr_factory(update: &UpdateID, config: Config) -> Box<dyn TestReport + Send + Sync> {
    match update.id.kind {
        RequestKind::Slfo => Box::new(SlReport::new(config)),
        RequestKind::Pi => Box::new(PiReport::new(config)),
        RequestKind::Maintenance => Box::new(ObsReport::new(config)),
    }
}

/// A [`NullReport`] carrying the reason its load failed.
///
/// The load-failure substitute [`make_testreport`] returns instead of a real
/// report. The reason is stashed on [`TestReportBase::load_error`] so the caller
/// (`Session::load_update` → `load_template`) can surface *why* the load failed
/// rather than a bare "could not load".
fn null_with_error(config: Config, reason: String) -> NullReport {
    let mut report = NullReport::new(config);
    report.base_mut().load_error = Some(reason);
    report
}

/// Builds and populates a [`TestReport`] for `update` (upstream
/// `UpdateID.make_testreport` → `_checkout`).
///
/// 1. Selects the report class by RRID kind ([`tr_factory`]).
/// 2. Drives [`checkout_and_read`]: reads `template_dir/<rrid>/log`; a missing
///    template triggers a `svn` checkout and one retry (upstream `_checkout`).
/// 3. On a load failure returns a [`NullReport`] (upstream returns
///    `NullTestReport`), so the caller can add a benign inactive template rather
///    than propagate an error.
/// 4. Runs the Gitea token + template-hash verification
///    ([`TestReport::check_hash`], upstream's `check_hash` at the tail of `read`
///    inside `_checkout`). A missing token, a failed Gitea call, or a stale
///    template hash logs the upstream message and abandons the load (a
///    [`NullReport`]). The interactive TeReGen regenerate / force-continue /
///    delete-checkout handling for a stale hash lands in Phase C.
/// 5. Sets the workflow from `kind`. For the `-a` (auto) kind, builds the
///    [`QemIncident`] and runs [`DashboardAutoOpenQA`]; when the auto result has
///    no install jobs (or the dashboard is unreachable) the workflow is
///    **downgraded to [`Workflow::Manual`]** (upstream's auto→manual fallback).
///
/// `autoconnect` is the caller's explicit choice (upstream's `make_testreport`
/// argument). A reference-host connect is deferred (via
/// [`TestReportBase::autoconnect_pending`], honoured by the composition root
/// *after* wiring the host arbiter) **only** when `autoconnect` is `true` **and**
/// the auto load downgraded to `MANUAL` — matching upstream, which sets
/// `_autoconnect_pending` only on that path. The auto happy-path (workflow stays
/// `AUTO`) and the kernel kind never autoconnect on load.
pub async fn make_testreport(
    update: &UpdateID,
    config: Config,
    kind: UpdateKind,
    autoconnect: bool,
    is_repl: bool,
    prompter: Option<&Prompter>,
) -> Box<dyn TestReport + Send + Sync> {
    let template_dir = config.template_dir.clone();
    let svn_path = config.svn_path.clone();
    let max_parallel = config.max_parallel as usize;
    let mut report = tr_factory(update, config);

    let rrid_dir = template_dir.join(update.id.to_string());
    let trpath = rrid_dir.join("log");

    let runner = TokioSvnRunner;
    let checkout_config = report.base().config.clone();
    let rrid = update.id.clone();

    // `_checkout`: read the template; on ENOENT run `svn co` and retry. The
    // seam's shape is inlined here (rather than via `checkout_and_read`) because
    // the `read` step must mutate `report`, which would otherwise clash with the
    // borrows the closures need — the three-step orchestration is small.
    // On failure, `Err(reason)` carries the human-readable cause so the caller
    // (via `NullReport.base().load_error`) can surface *why* the load failed.
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

    if let Err(reason) = loaded {
        info!("TestReport isn't loaded");
        return Box::new(null_with_error(checkout_config, reason));
    }

    // Upstream runs `check_hash` at the tail of `TestReport.read`; because
    // `read` is sync and `check_hash` is async, the check fires here instead —
    // right after a successful read, before the report is handed back. This is
    // the Gitea token + template-hash verification that `UpdateID._checkout`
    // wraps (missing token / failed call / stale hash). See
    // `plans/gitea-hash-check-on-load.md`.
    match report.check_hash().await {
        HashCheck::Ok => {}
        HashCheck::MissingToken => {
            // Upstream `MissingGiteaTokenError`: the exact operator-facing
            // message, then the load is abandoned (a null report here mirrors
            // `make_testreport` catching the re-raised error into a null).
            let msg = "Gitea API token is not configured. Pass -g/--gitea_token, \
                 set GITEA_TOKEN in your environment, or add a [gitea] token \
                 entry to ~/.mtuirc.";
            error!("{msg}");
            return Box::new(null_with_error(checkout_config, msg.to_owned()));
        }
        HashCheck::Failed(e) => {
            // Upstream `FailedGiteaCallError`.
            error!("Gitea API call failed");
            info!(error = %e, "TestReport isn't loaded");
            return Box::new(null_with_error(
                checkout_config,
                format!("Gitea API call failed: {e}"),
            ));
        }
        HashCheck::Mismatch { .. } => {
            // Upstream `InvalidGiteaHashError` handling in `_checkout`: offer a
            // TeReGen regeneration, then fall back to the manual force-continue
            // / delete-checkout prompts. Returns `Some(report)` when a report
            // (fresh or force-kept-stale) should load, `None` for the null path.
            match handle_stale_hash(
                update,
                &checkout_config,
                &svn_path,
                &rrid_dir,
                &trpath,
                is_repl,
                prompter,
            )
            .await
            {
                Some(regenerated) => {
                    if let Some(fresh) = regenerated {
                        // A regenerated report replaces the stale one. Apply the
                        // workflow/autoconnect below to the fresh report.
                        report = fresh;
                    }
                    // else: force-continue kept the (stale) `report` as-is.
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

    // Upstream `AutoOBSUpdateID.make_testreport`: the `-a` (auto) kind fetches
    // the QEM-dashboard auto-openQA result at load time and downgrades the
    // working mode to MANUAL when there are no install jobs (or they failed).
    // The `-k` (kernel) kind keeps its KERNEL workflow and never autoconnects.
    if kind == UpdateKind::Auto {
        // Snapshot config primitives before the awaits (no `&report`/borrow
        // crosses `.await`).
        let dashboard_api = report.base().config.qem_dashboard_api.clone();
        let openqa_instance = report.base().config.openqa_instance.clone();
        let max_parallel = report.base().config.max_parallel as usize;
        let policy = resolve_verify(
            VerifyPolicy::Default(true),
            Some(VerifyPolicy::from_config(&report.base().config.ssl_verify)),
        );

        // Build the incident handle (a failed dashboard fetch folds into
        // `data = None`, not an error) and run the auto connector. Both are
        // best-effort — network failure leaves `results = None`, which the
        // fallback below treats exactly like "no install jobs".
        match QemIncident::new(rrid.clone(), dashboard_api, policy).await {
            Ok(incident) => {
                info!("Getting data from QEM Dashboard");
                let mut auto = DashboardAutoOpenQA::new(
                    openqa_instance,
                    &incident,
                    rrid.clone(),
                    max_parallel,
                );
                // Load time is deliberately best-effort: a failed dashboard fetch
                // is folded to "no results" here (the same as an empty result),
                // so the workflow downgrades to manual rather than aborting the
                // report load. The interactive `set_workflow`/`reload_openqa`
                // commands surface the same failure as `Err` instead.
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
                        // Defer the connect to the composition root, which wires
                        // the arbiter first so refhosts_from_tp draws one host
                        // per slot. Upstream connects only on this manual path.
                        report.base_mut().autoconnect_pending = true;
                    }
                }
            }
            Err(e) => {
                // Could not even build the dashboard client: treat as no
                // results (downgrade to manual), mirroring the best-effort
                // upstream behaviour where a missing auto result flips to manual.
                warn!(error = %e, "QEM Dashboard unavailable; switching mode to manual");
                report.base_mut().workflow = Workflow::Manual;
                if autoconnect {
                    report.base_mut().autoconnect_pending = true;
                }
            }
        }
    }

    // Reconcile the report's targets group to the session mode once, at load
    // time (the group was default-built headless). The session is the single
    // source of truth for REPL-vs-headless; this is the only place it is set, and
    // it is never toggled afterwards. Empty group here, so this only sets the flag
    // that every later `add` / fan-out (spinner prompt) reads.
    report.base_mut().targets.set_is_repl(is_repl);
    // Push the configured fan-out bound (`[connection] max_parallel`) onto the
    // group alongside the session mode, so every later fan-out is bounded.
    report.base_mut().targets.set_max_parallel(max_parallel);

    report
}

/// Handles a stale template hash (upstream `_checkout`'s `InvalidGiteaHashError`
/// branch): log, offer TeReGen regeneration, then the manual force-continue /
/// delete-checkout fallback.
///
/// Returns:
/// * `Some(Some(fresh))` — TeReGen regenerated a fresh, verified report to use;
/// * `Some(None)` — the operator chose to force-continue with the stale report
///   (the caller keeps its existing `report`);
/// * `None` — abandon the load (the caller substitutes a [`NullReport`]).
///
/// `prompter` is `Some` only in interactive mode; upstream's non-interactive
/// `prompt_user` never requests input and returns `false`, so every prompt here
/// is gated on `interactive && prompter.is_some()` and defaults to the
/// non-interactive answer otherwise.
#[allow(clippy::too_many_arguments)]
async fn handle_stale_hash(
    update: &UpdateID,
    config: &Config,
    svn_path: &str,
    rrid_dir: &std::path::Path,
    trpath: &std::path::Path,
    is_repl: bool,
    prompter: Option<&Prompter>,
) -> Option<Option<Box<dyn TestReport + Send + Sync>>> {
    let rrid = update.id.clone();
    error!("Invalid Gitea hash");
    warn!("TestReport hash differs from the Gitea PR; the template is stale");

    // "Regenerate the template now via TeReGen? [y/N]" (default no).
    let regenerate = match (is_repl, prompter) {
        (true, Some(p)) => {
            p.confirm("Regenerate the template now via TeReGen? [y/N]: ", false)
                .await
        }
        _ => false,
    };

    if regenerate {
        if let Some(fresh) =
            regenerate_via_teregen(update, config, svn_path, rrid_dir, trpath).await
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

    // Manual fallback: "Force continue loading template ? [y/N]" (default no).
    let force_continue = match (is_repl, prompter) {
        (true, Some(p)) => {
            p.confirm("Force continue loading template ? [y/N]: ", false)
                .await
        }
        _ => false,
    };
    if force_continue {
        warn!("Template is loaded, but hash differs");
        // Keep the caller's existing (stale) report.
        return Some(None);
    }

    // Declined: optionally delete the stale checkout, then abandon the load.
    // "Delete checked out template <dir>? [Y/n]" (default yes).
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

/// Regenerates a stale template via TeReGen, then re-checks-out and re-reads it
/// (upstream `_regenerate`).
///
/// Returns the freshly loaded, hash-verified report on success, or `None` so the
/// caller falls back to the manual force/decline handling. Any TeReGen failure,
/// checkout/read failure, or a *still*-failing hash on the fresh template is a
/// `None` (upstream "Reload after regeneration failed").
async fn regenerate_via_teregen(
    update: &UpdateID,
    config: &Config,
    svn_path: &str,
    rrid_dir: &std::path::Path,
    trpath: &std::path::Path,
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

    // Fresh checkout + read of the regenerated template (upstream re-runs
    // `_vcs_checkout` + `read`; a still-failing hash here is a reload failure).
    let mut fresh = tr_factory(update, config.clone());
    let runner = TokioSvnRunner;
    if let Err(e) = crate::checkout::testreport_svn_checkout(&runner, config, svn_path, &rrid).await
    {
        error!("Reload after regeneration failed: {e}");
        return None;
    }
    if let Err(e) = fresh.read(trpath) {
        error!("Reload after regeneration failed: {e}");
        return None;
    }
    match fresh.check_hash().await {
        HashCheck::Ok => Some(fresh),
        other => {
            error!("Reload after regeneration failed: hash still not verified ({other:?})");
            None
        }
    }
}

/// Maps a [`TestReport::read`] result to the checkout seam's [`ReadOutcome`].
///
/// A present-but-unparseable `metadata.json` becomes a **non-ENOENT** read error
/// so the seam does not loop into a (pointless) checkout for it.
fn to_outcome(res: Result<(), ReadError>) -> ReadOutcome {
    match res {
        Ok(()) => ReadOutcome::Ok,
        Err(ReadError::Template(e)) => ReadOutcome::Io(e),
        Err(_) => ReadOutcome::Io(crate::checkout::TemplateIoError::from_io(
            &std::io::Error::other("metadata.json present but could not be parsed"),
        )),
    }
}
