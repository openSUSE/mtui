//! Concrete [`TestReport`](crate::TestReport) implementations.
//!
//! Phase 4.1 ships only the null object ([`NullReport`]). The real reports
//! (SL/PI/OBS) arrive in the tasks that depend on this skeleton.

pub mod null;

pub use null::NullReport;
