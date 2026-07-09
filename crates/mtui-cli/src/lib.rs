//! `mtui-cli` — the interactive REPL library behind the `mtui` binary.
//!
//! The binary ([`main.rs`](../main.rs)) is a thin shell: it parses the
//! top-level args, builds the [`Session`](mtui_core::Session) and command
//! [`Registry`](mtui_core::Registry), and drives [`Repl::run`]. Exposing the
//! REPL as a library lets the `tests/**` suite (and the P6.8 test task) exercise
//! the loop's [`step`](repl::step) seam without a TTY.

pub mod completer;
pub mod highlighter;
pub mod history;
pub mod prompt;
pub mod repl;

pub use completer::MtuiCompleter;
pub use highlighter::MtuiHighlighter;
pub use history::file_backed_history;
pub use prompt::MtuiPrompt;
pub use repl::{Repl, step};
