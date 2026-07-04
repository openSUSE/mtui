//! `mtui-types` — domain types and the error hierarchy for mtui-rs.
//!
//! Foundation crate: no I/O, no async. Real types land in Phase 1.

pub mod enums;
pub mod error;
pub mod hostlog;
pub mod package;
pub mod product;
pub mod refhost;
pub mod rpmver;
pub mod rrid;
pub mod system;
pub mod updateid;
pub mod version;

pub use enums::{ExecutionMode, RequestKind, TargetState, Workflow};
pub use error::{
    Error, RefhostsParseError, RequestKindParseError, Result, RpmVersionParseError, RridParseError,
};
pub use product::{Addon, Host, Product};
pub use refhost::load_refhosts;
pub use rpmver::RPMVersion;
pub use rrid::RequestReviewID;
pub use updateid::UpdateID;
pub use version::Version;
