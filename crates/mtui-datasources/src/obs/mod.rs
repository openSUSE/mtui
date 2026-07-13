//! The native OBS/IBS review backend (direct OBS API, no `osc` subprocess).
//!
//! Ported from upstream `mtui/data_sources/obs/`. This backend replaces the
//! `osc qam` subprocess wrapper ([`crate::oscqam`]) with a native Rust OBS API
//! client. The transport foundation ([`client`], [`errors`]) lands first; later
//! subtasks add the oscrc reader, the SSH-signature signer, the XML models, the
//! assignment-inference state machine, and the five QAM operations.

pub mod client;
pub mod errors;

pub use client::{NoAuth, ObsAuth, ObsClient, error_summary};
pub use errors::ObsError;
