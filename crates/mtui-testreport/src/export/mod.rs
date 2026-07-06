//! The update-workflow export subsystem.
//!
//! Ports upstream `mtui.update_workflow.export`: a shared [`base`] with the
//! common template-mutation helpers, three concrete exporters
//! ([`auto`], [`manual`], [`kernel`]), a log [`downloader`], and the idempotent
//! [`overview_inject`] block writer.
//!
//! ## Exporter selection
//!
//! Upstream picks the exporter by [`Workflow`](mtui_types::Workflow)
//! (`AUTO`/`MANUAL`/`KERNEL` in `enums.py`). The three concrete types here have
//! deliberately different constructors — [`ManualExport`] needs the connected
//! hosts, [`KernelExport`] needs the kernel connectors — so a single boxed
//! factory would flatten inputs that legitimately differ. The composition root
//! (Phase 5) matches on `Workflow` and constructs the right exporter directly;
//! this module exposes the concrete types rather than prescribing that match.

pub mod auto;
pub mod base;
pub mod downloader;
pub mod kernel;
pub mod manual;
pub mod overview_inject;

pub use auto::AutoExport;
pub use base::{DenyOverwrite, ExportContext, Exporter, OverwritePrompt};
pub use downloader::{BytesFetcher, ErrorMode, ResultsMissingError, download_logs};
pub use kernel::KernelExport;
pub use manual::{ManualExport, ManualHost};
pub use overview_inject::inject_overview;
