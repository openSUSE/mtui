//! `mtui-hosts` — SSH/SFTP host layer (russh), host groups, locks, targets.
//!
//! Phase 2 builds this crate up incrementally. The current surface is the
//! [`Connection`] abstraction, the russh-backed [`SshConnection`] (connect /
//! run-with-timeout / SFTP transfers), the scriptable [`MockConnection`] test
//! double, the [`HostError`] hierarchy, the [`Target`] state machine
//! (enabled/dryrun/disabled command gating + connection-only `connect`), and the
//! [`HostsGroup`] composite with its parallel/serial command + SFTP fan-out.
//! Still to come in subsequent Phase 2 tasks: remote locks, the host arbiter,
//! and the interactive PTY shell (P2.10).

pub mod connection;
pub mod error;
pub mod target;

pub use connection::{
    CommandTimeout, Connection, HostKeyPolicy, MockConnection, MockSftpOp, SshConnection,
};
pub use error::{HostError, Result};
pub use target::{
    Command, HostsGroup, RunCommand, Target, run_parallel, sftp_get_all, sftp_put_all,
    sftp_remove_all,
};
