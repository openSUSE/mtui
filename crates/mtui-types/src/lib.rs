//! `mtui-types` — domain types and the error hierarchy for mtui-rs.
//!
//! Foundation crate: no I/O, no async. Real types land in Phase 1.

pub mod enums;
pub mod error;
pub mod product;
pub mod version;

pub use enums::{ExecutionMode, RequestKind, TargetState, Workflow};
pub use error::{Error, RequestKindParseError, Result, RridParseError};
pub use product::{Addon, Host, Product};
pub use version::Version;
