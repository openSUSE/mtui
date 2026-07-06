//! The update-workflow export subsystem.
//!
//! Ports upstream `mtui.update_workflow.export`: a shared [`base`] with the
//! common template-mutation helpers, three concrete exporters
//! ([`auto`], [`manual`], [`kernel`]), a log [`downloader`], and the idempotent
//! [`overview_inject`] block writer.

pub mod base;
pub mod downloader;
pub mod overview_inject;

pub use base::{DenyOverwrite, ExportContext, Exporter, OverwritePrompt};
pub use downloader::{BytesFetcher, ErrorMode, ResultsMissingError, download_logs};
pub use overview_inject::inject_overview;
