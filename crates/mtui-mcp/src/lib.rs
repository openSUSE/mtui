//! `mtui-mcp` library — MCP server internals synthesised from the command
//! registry.
//!
//! The `mtui-mcp` binary is a thin runner over this crate ([`run`]). The server
//! modules ([`capture`], [`session`], [`provider`], [`server`], …) are gated
//! behind the `mcp` feature so the default build (and the `mtui` REPL, which
//! never depends on this crate's server) does not pull in the rmcp SDK.

// `args` is ungated: it holds only the process-arg parser (`McpArgs`/`Transport`)
// and depends on `clap`/`mtui-config`/`mtui-core::ColorArg` — no rmcp/server code.
// Keeping it out of the `mcp` gate lets the `xtask` completion/man generator reach
// `McpArgs::command()` without dragging the MCP server graph into that build.
pub mod args;
#[cfg(feature = "mcp")]
pub(crate) mod argv;
#[cfg(feature = "mcp")]
pub mod capture;
#[cfg(feature = "mcp")]
pub mod concurrency;
#[cfg(feature = "mcp")]
pub mod deny;
#[cfg(feature = "mcp")]
pub mod profiles;
#[cfg(feature = "mcp")]
pub mod provider;
#[cfg(feature = "mcp")]
pub(crate) mod schema;
#[cfg(feature = "mcp")]
pub mod server;
#[cfg(feature = "mcp")]
pub mod session;
#[cfg(feature = "mcp")]
pub mod slim;
#[cfg(feature = "mcp")]
pub mod testreport_tools;
#[cfg(feature = "mcp")]
pub mod tools;

pub use args::{McpArgs, Transport};
#[cfg(feature = "mcp")]
pub use profiles::{CORE, resolve_keep_set};
#[cfg(feature = "mcp")]
pub use provider::{SessionProvider, SessionRegistry, StdioProvider};
#[cfg(feature = "mcp")]
pub use server::McpServer;
#[cfg(feature = "mcp")]
pub use session::{JobState, JobView, McpCommandError, McpSession};
#[cfg(feature = "mcp")]
pub use slim::slim_input_schema;
#[cfg(feature = "mcp")]
pub use testreport_tools::{dispatch_testreport_tool, testreport_tool_descriptors};
#[cfg(feature = "mcp")]
pub use tools::{ToolDescriptor, ToolRoute, build_tools, job_tool_descriptors};

#[cfg(feature = "mcp")]
mod runner;
#[cfg(feature = "mcp")]
pub use runner::run;
