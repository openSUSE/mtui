//! `mtui-testreport` — TestReport lifecycle, document ingest, update workflow.
//!
//! Provides the [`TestReport`] trait, the shared-state [`TestReportBase`]
//! carrier, the [`NullReport`] null object, and the concrete reports (SL, PI,
//! OBS — with the [`repoparse`](reports::repoparse) helpers), plus the
//! report-document ingest, product-normalization tables, and update workflow.

pub mod authoring;
pub mod commit_upload;
pub mod document_refresh;
pub mod export;
pub mod export_authoring;
pub mod ingest;
pub mod lifecycle;
pub mod products;
pub mod reports;
pub mod support;
pub mod testreport;
pub mod update_workflow;

pub use authoring::author_document;
pub use authoring::auto::openqa_install_from_auto;
pub use authoring::kernel::regression_from_kernel;
pub use authoring::manual::install_from_hosts;
pub use authoring::overview::openqa_extra_from_overview;
pub use commit_upload::{
    CollectError, Collected, CommitReport, CommitUploadError, collect_artifacts, upload_current,
};
pub use document_refresh::{RefreshError, Refreshed, refresh_document};
pub use export::{
    AutoExport, BytesFetcher, DenyOverwrite, DownloadError, ErrorMode, ExportContext, KernelExport,
    ManualExport, ManualHost, OverwritePrompt, ResultsMissingError, download_logs, inject_overview,
};
pub use export_authoring::author_export;
pub use ingest::apply_document;
pub use lifecycle::{UpdateKind, make_testreport};
pub use products::{normalize, normalize_16};
pub use reports::repoparse::{
    ProductParseError, gitrepoparse, parse_product, reporepoparse, slrepoparse,
};
pub use reports::{NullReport, ObsReport, PiReport, SlReport};
pub use support::{FileList, atomic_write_file, detect_system, system_info};
pub use testreport::{HashCheck, ReportOpenQA, SlackReviewMarker, TestReport, TestReportBase};
pub use update_workflow::{Diagnostic, UpdateError};
