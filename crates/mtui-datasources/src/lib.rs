//! `mtui-datasources` — shared HTTP client, refhosts, openQA/QEM/Gitea/osc-qam.
//!
//! Every outbound integration lives here; consumers (commands, MCP) get typed
//! clients. The first landed surface is the shared HTTP policy layer
//! ([`mod@http`]) ported from upstream `mtui/support/http.py`: one client with a
//! unified timeout and TLS-verify posture that every later Phase-3 client
//! builds on.

pub mod error;
pub mod http;
pub mod refhost;

pub use error::{HttpError, RefhostError, Result};
pub use http::{
    HTTP_TIMEOUT, HttpClient, VerifyPolicy, default_pool_size, disable_insecure_warnings,
    is_ssl_verification_error, resolve_verify, ssl_verification_hint, system_ca_bundle,
};
pub use refhost::{Attributes, ProductDiff, Refhosts};
