//! `mtui-testreport` — TestReport lifecycle, metadata parsers, update workflow.
//!
//! Phase 4.1 lands the skeleton: the [`TestReport`] trait, the shared-state
//! [`TestReportBase`] carrier, and the [`NullReport`] null object. The concrete
//! reports, metadata parsers, checkout backends, and update workflow arrive in
//! the later Phase 4 tasks that depend on this skeleton.

pub mod metadata_parsers;
pub mod reports;
pub mod testreport;

pub use metadata_parsers::{JSONParser, MetadataEnvelope, ReducedMetadataParser, patchinfo_titles};
pub use reports::NullReport;
pub use testreport::{TestReport, TestReportBase};
