//! The interactive PTY shell primitive (feature `shell`).
//!
//! Only the transport primitive, [`ShellChannel`], lives here. The raw-`termios`
//! bridge and the `shell` command that drive it live in the CLI crate, which can
//! test them offline against [`MockConnection`](super::MockConnection).

use async_trait::async_trait;

use crate::error::Result;

/// An open interactive shell channel to a single host: an object-safe async
/// duplex over the remote PTY.
///
/// Returned by [`Connection::shell`](super::Connection::shell). Object-safe, so the
/// russh-backed channel and the test double are interchangeable.
#[async_trait]
pub trait ShellChannel: Send {
    /// Reads available shell output into `buf`, returning the number of bytes
    /// written. `Ok(0)` means the remote shell exited; a short read is normal.
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
