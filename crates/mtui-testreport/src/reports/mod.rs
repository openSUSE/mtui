//! Concrete [`TestReport`](crate::TestReport) implementations.
//!
//! Ships the null object ([`NullReport`]), the SUSE Linux report ([`SlReport`]),
//! and the PI report ([`PiReport`]). The remaining report (OBS) arrives when a
//! consumer needs it. The [`repoparse`] helpers derive each report's update-repo
//! map.

pub mod null;
pub mod pi;
pub mod repoparse;
pub mod sl;

pub use null::NullReport;
pub use pi::PiReport;
pub use sl::SlReport;
