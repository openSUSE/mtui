//! Concrete [`TestReport`](crate::TestReport) implementations.
//!
//! Ships the null object ([`NullReport`]) and the SUSE Linux report
//! ([`SlReport`]). The remaining reports (PI/OBS) arrive when a consumer needs
//! them. The [`repoparse`] helpers derive each report's update-repo map.

pub mod null;
pub mod repoparse;
pub mod sl;

pub use null::NullReport;
pub use sl::SlReport;
