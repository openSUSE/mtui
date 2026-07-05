//! `mtui-hosts` — SSH/SFTP host layer (russh), host groups, locks, targets.
//!
//! Phase 2 builds this crate up incrementally. The current surface is the
//! [`Connection`] abstraction, the russh-backed [`SshConnection`] (connect /
//! run-with-timeout / SFTP transfers), the scriptable [`MockConnection`] test
//! double, and the [`HostError`] hierarchy. Still to come in subsequent Phase 2
//! tasks: the `Target` state machine, `HostsGroup` fan-out, remote locks, the
//! host arbiter, and the interactive PTY shell (P2.10).

pub mod connection;
pub mod error;

pub use connection::{
    CommandTimeout, Connection, HostKeyPolicy, MockConnection, MockSftpOp, SshConnection,
};
pub use error::{HostError, Result};
