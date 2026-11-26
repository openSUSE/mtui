//! The batched SFTP session primitive.
//!
//! Opens **one** SFTP client for several reads against a host, so a multi-step
//! probe (e.g. [`parse_system`](crate::target::parse_system)) pays the
//! channel+subsystem handshake once. Only the read verbs the discovery parser uses
//! live here; transfer and write verbs stay on [`Connection`](super::Connection)
//! so the remote-lock protocol's exclusive-create semantics are untouched.

use std::path::Path;

use async_trait::async_trait;

use crate::error::Result;

/// A batched SFTP session to a single host: one open channel+subsystem reused
/// across several reads.
///
/// Returned by [`Connection::sftp_session`](super::Connection::sftp_session).
/// Mid-session errors propagate; there is no auto-retry, so a caller that wants
/// retry wraps the whole batch (as [`Target::connect`](crate::Target) does).
/// Object-safe, so the russh-backed session and the test double are interchangeable.
#[async_trait]
pub trait SftpSession: Send {
    /// Reads a remote file's full contents over the shared session.
    ///
    /// # Errors
    ///
    /// Returns [`HostError::SftpNotFound`](crate::HostError::SftpNotFound) when
    /// the file is missing, or [`HostError::Sftp`](crate::HostError::Sftp) for
    /// any other SFTP/transport failure — identical mapping to the per-op path.
    async fn open(&mut self, path: &Path) -> Result<Vec<u8>>;

    /// Lists the entries of a remote directory over the shared session.
    ///
    /// # Errors
    ///
    /// Returns [`HostError::SftpNotFound`](crate::HostError::SftpNotFound) when
    /// the directory is missing, or [`HostError::Sftp`](crate::HostError::Sftp)
    /// otherwise.
    async fn listdir(&mut self, path: &Path) -> Result<Vec<String>>;

    /// Returns the target of a remote symbolic link over the shared session.
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
