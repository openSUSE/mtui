//! The update-workflow export subsystem.
//!
//! A shared [`base`] with the common template-mutation helpers, three
//! concrete exporters ([`auto`], [`manual`], [`kernel`]), a log
//! [`downloader`], and the idempotent [`overview_inject`] block writer.
//!
//! The exporter is picked by [`Workflow`](mtui_types::Workflow) in the
//! composition root (`mtui-core`), which constructs the concrete type directly.
//! There is no boxed factory here: the constructors differ legitimately —
//! [`ManualExport`] needs the connected hosts, [`KernelExport`] the kernel
//! connectors — and one factory would flatten that.

pub mod auto;
pub mod base;
pub mod downloader;
pub mod kernel;
pub mod manual;
pub mod overview_inject;

pub use auto::AutoExport;
pub use base::{DenyOverwrite, ExportContext, OverwritePrompt};
pub use downloader::{BytesFetcher, DownloadError, ErrorMode, ResultsMissingError, download_logs};
pub use kernel::KernelExport;
pub use manual::{ManualExport, ManualHost};
pub use overview_inject::inject_overview;
