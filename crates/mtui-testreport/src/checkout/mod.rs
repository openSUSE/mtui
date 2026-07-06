//! Test-report checkout: SVN backend + the `UpdateID` checkout seam.
//!
//! Ports upstream `mtui/test_reports/svn_io.py` (the `svn` subprocess helpers
//! and their exceptions) and the checkout-orchestration slice of
//! `mtui/types/updateid.py` (`UpdateID._checkout`).
//!
//! Test reports live in **SVN** — this is the only checkout mechanism, shared by
//! OBS/IBS and SLFO incidents alike. Gitea (SLFO) and `osc qam` (OBS/IBS) are
//! review-workflow backends (assign/approve/reject/comment) and never check a
//! template out; see [`runner`] for why this crate does not reuse the `oscqam`
//! command runner.
//!
//! The checkout exceptions and their user-facing messages are colocated here
//! (rather than in `mtui-types`) to mirror upstream, where they live in
//! `svn_io.py` / `support/messages.py`, and to keep `mtui-types` thin.

pub mod runner;
pub mod svn;

use std::io;
use std::path::Path;

use thiserror::Error;
use tracing::error;

pub use runner::{SvnOutcome, SvnRunner, TokioSvnRunner};
pub use svn::{svn_commit_testreport, testreport_svn_checkout};

/// Errors raised while checking out or committing a test report.
///
/// Each `Display` string is reproduced byte-for-byte from upstream
/// `support/messages.py` — these are user-facing contracts.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum CheckoutError {
    /// The configured `template_dir` could not be created or used.
    ///
    /// Mirrors upstream `TemplateDirNotUsableError`.
    #[error(
        "Cannot create template directory {path}: {reason}\n\
         Please check the [mtui] template_dir option in your configuration."
    )]
    TemplateDirNotUsable {
        /// The offending `template_dir` path.
        path: String,
        /// Why it could not be used.
        reason: String,
    },

    /// The `svn co` was interrupted (e.g. Ctrl-C).
    ///
    /// Mirrors upstream `SvnCheckoutInterruptedError`, whose `{0!r}` renders the
    /// URI single-quoted.
    #[error("Svn checkout of '{uri}' interrupted")]
    SvnCheckoutInterrupted {
        /// The repository URI whose checkout was interrupted.
        uri: String,
    },

    /// The test report does not exist / `svn co` failed.
    ///
    /// Mirrors upstream `SvnCheckoutFailed`. The cryptic `svn` error code is
    /// intentionally **not** part of this message (it is logged at debug).
    #[error(
        "Test report for {rrid} does not exist.\nPlease check {report_url} for potential issues."
    )]
    SvnCheckoutFailed {
        /// The RRID whose report was not found.
        rrid: String,
        /// The `fancy_reports_url`-derived log URL to check.
        report_url: String,
    },
}

/// A recoverable I/O error reading a template, carrying its `errno`.
///
/// Mirrors upstream `TemplateIOError(IOError)`. The checkout seam branches on
/// [`is_not_found`](Self::is_not_found) (upstream `e.errno != ENOENT`) to decide
/// whether a missing template should trigger a fresh checkout.
#[derive(Debug, Error)]
#[error("{message}")]
pub struct TemplateIoError {
    /// The OS error number, when known.
    pub errno: Option<i32>,
    /// Whether the underlying error was a "not found" condition. Captured
    /// separately because a synthetic [`io::Error`] built from an
    /// [`io::ErrorKind`] carries no `raw_os_error`.
    not_found: bool,
    /// The human-readable message.
    pub message: String,
}

impl TemplateIoError {
    /// Builds a `TemplateIoError` from an [`io::Error`], preserving its `errno`.
    #[must_use]
    pub fn from_io(err: &io::Error) -> Self {
        Self {
            errno: err.raw_os_error(),
            // ENOENT (2) either as a real OS errno or as the `NotFound` kind, so
            // synthetic errors (tests, in-memory readers) map correctly too.
            not_found: err.raw_os_error() == Some(ENOENT) || err.kind() == io::ErrorKind::NotFound,
            message: err.to_string(),
        }
    }

    /// Whether this error is a "not found" (`ENOENT`) condition.
    ///
    /// A missing template on disk is the one case that triggers a fresh
    /// checkout; every other read error propagates unchanged (upstream
    /// `if e.errno != ENOENT: raise`).
    #[must_use]
    pub fn is_not_found(&self) -> bool {
        self.not_found
    }
}

/// `ENOENT` (2) — inlined so the seam reads clearly without a `libc` dep.
const ENOENT: i32 = 2;

/// Raised when the checkout seam could not load a test report.
///
/// Mirrors upstream `TestReportNotLoadedError`; its `Display` string is a
/// user-facing contract.
#[derive(Debug, Error)]
#[error("TestReport not loaded")]
pub struct TestReportNotLoaded;

/// Outcome of a template read attempt inside the checkout seam.
///
/// The seam is generic over how a report is read so it can be wired now, before
/// the `TestReport::read` lifecycle method lands (a later Phase 4 task). Upstream
/// `_checkout` calls `tr.read(trpath)`, catching `TemplateIOError` (missing
/// template → checkout) and letting Gitea errors from the retry propagate.
#[derive(Debug)]
pub enum ReadOutcome {
    /// The template was read successfully.
    Ok,
    /// The template was missing / unreadable, with the `errno`-bearing error.
    Io(TemplateIoError),
}

/// Runs the `UpdateID._checkout` orchestration.
///
/// Ports the inner block of upstream `UpdateID._checkout`:
///
/// 1. `read` the template; if it exists, done.
/// 2. On a **non-ENOENT** read error, propagate it unchanged.
/// 3. On ENOENT, run `checkout`; any [`CheckoutError`] is logged and mapped to
///    [`TestReportNotLoaded`] (matching upstream's `raise
///    TestReportNotLoadedError from e`).
/// 4. Retry `read` once the template is on disk; a residual read failure also
///    surfaces as [`TestReportNotLoaded`].
///
/// The Gitea-token / regeneration (`TeReGen`) outer handling and the
/// `make_testreport` workflow variants are deferred to their own tasks; this is
/// the checkout-and-map slice only.
///
/// # Errors
///
/// Returns [`TestReportNotLoaded`] when the report cannot be loaded after a
/// checkout attempt, or the original read error for a non-ENOENT failure.
pub async fn checkout_and_read<Read, Checkout>(
    trpath: &Path,
    mut read: Read,
    checkout: Checkout,
) -> Result<(), CheckoutRunError>
where
    Read: FnMut(&Path) -> ReadOutcome,
    Checkout: AsyncFnOnce() -> Result<(), CheckoutError>,
{
    match read(trpath) {
        ReadOutcome::Ok => Ok(()),
        ReadOutcome::Io(e) if !e.is_not_found() => {
            // A non-ENOENT read error is not a "needs checkout" signal; it
            // propagates unchanged (upstream `if e.errno != ENOENT: raise`).
            Err(CheckoutRunError::Read(e))
        }
        ReadOutcome::Io(_missing) => {
            if let Err(e) = checkout().await {
                error!("{e}");
                return Err(CheckoutRunError::NotLoaded(TestReportNotLoaded));
            }
            // Retry the read now that the template is on disk.
            match read(trpath) {
                ReadOutcome::Ok => Ok(()),
                ReadOutcome::Io(e) => {
                    error!("{e}");
                    Err(CheckoutRunError::NotLoaded(TestReportNotLoaded))
                }
            }
        }
    }
}

/// The error surface of [`checkout_and_read`].
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum CheckoutRunError {
    /// The report could not be loaded after a checkout attempt.
    #[error(transparent)]
    NotLoaded(#[from] TestReportNotLoaded),

    /// A non-ENOENT template read error propagated unchanged.
    #[error(transparent)]
    Read(#[from] TemplateIoError),
}
