//! `mtui-core` — the command engine and composition root.
//!
//! This crate wires the lower crates together behind a uniform command surface
//! consumed by both the REPL (Phase 6) and MCP (Phase 7). Phase 5.1 lands the
//! foundation:
//!
//! * [`Command`] — the trait every command implements, with the template
//!   fan-out engine ([`Command::run`]) and its [`Scope`] policy.
//! * [`CommandError`] / [`CommandResult`] — the command-layer error hierarchy.
//! * [`Session`] — the explicitly-passed command state (the Rust replacement for
//!   upstream's `CommandPrompt`), holding the [`TemplateRegistry`] and
//!   [`CommandPromptDisplay`].
//!
//! P5.2 adds the explicit command [`Registry`] and the line-dispatch
//! [`engine`], the single machinery both the REPL and MCP dispatch through. P5.4
//! adds [`args`] — the top-level process argument parser (`clap`) that mirrors
//! upstream `mtui.cli.args`, distinct from the per-command parsers the engine
//! synthesises. The wider display surface (P5.3) and the wiring composition root
//! (P5.5) build on top of these.

pub mod args;
pub mod command;
pub mod display;
pub mod engine;
pub mod error;
pub mod registry;
pub mod session;
pub mod template_registry;

pub use args::{Args, ColorArg, Sut, Update};
pub use command::{Command, Scope};
pub use display::{ColorMode, CommandPromptDisplay};
pub use engine::{EngineError, dispatch_argv, dispatch_line};
pub use error::{CommandError, CommandResult};
pub use registry::{Registry, register_all};
pub use session::Session;
pub use template_registry::TemplateRegistry;
