//! The host-layer error hierarchy.
//!
//! Lives in `mtui-hosts` (not `mtui-types`) so the foundation crate stays
//! I/O-free. The variants cover the SSH connection, command-timeout and SFTP
//! transfer layers; authentication is public-key only, so **no** variant
//! describes a password fallback because there is none.
//!
//! `#[non_exhaustive]`, so adding variants is not a breaking change. Like
//! `mtui-datasources`'s enums it deliberately stays standalone rather than
//! folding into `mtui-types::Error` via `#[from]`.

use thiserror::Error;

/// Convenience alias for `Result<T, `[`HostError`]`>`.
pub type Result<T> = std::result::Result<T, HostError>;

/// Errors produced by the host connection layer.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum HostError {
    /// The TCP connect / SSH handshake to a host failed (host unreachable,
    /// banner/auth timeout, or a general SSH-level failure), after logging a
    /// single user-facing line.
    #[error("no valid connection to {host}: {reason}")]
    Connect {
        /// The host that could not be reached.
        host: String,
        /// A human-readable reason (transport/OS message).
        reason: String,
    },

    /// Public-key authentication or host-key verification was rejected.
    ///
    /// MTUI is pubkey-only by design — there is no password fallback, so the
    /// only fix is working SSH key auth to the target.
    #[error(
        "authentication failed on {host}: SSH key authentication did not succeed \
         (set up working SSH key auth, verify with \"ssh root@{host}\")"
    )]
    Auth {
        /// The host that rejected authentication.
        host: String,
    },

    /// A remote command timed out with no output within the timeout window.
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

    /// A reboot command could not be handed to the host at all.
    ///
    /// Distinct from [`ReconnectFailed`](Self::ReconnectFailed) because the
    /// host is still **reachable**: `fire_and_forget` drops the local link only
    /// after channel open *and* `exec` succeed, so a failed dispatch leaves a
    /// live session and the follow-up reconnect succeeds trivially — a reboot
    /// that never left the client would read as a successful one.
    #[error("failed to dispatch the reboot to {host}: {reason}")]
    RebootNotDispatched {
        /// The host the reboot never reached.
        host: String,
        /// What went wrong, rendered from the underlying transport error.
        reason: String,
    },

    /// The host answered after its reboot with an unchanged boot id.
    ///
    /// `/proc/sys/kernel/random/boot_id` is regenerated on every boot, so an
    /// unchanged value means the reboot was accepted and silently did nothing
    /// (a masked `rebootmgr`, a systemd inhibitor, a polkit denial). On a
    /// transactional host that leaves the new snapshot **un-activated**.
    #[error("{host} did not reboot: its boot id is unchanged")]
    RebootDidNotHappen {
        /// The host that never went down.
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

    /// An SFTP operation failed (open/put/get/listdir/remove).
    #[error("sftp error on {host}: {reason}")]
    Sftp {
        /// The host the error occurred on.
        host: String,
        /// A human-readable reason (SFTP status / I/O message).
        reason: String,
    },

    /// An SFTP request exceeded its per-request timeout (russh-sftp's
    /// `Error::Timeout`, derived from `connect_timeout` — see
    /// [`SshConnection`](crate::connection::SshConnection)).
    ///
    /// Distinct from the catch-all [`Sftp`](Self::Sftp)/[`Transport`](Self::Transport)
    /// because a timed-out exclusive create may have landed server-side even
    /// though the client never saw the reply: the lock protocol matches on this
    /// variant to re-read and adopt the file rather than calling the group
    /// unowned.
    #[error("sftp request timed out on {host}: {path}")]
    SftpTimeout {
        /// The host the request timed out against.
        host: String,
        /// The remote path the timed-out request targeted.
        path: String,
    },

    /// An SFTP operation referenced a path that does not exist
    /// (`SSH_FX_NO_SUCH_FILE`).
    ///
    /// Distinct from the catch-all [`Sftp`](Self::Sftp) because the host-system
    /// parser branches on "not found": no `/etc/products.d` means "not a SUSE
    /// host", no `/etc/os-release` means "fall back to RHEL", and a missing
    /// product file behind `baseproduct` means "dangling symlink".
    #[error("sftp path not found on {host}: {path}")]
    SftpNotFound {
        /// The host the error occurred on.
        host: String,
        /// The remote path that did not exist.
        path: String,
    },

    /// A host requested from a group is not a member of it — raised by
    /// `HostsGroup::select`.
    #[error("host {host} is not connected")]
    NotConnected {
        /// The host that is not a member of the group.
        host: String,
    },

    /// An exclusive SFTP create ([`Connection::sftp_write`] with
    /// `exclusive = true`) lost the race: the remote file already exists.
    ///
    /// The lock protocol matches on this variant to reconcile a concurrent
    /// claim rather than clobbering the winner.
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
    /// acquired (or force-released). The message is `TargetLock::locked_by_msg`.
    #[error("{0}")]
    TargetLocked(String),

    /// One or more hosts in the group were locked by another owner when the
    /// group operation lock was being acquired.
    ///
    /// Raised by [`HostsGroup::update_lock`](crate::HostsGroup) *after* it has
    /// released the locks it did take, so an update/prepare/downgrade workflow
    /// aborts before running against a fleet it does not fully own.
    #[error("{0}")]
    Update(String),

    /// No installer "doer" is defined for the given product release.
    ///
    /// Raised by [`InstallOperation`](crate::InstallOperation), which logs and
    /// returns before touching any locks.
    #[error("Missing Installer for {release}")]
    MissingInstaller {
        /// The product release with no configured installer.
        release: String,
    },

    /// No uninstaller "doer" is defined for the given product release.
    ///
    /// Raised by [`UninstallOperation`](crate::UninstallOperation) under the
    /// same early-return contract as [`MissingInstaller`](Self::MissingInstaller).
    #[error("Missing Uninstaller for {release}")]
    MissingUninstaller {
        /// The product release with no configured uninstaller.
        release: String,
    },

    /// No preparer "doer" is defined for the given product release.
    #[error("Missing Preparer for {release}")]
    MissingPreparer {
        /// The product release with no configured preparer.
        release: String,
    },

    /// No updater "doer" is defined for the given product release.
    #[error("Missing Updater for {release}")]
    MissingUpdater {
        /// The product release with no configured updater.
        release: String,
    },

    /// No downgrader "doer" is defined for the given product release.
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
    /// was driven against an un-injected group.
    /// [`Operation::run`](crate::Operation::run) must *return* it, never log
    /// and carry on: swallowing it is how the whole install/uninstall path came
    /// to run nothing while reporting success.
    #[error("no update-workflow provider wired into the host group")]
    NoPlanProvider,

    /// An SFTP folder download returned an entry name that is not a single
    /// ordinary path component (contains a separator, is `.`/`..`, is absolute,
    /// or carries a control byte).
    ///
    /// The peer controls entry names, so concatenating one verbatim into a
    /// local path is a path-traversal hole. The crafted name is quoted via
    /// `{name:?}` and no local path is echoed, so the diagnostic cannot leak
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
