//! `mtui-mcp` library — MCP server internals synthesised from the command
//! registry.
//!
//! The `mtui-mcp` binary is a thin runner over this crate ([`run`]). The server
//! modules ([`capture`], [`session`], [`provider`], [`server`], …) are gated
//! behind the `mcp` feature so the default build (and the `mtui` REPL, which
//! never depends on this crate's server) does not pull in the rmcp SDK.

#[cfg(feature = "mcp")]
pub mod args;
#[cfg(feature = "mcp")]
pub mod argv;
#[cfg(feature = "mcp")]
pub mod capture;
#[cfg(feature = "mcp")]
pub mod deny;
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
pub mod tools;

#[cfg(feature = "mcp")]
pub use args::{McpArgs, Transport};
#[cfg(feature = "mcp")]
pub use provider::{SessionProvider, SessionRegistry, StdioProvider};
#[cfg(feature = "mcp")]
pub use server::{McpServer, SpikeServer};
#[cfg(feature = "mcp")]
pub use session::{McpCommandError, McpSession};
#[cfg(feature = "mcp")]
pub use tools::{
    ToolDescriptor, ToolRoute, build_tools, dispatch_job_tool, dispatch_tool, job_tool_descriptors,
    tool_routes,
};

#[cfg(feature = "mcp")]
mod runner;
#[cfg(feature = "mcp")]
pub use runner::run;
