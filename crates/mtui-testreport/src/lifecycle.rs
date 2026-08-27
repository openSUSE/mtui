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

use crate::checkout::{ReadOutcome, TokioSvnRunner};
use crate::reports::{NullReport, ObsReport, PiReport, SlReport};
use crate::testreport::{HashCheck, ReadError, TestReport};

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

    // Inlined rather than routed through `checkout_and_read`: the `read` step
    // must mutate `report`, which clashes with the borrows the closures need.
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
/// answer.
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

    // Manual fallback.
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

/// Regenerates a stale template via TeReGen, then re-checks-out and re-reads
/// it.
///
/// Returns the freshly loaded, hash-verified report on success, or `None` so the
/// caller falls back to the manual force/decline handling. Any TeReGen failure,
/// checkout/read failure, or a *still*-failing hash on the fresh template is a
/// `None` (logged as "Reload after regeneration failed").
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

    // A still-failing hash on the fresh template is a reload failure.
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
