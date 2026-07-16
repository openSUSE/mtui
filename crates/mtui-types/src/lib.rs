//! `mtui-types` — domain types and the error hierarchy for mtui-rs.
//!
//! Foundation crate: no I/O, no async. Real types land in Phase 1.

pub mod enums;
pub mod error;
pub mod hostlog;
pub mod oqaresults;
pub mod package;
pub mod package_spec;
pub mod product;
pub mod refhost;
pub mod rpmver;
pub mod rrid;
pub mod shellquote;
pub mod system;
pub mod test;
pub mod updateid;
pub mod urls;
pub mod version;

pub use enums::{Assignment, ExecutionMode, RequestKind, TargetState, Workflow};
pub use error::{
    Error, PackageSpecParseError, RefhostsParseError, RequestKindParseError, Result,
    RpmVersionParseError, RridParseError,
};
pub use oqaresults::{OpenQAResult, OpenQAResults, OverviewResult};
pub use package_spec::PackageSpec;
pub use product::{Addon, Host, Product};
pub use refhost::load_refhosts;
pub use rpmver::RPMVersion;
pub use rrid::RequestReviewID;
pub use shellquote::quote_args;
pub use system::{System, SystemProduct, UnknownSystemError};
pub use test::Test;
pub use updateid::UpdateID;
pub use urls::URLs;
pub use version::{Version, VersionField};
