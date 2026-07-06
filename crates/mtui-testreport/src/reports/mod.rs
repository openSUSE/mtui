//! Concrete [`TestReport`](crate::TestReport) implementations.
//!
//! Ships the null object ([`NullReport`]), the SUSE Linux report ([`SlReport`]),
//! the PI report ([`PiReport`]), and the OBS report ([`ObsReport`]). The
//! [`repoparse`] helpers derive each report's update-repo map.

pub mod null;
pub mod obs;
pub mod pi;
pub mod repoparse;
pub mod sl;

pub use null::NullReport;
pub use obs::ObsReport;
pub use pi::PiReport;
pub use sl::SlReport;
