//! Output capture seam for MCP tool dispatch.
//!
//! Commands write human-readable output to [`Session`]'s
//! [`CommandPromptDisplay`]. The REPL points that at stdout; an MCP tool must
//! instead *capture* it so it can be returned as the tool result. This module
//! provides a shared in-memory sink and a [`session`] constructor that wires it
//! in via the public [`Session::with_display`] seam.

use std::io::Write;
use std::sync::{Arc, Mutex};

use mtui_config::Config;
use mtui_core::{ColorMode, CommandPromptDisplay, Session};

/// A cloneable handle to a command's captured output.
///
/// Backed by an `Arc<Mutex<Vec<u8>>>` shared with the [`Session`]'s display
/// sink. [`take`](SharedBuf::take) atomically reads and clears it, which is how
/// each `call_tool` isolates its own output.
#[derive(Clone, Default)]
pub struct SharedBuf(Arc<Mutex<Vec<u8>>>);

impl SharedBuf {
    /// Reads the buffered output as a UTF-8 string and clears the buffer.
    ///
    /// Invalid UTF-8 is lossily converted (display output is text, so this is a
    /// defensive fallback rather than an expected path).
    #[must_use]
    pub fn take(&self) -> String {
        let mut guard = self.0.lock().expect("capture buffer poisoned");
        let bytes = std::mem::take(&mut *guard);
        String::from_utf8_lossy(&bytes).into_owned()
    }
}

impl Write for SharedBuf {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        self.0
            .lock()
            .expect("capture buffer poisoned")
            .extend_from_slice(data);
        Ok(data.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Builds a headless [`Session`] whose display is captured into a [`SharedBuf`].
///
/// `is_repl` is `false`: this is a non-interactive (MCP) session. Color is
/// disabled so captured text is plain (an LLM client renders the raw string).
#[must_use]
pub fn session(config: Config) -> (Session, SharedBuf) {
    let buf = SharedBuf::default();
    let display = CommandPromptDisplay::with_sink(Box::new(buf.clone()), ColorMode::Never);
    (Session::with_display(config, false, display), buf)
}
