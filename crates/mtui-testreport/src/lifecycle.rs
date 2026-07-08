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
use mtui_types::enums::RequestKind;
use mtui_types::{UpdateID, Workflow};
use tracing::info;

use crate::checkout::{ReadOutcome, TokioSvnRunner};
use crate::reports::{NullReport, ObsReport, PiReport, SlReport};
use crate::testreport::{ReadError, TestReport};

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
/// 4. Sets the workflow from `kind` and records the deferred-autoconnect intent.
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

    if loaded {
        report.base_mut().workflow = kind.workflow();
        if autoconnect && kind.autoconnect_default() {
            // Defer the connect to the composition root, which wires the arbiter
            // first so refhosts_from_tp draws one host per slot.
            report.base_mut().autoconnect_pending = true;
        }
        // TODO(mtui-rs-m1w/zs4/plt): QEM Dashboard + auto-openQA enrichment and
        // its auto→manual fallback are deferred to their own beads.
        report
    } else {
        info!("TestReport isn't loaded");
        Box::new(NullReport::new(checkout_config))
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
