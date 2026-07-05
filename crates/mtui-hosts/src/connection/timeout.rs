//! Command-timeout value type and SSH host-key policy mapping.
//!
//! Ported from upstream `mtui/hosts/connection/timeout.py`, which packages two
//! small, connection-independent helpers next to the SSH wrapper:
//!
//! * `CommandTimeoutError` — the exception raised when a remote command times
//!   out. In this port that failure is already modelled by
//!   [`HostError::Timeout`](crate::HostError::Timeout); the *value* used to arm
//!   that timeout is captured here as [`CommandTimeout`].
//! * `policy_from_config` — maps the `ssh_strict_host_key_checking` config
//!   string onto a paramiko `MissingHostKeyPolicy`. paramiko does not exist in
//!   this port, so the mapping target is the transport-agnostic
//!   [`HostKeyPolicy`] enum; the russh impl (P2.3) translates it into a russh
//!   client handler.
//!
//! Keeping these here mirrors upstream's separation and keeps the eventual
//! russh `Connection` impl focused on the SSH/SFTP wrapper proper.

use std::fmt;
use std::str::FromStr;
use std::time::Duration;

use mtui_types::enums::ParseEnumError;

/// The SSH connect + per-command timeout, as a typed [`Duration`].
///
/// Sourced from `mtui-config`'s `connection_timeout` (an integer number of
/// seconds, default `300`). The russh impl (P2.3) uses this both to bound the
/// TCP connect / banner / auth handshake and to abort a command whose channel
/// produces no output within the window — mirroring upstream, where the same
/// `connection_timeout` arms `paramiko.connect(timeout=…)` and the
/// `select`-based read loop that raises `CommandTimeoutError`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CommandTimeout(Duration);

impl CommandTimeout {
    /// The upstream default timeout, matching `mtui-config`'s
    /// `default_connection_timeout` (300 seconds).
    pub const DEFAULT_SECS: u64 = 300;

    /// Builds a timeout from a whole number of seconds.
    ///
    /// This is the shape `connection_timeout` takes in config, so it is the
    /// primary constructor.
    #[must_use]
    pub const fn from_secs(secs: u64) -> Self {
        Self(Duration::from_secs(secs))
    }

    /// Builds a timeout from an arbitrary [`Duration`].
    #[must_use]
    pub const fn new(duration: Duration) -> Self {
        Self(duration)
    }

    /// Returns the timeout as a [`Duration`], ready to hand to tokio timers or
    /// the russh transport.
    #[must_use]
    pub const fn as_duration(self) -> Duration {
        self.0
    }

    /// Returns the timeout as a whole number of seconds (truncating any
    /// sub-second remainder).
    #[must_use]
    pub const fn as_secs(self) -> u64 {
        self.0.as_secs()
    }
}

impl Default for CommandTimeout {
    /// The upstream default of 300 seconds.
    fn default() -> Self {
        Self::from_secs(Self::DEFAULT_SECS)
    }
}

impl From<Duration> for CommandTimeout {
    fn from(duration: Duration) -> Self {
        Self(duration)
    }
}

impl From<CommandTimeout> for Duration {
    fn from(timeout: CommandTimeout) -> Self {
        timeout.0
    }
}

/// How the SSH client reacts to a host key that is not already in
/// `known_hosts`.
///
/// Mirrors upstream's `_HOST_KEY_POLICIES` mapping of paramiko policies. The
/// wire tokens are the exact `ssh_strict_host_key_checking` config values
/// (`auto_add` / `warn` / `reject`), so a config string round-trips through
/// [`FromStr`](std::str::FromStr)/[`Display`](std::fmt::Display).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum HostKeyPolicy {
    /// Silently add an unknown host key and continue (paramiko `AutoAddPolicy`).
    ///
    /// The upstream and config default.
    #[default]
    AutoAdd,
    /// Warn about an unknown host key but continue (paramiko `WarningPolicy`).
    Warn,
    /// Reject the connection on an unknown host key (paramiko `RejectPolicy`).
    Reject,
}

impl HostKeyPolicy {
    /// Returns the config wire token for this policy.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::AutoAdd => "auto_add",
            Self::Warn => "warn",
            Self::Reject => "reject",
        }
    }

    /// Maps an `ssh_strict_host_key_checking` config value to a policy, falling
    /// back to [`AutoAdd`](HostKeyPolicy::AutoAdd) on an unrecognised value.
    ///
    /// This mirrors upstream `policy_from_config`, which preserves the legacy
    /// auto-add behaviour for unknown values and emits a warning so the
    /// misconfiguration stays visible. Because `mtui-config` loads leniently
    /// (a bad value never hard-fails), this lenient mapping is the right seam
    /// for turning the stored string into a typed policy.
    #[must_use]
    pub fn from_config(name: &str) -> Self {
        name.parse().unwrap_or_else(|_| {
            tracing::warn!(
                value = name,
                "unknown ssh_strict_host_key_checking; falling back to auto_add"
            );
            Self::AutoAdd
        })
    }
}

impl fmt::Display for HostKeyPolicy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for HostKeyPolicy {
    type Err = ParseEnumError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "auto_add" => Ok(Self::AutoAdd),
            "warn" => Ok(Self::Warn),
            "reject" => Ok(Self::Reject),
            other => Err(ParseEnumError {
                kind: "HostKeyPolicy",
                got: other.to_owned(),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- CommandTimeout ---

    #[test]
    fn command_timeout_default_matches_upstream_300s() {
        assert_eq!(CommandTimeout::default().as_secs(), 300);
        assert_eq!(
            CommandTimeout::default(),
            CommandTimeout::from_secs(CommandTimeout::DEFAULT_SECS)
        );
    }

    #[test]
    fn command_timeout_from_secs_round_trips() {
        let t = CommandTimeout::from_secs(450);
        assert_eq!(t.as_secs(), 450);
        assert_eq!(t.as_duration(), Duration::from_secs(450));
    }

    #[test]
    fn command_timeout_converts_to_and_from_duration() {
        let d = Duration::from_millis(1500);
        let t = CommandTimeout::from(d);
        assert_eq!(t, CommandTimeout::new(d));
        // Sub-second remainder is truncated by as_secs but preserved as Duration.
        assert_eq!(t.as_secs(), 1);
        assert_eq!(Duration::from(t), d);
    }

    #[test]
    fn command_timeout_orders_by_duration() {
        assert!(CommandTimeout::from_secs(10) < CommandTimeout::from_secs(20));
    }

    // --- HostKeyPolicy: upstream _HOST_KEY_POLICIES contract. ---

    #[test]
    fn host_key_policy_carries_config_wire_values() {
        assert_eq!(HostKeyPolicy::AutoAdd.as_str(), "auto_add");
        assert_eq!(HostKeyPolicy::Warn.as_str(), "warn");
        assert_eq!(HostKeyPolicy::Reject.as_str(), "reject");
    }

    #[test]
    fn host_key_policy_default_is_auto_add() {
        assert_eq!(HostKeyPolicy::default(), HostKeyPolicy::AutoAdd);
    }

    #[test]
    fn host_key_policy_round_trips_through_str() {
        for policy in [
            HostKeyPolicy::AutoAdd,
            HostKeyPolicy::Warn,
            HostKeyPolicy::Reject,
        ] {
            assert_eq!(policy.to_string().parse::<HostKeyPolicy>().unwrap(), policy);
        }
    }

    #[test]
    fn host_key_policy_from_str_rejects_unknown() {
        let err = "bogus".parse::<HostKeyPolicy>().unwrap_err();
        assert_eq!(err.kind, "HostKeyPolicy");
        assert_eq!(err.got, "bogus");
    }

    #[test]
    fn from_config_maps_each_known_value() {
        assert_eq!(
            HostKeyPolicy::from_config("auto_add"),
            HostKeyPolicy::AutoAdd
        );
        assert_eq!(HostKeyPolicy::from_config("warn"), HostKeyPolicy::Warn);
        assert_eq!(HostKeyPolicy::from_config("reject"), HostKeyPolicy::Reject);
    }

    #[test]
    fn from_config_falls_back_to_auto_add_on_unknown() {
        // Mirrors upstream policy_from_config: unknown -> auto_add (+ warn).
        assert_eq!(HostKeyPolicy::from_config("strict"), HostKeyPolicy::AutoAdd);
        assert_eq!(HostKeyPolicy::from_config(""), HostKeyPolicy::AutoAdd);
    }
}
