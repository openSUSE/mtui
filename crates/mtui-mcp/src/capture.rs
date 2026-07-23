//! Output capture seam for MCP tool dispatch.
//!
//! Commands write human-readable output to [`Session`]'s
//! [`CommandPromptDisplay`]. The REPL points that at stdout; an MCP tool must
//! instead *capture* it so it can be returned as the tool result. This module
//! provides a shared in-memory sink and a [`session`] constructor that wires it
//! in via the public [`Session::with_display`] seam.
//!
//! ## Write-time cap (bead `mtui-rs-th4o.8`)
//!
//! The sink is **bounded**: it accepts at most `limit` bytes and *discards* the
//! overflow at write time, recording the dropped-byte count. A command that
//! emits gigabytes (a huge fan-out `run` log) therefore never buffers more than
//! `limit` bytes of it in memory — the cap applies before allocation, not after.
//! [`SharedBuf::take_with_dropped`] returns the captured bytes plus the overflow
//! count so [`crate::session::McpSession::run_command`] can append the same
//! truncation notice [`crate::slim::cap_output`] would (exactly once, with a
//! correct count). `limit == 0` disables the cap (the buffer is unbounded, the
//! prior behaviour).

use std::io::Write;
use std::sync::{Arc, Mutex};

use mtui_config::Config;
use mtui_core::{ColorMode, CommandPromptDisplay, Session};

/// The shared, bounded capture state behind a [`SharedBuf`].
#[derive(Default)]
struct Inner {
    /// The captured bytes, held to at most `limit` bytes.
    bytes: Vec<u8>,
    /// Byte budget; `0` means unbounded (never discard).
    limit: usize,
    /// Total bytes discarded because they exceeded `limit` since the last
    /// [`take_with_dropped`](SharedBuf::take_with_dropped).
    dropped: usize,
}

/// A cloneable handle to a command's captured output.
///
/// Backed by an `Arc<Mutex<Inner>>` shared with the [`Session`]'s display sink.
/// [`take`](SharedBuf::take) / [`take_with_dropped`](SharedBuf::take_with_dropped)
/// atomically read and clear it, which is how each `call_tool` isolates its own
/// output. Writes beyond `limit` are discarded and counted (see the module docs).
#[derive(Clone, Default)]
pub struct SharedBuf(Arc<Mutex<Inner>>);

impl SharedBuf {
    /// Builds a sink bounded to `limit` bytes (`0` = unbounded).
    #[must_use]
    pub(crate) fn with_limit(limit: usize) -> Self {
        Self(Arc::new(Mutex::new(Inner {
            bytes: Vec::new(),
            limit,
            dropped: 0,
        })))
    }

    /// Reads the buffered output as a UTF-8 string and clears the buffer,
    /// discarding the dropped-byte count.
    ///
    /// Invalid UTF-8 is lossily converted (display output is text, so this is a
    /// defensive fallback rather than an expected path).
    #[must_use]
    pub fn take(&self) -> String {
        self.take_with_dropped().0
    }

    /// Reads the buffered output plus the number of overflow bytes discarded
    /// since the last take, then clears both.
    ///
    /// `dropped == 0` means nothing was truncated at write time; a non-zero
    /// value is the budget overrun (mirrors `cap_output`'s `total − limit`
    /// accounting), independent of the small extra bytes a codepoint-boundary
    /// trim may shed.
    #[must_use]
    pub(crate) fn take_with_dropped(&self) -> (String, usize) {
        let mut guard = self.0.lock().expect("capture buffer poisoned");
        let bytes = std::mem::take(&mut guard.bytes);
        let dropped = std::mem::replace(&mut guard.dropped, 0);
        (String::from_utf8_lossy(&bytes).into_owned(), dropped)
    }
}

impl Write for SharedBuf {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        let mut guard = self.0.lock().expect("capture buffer poisoned");
        if guard.limit == 0 {
            guard.bytes.extend_from_slice(data);
            return Ok(data.len());
        }
        let remaining = guard.limit.saturating_sub(guard.bytes.len());
        if remaining == 0 {
            // Already at budget: discard the whole write, count it, but report
            // the bytes as consumed so the writer does not error/retry.
            guard.dropped += data.len();
            return Ok(data.len());
        }
        let take = remaining.min(data.len());
        guard.bytes.extend_from_slice(&data[..take]);
        guard.dropped += data.len() - take;
        Ok(data.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Builds a headless [`Session`] whose display is captured into a [`SharedBuf`]
/// bounded to `config.mcp_max_output_bytes`.
///
/// `is_repl` is `false`: this is a non-interactive (MCP) session. Color is
/// disabled so captured text is plain (an LLM client renders the raw string).
#[must_use]
pub(crate) fn session(config: Config) -> (Session, SharedBuf) {
    let buf = SharedBuf::with_limit(config.mcp_max_output_bytes);
    let display = CommandPromptDisplay::with_sink(Box::new(buf.clone()), ColorMode::Never);
    (Session::with_display(config, false, display), buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unbounded_captures_everything() {
        let mut buf = SharedBuf::with_limit(0);
        let payload = "x".repeat(10_000);
        buf.write_all(payload.as_bytes()).unwrap();
        let (text, dropped) = buf.take_with_dropped();
        assert_eq!(text, payload);
        assert_eq!(dropped, 0);
    }

    #[test]
    fn under_limit_is_byte_identical_and_drops_nothing() {
        let mut buf = SharedBuf::with_limit(100);
        buf.write_all(b"hello world").unwrap();
        let (text, dropped) = buf.take_with_dropped();
        assert_eq!(text, "hello world");
        assert_eq!(dropped, 0);
    }

    #[test]
    fn over_limit_stops_appending_and_counts_overflow() {
        let mut buf = SharedBuf::with_limit(4);
        // One write straddling the budget: 4 kept, 6 discarded.
        let n = buf.write(b"abcdefghij").unwrap();
        assert_eq!(
            n, 10,
            "reports full write consumed so the writer won't retry"
        );
        let (text, dropped) = buf.take_with_dropped();
        assert_eq!(text, "abcd", "head kept up to the budget");
        assert_eq!(dropped, 6, "budget overrun counted");
    }

    #[test]
    fn overflow_accumulates_across_writes() {
        let mut buf = SharedBuf::with_limit(4);
        buf.write_all(b"abc").unwrap(); // 3 kept
        buf.write_all(b"def").unwrap(); // 1 kept ("d"), 2 dropped
        buf.write_all(b"ghi").unwrap(); // 0 kept, 3 dropped
        let (text, dropped) = buf.take_with_dropped();
        assert_eq!(text, "abcd");
        assert_eq!(dropped, 5);
    }

    #[test]
    fn take_clears_state_for_the_next_call() {
        let mut buf = SharedBuf::with_limit(4);
        buf.write_all(b"abcdef").unwrap();
        let (_, first_dropped) = buf.take_with_dropped();
        assert_eq!(first_dropped, 2);
        // A fresh call starts clean: no residual bytes or dropped count.
        buf.write_all(b"xy").unwrap();
        let (text, dropped) = buf.take_with_dropped();
        assert_eq!(text, "xy");
        assert_eq!(dropped, 0);
    }
}
