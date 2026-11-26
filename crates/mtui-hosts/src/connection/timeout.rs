//! Command-timeout value type and SSH host-key policy mapping.

use std::fmt;
use std::str::FromStr;
use std::time::Duration;

use mtui_types::enums::ParseEnumError;

/// A duration budget for either the per-command no-output window (`connection_timeout`,
/// default 300s) or the SSH connect handshake (`connect_timeout`, default 60s).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CommandTimeout(Duration);

impl CommandTimeout {
    /// The default timeout, 300 seconds.
    const DEFAULT_SECS: u64 = 300;

    /// Builds a timeout from a whole number of seconds.
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
    pub(crate) const fn as_duration(self) -> Duration {
        self.0
    }

    /// Returns the timeout as whole seconds (truncating any sub-second remainder).
    #[must_use]
    pub(crate) const fn as_secs(self) -> u64 {
        self.0.as_secs()
    }
}

impl Default for CommandTimeout {
    /// The default of 300 seconds.
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

/// How the SSH client reacts to a host key that is **not already in**
/// `known_hosts`.
///
/// The policy is only consulted for *unknown* hosts: a key matching a recorded
/// entry is always accepted and one that *differs* is always rejected.
///
/// For an unknown host:
/// * [`AutoAdd`](Self::AutoAdd) accepts and persists the key (atomically).
/// * [`Warn`](Self::Warn) accepts with a warning but does not persist.
/// * [`Reject`](Self::Reject) refuses — this is the **strict/verified** mode:
///   only hosts already present in `known_hosts` connect.
///
/// The wire tokens are the exact `ssh_strict_host_key_checking` config values
/// (`auto_add` / `warn` / `reject`) and round-trip through [`FromStr`]/[`Display`](std::fmt::Display).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum HostKeyPolicy {
    /// Silently add an unknown host key and continue.
    ///
    /// The config default.
    #[default]
    AutoAdd,
    /// Warn about an unknown host key but continue.
    Warn,
    /// Reject the connection on an unknown host key.
    Reject,
}

impl HostKeyPolicy {
    /// Returns the config wire token for this policy.
    #[must_use]
    const fn as_str(self) -> &'static str {
        match self {
            Self::AutoAdd => "auto_add",
            Self::Warn => "warn",
            Self::Reject => "reject",
        }
    }

    /// Maps an `ssh_strict_host_key_checking` config value to a policy, falling
    /// back to [`AutoAdd`](HostKeyPolicy::AutoAdd) on an unrecognised value.
    ///
    /// Unknown values keep the auto-add default and emit a warning, since
    /// `mtui-config` loads leniently and never hard-fails on a bad value.
    #[must_use]
    pub(crate) fn from_config(name: &str) -> Self {
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
    fn command_timeout_default_is_300s() {
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

    // --- HostKeyPolicy: config wire-value contract. ---

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
        // Unknown values fall back to auto_add (+ warn).
        assert_eq!(HostKeyPolicy::from_config("strict"), HostKeyPolicy::AutoAdd);
        assert_eq!(HostKeyPolicy::from_config(""), HostKeyPolicy::AutoAdd);
    }
}
