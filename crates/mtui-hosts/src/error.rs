//! The host-layer error hierarchy.
//!
//! Lives in `mtui-hosts` (not `mtui-types`) so the foundation crate stays
//! I/O-free per the workspace architecture. The variants cover the failure
//! modes of the SSH connection and command-timeout layers: authentication is
//! public-key only (there is **no** password
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
    /// Raised when the connect / SSH handshake fails, after logging a single
    /// user-facing line.
    #[error("no valid connection to {host}: {reason}")]
    Connect {
        /// The host that could not be reached.
        host: String,
        /// A human-readable reason (transport/OS message).
        reason: String,
    },

    /// Public-key authentication was rejected.
    ///
    /// Raised when authentication or host-key verification fails.
    /// MTUI is pubkey-only by design — there is no password fallback;
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
    /// The message is the repr of the timed-out command.
    #[error("command timed out: {command:?}")]
    Timeout {
        /// The command that timed out.
        command: String,
    },

    /// The reconnect loop exhausted its retries.
    #[error("failed to reconnect to {host}")]
    ReconnectFailed {
        /// The host that could not be reconnected.
        host: String,
    },

    /// A channel/transport-level SSH error occurred while running a command
    /// (channel open/exec failure, unexpected EOF, protocol error).
    #[error("transport error on {host}: {reason}")]
    Transport {
        /// The host the error occurred on.
        host: String,
        /// A human-readable reason (transport/protocol message).
        reason: String,
    },

    /// An SFTP operation failed
    /// (open/put/get/listdir/remove).
    #[error("sftp error on {host}: {reason}")]
    Sftp {
        /// The host the error occurred on.
        host: String,
        /// A human-readable reason (SFTP status / I/O message).
        reason: String,
    },

    /// An SFTP operation referenced a path that does not exist
    /// (`SSH_FX_NO_SUCH_FILE`).
    ///
    /// Distinguished from the catch-all [`Sftp`](Self::Sftp) variant because
    /// the host-system parser branches on "not found": a missing
    /// `/etc/products.d` means "not a SUSE host", a missing `/etc/os-release`
    /// means "fall back to RHEL", and a missing product file behind
    /// `baseproduct` means "dangling symlink".
    #[error("sftp path not found on {host}: {path}")]
    SftpNotFound {
        /// The host the error occurred on.
        host: String,
        /// The remote path that did not exist.
        path: String,
    },

    /// A host requested from a group is not a member of it.
    ///
    /// Raised by
    /// `HostsGroup::select` when a caller names a host the group does not hold.
    #[error("host {host} is not connected")]
    NotConnected {
        /// The host that is not a member of the group.
        host: String,
    },

    /// An exclusive SFTP create ([`Connection::sftp_write`] with
    /// `exclusive = true`) lost the race: the remote file already exists.
    ///
    /// This is the object-safe signal that an `O_CREAT | O_EXCL` create found
    /// the file already present. The lock protocol
    /// matches on this variant to reconcile a concurrent claim rather than
    /// clobbering the winner.
    ///
    /// [`Connection::sftp_write`]: crate::Connection::sftp_write
    #[error("path already exists on {host}: {path}")]
    AlreadyExists {
        /// The host the error occurred on.
        host: String,
        /// The remote path that already existed.
        path: String,
    },

    /// A remote target is locked by another owner and the lock could not be
    /// acquired (or force-released).
    ///
    /// The message is the human-readable
    /// "locked by" string (see `TargetLock::locked_by_msg`).
    #[error("{0}")]
    TargetLocked(String),

    /// One or more hosts in the group were locked by another owner when the
    /// group operation lock was being acquired.
    ///
    /// Raised by
    /// [`HostsGroup::update_lock`](crate::HostsGroup) after it has released the
    /// locks it did take, so a bespoke update/prepare/downgrade workflow aborts
    /// before running against a fleet it does not fully own.
    #[error("{0}")]
    Update(String),

    /// No installer "doer" is defined for the given product release.
    ///
    /// The message is `Missing Installer for
    /// {release}`. Raised by [`InstallOperation`](crate::InstallOperation) when
    /// a target's product has no configured installer, causing the operation to
    /// log and return before touching any locks.
    #[error("Missing Installer for {release}")]
    MissingInstaller {
        /// The product release with no configured installer.
        release: String,
    },

    /// No uninstaller "doer" is defined for the given product release.
    ///
    /// The message is `Missing Uninstaller for
    /// {release}`. Raised by [`UninstallOperation`](crate::UninstallOperation)
    /// under the same early-return contract as
    /// [`MissingInstaller`](Self::MissingInstaller).
    #[error("Missing Uninstaller for {release}")]
    MissingUninstaller {
        /// The product release with no configured uninstaller.
        release: String,
    },

    /// No preparer "doer" is defined for the given product release.
    ///
    /// The message is `Missing Preparer for {release}`.
    /// Raised when a target's product has no configured prepare command.
    #[error("Missing Preparer for {release}")]
    MissingPreparer {
        /// The product release with no configured preparer.
        release: String,
    },

    /// No updater "doer" is defined for the given product release.
    ///
    /// The message is `Missing Updater for {release}`.
    /// Raised when a target's product has no configured update command.
    #[error("Missing Updater for {release}")]
    MissingUpdater {
        /// The product release with no configured updater.
        release: String,
    },

    /// No downgrader "doer" is defined for the given product release.
    ///
    /// The message is `Missing Downgrader for
    /// {release}`. Raised when a target's product has no configured downgrade
    /// command.
    #[error("Missing Downgrader for {release}")]
    MissingDowngrader {
        /// The product release with no configured downgrader.
        release: String,
    },

    /// No update-workflow [`PlanProvider`](crate::PlanProvider) has been wired
    /// into the [`HostsGroup`](crate::HostsGroup).
    ///
    /// The doer/check resolver is injected by `mtui-testreport`'s
    /// `update_flow::perform_install` / `perform_uninstall` (keeping the crate
    /// graph acyclic), so reaching this means an [`Operation`](crate::Operation)
    /// was driven directly against an un-injected group.
    ///
    /// [`Operation::run`](crate::Operation::run) returns it rather than logging
    /// and carrying on. It used to log and return `()`, which is how the whole
    /// install/uninstall path came to run nothing while reporting success.
    #[error("no update-workflow provider wired into the host group")]
    NoPlanProvider,

    /// An SFTP folder download returned an entry name that is not a single
    /// ordinary path component (contains a separator, is `.`/`..`, is absolute,
    /// or carries a control byte).
    ///
    /// The remote peer controls directory-entry names; concatenating one
    /// verbatim into a local path lets a hostile/compromised host escape the
    /// download destination and overwrite arbitrary local files
    /// (path traversal). Such entries are rejected — the crafted name is quoted
    /// via `{name:?}` and no local path is echoed, so the diagnostic cannot leak
    /// the attacker-chosen target.
    #[error("unsafe SFTP entry name from {host}: {name:?}")]
    UnsafeSftpName {
        /// The host that supplied the entry name.
        host: String,
        /// The rejected (attacker-controlled) entry name.
        name: String,
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

    #[test]
    fn missing_installer_display_format() {
        let err = HostError::MissingInstaller {
            release: "opensuse-15.4".to_owned(),
        };
        assert_eq!(err.to_string(), "Missing Installer for opensuse-15.4");
    }

    #[test]
    fn missing_uninstaller_display_format() {
        let err = HostError::MissingUninstaller {
            release: "opensuse-15.4".to_owned(),
        };
        assert_eq!(err.to_string(), "Missing Uninstaller for opensuse-15.4");
    }

    #[test]
    fn missing_preparer_display_format() {
        let err = HostError::MissingPreparer {
            release: "opensuse-15.4".to_owned(),
        };
        assert_eq!(err.to_string(), "Missing Preparer for opensuse-15.4");
    }

    #[test]
    fn missing_updater_display_format() {
        let err = HostError::MissingUpdater {
            release: "opensuse-15.4".to_owned(),
        };
        assert_eq!(err.to_string(), "Missing Updater for opensuse-15.4");
    }

    #[test]
    fn missing_downgrader_display_format() {
        let err = HostError::MissingDowngrader {
            release: "opensuse-15.4".to_owned(),
        };
        assert_eq!(err.to_string(), "Missing Downgrader for opensuse-15.4");
    }
}
