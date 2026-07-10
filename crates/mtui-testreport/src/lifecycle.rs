//! The test-report construction lifecycle (`make_testreport`).
//!
//! Ports the report-construction slice of upstream `mtui/types/updateid.py`:
//! `UpdateID.tr_factory` (report class by RRID kind), `_checkout` (checkout +
//! read), and `AutoOBSUpdateID.make_testreport` / `KernelOBSUpdateID.make_testreport`
//! (workflow selection + deferred-autoconnect flag).
//!
//! The Rust split keeps this crate free of the host-connect and openQA layers:
//! * The actual host connect is **deferred to the caller** (the composition
//!   root, `mtui-core::Session::load_update`), which owns the arbiter wiring and
//!   the refhosts-from-testplatform resolution. `make_testreport` only records
//!   the intent via [`TestReportBase::autoconnect_pending`].
//! * The QEM Dashboard / auto-openQA enrichment upstream runs inside
//!   `make_testreport` (`QEMIncident`, `DashboardAutoOpenQA`, and the
//!   auto→manual fallback it drives) is a documented **no-op stub** here; it
//!   lands with its own beads (`mtui-rs-m1w` export enrichment, `mtui-rs-zs4`
//!   reload_openqa, `mtui-rs-plt` set_workflow). See [`ReportKind::autoconnect_default`].

use mtui_config::options::Config;
use mtui_datasources::TeReGen;
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
    pub fn workflow(self) -> Workflow {
        match self {
            Self::Auto => Workflow::Auto,
            Self::Kernel => Workflow::Kernel,
        }
    }

    /// Whether `make_testreport` defaults to autoconnect for this kind.
    ///
    /// Mirrors the upstream `make_testreport` signatures: `AutoOBSUpdateID`
    /// defaults `autoconnect=True`, `KernelOBSUpdateID` defaults
    /// `autoconnect=False`. The explicit `autoconnect` argument to
    /// [`make_testreport`] still overrides this (e.g. non-interactive startup
    /// with `--sut` suppresses it).
    #[must_use]
    pub fn autoconnect_default(self) -> bool {
        matches!(self, Self::Auto)
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
/// 5. Sets the workflow from `kind` and records the deferred-autoconnect intent.
///
/// `autoconnect` is the caller's explicit choice (upstream's `make_testreport`
/// argument); when `true` **and** the update kind autoconnects by default
/// ([`UpdateKind::autoconnect_default`]), [`TestReportBase::autoconnect_pending`]
/// is set so the composition root connects the report's reference hosts *after*
/// wiring the host arbiter.
///
/// The QEM/auto-openQA enrichment and its auto→manual fallback are a documented
/// no-op here (see the module docs); it lands with `mtui-rs-m1w`/`zs4`/`plt`.
pub async fn make_testreport(
    update: &UpdateID,
    config: Config,
    kind: UpdateKind,
    autoconnect: bool,
    interactive: bool,
    prompter: Option<&Prompter>,
) -> Box<dyn TestReport + Send + Sync> {
    let template_dir = config.template_dir.clone();
    let svn_path = config.svn_path.clone();
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
    let loaded = match to_outcome(report.read(&trpath)) {
        ReadOutcome::Ok => true,
        ReadOutcome::Io(e) if !e.is_not_found() => {
            // A non-ENOENT read error is not a "needs checkout" signal.
            info!("{e}");
            false
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
                Ok(()) => matches!(to_outcome(report.read(&trpath)), ReadOutcome::Ok),
                Err(e) => {
                    info!("{e}");
                    false
                }
            }
        }
    };

    if !loaded {
        info!("TestReport isn't loaded");
        return Box::new(NullReport::new(checkout_config));
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
            error!(
                "Gitea API token is not configured. Pass -g/--gitea_token, \
                 set GITEA_TOKEN in your environment, or add a [gitea] token \
                 entry to ~/.mtuirc."
            );
            return Box::new(NullReport::new(checkout_config));
        }
        HashCheck::Failed(e) => {
            // Upstream `FailedGiteaCallError`.
            error!("Gitea API call failed");
            info!(error = %e, "TestReport isn't loaded");
            return Box::new(NullReport::new(checkout_config));
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
                interactive,
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
                None => return Box::new(NullReport::new(checkout_config)),
            }
        }
    }

    report.base_mut().workflow = kind.workflow();
    if autoconnect && kind.autoconnect_default() {
        // Defer the connect to the composition root, which wires the arbiter
        // first so refhosts_from_tp draws one host per slot.
        report.base_mut().autoconnect_pending = true;
    }
    // TODO(mtui-rs-m1w/zs4/plt): QEM Dashboard + auto-openQA enrichment and
    // its auto→manual fallback are deferred to their own beads.
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
    interactive: bool,
    prompter: Option<&Prompter>,
) -> Option<Option<Box<dyn TestReport + Send + Sync>>> {
    let rrid = update.id.clone();
    error!("Invalid Gitea hash");
    warn!("TestReport hash differs from the Gitea PR; the template is stale");

    // "Regenerate the template now via TeReGen? [y/N]" (default no).
    let regenerate = match (interactive, prompter) {
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
    let force_continue = match (interactive, prompter) {
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
    let delete = match (interactive, prompter) {
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
        let _ = std::fs::remove_dir_all(rrid_dir);
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
        let _ = std::fs::remove_dir_all(rrid_dir);
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
