//! `mtui-mcp` library — MCP server internals synthesised from the command
//! registry.
//!
//! The `mtui-mcp` binary is a thin runner over this crate. The server modules
//! ([`capture`], [`session`], [`provider`], [`server`]) are gated behind the
//! `mcp` feature so the default build (and the `mtui` REPL, which never depends
//! on this crate's server) does not pull in the rmcp SDK.

#[cfg(feature = "mcp")]
pub mod argv;
#[cfg(feature = "mcp")]
pub mod capture;
#[cfg(feature = "mcp")]
pub mod provider;
#[cfg(feature = "mcp")]
pub mod schema;
#[cfg(feature = "mcp")]
pub mod server;
#[cfg(feature = "mcp")]
pub mod session;
#[cfg(feature = "mcp")]
pub mod slim;

#[cfg(feature = "mcp")]
pub use provider::{SessionProvider, StdioProvider};
#[cfg(feature = "mcp")]
pub use session::{McpCommandError, McpSession};
