//! The native OBS/IBS review backend (direct OBS API, no `osc` subprocess).
//!
//! A native Rust OBS API client in place of the `osc qam` subprocess wrapper:
//! the transport foundation ([`client`], [`errors`]), the oscrc credential
//! reader ([`oscrc`]), the XML models ([`models`]), the assignment-inference
//! state machine ([`inference`]), the SSH-signature auth ([`auth`],
//! [`sshsig`]) and the QAM operations ([`qam`]).

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
