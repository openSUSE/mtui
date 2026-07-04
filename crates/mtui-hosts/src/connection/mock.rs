//! A scriptable [`Connection`] test double.
//!
//! Per the workspace testing conventions, host access is mocked rather than
//! hitting real sshd: unit tests drive a [`MockConnection`] entirely offline.
//! It records every command issued (so callers can assert ordering / fan-out),
//! serves canned [`CommandLog`] responses keyed by command (with a default),
//! and can be scripted to fail a specific command so the retry / timeout paths
//! in later Phase 2 tasks (P2.3 reconnect, P2.5 parallel fan-out) are testable.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use mtui_types::hostlog::CommandLog;

use super::Connection;
use crate::error::{HostError, Result};

/// The outcome scripted for a command run against a [`MockConnection`].
#[derive(Debug, Clone)]
enum Outcome {
    /// Return this command log.
    Ok(CommandLog),
    /// Fail the run with a timeout for the command.
    Timeout,
}

/// A scriptable, in-memory [`Connection`] implementation for tests.
///
/// Construct with [`MockConnection::new`], script responses with
/// [`with_response`](MockConnection::with_response) /
/// [`with_default`](MockConnection::with_default) /
/// [`with_timeout`](MockConnection::with_timeout), then inspect issued commands
/// via [`commands`](MockConnection::commands).
#[derive(Debug, Clone)]
pub struct MockConnection {
    hostname: String,
    /// Per-command scripted outcomes.
    responses: HashMap<String, Outcome>,
    /// Fallback outcome when a command has no scripted response.
    default: Outcome,
    /// Whether the transport reports as active.
    active: bool,
    /// Commands issued, in order (shared so `Clone`d handles observe the same
    /// log — a `Box<dyn Connection>` may be moved but tests keep a handle).
    issued: Arc<Mutex<Vec<String>>>,
    /// Set once [`close`](Connection::close) has been called.
    closed: Arc<Mutex<bool>>,
}

impl MockConnection {
    /// Creates a mock for `hostname` whose default response is an empty,
    /// successful [`CommandLog`] (exit code 0).
    #[must_use]
    pub fn new(hostname: impl Into<String>) -> Self {
        Self {
            hostname: hostname.into(),
            responses: HashMap::new(),
            default: Outcome::Ok(CommandLog::new("", "", "", 0, 0)),
            active: true,
            issued: Arc::new(Mutex::new(Vec::new())),
            closed: Arc::new(Mutex::new(false)),
        }
    }

    /// Scripts a full [`CommandLog`] response for an exact command string.
    #[must_use]
    pub fn with_response(mut self, command: impl Into<String>, log: CommandLog) -> Self {
        self.responses.insert(command.into(), Outcome::Ok(log));
        self
    }

    /// Scripts `command` to time out (surfaced as [`HostError::Timeout`]).
    #[must_use]
    pub fn with_timeout(mut self, command: impl Into<String>) -> Self {
        self.responses.insert(command.into(), Outcome::Timeout);
        self
    }

    /// Overrides the fallback response used when a command is not explicitly
    /// scripted.
    #[must_use]
    pub fn with_default(mut self, log: CommandLog) -> Self {
        self.default = Outcome::Ok(log);
        self
    }

    /// Marks the transport as inactive (e.g. to test `is_active` handling).
    #[must_use]
    pub fn inactive(mut self) -> Self {
        self.active = false;
        self
    }

    /// Returns a snapshot of the commands issued so far, in order.
    #[must_use]
    pub fn commands(&self) -> Vec<String> {
        self.issued.lock().expect("mock issued lock").clone()
    }

    /// Returns whether [`close`](Connection::close) has been called.
    #[must_use]
    pub fn is_closed(&self) -> bool {
        *self.closed.lock().expect("mock closed lock")
    }
}

#[async_trait]
impl Connection for MockConnection {
    fn hostname(&self) -> &str {
        &self.hostname
    }

    async fn run(&mut self, command: &str) -> Result<CommandLog> {
        self.issued
            .lock()
            .expect("mock issued lock")
            .push(command.to_owned());

        let outcome = self.responses.get(command).unwrap_or(&self.default);
        match outcome {
            Outcome::Ok(log) => Ok(log.clone()),
            Outcome::Timeout => Err(HostError::Timeout {
                command: command.to_owned(),
            }),
        }
    }

    fn is_active(&self) -> bool {
        self.active
    }

    async fn close(&mut self) -> Result<()> {
        *self.closed.lock().expect("mock closed lock") = true;
        self.active = false;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn default_response_is_success() {
        let mut conn = MockConnection::new("h1");
        let log = conn.run("uptime").await.expect("run ok");
        assert_eq!(log.exitcode, 0);
        assert_eq!(conn.hostname(), "h1");
    }

    #[tokio::test]
    async fn scripted_response_is_returned() {
        let mut conn = MockConnection::new("h1").with_response(
            "cat /etc/os-release",
            CommandLog::new("cat", "SLES", "", 0, 1),
        );
        let log = conn.run("cat /etc/os-release").await.expect("run ok");
        assert_eq!(log.stdout, "SLES");
        assert_eq!(log.runtime, 1);
    }

    #[tokio::test]
    async fn commands_are_recorded_in_order() {
        let mut conn = MockConnection::new("h1");
        conn.run("a").await.expect("a");
        conn.run("b").await.expect("b");
        conn.run("c").await.expect("c");
        assert_eq!(conn.commands(), ["a", "b", "c"]);
    }

    #[tokio::test]
    async fn scripted_timeout_surfaces_host_error() {
        let mut conn = MockConnection::new("h1").with_timeout("sleep 999");
        let err = conn.run("sleep 999").await.expect_err("should time out");
        assert!(matches!(err, HostError::Timeout { command } if command == "sleep 999"));
        // The command is still recorded even though it failed.
        assert_eq!(conn.commands(), ["sleep 999"]);
    }

    #[tokio::test]
    async fn close_marks_inactive_and_closed() {
        let mut conn = MockConnection::new("h1");
        assert!(conn.is_active());
        assert!(!conn.is_closed());
        conn.close().await.expect("close ok");
        assert!(!conn.is_active());
        assert!(conn.is_closed());
    }

    #[tokio::test]
    async fn inactive_builder_reports_not_active() {
        let conn = MockConnection::new("h1").inactive();
        assert!(!conn.is_active());
    }

    #[tokio::test]
    async fn with_default_overrides_fallback() {
        let mut conn =
            MockConnection::new("h1").with_default(CommandLog::new("", "", "boom", 7, 0));
        let log = conn.run("anything").await.expect("run ok");
        assert_eq!(log.exitcode, 7);
        assert_eq!(log.stderr, "boom");
    }

    #[tokio::test]
    async fn usable_behind_boxed_trait_object() {
        let mut conn: Box<dyn Connection> = Box::new(MockConnection::new("h1"));
        let log = conn.run("whoami").await.expect("run ok");
        assert_eq!(log.exitcode, 0);
        conn.close().await.expect("close ok");
    }
}
