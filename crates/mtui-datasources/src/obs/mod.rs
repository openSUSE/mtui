//! The native OBS/IBS review backend (direct OBS API, no `osc` subprocess).
//!
//! Ported from upstream `mtui/data_sources/obs/`. This backend replaces the
//! `osc qam` subprocess wrapper ([`crate::oscqam`]) with a native Rust OBS API
//! client. The transport foundation ([`client`], [`errors`]), the native oscrc
//! credential reader ([`oscrc`]) and the XML models ([`models`]) have landed;
//! later subtasks add the SSH-signature signer, the assignment-inference state
//! machine, and the five QAM operations.

pub mod client;
pub mod errors;
pub mod models;
pub mod oscrc;

pub use client::{NoAuth, ObsAuth, ObsClient, error_summary};
pub use errors::ObsError;
pub use models::{
    HistoryEvent, REJECT_REASON_NAME, REJECT_REASON_NAMESPACE, Request, Review,
    build_reject_reason_body, is_qam_group, parse_group_directory, parse_reject_reason_values,
    parse_request, parse_request_collection,
};
pub use oscrc::{ObsCredentials, read_credentials};
