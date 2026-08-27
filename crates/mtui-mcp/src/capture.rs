//! Output capture seam for MCP tool dispatch.
//!
//! Commands write human-readable output to [`Session`]'s
//! [`CommandPromptDisplay`]. The REPL points that at stdout; an MCP tool must
//! *capture* it to return it as the tool result, so this module provides a shared
//! in-memory sink and a `session` constructor wiring it in through the public
//! [`Session::with_display`] seam.
//!
//! ## Write-time cap
//!
//! The sink accepts at most `limit` bytes and *discards* the overflow at write
//! time, counting it: a command emitting gigabytes (a huge fan-out `run` log)
//! never buffers more than `limit`, because the cap applies before allocation.
//! `SharedBuf::take_with_dropped` hands back the bytes plus that count, so
//! [`crate::session::McpSession::run_command`] can append the same truncation
//! notice `crate::slim::cap_output` would, once and with a correct count.
//! `limit == 0` disables the cap.

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
/// [`take`](SharedBuf::take) / `take_with_dropped` atomically read and clear it,
/// which is how each `call_tool` isolates its own output.
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

    /// Reads the buffered output as a UTF-8 string (lossily, defensively) and
    /// clears the buffer, discarding the dropped-byte count.
    #[must_use]
    pub fn take(&self) -> String {
        self.take_with_dropped().0
    }

    /// Reads the buffered output plus the number of overflow bytes discarded
    /// since the last take, then clears both.
    ///
    /// A non-zero `dropped` is the budget overrun, mirroring `cap_output`'s
    /// `total − limit` accounting.
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
            // At budget: discard and count, but report the bytes as consumed so
            // the writer does not error or retry.
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
/// `is_repl` is `false` and color is disabled, so the captured text is the plain
/// string an LLM client renders.
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
        buf.write_all(b"xy").unwrap();
        let (text, dropped) = buf.take_with_dropped();
        assert_eq!(text, "xy");
        assert_eq!(dropped, 0);
    }
}
