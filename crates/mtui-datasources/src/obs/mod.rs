//! The native OBS/IBS review backend (direct OBS API, no `osc` subprocess).
//!
//! Ported from upstream `mtui/data_sources/obs/`. This backend replaces the
//! `osc qam` subprocess wrapper ([`crate::oscqam`]) with a native Rust OBS API
//! client. The transport foundation ([`client`], [`errors`]), the native oscrc
//! credential reader ([`oscrc`]), the XML models ([`models`]), the
//! assignment-inference state machine ([`inference`]) and the SSH-signature
//! auth ([`auth`], [`sshsig`]) have landed; later subtasks add the five QAM
//! operations.

pub mod auth;
pub mod client;
pub mod errors;
pub mod facade;
pub mod inference;
pub mod models;
pub mod oscrc;
pub(crate) mod preconditions;
pub mod qam;
pub mod sshsig;

pub use auth::{AgentKeys, ObsSignatureAuth};
pub use client::{NoAuth, ObsAuth, ObsClient};
pub use errors::ObsError;
pub use facade::Osc;
pub use oscrc::{ObsCredentials, read_credentials};
