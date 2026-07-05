//! The host-layer error hierarchy.
//!
//! Lives in `mtui-hosts` (not `mtui-types`) so the foundation crate stays
//! I/O-free per the workspace architecture. The variants mirror the failure
//! modes of upstream `mtui/hosts/connection/connection.py` and
//! `timeout.py`: authentication is public-key only (there is **no** password
//! fallback), a remote command may time out, and a reconnect loop may give up.
//!
//! Later Phase 2 tasks (the russh impl, SFTP transfers) extend this enum with
//! transport/SFTP variants; it is `#[non_exhaustive]` so adding them is not a
//! breaking change. It will be wired into the top-level `mtui-types::Error`
//! via `#[from]` once a real consumer needs the unified type.

use thiserror::Error;

/// Convenience alias for `Result<T, `[`HostError`]`>`.
pub type Result<T> = std::result::Result<T, HostError>;

/// Errors produced by the host connection layer.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum HostError {
    /// The TCP connect / SSH handshake to a host failed (host unreachable,
    /// banner/auth timeout, or a general SSH-level failure).
    ///
    /// Mirrors upstream `Connection.connect` re-raising `OSError` /
    /// `paramiko.SSHException` after logging a single user-facing line.
    #[error("no valid connection to {host}: {reason}")]
    Connect {
        /// The host that could not be reached.
        host: String,
        /// A human-readable reason (transport/OS message).
        reason: String,
    },

    /// Public-key authentication was rejected.
    ///
    /// Mirrors upstream's `AuthenticationException` / `BadHostKeyException`
    /// branch. MTUI is pubkey-only by design — there is no password fallback;
    /// the fix is to set up working SSH key auth to the target.
    #[error(
        "authentication failed on {host}: SSH key authentication did not succeed \
         (set up working SSH key auth, verify with \"ssh root@{host}\")"
    )]
    Auth {
        /// The host that rejected authentication.
        host: String,
    },

    /// A remote command timed out with no output within the timeout window.
    ///
    /// Mirrors upstream `CommandTimeoutError`, whose `str()` is the repr of the
    /// timed-out command.
    #[error("command timed out: {command:?}")]
    Timeout {
        /// The command that timed out.
        command: String,
    },

    /// The reconnect loop exhausted its retries.
    ///
    /// Mirrors upstream `ReConnectFailed(hostname)`.
    #[error("failed to reconnect to {host}")]
    ReconnectFailed {
        /// The host that could not be reconnected.
        host: String,
    },

    /// A channel/transport-level SSH error occurred while running a command
    /// (channel open/exec failure, unexpected EOF, protocol error).
    ///
    /// Mirrors upstream re-raising `paramiko.ChannelException` /
    /// `paramiko.SSHException` from the command path.
    #[error("transport error on {host}: {reason}")]
    Transport {
        /// The host the error occurred on.
        host: String,
        /// A human-readable reason (transport/protocol message).
        reason: String,
    },

    /// An SFTP operation failed.
    ///
    /// Mirrors upstream's `sftp_*` methods surfacing paramiko/`OSError`
    /// failures (open/put/get/listdir/remove).
    #[error("sftp error on {host}: {reason}")]
    Sftp {
        /// The host the error occurred on.
        host: String,
        /// A human-readable reason (SFTP status / I/O message).
        reason: String,
    },

    /// A host requested from a group is not a member of it.
    ///
    /// Mirrors upstream `HostIsNotConnectedError`, raised by
    /// `HostsGroup.select` when a caller names a host the group does not hold.
    #[error("host {host} is not connected")]
    NotConnected {
        /// The host that is not a member of the group.
        host: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timeout_display_shows_quoted_command() {
        let err = HostError::Timeout {
            command: "zypper -n patch".to_owned(),
        };
        assert_eq!(err.to_string(), "command timed out: \"zypper -n patch\"");
    }

    #[test]
    fn reconnect_failed_display_names_host() {
        let err = HostError::ReconnectFailed {
            host: "host.example".to_owned(),
        };
        assert_eq!(err.to_string(), "failed to reconnect to host.example");
    }

    #[test]
    fn auth_display_is_pubkey_only_guidance() {
        let err = HostError::Auth {
            host: "h1".to_owned(),
        };
        let msg = err.to_string();
        assert!(msg.contains("authentication failed on h1"));
        assert!(msg.contains("ssh root@h1"));
    }

    #[test]
    fn connect_display_includes_host_and_reason() {
        let err = HostError::Connect {
            host: "h2".to_owned(),
            reason: "connection refused".to_owned(),
        };
        assert_eq!(
            err.to_string(),
            "no valid connection to h2: connection refused"
        );
    }
}
