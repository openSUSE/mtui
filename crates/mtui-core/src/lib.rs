//! `mtui-core` — the command engine and composition root.
//!
//! This crate wires the lower crates together behind a uniform command surface
//! consumed by both the REPL and MCP:
//!
//! * [`Command`] — the trait every command implements, with the template
//!   fan-out engine ([`Command::run`]) and its [`Scope`] policy.
//! * [`CommandError`] / [`CommandResult`] — the command-layer error hierarchy.
//! * [`Session`] — the explicitly-passed command state, holding the
//!   [`TemplateRegistry`] and [`CommandPromptDisplay`].
//! * The explicit command [`Registry`] and the line-dispatch [`engine`], the
//!   single machinery both the REPL and MCP dispatch through.
//! * [`args`] — the top-level process argument parser (`clap`), distinct from
//!   the per-command parsers the engine synthesises.
//! * [`log_filter`] — the `tracing` filter policy (the HTTP-transport carve-out
//!   and how it composes with `RUST_LOG`), shared by both entrypoints because
//!   neither can see the other.
//! * The [`display`] surface: the `list_*` family, `show_log`, the three-way
//!   [`ColorMode`], and the `page` pager.
//! * [`entrypoint`] — the process [`ExitStatus`] contract. Both `mtui`'s REPL
//!   and `mtui-mcp` exit through it; neither has a headless single-command CLI
//!   mode.

pub mod args;
pub mod command;
pub mod commands;
pub mod display;
pub mod engine;
pub mod entrypoint;
pub mod error;
pub mod log_filter;
pub mod registry;
pub mod session;
pub mod template_registry;

pub use args::{Args, ColorArg, Sut, Update};
pub use command::{Command, Scope, resolve_command_rrids};
pub use display::{ColorMode, CommandPromptDisplay};
pub use engine::{EngineError, command_parser, dispatch_argv, dispatch_command, dispatch_line};
pub use entrypoint::ExitStatus;
pub use error::{CommandError, CommandResult};
pub use log_filter::{
    LogDirectives, TRANSPORT_DEBUG_NOTICE, TRANSPORT_LOG_CARVE_OUT, TRANSPORT_LOG_TARGETS,
    resolve_log_directives, resolve_log_directives_from,
};
pub use registry::{MCP_DENYLIST, Registry, register_all};
pub use session::{Activation, HOST_CLOSE_TIMEOUT, LogLevel, LogLevelSink, NotifySink, Session};
pub use template_registry::{ReportEntry, TemplateRegistry};
