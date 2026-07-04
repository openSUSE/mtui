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
//! * **P2.3** — `reconnect`, `fire_and_forget`, and the `sftp_*` transfer
//!   family (`put` / `get` / `get_folder` / `listdir` / `open` / `remove` /
//!   `rmdir` / `readlink`).
//!
//! The trait is object-safe so callers hold `Box<dyn Connection>` and swap the
//! russh impl for [`MockConnection`] freely.

mod mock;

pub use mock::MockConnection;

use async_trait::async_trait;
use mtui_types::hostlog::CommandLog;

use crate::error::Result;

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
