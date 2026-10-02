//! The report-document editor: a schema walker, a form model over a
//! [`ReportDocument`](mtui_types::report_document::ReportDocument) and the
//! full-screen application that drives it.
//!
//! Nothing here touches the terminal directly; the caller owns raw mode and the
//! event loop.

pub mod form;
pub mod policy;
pub mod schema;

pub use form::{Field, FieldError, Form, Saved, Tab, Widget};
pub use schema::{Schema, SchemaError};
