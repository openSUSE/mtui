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
//! The registry + line-dispatch engine (P5.2), the wider display surface (P5.3),
//! the argparse↔clap fidelity layer (P5.4), and the wiring composition root
//! (P5.5) build on top of these.

pub mod command;
pub mod display;
pub mod error;
pub mod session;
pub mod template_registry;

pub use command::{Command, Scope};
pub use display::{ColorMode, CommandPromptDisplay};
pub use error::{CommandError, CommandResult};
pub use session::Session;
pub use template_registry::TemplateRegistry;
