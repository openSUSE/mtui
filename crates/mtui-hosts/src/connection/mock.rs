//! A scriptable [`Connection`] test double.
//!
//! Per the workspace testing conventions, host access is mocked rather than
//! hitting real sshd: unit tests drive a [`MockConnection`] entirely offline.
//! It records every command issued (so callers can assert ordering / fan-out),
//! serves canned [`CommandLog`] responses keyed by command (with a default),
//! and can be scripted to fail a specific command so the retry / timeout paths
//! in later Phase 2 tasks (P2.3 reconnect, P2.5 parallel fan-out) are testable.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
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

/// An SFTP operation observed by a [`MockConnection`], recorded in order so
/// tests can assert exactly what a caller did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MockSftpOp {
    /// `sftp_put(local, remote)`.
    Put {
        /// The local source path.
        local: PathBuf,
        /// The remote destination path.
        remote: PathBuf,
    },
    /// `sftp_get(remote, local)`.
    Get {
        /// The remote source path.
        remote: PathBuf,
        /// The local destination path.
        local: PathBuf,
    },
    /// `sftp_get_folder(remote, local)`.
    GetFolder {
        /// The remote source folder.
        remote: PathBuf,
        /// The local destination folder.
        local: PathBuf,
    },
    /// `sftp_listdir(path)`.
    Listdir(PathBuf),
    /// `sftp_open(path)`.
    Open(PathBuf),
    /// `sftp_write(path, .., exclusive)`.
    Write {
        /// The remote path written.
        path: PathBuf,
        /// Whether the write was an exclusive (atomic-create) write.
        exclusive: bool,
    },
    /// `sftp_remove(path)`.
    Remove(PathBuf),
    /// `sftp_rmdir(path)`.
    Rmdir(PathBuf),
    /// `sftp_readlink(path)`.
    Readlink(PathBuf),
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
    /// Number of times [`reconnect`](Connection::reconnect) has been called.
    reconnects: Arc<Mutex<usize>>,
    /// When `true`, [`reconnect`](Connection::reconnect) fails.
    reconnect_fails: bool,
    /// Commands dispatched via [`fire_and_forget`](Connection::fire_and_forget).
    fired: Arc<Mutex<Vec<String>>>,
    /// SFTP operations observed, in order.
    sftp_ops: Arc<Mutex<Vec<MockSftpOp>>>,
    /// Canned directory listings keyed by remote path (for `sftp_listdir` /
    /// `sftp_get_folder`).
    listings: HashMap<PathBuf, Vec<String>>,
    /// File contents keyed by remote path (for `sftp_open` / `sftp_write`).
    ///
    /// Shared + mutable so `sftp_write` can create/overwrite entries and a
    /// later `sftp_open` observes them — this is what makes the lock protocol
    /// (exclusive create, reconcile, read-back) testable end-to-end against the
    /// mock. `Clone`d handles share the same table.
    files: Arc<Mutex<HashMap<PathBuf, Vec<u8>>>>,
    /// Canned symlink targets keyed by remote path (for `sftp_readlink`).
    links: HashMap<PathBuf, String>,
    /// When `true`, [`sftp_remove`](Connection::sftp_remove) fails, exercising a
    /// caller's directory-removal fallback (e.g. `Target::sftp_remove`).
    sftp_remove_fails: bool,
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
            reconnects: Arc::new(Mutex::new(0)),
            reconnect_fails: false,
            fired: Arc::new(Mutex::new(Vec::new())),
            sftp_ops: Arc::new(Mutex::new(Vec::new())),
            listings: HashMap::new(),
            files: Arc::new(Mutex::new(HashMap::new())),
            links: HashMap::new(),
            sftp_remove_fails: false,
        }
    }

    /// Makes [`sftp_remove`](Connection::sftp_remove) fail so a caller's
    /// directory-removal fallback path can be exercised.
    #[must_use]
    pub fn failing_sftp_remove(mut self) -> Self {
        self.sftp_remove_fails = true;
        self
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

    /// Scripts [`reconnect`](Connection::reconnect) to fail with
    /// [`HostError::ReconnectFailed`].
    #[must_use]
    pub fn failing_reconnect(mut self) -> Self {
        self.reconnect_fails = true;
        self
    }

    /// Scripts a canned directory listing for `sftp_listdir` /
    /// `sftp_get_folder` on `path`.
    #[must_use]
    pub fn with_listing(
        mut self,
        path: impl Into<PathBuf>,
        entries: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        self.listings
            .insert(path.into(), entries.into_iter().map(Into::into).collect());
        self
    }

    /// Scripts canned file contents for `sftp_open` on `path`.
    #[must_use]
    pub fn with_file(self, path: impl Into<PathBuf>, contents: impl Into<Vec<u8>>) -> Self {
        self.files
            .lock()
            .expect("mock files lock")
            .insert(path.into(), contents.into());
        self
    }

    /// Returns the current in-memory contents of a remote file written via
    /// [`sftp_write`](Connection::sftp_write) (or seeded with
    /// [`with_file`](Self::with_file)), or `None` when absent.
    #[must_use]
    pub fn file_contents(&self, path: impl AsRef<Path>) -> Option<Vec<u8>> {
        self.files
            .lock()
            .expect("mock files lock")
            .get(path.as_ref())
            .cloned()
    }

    /// Scripts a canned symlink target for `sftp_readlink` on `path`.
    #[must_use]
    pub fn with_link(mut self, path: impl Into<PathBuf>, target: impl Into<String>) -> Self {
        self.links.insert(path.into(), target.into());
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

    /// Returns how many times [`reconnect`](Connection::reconnect) was called.
    #[must_use]
    pub fn reconnect_count(&self) -> usize {
        *self.reconnects.lock().expect("mock reconnects lock")
    }

    /// Returns the commands dispatched via
    /// [`fire_and_forget`](Connection::fire_and_forget), in order.
    #[must_use]
    pub fn fired_commands(&self) -> Vec<String> {
        self.fired.lock().expect("mock fired lock").clone()
    }

    /// Returns the SFTP operations observed so far, in order.
    #[must_use]
    pub fn sftp_ops(&self) -> Vec<MockSftpOp> {
        self.sftp_ops.lock().expect("mock sftp lock").clone()
    }

    fn record_sftp(&self, op: MockSftpOp) {
        self.sftp_ops.lock().expect("mock sftp lock").push(op);
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

    async fn reconnect(&mut self) -> Result<()> {
        *self.reconnects.lock().expect("mock reconnects lock") += 1;
        if self.reconnect_fails {
            return Err(HostError::ReconnectFailed {
                host: self.hostname.clone(),
            });
        }
        self.active = true;
        Ok(())
    }

    async fn fire_and_forget(&mut self, command: &str) -> Result<()> {
        self.fired
            .lock()
            .expect("mock fired lock")
            .push(command.to_owned());
        // Mirrors upstream: dispatch, then tear down the local link.
        self.active = false;
        *self.closed.lock().expect("mock closed lock") = true;
        Ok(())
    }

    async fn sftp_put(&mut self, local: &Path, remote: &Path) -> Result<()> {
        self.record_sftp(MockSftpOp::Put {
            local: local.to_path_buf(),
            remote: remote.to_path_buf(),
        });
        Ok(())
    }

    async fn sftp_get(&mut self, remote: &Path, local: &Path) -> Result<()> {
        self.record_sftp(MockSftpOp::Get {
            remote: remote.to_path_buf(),
            local: local.to_path_buf(),
        });
        Ok(())
    }

    async fn sftp_get_folder(&mut self, remote: &Path, local: &Path) -> Result<()> {
        self.record_sftp(MockSftpOp::GetFolder {
            remote: remote.to_path_buf(),
            local: local.to_path_buf(),
        });
        Ok(())
    }

    async fn sftp_listdir(&mut self, path: &Path) -> Result<Vec<String>> {
        self.record_sftp(MockSftpOp::Listdir(path.to_path_buf()));
        Ok(self.listings.get(path).cloned().unwrap_or_default())
    }

    async fn sftp_open(&mut self, path: &Path) -> Result<Vec<u8>> {
        self.record_sftp(MockSftpOp::Open(path.to_path_buf()));
        self.files
            .lock()
            .expect("mock files lock")
            .get(path)
            .cloned()
            .ok_or_else(|| HostError::Sftp {
                host: self.hostname.clone(),
                reason: format!("no such file: {}", path.display()),
            })
    }

    async fn sftp_write(&mut self, path: &Path, data: &[u8], exclusive: bool) -> Result<()> {
        self.record_sftp(MockSftpOp::Write {
            path: path.to_path_buf(),
            exclusive,
        });
        let mut files = self.files.lock().expect("mock files lock");
        if exclusive && files.contains_key(path) {
            // Atomic exclusive create lost the race: the file already exists.
            return Err(HostError::AlreadyExists {
                host: self.hostname.clone(),
                path: path.display().to_string(),
            });
        }
        files.insert(path.to_path_buf(), data.to_vec());
        Ok(())
    }

    async fn sftp_remove(&mut self, path: &Path) -> Result<()> {
        self.record_sftp(MockSftpOp::Remove(path.to_path_buf()));
        if self.sftp_remove_fails {
            return Err(HostError::Sftp {
                host: self.hostname.clone(),
                reason: "scripted sftp_remove failure".to_owned(),
            });
        }
        Ok(())
    }

    async fn sftp_rmdir(&mut self, path: &Path) -> Result<()> {
        self.record_sftp(MockSftpOp::Rmdir(path.to_path_buf()));
        Ok(())
    }

    async fn sftp_readlink(&mut self, path: &Path) -> Result<String> {
        self.record_sftp(MockSftpOp::Readlink(path.to_path_buf()));
        self.links
            .get(path)
            .cloned()
            .ok_or_else(|| HostError::Sftp {
                host: self.hostname.clone(),
                reason: format!("not a link: {}", path.display()),
            })
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

    #[tokio::test]
    async fn reconnect_counts_and_reactivates() {
        let mut conn = MockConnection::new("h1").inactive();
        assert!(!conn.is_active());
        conn.reconnect().await.expect("reconnect ok");
        assert!(conn.is_active());
        assert_eq!(conn.reconnect_count(), 1);
    }

    #[tokio::test]
    async fn failing_reconnect_surfaces_error() {
        let mut conn = MockConnection::new("h1").failing_reconnect();
        let err = conn.reconnect().await.expect_err("should fail");
        assert!(matches!(err, HostError::ReconnectFailed { host } if host == "h1"));
        assert_eq!(conn.reconnect_count(), 1);
    }

    #[tokio::test]
    async fn fire_and_forget_records_and_tears_down() {
        let mut conn = MockConnection::new("h1");
        conn.fire_and_forget("reboot").await.expect("dispatch ok");
        assert_eq!(conn.fired_commands(), ["reboot"]);
        assert!(!conn.is_active());
        assert!(conn.is_closed());
    }

    #[tokio::test]
    async fn sftp_put_get_are_recorded_in_order() {
        let mut conn = MockConnection::new("h1");
        conn.sftp_put(Path::new("/tmp/a"), Path::new("/remote/a"))
            .await
            .expect("put ok");
        conn.sftp_get(Path::new("/remote/b"), Path::new("/tmp/b"))
            .await
            .expect("get ok");
        assert_eq!(
            conn.sftp_ops(),
            [
                MockSftpOp::Put {
                    local: PathBuf::from("/tmp/a"),
                    remote: PathBuf::from("/remote/a"),
                },
                MockSftpOp::Get {
                    remote: PathBuf::from("/remote/b"),
                    local: PathBuf::from("/tmp/b"),
                },
            ]
        );
    }

    #[tokio::test]
    async fn sftp_listdir_returns_scripted_entries() {
        let mut conn = MockConnection::new("h1").with_listing("/var/log", ["a.log", "b.log"]);
        let entries = conn.sftp_listdir(Path::new("/var/log")).await.expect("ok");
        assert_eq!(entries, ["a.log", "b.log"]);
        // Unscripted paths list empty, not error.
        let empty = conn.sftp_listdir(Path::new("/nope")).await.expect("ok");
        assert!(empty.is_empty());
    }

    #[tokio::test]
    async fn sftp_open_returns_scripted_bytes_or_errors() {
        let mut conn = MockConnection::new("h1").with_file("/etc/os-release", b"SLES".to_vec());
        let bytes = conn
            .sftp_open(Path::new("/etc/os-release"))
            .await
            .expect("ok");
        assert_eq!(bytes, b"SLES");
        let err = conn
            .sftp_open(Path::new("/missing"))
            .await
            .expect_err("should error");
        assert!(matches!(err, HostError::Sftp { .. }));
    }

    #[tokio::test]
    async fn sftp_readlink_returns_scripted_target() {
        let mut conn = MockConnection::new("h1").with_link("/link", "/target");
        let target = conn.sftp_readlink(Path::new("/link")).await.expect("ok");
        assert_eq!(target, "/target");
        assert!(conn.sftp_readlink(Path::new("/nope")).await.is_err());
    }

    #[tokio::test]
    async fn sftp_write_creates_and_is_readable() {
        let mut conn = MockConnection::new("h1");
        conn.sftp_write(Path::new("/var/lock/mtui.lock"), b"ts:user:1", false)
            .await
            .expect("write ok");
        let back = conn
            .sftp_open(Path::new("/var/lock/mtui.lock"))
            .await
            .expect("read ok");
        assert_eq!(back, b"ts:user:1");
    }

    #[tokio::test]
    async fn sftp_write_exclusive_collides_when_present() {
        let mut conn = MockConnection::new("h1");
        // First exclusive create wins.
        conn.sftp_write(Path::new("/f"), b"first", true)
            .await
            .expect("first exclusive create wins");
        // A second exclusive create loses the race.
        let err = conn
            .sftp_write(Path::new("/f"), b"second", true)
            .await
            .expect_err("second exclusive create must collide");
        assert!(matches!(err, HostError::AlreadyExists { .. }));
        // The winner's bytes are preserved (loser did not clobber).
        assert_eq!(conn.file_contents("/f").as_deref(), Some(&b"first"[..]));
    }

    #[tokio::test]
    async fn sftp_write_overwrite_replaces_and_records_order() {
        let mut conn = MockConnection::new("h1").with_file("/f", b"old".to_vec());
        // Non-exclusive overwrite replaces existing contents.
        conn.sftp_write(Path::new("/f"), b"new", false)
            .await
            .expect("overwrite ok");
        assert_eq!(conn.file_contents("/f").as_deref(), Some(&b"new"[..]));
        assert_eq!(
            conn.sftp_ops(),
            [MockSftpOp::Write {
                path: PathBuf::from("/f"),
                exclusive: false,
            }]
        );
    }

    #[tokio::test]
    async fn sftp_remove_rmdir_getfolder_recorded() {
        let mut conn = MockConnection::new("h1");
        conn.sftp_remove(Path::new("/f")).await.expect("ok");
        conn.sftp_rmdir(Path::new("/d")).await.expect("ok");
        conn.sftp_get_folder(Path::new("/rd"), Path::new("/ld"))
            .await
            .expect("ok");
        assert_eq!(
            conn.sftp_ops(),
            [
                MockSftpOp::Remove(PathBuf::from("/f")),
                MockSftpOp::Rmdir(PathBuf::from("/d")),
                MockSftpOp::GetFolder {
                    remote: PathBuf::from("/rd"),
                    local: PathBuf::from("/ld"),
                },
            ]
        );
    }
}
