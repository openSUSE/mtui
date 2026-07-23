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
//! * **P2.6** (landed) — `sftp_write` (atomic exclusive create / truncating
//!   overwrite), the write primitive the remote-lock protocol is built on.
//! * **P2.10** (landed, feature `shell`) — the interactive PTY `shell`,
//!   returning an object-safe `ShellChannel` duplex. Only the transport
//!   primitive lives here; the raw-`termios` local TTY bridge and `shell` REPL
//!   command that consume it are a CLI concern (Phase 6).
//!
//! The trait is object-safe so callers hold `Box<dyn Connection>` and swap the
//! russh impl for [`MockConnection`] freely.

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
pub use ssh::{MAX_STREAM_BYTES, MAX_TOTAL_BYTES, SshConnection, TimeoutPrompt};
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

    /// Clones this connection into a fresh `Box<dyn Connection>` that shares the
    /// same underlying transport channel.
    ///
    /// Upstream a [`Target`](crate::Target) and its
    /// [`TargetLock`](crate::TargetLock) hold the *same* connection object; in
    /// Rust each owns a `Box<dyn Connection>`, so the lock is built from a clone
    /// of the target's connection. The clone is cheap and shares the live link
    /// (russh's `Handle` is an `mpsc` sender; [`MockConnection`] shares its
    /// scripted state via `Arc`), so a command or SFTP op issued through either
    /// handle hits the same host — preserving the single-connection-per-host
    /// contract.
    fn clone_box(&self) -> Box<dyn Connection>;

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
    /// Mirrors upstream `Connection.reconnect(retry, timeout, backoff)`:
    /// `retry` is the number of probe attempts beyond the first, and
    /// `backoff` selects between a flat per-probe sleep and one that grows
    /// (`2*(base + 5*count)`) across attempts. Callers recovering from a
    /// reboot pass a generous `retry` with `backoff = true`; every other
    /// caller (a dead link mid-command) passes `(0, false)` to fail fast.
    ///
    /// # Errors
    ///
    /// Returns [`HostError::ReconnectFailed`](crate::HostError::ReconnectFailed)
    /// if the retry budget is exhausted while the link is still down.
    async fn reconnect(&mut self, retry: usize, backoff: bool) -> Result<()>;

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

    /// Transfers already-read bytes to the remote host over SFTP, with the same
    /// parent-directory creation and `0770` executable contract as
    /// [`sftp_put`](Self::sftp_put).
    ///
    /// This exists so a fan-out upload can read an immutable local payload
    /// **once** and dispatch the shared bytes to every host, rather than
    /// re-reading the same file per host. [`sftp_put`](Self::sftp_put) is the
    /// convenience wrapper that reads `local` then calls this.
    ///
    /// # Errors
    ///
    /// Returns an SFTP/transport error if the transfer fails.
    async fn sftp_put_bytes(&mut self, data: &[u8], remote: &Path) -> Result<()>;

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
    /// The peer controls the directory-entry names; each is validated to be a
    /// single ordinary path component before it is used to build a local path.
    /// Names that would escape the destination (`../x`, `/etc/x`, `a/b`, `.`,
    /// `..`, or control bytes) are skipped rather than written, defeating path
    /// traversal by a hostile host. File bytes are streamed rather than buffered
    /// whole in memory.
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

    /// Writes `data` to a remote file over SFTP.
    ///
    /// This is the object-safe write counterpart to
    /// [`sftp_open`](Self::sftp_open) and the primitive the remote-lock
    /// protocol is built on. It ports upstream's two lockfile write modes:
    ///
    /// * `exclusive = true` — an **atomic create** that fails if the file
    ///   already exists (paramiko mode `"x"` → `O_CREAT | O_EXCL`). A
    ///   collision returns [`HostError::AlreadyExists`](crate::HostError::AlreadyExists)
    ///   so a racing caller can reconcile instead of clobbering the winner —
    ///   this is what closes the read-then-write TOCTOU window. Because SFTPv3
    ///   has no "file exists" status, the collision is detected as the generic
    ///   `Failure` status; **only** that maps to `AlreadyExists`. Any other
    ///   failure (permission denied, connection lost, …) propagates as a real
    ///   SFTP/transport error — the exclusive create fails *closed*, never
    ///   silently reconciled as if it had lost a race.
    /// * `exclusive = false` — a truncating overwrite (paramiko mode `"w+"`).
    ///
    /// # Errors
    ///
    /// Returns [`HostError::AlreadyExists`](crate::HostError::AlreadyExists)
    /// when `exclusive` is set and the file exists, or an SFTP/transport error
    /// if the write otherwise fails (including a non-collision failure of the
    /// exclusive create).
    async fn sftp_write(&mut self, path: &Path, data: &[u8], exclusive: bool) -> Result<()>;

    /// Atomically appends `data` to the end of a remote file over SFTP.
    ///
    /// This is the additive counterpart to [`sftp_write`](Self::sftp_write):
    /// it opens the file with `O_APPEND` (paramiko mode `"a+"`) so every write
    /// lands at the current end-of-file, and **creates the file if it is
    /// missing** (`O_CREAT`). Unlike the exclusive [`sftp_write`] path there is
    /// no read-modify-write window and no TOCTOU to close — concurrent
    /// appenders each extend the file without clobbering one another, which is
    /// exactly what the shared `/var/log/mtui.log` history contract needs when a
    /// Rust and a Python mtui write to the same host.
    ///
    /// It never truncates: existing contents are preserved and `data` is placed
    /// after them, byte-for-byte.
    ///
    /// # Errors
    ///
    /// Returns an SFTP/transport error if the file cannot be opened, created,
    /// or written.
    async fn sftp_append(&mut self, path: &Path, data: &[u8]) -> Result<()>;

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

    /// Opens a batched SFTP session that reuses one channel+subsystem across
    /// several reads, returning an object-safe [`SftpSession`] handle.
    ///
    /// Ports upstream `Connection.sftp_session`: a caller running several reads
    /// in a row (e.g. [`parse_system`](crate::target::parse_system) on a host
    /// with many product files) opens one session, issues its reads through the
    /// handle, then closes it — paying the SFTP handshake **once** instead of
    /// per op. The per-op `sftp_*` methods keep opening their own session; this
    /// is purely an optimization boundary for multi-step probes and carries no
    /// behavioural contract beyond identical per-read semantics.
    ///
    /// The session reconnects the transport first if it has dropped (like the
    /// per-op path); **mid-session** errors propagate without auto-retry.
    ///
    /// # Errors
    ///
    /// Returns a reconnect error if the link is down and cannot be
    /// re-established, or an SFTP/transport error if the subsystem cannot be
    /// opened.
    async fn sftp_session(&mut self) -> Result<Box<dyn SftpSession + '_>>;

    /// Opens an interactive PTY shell on the host, returning an object-safe
    /// [`ShellChannel`] duplex.
    ///
    /// Requests an `xterm` PTY sized `cols`×`rows` and invokes a login shell,
    /// mirroring upstream `Connection.__invoke_shell` + `shell`. The returned
    /// handle carries the transport only — the raw-`termios` local-terminal
    /// bridge (stdin↔channel↔stdout) that upstream runs inline is a CLI concern
    /// (Phase 6) and deliberately not part of the host library.
    ///
    /// Available only with the `shell` feature.
    ///
    /// # Errors
    ///
    /// Returns [`HostError::Transport`](crate::HostError::Transport) if the
    /// channel, PTY request, or shell request fails, or a reconnect error if
    /// the link is down and cannot be re-established.
    #[cfg(feature = "shell")]
    async fn shell(&mut self, cols: u32, rows: u32) -> Result<Box<dyn ShellChannel>>;
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
