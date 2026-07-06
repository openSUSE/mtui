//! The update-workflow export subsystem.
//!
//! Ports upstream `mtui.update_workflow.export`: a shared [`base`] with the
//! common template-mutation helpers, three concrete exporters
//! ([`auto`], [`manual`], [`kernel`]), a log [`downloader`], and the idempotent
//! [`overview_inject`] block writer.

pub mod overview_inject;

pub use overview_inject::inject_overview;
