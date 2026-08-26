//! `mtui-types` — domain types and the error hierarchy for mtui.
//!
//! Foundation crate: no I/O, no async.

pub mod enums;
pub mod error;
pub mod hostlog;
pub mod oqaresults;
pub mod package;
pub mod package_spec;
pub mod product;
pub mod refhost;
pub mod repo_url;
pub mod rpmver;
pub mod rrid;
pub mod shellquote;
pub mod system;
pub mod test;
pub mod update_source;
pub mod updateid;
pub mod urls;
pub mod version;

pub use enums::{Assignment, RequestKind, TargetState, Workflow};
pub use error::{
    Error, PackageSpecParseError, RefhostsParseError, RepoUrlParseError, RequestKindParseError,
    Result, RpmVersionParseError, RridParseError,
};
pub use oqaresults::{OpenQAResult, OpenQAResults, OverviewResult};
pub use package_spec::{PackageSpec, parse_rpm_filename};
pub use product::{Addon, Host, Product};
pub use refhost::load_refhosts;
pub use repo_url::RepoUrl;
pub use rpmver::RPMVersion;
pub use rrid::RequestReviewID;
pub use shellquote::quote_args;
pub use system::{System, SystemProduct, UnknownSystemError};
pub use test::Test;
pub use update_source::UpdateSource;
pub use updateid::UpdateID;
pub use urls::URLs;
pub use version::{Version, VersionField};
