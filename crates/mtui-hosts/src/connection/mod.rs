//! The [`Connection`] abstraction: one SSH/SFTP link to a single host.
//!
//! This module defines the **trait** and a scriptable [`MockConnection`] test
//! double. The concrete russh-backed implementation lands in a later Phase 2
//! task (P2.3); the [`Target`](crate) state machine (P2.4) drives one
//! `Box<dyn Connection>` per host and swaps in the mock under test.
//!
//! ## Reference
//!
//! Ported from upstream `mtui/hosts/connection/connection.py` (`Connection`).
//! The result of running a command is modelled with
//! [`mtui_types::hostlog::CommandLog`], which already carries exactly the
//! fields upstream records — the command, its stdout/stderr, the exit code
//! (with the `-1` "no exit code / timed out" sentinel), and the runtime.
//!
//! ## Scope
//!
//! This trait defines the **minimal core** needed to make the layer testable
//! and to unblock [`Target`](crate) (P2.4): [`run`](Connection::run),
//! [`is_active`](Connection::is_active), [`close`](Connection::close), and the
//! cheap [`hostname`](Connection::hostname) accessor. Later tasks extend it
//! deliberately:
//!
//! * **P2.3** (landed) — `reconnect`, `fire_and_forget`, and the `sftp_*`
//!   transfer family (`put` / `get` / `get_folder` / `listdir` / `open` /
//!   `remove` / `rmdir` / `readlink`), plus the russh-backed
//!   [`SshConnection`].
//!
//! Still pending: the interactive PTY [`shell`] (P2.10).
//!
//! [`shell`]: https://github.com/openSUSE/mtui
//!
//! The trait is object-safe so callers hold `Box<dyn Connection>` and swap the
//! russh impl for [`MockConnection`] freely.

mod mock;
mod ssh;
mod timeout;

use std::path::Path;

pub use mock::{MockConnection, MockSftpOp};
pub use ssh::SshConnection;
pub use timeout::{CommandTimeout, HostKeyPolicy};

use async_trait::async_trait;
use mtui_types::hostlog::CommandLog;

use crate::error::Result;

/// The default SSH login user when `~/.ssh/config` names none, matching
/// upstream's `opts.get("user", "root")`.
pub(crate) const DEFAULT_USER: &str = "root";

/// One SSH/SFTP connection to a single remote host.
///
/// Object-safe (`Box<dyn Connection>`); see the module docs for the planned
/// method-surface growth in later Phase 2 tasks.
#[async_trait]
pub trait Connection: Send + Sync {
    /// The hostname this connection targets.
    fn hostname(&self) -> &str;

    /// Runs a command over the channel, blocking until it terminates.
    ///
    /// Returns a [`CommandLog`] capturing the command, its stdout/stderr, the
    /// exit code, and the runtime. Mirrors upstream `Connection.run`, which
    /// returns `-1` as the exit code when the command could not complete
    /// (killed / timed out); the same sentinel convention applies here.
    ///
    /// # Errors
    ///
    /// Returns [`HostError::Timeout`](crate::HostError::Timeout) if the command
    /// times out with no output, or a connection/reconnect error if the link
    /// is lost and cannot be re-established.
    async fn run(&mut self, command: &str) -> Result<CommandLog>;

    /// Reports whether the underlying transport is currently active.
    ///
    /// Mirrors upstream `Connection.is_active`.
    fn is_active(&self) -> bool;

    /// Closes the channel and disconnects.
    ///
    /// Mirrors upstream `Connection.close`.
    ///
    /// # Errors
    ///
    /// Returns an error only if an orderly shutdown of the transport fails; a
    /// best-effort implementation may treat an already-closed link as success.
    async fn close(&mut self) -> Result<()>;

    /// Re-establishes the transport if it has dropped.
    ///
    /// Mirrors upstream `Connection.reconnect`: bounded retries, quiet while
    /// the host is (e.g.) rebooting, then a single surfaced failure.
    ///
    /// # Errors
    ///
    /// Returns [`HostError::ReconnectFailed`](crate::HostError::ReconnectFailed)
    /// if the retry budget is exhausted while the link is still down.
    async fn reconnect(&mut self) -> Result<()>;

    /// Dispatches a command without waiting for it to complete, then closes the
    /// local connection.
    ///
    /// Intended for commands that deliberately tear down the link (e.g. a
    /// reboot): no output or exit status is collected and a dropped link is
    /// expected — callers should follow up with [`reconnect`](Self::reconnect).
    /// Mirrors upstream `Connection.fire_and_forget`.
    ///
    /// # Errors
    ///
    /// Returns a connection error only if the command could not be dispatched
    /// at all (no live channel); a link dropped *after* dispatch is expected
    /// and not an error.
    async fn fire_and_forget(&mut self, command: &str) -> Result<()>;

    /// Transfers a local file to the remote host over SFTP, creating parent
    /// directories and making the uploaded file executable (mode `0770`).
    ///
    /// Mirrors upstream `Connection.sftp_put`.
    ///
    /// # Errors
    ///
    /// Returns an SFTP/transport error if the transfer fails.
    async fn sftp_put(&mut self, local: &Path, remote: &Path) -> Result<()>;

    /// Transfers a remote file to the local host over SFTP.
    ///
    /// Mirrors upstream `Connection.sftp_get`.
    ///
    /// # Errors
    ///
    /// Returns an SFTP/transport error if the transfer fails.
    async fn sftp_get(&mut self, remote: &Path, local: &Path) -> Result<()>;

    /// Transfers every file in a remote folder to the local host, suffixing
    /// each local filename with `.{hostname}`.
    ///
    /// Mirrors upstream `Connection.sftp_get_folder`, whose per-host suffix is
    /// a workflow contract (parallel fan-out writes many hosts' copies into one
    /// local dir without clobbering).
    ///
    /// # Errors
    ///
    /// Returns an SFTP/transport error if listing or any transfer fails.
    async fn sftp_get_folder(&mut self, remote: &Path, local: &Path) -> Result<()>;

    /// Lists the entries of a remote directory.
    ///
    /// Mirrors upstream `Connection.sftp_listdir`.
    ///
    /// # Errors
    ///
    /// Returns an SFTP/transport error if the directory cannot be listed.
    async fn sftp_listdir(&mut self, path: &Path) -> Result<Vec<String>>;

    /// Reads a remote file's full contents over SFTP.
    ///
    /// The upstream `Connection.sftp_open` returns a paramiko `SFTPFile`
    /// handle; in this port the object-safe surface returns the file's bytes,
    /// which covers every current caller (small config/metadata reads).
    ///
    /// # Errors
    ///
    /// Returns an SFTP/transport error if the file cannot be opened or read.
    async fn sftp_open(&mut self, path: &Path) -> Result<Vec<u8>>;

    /// Deletes a remote file over SFTP.
    ///
    /// Mirrors upstream `Connection.sftp_remove`.
    ///
    /// # Errors
    ///
    /// Returns an SFTP/transport error if the file cannot be removed.
    async fn sftp_remove(&mut self, path: &Path) -> Result<()>;

    /// Recursively deletes a remote directory over SFTP (files then the dir).
    ///
    /// Mirrors upstream `Connection.sftp_rmdir`.
    ///
    /// # Errors
    ///
    /// Returns an SFTP/transport error if the directory cannot be removed.
    async fn sftp_rmdir(&mut self, path: &Path) -> Result<()>;

    /// Returns the target of a remote symbolic link.
    ///
    /// Mirrors upstream `Connection.sftp_readlink`.
    ///
    /// # Errors
    ///
    /// Returns an SFTP/transport error if the link cannot be read.
    async fn sftp_readlink(&mut self, path: &Path) -> Result<String>;
}

#[cfg(test)]
mod tests {
    use super::*;

    // Compile-time proof that the trait is object-safe: if `Connection` were
    // not object-safe this function would fail to type-check.
    fn _assert_object_safe(_: &dyn Connection) {}

    #[test]
    fn trait_is_object_safe() {
        // Exercising the assertion via a boxed mock keeps the check live.
        let conn: Box<dyn Connection> = Box::new(MockConnection::new("host.example"));
        _assert_object_safe(conn.as_ref());
    }
}
