//! The interactive PTY shell primitive (feature `shell`, P2.10).
//!
//! Opens a session channel, requests an `xterm` PTY, and invokes a login
//! shell, then bridges the local terminal to the channel with a raw-mode
//! `select()` loop.
//!
//! ## Scope split
//!
//! This module provides only the **transport primitive**: [`ShellChannel`], an
//! object-safe async duplex handle over the remote shell's PTY, returned by
//! [`Connection::shell`](super::Connection::shell). The raw-`termios`
//! stdin↔channel↔stdout bridge and the `shell` REPL command that *drive* this
//! handle are a terminal concern and live in the CLI crate (Phase 6) — a host
//! library has no business toggling the local TTY into raw mode.
//!
//! Keeping the loop out of the library also keeps it testable: the CLI bridge
//! runs against a scriptable in-memory [`ShellChannel`] (from
//! [`MockConnection`](super::MockConnection)) entirely offline, exactly as the
//! rest of the host layer is mocked.
//!
//! ## Terminal size
//!
//! [`ShellChannel::resize`] forwards an SSH `window-change` so the CLI *can*
//! propagate `SIGWINCH` if it chooses — a caller that only sets the size once
//! at spawn may simply never call it.

use async_trait::async_trait;

use crate::error::Result;

/// An open interactive shell channel to a single host: an object-safe async
/// duplex over the remote PTY.
///
/// Returned by [`Connection::shell`](super::Connection::shell). The consumer
/// (the Phase 6 CLI bridge) pumps bytes both ways:
///
/// * [`read`](Self::read) drains shell output (stdout/stderr merged onto the
///   PTY, as a real terminal sees it) into `buf`, returning the byte count;
///   `0` signals the remote shell exited (channel EOF/close) — the loop's
///   termination condition.
/// * [`write`](Self::write) sends local keystrokes to the shell.
/// * [`resize`](Self::resize) forwards a terminal size change.
/// * [`close`](Self::close) tears the channel down.
///
/// Object-safe by construction (`Box<dyn ShellChannel>`), so the russh-backed
/// channel and the test double are interchangeable.
#[async_trait]
pub trait ShellChannel: Send {
    /// Reads available shell output into `buf`, returning the number of bytes
    /// written to it.
    ///
    /// Returns `Ok(0)` when the remote shell has exited (channel EOF/close);
    /// the bridge loop treats that as its stop condition. A short read is
    /// normal and not an error.
    ///
    /// # Errors
    ///
    /// Returns [`HostError::Transport`](crate::HostError::Transport) if the
    /// channel fails mid-session.
    async fn read(&mut self, buf: &mut [u8]) -> Result<usize>;

    /// Sends `data` (local keystrokes) to the remote shell.
    ///
    /// # Errors
    ///
    /// Returns [`HostError::Transport`](crate::HostError::Transport) if the
    /// write fails.
    async fn write(&mut self, data: &[u8]) -> Result<()>;

    /// Informs the remote of a terminal size change (SSH `window-change`).
    ///
    /// `cols`/`rows` are character cells; this is offered so the CLI *may*
    /// honour `SIGWINCH`.
    ///
    /// # Errors
    ///
    /// Returns [`HostError::Transport`](crate::HostError::Transport) if the
    /// request fails.
    async fn resize(&mut self, cols: u32, rows: u32) -> Result<()>;

    /// Closes the shell channel.
    ///
    /// # Errors
    ///
    /// Returns [`HostError::Transport`](crate::HostError::Transport) only if an
    /// orderly channel close fails; an already-closed channel is success.
    async fn close(&mut self) -> Result<()>;
}
