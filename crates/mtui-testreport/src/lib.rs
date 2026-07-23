//! `mtui-testreport` — TestReport lifecycle, metadata parsers, update workflow.
//!
//! Lands the [`TestReport`] trait, the shared-state [`TestReportBase`] carrier,
//! the [`NullReport`] null object, and the SUSE Linux report [`SlReport`] (with
//! its [`repoparse`](reports::repoparse) helpers), plus the metadata parsers and
//! product-normalization tables. The remaining concrete reports, checkout
//! backends, and update workflow arrive in the later Phase 4 tasks.

pub mod checkout;
pub mod export;
pub mod lifecycle;
pub mod metadata_parsers;
pub mod products;
pub mod reports;
pub mod support;
pub mod testreport;
pub mod update_workflow;

pub use checkout::{
    CheckoutError, CheckoutRunError, ReadOutcome, SvnOutcome, SvnRunner, TemplateIoError,
    TestReportNotLoaded, TokioSvnRunner, checkout_and_read, svn_commit_testreport,
    testreport_svn_checkout,
};
pub use export::{
    AutoExport, BytesFetcher, DenyOverwrite, DownloadError, ErrorMode, ExportContext, KernelExport,
    ManualExport, ManualHost, OverwritePrompt, ResultsMissingError, download_logs, inject_overview,
};
pub use lifecycle::{UpdateKind, make_testreport};
pub use metadata_parsers::{JSONParser, ReducedMetadataParser, patchinfo_titles};
pub use products::{normalize, normalize_16};
pub use reports::repoparse::{
    ProductParseError, gitrepoparse, obsrepoparse, parse_product, reporepoparse, slrepoparse,
};
pub use reports::{NullReport, ObsReport, PiReport, SlReport};
pub use support::{FileList, atomic_write_file, detect_system, system_info};
pub use testreport::{
    HashCheck, ReadError, ReportOpenQA, ReviewerError, SlackReviewError, SlackReviewMarker,
    TestReport, TestReportBase,
};
pub use update_workflow::{Diagnostic, UpdateError};
