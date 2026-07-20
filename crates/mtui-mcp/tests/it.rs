//! Consolidated integration-test entry point.
//!
//! Every integration test in this crate is compiled into this single binary
//! (see `autotests = false` + `[[test]] name = "it"` in Cargo.toml) so the
//! crate + its heavy deps (rmcp/axum under `--all-features`) link once, not once
//! per file. Add new integration tests as a module here, not as a new top-level
//! `tests/*.rs`. Each module keeps its own `#![cfg(feature = "mcp")]` gate.

#[path = "http_body_limit.rs"]
mod http_body_limit;
#[path = "http_isolation.rs"]
mod http_isolation;
#[path = "mcp_jobs.rs"]
mod mcp_jobs;
#[path = "mcp_version.rs"]
mod mcp_version;
#[path = "session_close.rs"]
mod session_close;
#[path = "session_concurrency.rs"]
mod session_concurrency;
#[path = "session_registry.rs"]
mod session_registry;
#[path = "slim_profile.rs"]
mod slim_profile;
#[path = "stdio_roundtrip.rs"]
mod stdio_roundtrip;
#[path = "testreport_tools.rs"]
mod testreport_tools;
#[path = "tools_synthesis.rs"]
mod tools_synthesis;
