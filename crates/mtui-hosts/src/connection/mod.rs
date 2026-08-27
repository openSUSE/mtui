//! The [`Connection`] abstraction: one SSH/SFTP link to a single host.
//!
//! Defines the trait, the russh-backed [`SshConnection`], and the scriptable
//! [`MockConnection`] double. The [`Target`](crate) state machine drives one
//! `Box<dyn Connection>` per host, so the trait is kept object-safe and the
//! mock swaps in freely under test. Command results are
//! [`mtui_types::hostlog::CommandLog`], whose exit code uses `-1` as the "no
//! exit code / timed out" sentinel.
//!
//! `shell` (feature `shell`) returns the transport primitive only — the
//! raw-`termios` local TTY bridge and the `shell` REPL command that consume it
//! are a CLI concern (`mtui-cli`).

mod mock;
mod sftp_session;
#[cfg(feature = "shell")]
mod shell;
mod ssh;
mod timeout;

use std::path::Path;

pub use mock::{MockConnection, MockSftpOp};
pub use sftp_session::SftpSession;
#[cfg(feature = "shell")]
pub use shell::ShellChannel;
pub use ssh::{MAX_STREAM_BYTES, SshConnection, TimeoutPrompt};
pub use timeout::{CommandTimeout, HostKeyPolicy};

use async_trait::async_trait;
use mtui_types::hostlog::CommandLog;

use crate::error::Result;

/// The default SSH login user when `~/.ssh/config` names none.
pub(crate) const DEFAULT_USER: &str = "root";

/// One SSH/SFTP connection to a single remote host.
///
/// Object-safe (`Box<dyn Connection>`).
#[async_trait]
pub trait Connection: Send + Sync {
    /// The hostname this connection targets.
    fn hostname(&self) -> &str;

    /// Clones this connection into a fresh `Box<dyn Connection>` that shares the
    /// same underlying transport channel.
    ///
    /// A [`Target`](crate::Target) and its [`TargetLock`](crate::TargetLock)
    /// each own one, so the lock is built from a clone. The clone is cheap and
    /// reaches the same host ([`MockConnection`] shares its scripted state via
    /// `Arc`), preserving the single-connection-per-host contract.
    fn clone_box(&self) -> Box<dyn Connection>;

    /// Runs a command over the channel, blocking until it terminates.
    ///
    /// The returned [`CommandLog`]'s exit code is `-1` when the command could
    /// not complete (killed / timed out).
    ///
    /// # Errors
    ///
    /// [`HostError::Timeout`](crate::HostError::Timeout) on a timeout with no
    /// output, or a connection/reconnect error if the link is lost.
    async fn run(&mut self, command: &str) -> Result<CommandLog>;

    /// Reports whether the underlying transport is currently active.
    fn is_active(&self) -> bool;

    /// Closes the channel and disconnects.
    ///
    /// # Errors
    ///
    /// Only if an orderly shutdown fails; an already-closed link may be treated
    /// as success.
    async fn close(&mut self) -> Result<()>;

    /// Re-establishes the transport if it has dropped.
    ///
    /// `retry` is the number of probe attempts beyond the first; `backoff`
    /// selects a growing per-probe sleep (`2*(base + 5*count)`) over a flat
    /// one. Only callers recovering from a reboot pass a generous `retry` with
    /// `backoff = true`; a dead link mid-command passes `(0, false)` to fail
    /// fast.
    ///
    /// # Errors
    ///
    /// [`HostError::ReconnectFailed`](crate::HostError::ReconnectFailed) if the
    /// retry budget is exhausted while the link is still down.
    async fn reconnect(&mut self, retry: usize, backoff: bool) -> Result<()>;

    /// Dispatches a command without waiting for it to complete, then closes the
    /// local connection.
    ///
    /// For commands that deliberately tear down the link (e.g. a reboot);
    /// callers follow up with [`reconnect`](Self::reconnect).
    ///
    /// # Errors
    ///
    /// Only if the command could not be dispatched at all (no live channel); a
    /// link dropped *after* dispatch is expected.
    async fn fire_and_forget(&mut self, command: &str) -> Result<()>;

    /// Transfers a local file to the remote host over SFTP, creating parent
    /// directories and making the uploaded file executable (mode `0770`).
    async fn sftp_put(&mut self, local: &Path, remote: &Path) -> Result<()>;

    /// Transfers already-read bytes to the remote host over SFTP, with the same
    /// parent-directory creation and `0770` executable contract as
    /// [`sftp_put`](Self::sftp_put).
    ///
    /// Lets a fan-out upload read an immutable local payload **once** and
    /// dispatch the shared bytes to every host; [`sftp_put`](Self::sftp_put) is
    /// the wrapper that reads `local` then calls this.
    async fn sftp_put_bytes(&mut self, data: &[u8], remote: &Path) -> Result<()>;

    /// Transfers a remote file to the local host over SFTP.
    async fn sftp_get(&mut self, remote: &Path, local: &Path) -> Result<()>;

    /// Transfers every file in a remote folder to the local host, suffixing
    /// each local filename with `.{hostname}`.
    ///
    /// The per-host suffix is a workflow contract: a parallel fan-out writes
    /// many hosts' copies into one local dir without clobbering. The peer
    /// controls the entry names, so each is validated to be a single ordinary
    /// path component; names that would escape the destination (`../x`,
    /// `/etc/x`, `a/b`, `.`, `..`, control bytes) are skipped rather than
    /// written. Bytes are streamed, not buffered whole.
    async fn sftp_get_folder(&mut self, remote: &Path, local: &Path) -> Result<()>;

    /// Lists the entries of a remote directory.
    async fn sftp_listdir(&mut self, path: &Path) -> Result<Vec<String>>;

    /// Reads a remote file's full contents over SFTP.
    ///
    /// Object safety means bytes rather than a live handle; that covers every
    /// current caller (small config/metadata reads).
    async fn sftp_open(&mut self, path: &Path) -> Result<Vec<u8>>;

    /// Writes `data` to a remote file over SFTP.
    ///
    /// The write counterpart to [`sftp_open`](Self::sftp_open) and the
    /// primitive the remote-lock protocol is built on. `exclusive = false` is a
    /// truncating overwrite; `exclusive = true` is an **atomic create**
    /// (`O_CREAT | O_EXCL`) closing the read-then-write TOCTOU window, whose
    /// collision returns
    /// [`HostError::AlreadyExists`](crate::HostError::AlreadyExists) so a racing
    /// caller reconciles instead of clobbering the winner.
    ///
    /// # Errors
    ///
    /// SFTPv3 has no "file exists" status, so a collision arrives as the
    /// generic `Failure` and **only** that maps to `AlreadyExists`; every other
    /// failure propagates, so the exclusive create fails *closed*.
    async fn sftp_write(&mut self, path: &Path, data: &[u8], exclusive: bool) -> Result<()>;

    /// Atomically appends `data` to the end of a remote file over SFTP.
    ///
    /// Opens with `O_APPEND | O_CREAT` and never truncates, so there is no
    /// read-modify-write window: concurrent appenders each extend the file
    /// without clobbering one another. That is what the shared
    /// `/var/log/mtui.log` history contract needs when several mtui processes
    /// (including older releases) write to the same host.
    async fn sftp_append(&mut self, path: &Path, data: &[u8]) -> Result<()>;

    /// Deletes a remote file over SFTP.
    async fn sftp_remove(&mut self, path: &Path) -> Result<()>;

    /// Recursively deletes a remote directory over SFTP (files then the dir).
    async fn sftp_rmdir(&mut self, path: &Path) -> Result<()>;

    /// Returns the target of a remote symbolic link.
    async fn sftp_readlink(&mut self, path: &Path) -> Result<String>;

    /// Opens a batched SFTP session that reuses one channel+subsystem across
    /// several reads, returning an object-safe [`SftpSession`] handle.
    ///
    /// A multi-read probe (e.g. [`parse_system`](crate::target::parse_system)
    /// on a host with many product files) pays the SFTP handshake **once**
    /// instead of per op. Purely an optimization boundary: no behavioural
    /// contract beyond identical per-read semantics. The session reconnects a
    /// dropped transport at entry like the per-op path, but **mid-session**
    /// errors propagate without auto-retry.
    async fn sftp_session(&mut self) -> Result<Box<dyn SftpSession + '_>>;

    /// Opens an interactive PTY shell on the host, returning an object-safe
    /// [`ShellChannel`] duplex.
    ///
    /// Requests an `xterm` PTY sized `cols`×`rows` and invokes a login shell.
    /// The handle carries the transport only — the raw-`termios` local-terminal
    /// bridge (stdin↔channel↔stdout) is deliberately a CLI concern. Available
    /// only with the `shell` feature.
    #[cfg(feature = "shell")]
    async fn shell(&mut self, cols: u32, rows: u32) -> Result<Box<dyn ShellChannel>>;
}

#[cfg(test)]
mod tests {
    use super::*;

    // Compile-time proof of object safety.
    fn _assert_object_safe(_: &dyn Connection) {}

    #[test]
    fn trait_is_object_safe() {
        let conn: Box<dyn Connection> = Box::new(MockConnection::new("host.example"));
        _assert_object_safe(conn.as_ref());
    }
}
