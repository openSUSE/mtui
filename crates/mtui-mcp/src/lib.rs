//! `mtui-mcp` library — MCP server internals synthesised from the command
//! registry.
//!
//! The `mtui-mcp` binary is a thin runner over this crate. The P7.1 spike
//! modules ([`capture`], [`server`]) are gated behind the `mcp` feature so the
//! default build (and the `mtui` REPL, which never depends on this crate's
//! server) does not pull in the rmcp SDK.

#[cfg(feature = "mcp")]
pub mod capture;
#[cfg(feature = "mcp")]
pub mod server;
