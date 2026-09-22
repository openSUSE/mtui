//! `mtui-testreport` — TestReport lifecycle, metadata parsers, update workflow.
//!
//! Provides the [`TestReport`] trait, the shared-state [`TestReportBase`]
//! carrier, the [`NullReport`] null object, and the concrete reports (SL, PI,
//! OBS — with the [`repoparse`](reports::repoparse) helpers), plus the
//! metadata parsers, product-normalization tables, checkout backends, and
//! update workflow.

#[cfg(feature = "api-ingest")]
pub mod authoring;
pub mod checkout;
pub mod export;
pub mod export_authoring;
#[cfg(feature = "api-ingest")]
pub mod ingest;
pub mod lifecycle;
pub mod metadata_parsers;
pub mod products;
pub mod reports;
pub mod support;
pub mod testreport;
pub mod update_workflow;

#[cfg(feature = "api-ingest")]
pub use authoring::author_document;
#[cfg(feature = "api-ingest")]
pub use authoring::auto::openqa_install_from_auto;
#[cfg(feature = "api-ingest")]
pub use authoring::kernel::regression_from_kernel;
#[cfg(feature = "api-ingest")]
pub use authoring::manual::install_from_hosts;
#[cfg(feature = "api-ingest")]
pub use authoring::overview::openqa_extra_from_overview;
pub use checkout::{
    CheckoutError, CheckoutRunError, ReadOutcome, SvnOutcome, SvnRunner, TemplateIoError,
    TestReportNotLoaded, TokioSvnRunner, checkout_and_read, svn_commit_testreport,
    testreport_svn_checkout,
};
pub use export::{
    AutoExport, BytesFetcher, DenyOverwrite, DownloadError, ErrorMode, ExportContext, KernelExport,
    ManualExport, ManualHost, OverwritePrompt, ResultsMissingError, download_logs, inject_overview,
};
pub use export_authoring::author_export;
#[cfg(feature = "api-ingest")]
pub use ingest::apply_document;
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
