//! The update-workflow export subsystem.
//!
//! A shared [`base`] with the install-log writer, three concrete exporters
//! ([`auto`], [`manual`], [`kernel`]) that write install logs, and a log
//! [`downloader`].
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

pub use auto::AutoExport;
pub use base::{DenyOverwrite, ExportContext, OverwritePrompt};
pub use downloader::{BytesFetcher, DownloadError, ErrorMode, ResultsMissingError, download_logs};
pub use kernel::KernelExport;
pub use manual::{ManualExport, ManualHost};
