//! `mtui-testreport` — TestReport lifecycle, metadata parsers, update workflow.
//!
//! Lands the [`TestReport`] trait, the shared-state [`TestReportBase`] carrier,
//! the [`NullReport`] null object, and the SUSE Linux report [`SlReport`] (with
//! its [`repoparse`](reports::repoparse) helpers), plus the metadata parsers and
//! product-normalization tables. The remaining concrete reports, checkout
//! backends, and update workflow arrive in the later Phase 4 tasks.

pub mod metadata_parsers;
pub mod products;
pub mod reports;
pub mod testreport;

pub use metadata_parsers::{JSONParser, MetadataEnvelope, ReducedMetadataParser, patchinfo_titles};
pub use products::{normalize, normalize_16};
pub use reports::repoparse::{gitrepoparse, parse_product, reporepoparse, slrepoparse};
pub use reports::{NullReport, SlReport};
pub use testreport::{TestReport, TestReportBase};
