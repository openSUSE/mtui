//! The batched SFTP session primitive (mtui-rs-0mop.3).
//!
//! Ported from upstream `mtui/hosts/connection/connection.py` — the
//! `sftp_session()` context manager, which opens **one** `open_sftp()` client
//! and yields it for several reads against the same host, so a multi-step probe
//! pays the SFTP channel+subsystem handshake **once** instead of per operation.
//!
//! ## Scope split
//!
//! The per-op `sftp_*` methods on [`Connection`](super::Connection) each open
//! and close their own SFTP session (mirroring upstream's individual `sftp_*`
//! helpers). [`SftpSession`] is the object-safe batching counterpart: a caller
//! that runs several reads in a row (e.g. [`parse_system`] on a host with many
//! product files) opens one session via
//! [`Connection::sftp_session`](super::Connection::sftp_session), issues its
//! reads, then closes it — restoring upstream's single-handshake shape.
//!
//! Only the **read verbs the discovery parser uses** live here
//! ([`open`](SftpSession::open) / [`listdir`](SftpSession::listdir) /
//! [`readlink`](SftpSession::readlink)); the transfer and write family stay on
//! [`Connection`](super::Connection) so the remote-lock protocol's
//! exclusive-create semantics are untouched.
//!
//! [`parse_system`]: crate::target::parse_system

use std::path::Path;

use async_trait::async_trait;

use crate::error::Result;

/// A batched SFTP session to a single host: one open channel+subsystem reused
/// across several reads.
///
/// Returned by [`Connection::sftp_session`](super::Connection::sftp_session).
/// The session is opened once (reconnecting the transport first if it has
/// dropped, like the per-op path) and closed once via [`close`](Self::close) or
/// on drop. **Mid-session errors propagate** — this handle does not auto-retry
/// (matching upstream `sftp_session`); a caller that wants retry wraps the whole
/// batch (as [`Target::connect`](crate::Target) does around `parse_system`).
///
/// Object-safe by construction (`Box<dyn SftpSession>`), so the russh-backed
/// session and the test double are interchangeable.
#[async_trait]
pub trait SftpSession: Send {
    /// Reads a remote file's full contents over the shared session.
    ///
    /// The object-safe counterpart to
    /// [`Connection::sftp_open`](super::Connection::sftp_open): returns the
    /// file's bytes.
    ///
    /// # Errors
    ///
    /// Returns [`HostError::SftpNotFound`](crate::HostError::SftpNotFound) when
    /// the file is missing, or [`HostError::Sftp`](crate::HostError::Sftp) for
    /// any other SFTP/transport failure — identical mapping to the per-op path.
    async fn open(&mut self, path: &Path) -> Result<Vec<u8>>;

    /// Lists the entries of a remote directory over the shared session.
    ///
    /// The object-safe counterpart to
    /// [`Connection::sftp_listdir`](super::Connection::sftp_listdir).
    ///
    /// # Errors
    ///
    /// Returns [`HostError::SftpNotFound`](crate::HostError::SftpNotFound) when
    /// the directory is missing, or [`HostError::Sftp`](crate::HostError::Sftp)
    /// otherwise.
    async fn listdir(&mut self, path: &Path) -> Result<Vec<String>>;

    /// Returns the target of a remote symbolic link over the shared session.
    ///
    /// The object-safe counterpart to
    /// [`Connection::sftp_readlink`](super::Connection::sftp_readlink).
    ///
    /// # Errors
    ///
    /// Returns [`HostError::SftpNotFound`](crate::HostError::SftpNotFound) when
    /// the link is missing, or [`HostError::Sftp`](crate::HostError::Sftp)
    /// otherwise.
    async fn readlink(&mut self, path: &Path) -> Result<String>;

    /// Closes the shared SFTP session.
    ///
    /// # Errors
    ///
    /// Returns an SFTP/transport error only if an orderly close fails; an
    /// already-closed session is success.
    async fn close(&mut self) -> Result<()>;
}
