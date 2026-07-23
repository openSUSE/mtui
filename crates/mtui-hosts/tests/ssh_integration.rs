//! Integration tests for the russh-backed [`SshConnection`] (P2.3).
//!
//! These run **offline** against an **ephemeral in-process russh server** with
//! a freshly generated host key — no Docker, no external sshd, so they execute
//! in a normal `cargo test`. The server implements just enough of the SSH
//! `exec` and SFTP surfaces to exercise every `Connection` method:
//!
//! * `run` — echoes a scripted stdout/stderr and exit status per command.
//! * a small **in-memory filesystem** backs the SFTP subsystem (put/get/
//!   listdir/open/remove/rmdir/readlink/mkdir).
//!
//! Auth accepts any offered public key (the client authenticates with a
//! generated key), proving the pubkey path end-to-end without a real key store.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use russh::keys::{Algorithm, PrivateKey};
use russh::server::{Auth, Msg, Server as _, Session};
use russh::{Channel, ChannelId};
use russh_sftp::protocol::{Data, File, FileAttributes, Handle, Name, Status, StatusCode, Version};
use tokio::net::TcpListener;
use tokio::sync::Mutex;

use mtui_hosts::{
    CommandTimeout, Connection, HostKeyPolicy, HostsGroup, MAX_STREAM_BYTES, SshConnection,
    TARGET_LOCK_PATH, Target, TargetLock,
};
use mtui_types::enums::{ExecutionMode, TargetState};

/// Bytes the `flood-stdout` scripted command emits: comfortably past the
/// per-stream capture cap so `run` must truncate.
const FLOOD_BYTES: usize = MAX_STREAM_BYTES + 512 * 1024;

// ----------------------------------------------------------------------------
// In-memory backing filesystem shared by the SFTP handler.
// ----------------------------------------------------------------------------

#[derive(Default)]
struct FakeFs {
    /// path -> file contents
    files: HashMap<String, Vec<u8>>,
    /// directory path -> child names
    dirs: HashMap<String, Vec<String>>,
    /// link path -> target
    links: HashMap<String, String>,
}

type SharedFs = Arc<Mutex<FakeFs>>;

// ----------------------------------------------------------------------------
// SSH server: exec + sftp subsystem.
// ----------------------------------------------------------------------------

#[derive(Clone)]
struct TestServer {
    fs: SharedFs,
}

impl russh::server::Server for TestServer {
    type Handler = TestSshSession;

    fn new_client(&mut self, _: Option<SocketAddr>) -> Self::Handler {
        TestSshSession {
            fs: self.fs.clone(),
            channels: Arc::new(Mutex::new(HashMap::new())),
            #[cfg(feature = "shell")]
            shell_channels: Arc::new(Mutex::new(std::collections::HashSet::new())),
        }
    }
}

struct TestSshSession {
    fs: SharedFs,
    channels: Arc<Mutex<HashMap<ChannelId, Channel<Msg>>>>,
    /// Channels that requested an interactive shell, so the `data` callback only
    /// echoes keystrokes for shells and leaves SFTP-subsystem data untouched.
    #[cfg(feature = "shell")]
    shell_channels: Arc<Mutex<std::collections::HashSet<ChannelId>>>,
}

impl russh::server::Handler for TestSshSession {
    type Error = russh::Error;

    async fn auth_publickey(
        &mut self,
        _user: &str,
        _key: &russh::keys::PublicKey,
    ) -> Result<Auth, Self::Error> {
        Ok(Auth::Accept)
    }

    async fn channel_open_session(
        &mut self,
        channel: Channel<Msg>,
        reply: russh::server::ChannelOpenHandle,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        self.channels.lock().await.insert(channel.id(), channel);
        reply.accept().await;
        Ok(())
    }

    async fn exec_request(
        &mut self,
        channel_id: ChannelId,
        data: &[u8],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        let command = String::from_utf8_lossy(data).to_string();

        // A command that never produces output and never exits: the server
        // deliberately sends nothing, so the client's no-output timeout fires.
        if command == "sleep-forever" {
            return Ok(());
        }

        // A command that floods stdout past the client's capture cap: emit
        // `FLOOD_BYTES` then exit cleanly. Proves `run` truncates rather than
        // buffering the whole stream.
        if command == "flood-stdout" {
            let chunk = vec![b'a'; 64 * 1024];
            let mut sent = 0;
            while sent < FLOOD_BYTES {
                session.data(channel_id, chunk.clone())?;
                sent += chunk.len();
            }
            session.exit_status_request(channel_id, 0)?;
            session.eof(channel_id)?;
            session.close(channel_id)?;
            return Ok(());
        }

        // A command that trickles output forever: one byte every 20ms, never
        // exits. Continuous output keeps the client's inactivity window from
        // firing, so only the absolute (non-interactive) deadline can stop it.
        if command == "trickle-forever" {
            loop {
                session.data(channel_id, b".".to_vec())?;
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }

        let (stdout, stderr, code) = scripted_command(&command);

        if !stdout.is_empty() {
            session.data(channel_id, stdout.into_bytes())?;
        }
        if !stderr.is_empty() {
            session.extended_data(channel_id, 1, stderr.into_bytes())?;
        }
        session.exit_status_request(channel_id, code)?;
        session.eof(channel_id)?;
        session.close(channel_id)?;
        Ok(())
    }

    async fn subsystem_request(
        &mut self,
        channel_id: ChannelId,
        name: &str,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        if name == "sftp" {
            let channel = self.channels.lock().await.remove(&channel_id).unwrap();
            session.channel_success(channel_id)?;
            let handler = SftpHandler {
                fs: self.fs.clone(),
                listed: HashMap::new(),
                append: std::collections::HashSet::new(),
            };
            russh_sftp::server::run(channel.into_stream(), handler).await;
        } else {
            session.channel_failure(channel_id)?;
        }
        Ok(())
    }

    // A minimal interactive shell: accept the PTY + shell requests, greet the
    // client, then echo keystrokes back. Sending the sentinel byte `\x04`
    // (Ctrl-D) closes the channel — the client's read loop sees EOF (`0`).
    #[cfg(feature = "shell")]
    async fn pty_request(
        &mut self,
        channel_id: ChannelId,
        _term: &str,
        _col_width: u32,
        _row_height: u32,
        _pix_width: u32,
        _pix_height: u32,
        _modes: &[(russh::Pty, u32)],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        session.channel_success(channel_id)?;
        Ok(())
    }

    #[cfg(feature = "shell")]
    async fn shell_request(
        &mut self,
        channel_id: ChannelId,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        self.shell_channels.lock().await.insert(channel_id);
        session.channel_success(channel_id)?;
        session.data(channel_id, b"welcome\n".to_vec())?;
        Ok(())
    }

    #[cfg(feature = "shell")]
    async fn data(
        &mut self,
        channel_id: ChannelId,
        data: &[u8],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        // Only interactive-shell channels echo here; SFTP-subsystem data is
        // routed by russh_sftp's own runner and must not be intercepted.
        if !self.shell_channels.lock().await.contains(&channel_id) {
            return Ok(());
        }
        if data.contains(&0x04) {
            // Ctrl-D: close the shell so the client read loop terminates.
            session.eof(channel_id)?;
            session.close(channel_id)?;
        } else {
            // Echo keystrokes back, as an interactive shell with echo on would.
            session.data(channel_id, data.to_vec())?;
        }
        Ok(())
    }
}

/// Maps a command string to a scripted `(stdout, stderr, exit_code)`.
fn scripted_command(cmd: &str) -> (String, String, u32) {
    match cmd {
        "echo hello" => ("hello\n".to_owned(), String::new(), 0),
        "exit 3" => (String::new(), String::new(), 3),
        "to-stderr" => (String::new(), "boom\n".to_owned(), 1),
        other => (format!("ran: {other}\n"), String::new(), 0),
    }
}

// ----------------------------------------------------------------------------
// SFTP handler over the in-memory FS.
// ----------------------------------------------------------------------------

struct SftpHandler {
    fs: SharedFs,
    /// readdir cursor: handle -> whether already returned entries.
    listed: HashMap<String, bool>,
    /// Handles opened with `O_APPEND`: writes ignore the client offset and land
    /// at the current end-of-file, as a real sshd does.
    append: std::collections::HashSet<String>,
}

impl SftpHandler {
    fn ok(id: u32) -> Status {
        Status {
            id,
            status_code: StatusCode::Ok,
            error_message: "Ok".to_owned(),
            language_tag: "en-US".to_owned(),
        }
    }
}

impl russh_sftp::server::Handler for SftpHandler {
    type Error = StatusCode;

    fn unimplemented(&self) -> Self::Error {
        StatusCode::OpUnsupported
    }

    async fn init(
        &mut self,
        _version: u32,
        _ext: HashMap<String, String>,
    ) -> Result<Version, Self::Error> {
        Ok(Version::new())
    }

    async fn realpath(&mut self, id: u32, path: String) -> Result<Name, Self::Error> {
        Ok(Name {
            id,
            files: vec![File::dummy(if path == "." { "/" } else { &path })],
        })
    }

    async fn open(
        &mut self,
        id: u32,
        filename: String,
        pflags: russh_sftp::protocol::OpenFlags,
        _attrs: FileAttributes,
    ) -> Result<Handle, Self::Error> {
        use russh_sftp::protocol::OpenFlags;
        // Honour O_EXCL: an exclusive create against an existing file fails,
        // as a real sshd would — this is what the lock protocol relies on.
        if pflags.contains(OpenFlags::EXCLUDE) {
            let fs = self.fs.lock().await;
            if fs.files.contains_key(&filename) {
                return Err(StatusCode::Failure);
            }
        }
        // A create (with or without O_EXCL) materialises the file so a
        // subsequent exclusive create sees it and a read observes the write.
        // TRUNCATE resets any existing contents (paramiko "w+"/"x" semantics).
        if pflags.contains(OpenFlags::CREATE) {
            let mut fs = self.fs.lock().await;
            if pflags.contains(OpenFlags::TRUNCATE) {
                fs.files.insert(filename.clone(), Vec::new());
            } else {
                fs.files.entry(filename.clone()).or_default();
            }
        }
        // O_APPEND: subsequent writes on this handle land at EOF regardless of
        // the client-supplied offset (russh-sftp's client does not seek to EOF).
        if pflags.contains(OpenFlags::APPEND) {
            self.append.insert(filename.clone());
        } else {
            self.append.remove(&filename);
        }
        Ok(Handle {
            id,
            handle: filename,
        })
    }

    async fn read(
        &mut self,
        id: u32,
        handle: String,
        offset: u64,
        len: u32,
    ) -> Result<Data, Self::Error> {
        let fs = self.fs.lock().await;
        let contents = fs.files.get(&handle).cloned().unwrap_or_default();
        let start = offset as usize;
        if start >= contents.len() {
            return Err(StatusCode::Eof);
        }
        let end = (start + len as usize).min(contents.len());
        Ok(Data {
            id,
            data: contents[start..end].to_vec(),
        })
    }

    async fn write(
        &mut self,
        id: u32,
        handle: String,
        offset: u64,
        data: Vec<u8>,
    ) -> Result<Status, Self::Error> {
        let append = self.append.contains(&handle);
        let mut fs = self.fs.lock().await;
        let buf = fs.files.entry(handle).or_default();
        // In append mode the write lands at the current EOF; otherwise honour
        // the client-supplied offset (overwrite/positioned write).
        let start = if append { buf.len() } else { offset as usize };
        if buf.len() < start + data.len() {
            buf.resize(start + data.len(), 0);
        }
        buf[start..start + data.len()].copy_from_slice(&data);
        Ok(Self::ok(id))
    }

    async fn close(&mut self, id: u32, _handle: String) -> Result<Status, Self::Error> {
        Ok(Self::ok(id))
    }

    async fn opendir(&mut self, id: u32, path: String) -> Result<Handle, Self::Error> {
        self.listed.insert(path.clone(), false);
        Ok(Handle { id, handle: path })
    }

    async fn readdir(&mut self, id: u32, handle: String) -> Result<Name, Self::Error> {
        if *self.listed.get(&handle).unwrap_or(&true) {
            return Err(StatusCode::Eof);
        }
        self.listed.insert(handle.clone(), true);
        let fs = self.fs.lock().await;
        let entries = fs.dirs.get(&handle).cloned().unwrap_or_default();
        if entries.is_empty() {
            return Err(StatusCode::Eof);
        }
        Ok(Name {
            id,
            files: entries
                .into_iter()
                .map(|name| File::new(name, FileAttributes::default()))
                .collect(),
        })
    }

    async fn mkdir(
        &mut self,
        id: u32,
        path: String,
        _attrs: FileAttributes,
    ) -> Result<Status, Self::Error> {
        self.fs.lock().await.dirs.entry(path).or_default();
        Ok(Self::ok(id))
    }

    async fn setstat(
        &mut self,
        id: u32,
        _path: String,
        _attrs: FileAttributes,
    ) -> Result<Status, Self::Error> {
        Ok(Self::ok(id))
    }

    async fn stat(
        &mut self,
        id: u32,
        path: String,
    ) -> Result<russh_sftp::protocol::Attrs, Self::Error> {
        let fs = self.fs.lock().await;
        if fs.files.contains_key(&path) || fs.dirs.contains_key(&path) {
            Ok(russh_sftp::protocol::Attrs {
                id,
                attrs: FileAttributes::default(),
            })
        } else {
            Err(StatusCode::NoSuchFile)
        }
    }

    async fn lstat(
        &mut self,
        id: u32,
        path: String,
    ) -> Result<russh_sftp::protocol::Attrs, Self::Error> {
        self.stat(id, path).await
    }

    async fn remove(&mut self, id: u32, filename: String) -> Result<Status, Self::Error> {
        self.fs.lock().await.files.remove(&filename);
        Ok(Self::ok(id))
    }

    async fn rmdir(&mut self, id: u32, path: String) -> Result<Status, Self::Error> {
        self.fs.lock().await.dirs.remove(&path);
        Ok(Self::ok(id))
    }

    async fn readlink(&mut self, id: u32, path: String) -> Result<Name, Self::Error> {
        let fs = self.fs.lock().await;
        match fs.links.get(&path) {
            Some(target) => Ok(Name {
                id,
                files: vec![File::dummy(target.clone())],
            }),
            None => Err(StatusCode::NoSuchFile),
        }
    }
}

// ----------------------------------------------------------------------------
// Harness: start the server on an ephemeral port and connect a client.
// ----------------------------------------------------------------------------

/// Process-wide host key shared by every fixture server.
///
/// One key, not one per server: all tests append to the same `known_hosts`
/// file, and the kernel recycles ephemeral loopback ports within a single
/// run. A recycled port whose new server presented a *different* key would
/// read as a changed key — rejected under every policy (`Unknown server
/// key`), failing whichever test drew the reused port.
static HOST_KEY: LazyLock<PrivateKey> =
    LazyLock::new(|| PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).expect("host key"));

/// Starts the in-process server, returning its bound port and the shared FS.
async fn start_server(fs: SharedFs) -> u16 {
    let config = Arc::new(russh::server::Config {
        auth_rejection_time: Duration::from_millis(1),
        keys: vec![HOST_KEY.clone()],
        ..Default::default()
    });
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.expect("bind");
    let port = listener.local_addr().expect("addr").port();

    tokio::spawn(async move {
        let mut server = TestServer { fs };
        // Accept loop: serve every incoming connection.
        loop {
            let Ok((stream, addr)) = listener.accept().await else {
                break;
            };
            let handler = server.new_client(Some(addr));
            let cfg = config.clone();
            tokio::spawn(async move {
                let _ = russh::server::run_stream(cfg, stream, handler).await;
            });
        }
    });

    port
}

/// Per-process temp directory holding this test binary's `known_hosts`.
///
/// The in-process fixture generates a fresh ephemeral host key each run and
/// loopback ports get reused over time, so with `AutoAdd` against the default
/// `~/.ssh/known_hosts` every run would append a `[127.0.0.1]:<port>` entry and
/// eventually collide (`Unknown server key`/`CHANGED`). Pointing every test
/// connection at this temp file keeps the developer's real file untouched.
static TEST_KNOWN_HOSTS_DIR: LazyLock<tempfile::TempDir> =
    LazyLock::new(|| tempfile::tempdir().expect("create temp known_hosts dir"));

/// Path to the shared per-process temp `known_hosts` file.
fn test_known_hosts() -> PathBuf {
    TEST_KNOWN_HOSTS_DIR.path().join("known_hosts")
}

async fn connect(port: u16, timeout: CommandTimeout) -> SshConnection {
    SshConnection::connect(
        "127.0.0.1",
        port,
        HostKeyPolicy::AutoAdd,
        timeout,
        Some(test_known_hosts()),
    )
    .await
    .expect("connect")
}

/// Builds a [`Target`] whose live connection is a real [`SshConnection`] to the
/// in-process fixture on `port`, labelled `name` and gated by `state`/`mode`.
///
/// This is the multi-target seam the P2.5 fan-out / P2.6 lock integration tests
/// need: several `Target`s can point at one shared server (one `SharedFs`), so
/// `HostsGroup::run` and the remote-lock protocol are exercised over real SSH
/// rather than a `MockConnection`.
async fn connect_target(name: &str, port: u16, state: TargetState, mode: ExecutionMode) -> Target {
    let conn = connect(port, CommandTimeout::from_secs(5)).await;
    Target::with_connection(name, state, mode, Box::new(conn))
}

// ----------------------------------------------------------------------------
// Tests.
// ----------------------------------------------------------------------------

#[tokio::test]
async fn connect_and_run_captures_stdout_and_exit_code() {
    let fs = SharedFs::default();
    let port = start_server(fs).await;
    let mut conn = connect(port, CommandTimeout::from_secs(5)).await;

    assert!(conn.is_active());
    let log = conn.run("echo hello").await.expect("run");
    assert_eq!(log.stdout, "hello\n");
    assert_eq!(log.exitcode, 0);
    assert_eq!(log.command, "echo hello");

    conn.close().await.expect("close");
    assert!(!conn.is_active());
}

#[tokio::test]
async fn run_captures_nonzero_exit_code() {
    let port = start_server(SharedFs::default()).await;
    let mut conn = connect(port, CommandTimeout::from_secs(5)).await;
    let log = conn.run("exit 3").await.expect("run");
    assert_eq!(log.exitcode, 3);
}

#[tokio::test]
async fn run_captures_stderr() {
    let port = start_server(SharedFs::default()).await;
    let mut conn = connect(port, CommandTimeout::from_secs(5)).await;
    let log = conn.run("to-stderr").await.expect("run");
    assert_eq!(log.stderr, "boom\n");
    assert_eq!(log.exitcode, 1);
}

#[tokio::test]
async fn run_times_out_on_silent_command() {
    let port = start_server(SharedFs::default()).await;
    // Very short window: the silent "sleep-forever" produces no output/exit.
    let mut conn = connect(port, CommandTimeout::new(Duration::from_millis(300))).await;
    let err = conn
        .run("sleep-forever")
        .await
        .expect_err("should time out");
    assert!(
        matches!(&err, mtui_hosts::HostError::Timeout { command } if command == "sleep-forever"),
        "unexpected error: {err:?}"
    );
}

#[tokio::test]
async fn run_truncates_oversized_stdout() {
    let port = start_server(SharedFs::default()).await;
    let mut conn = connect(port, CommandTimeout::from_secs(5)).await;
    let log = conn.run("flood-stdout").await.expect("run");
    // Captured output is capped, not the full flood, and the flag is set.
    assert_eq!(log.stdout.len(), MAX_STREAM_BYTES);
    assert!(log.stdout.len() < FLOOD_BYTES);
    assert!(log.truncated, "truncation flag should be set");
    assert!(!log.timed_out);
    assert_eq!(log.exitcode, 0);
}

#[tokio::test]
async fn run_aborts_continuous_output_at_absolute_deadline() {
    let port = start_server(SharedFs::default()).await;
    // Headless connection (no timeout prompt): a short inactivity window makes
    // the absolute deadline (window * COMMAND_DEADLINE_FACTOR) small, so the
    // trickling command — which never lets the inactivity window fire — is
    // stopped by the deadline rather than running forever. Connect with a normal
    // handshake timeout, then narrow only the *command* window: the short value
    // must not also bound the loopback handshake (which can exceed 50ms on a
    // loaded CI runner, failing the connect before the test even runs).
    let mut conn = connect(port, CommandTimeout::from_secs(5))
        .await
        .with_command_timeout(CommandTimeout::new(Duration::from_millis(50)));
    let err = tokio::time::timeout(Duration::from_secs(10), conn.run("trickle-forever"))
        .await
        .expect("run must return before the test-level timeout")
        .expect_err("continuous output should hit the absolute deadline");
    assert!(
        matches!(&err, mtui_hosts::HostError::Timeout { command } if command == "trickle-forever"),
        "unexpected error: {err:?}"
    );
}

#[tokio::test]
async fn run_small_output_is_not_flagged() {
    let port = start_server(SharedFs::default()).await;
    let mut conn = connect(port, CommandTimeout::from_secs(5)).await;
    let log = conn.run("echo hello").await.expect("run");
    assert_eq!(log.stdout, "hello\n");
    assert!(!log.truncated);
    assert!(!log.timed_out);
}

#[tokio::test]
async fn sftp_put_then_get_round_trips() {
    let fs = SharedFs::default();
    let port = start_server(fs).await;
    let mut conn = connect(port, CommandTimeout::from_secs(5)).await;

    let dir = tempfile::tempdir().expect("tmp");
    let local_src = dir.path().join("src.sh");
    let local_dst = dir.path().join("dst.sh");
    tokio::fs::write(&local_src, b"#!/bin/sh\necho hi\n")
        .await
        .expect("write local");

    conn.sftp_put(&local_src, std::path::Path::new("/remote/dir/src.sh"))
        .await
        .expect("put");
    conn.sftp_get(std::path::Path::new("/remote/dir/src.sh"), &local_dst)
        .await
        .expect("get");

    let got = tokio::fs::read(&local_dst).await.expect("read back");
    assert_eq!(got, b"#!/bin/sh\necho hi\n");
}

/// Regression for the SFTP "No such file" upload bug: a put to a *fresh*
/// (non-existent) remote path must succeed. The old code opened WRITE-only
/// (no CREATE), so a real sshd returned SSH_FX_NO_SUCH_FILE on every first
/// upload. A >64KB payload also exercises the client's multi-chunk write path.
#[tokio::test]
async fn sftp_put_creates_fresh_remote_path() {
    let fs = SharedFs::default();
    let port = start_server(fs.clone()).await;
    let mut conn = connect(port, CommandTimeout::from_secs(5)).await;

    let dir = tempfile::tempdir().expect("tmp");
    let local_src = dir.path().join("payload.bin");
    // Comfortably past a single 64KB SFTP write chunk.
    let payload: Vec<u8> = (0..80 * 1024).map(|i| (i % 251) as u8).collect();
    tokio::fs::write(&local_src, &payload)
        .await
        .expect("write local");

    // The remote path does not yet exist; the put must create it.
    let remote = std::path::Path::new("/fresh/dir/payload.bin");
    conn.sftp_put(&local_src, remote)
        .await
        .expect("put to fresh path");

    // The bytes landed intact through the multi-chunk write path.
    let stored = fs
        .lock()
        .await
        .files
        .get("/fresh/dir/payload.bin")
        .cloned()
        .expect("fresh file created on the remote");
    assert_eq!(stored, payload);
}

#[tokio::test]
async fn sftp_open_reads_remote_bytes() {
    let fs = SharedFs::default();
    fs.lock()
        .await
        .files
        .insert("/etc/os-release".to_owned(), b"SLES\n".to_vec());
    let port = start_server(fs).await;
    let mut conn = connect(port, CommandTimeout::from_secs(5)).await;

    let bytes = conn
        .sftp_open(std::path::Path::new("/etc/os-release"))
        .await
        .expect("open");
    assert_eq!(bytes, b"SLES\n");
}

#[tokio::test]
async fn sftp_write_exclusive_then_overwrite_and_read_back() {
    let fs = SharedFs::default();
    let port = start_server(fs).await;
    let mut conn = connect(port, CommandTimeout::from_secs(5)).await;

    let lock = std::path::Path::new("/var/lock/mtui.lock");

    // 1) Atomic exclusive create wins on a free path.
    conn.sftp_write(lock, b"1700000000:alice:42", true)
        .await
        .expect("exclusive create wins on free path");

    // 2) A second exclusive create loses the race (file now exists).
    let err = conn
        .sftp_write(lock, b"1700000000:bob:99", true)
        .await
        .expect_err("second exclusive create must collide");
    assert!(
        matches!(err, mtui_hosts::HostError::AlreadyExists { .. }),
        "expected AlreadyExists, got {err:?}"
    );

    // The winner's bytes survive the losing exclusive attempt.
    let after_collision = conn.sftp_open(lock).await.expect("read after collision");
    assert_eq!(after_collision, b"1700000000:alice:42");

    // 3) A non-exclusive overwrite ("w+") replaces the contents.
    conn.sftp_write(lock, b"1700000001:alice:42:comment", false)
        .await
        .expect("overwrite ok");
    let after_overwrite = conn.sftp_open(lock).await.expect("read after overwrite");
    assert_eq!(after_overwrite, b"1700000001:alice:42:comment");
}

#[tokio::test]
async fn sftp_append_creates_then_extends_at_eof_and_read_back() {
    let fs = SharedFs::default();
    let port = start_server(fs).await;
    let mut conn = connect(port, CommandTimeout::from_secs(5)).await;

    let log = std::path::Path::new("/var/log/mtui.log");

    // First append against a missing path creates the file (O_CREAT).
    conn.sftp_append(log, b"1700000000:alice:install\n")
        .await
        .expect("first append creates the file");
    // Subsequent appends land at EOF, preserving prior entries (O_APPEND) —
    // no read-modify-write, unlike the old emulation.
    conn.sftp_append(log, b"1700000001:bob:downgrade\n")
        .await
        .expect("second append extends at EOF");

    let back = conn.sftp_open(log).await.expect("read back");
    assert_eq!(
        back,
        b"1700000000:alice:install\n1700000001:bob:downgrade\n"
    );
}

#[tokio::test]
async fn sftp_listdir_returns_entries() {
    let fs = SharedFs::default();
    fs.lock().await.dirs.insert(
        "/var/log".to_owned(),
        vec!["a.log".to_owned(), "b.log".to_owned()],
    );
    let port = start_server(fs).await;
    let mut conn = connect(port, CommandTimeout::from_secs(5)).await;

    let mut entries = conn
        .sftp_listdir(std::path::Path::new("/var/log"))
        .await
        .expect("listdir");
    entries.sort();
    assert_eq!(entries, ["a.log", "b.log"]);
}

#[tokio::test]
async fn sftp_get_folder_suffixes_with_hostname() {
    let fs = SharedFs::default();
    {
        let mut g = fs.lock().await;
        g.dirs
            .insert("/logs".to_owned(), vec!["out.txt".to_owned()]);
        g.files.insert("/logs/out.txt".to_owned(), b"data".to_vec());
    }
    let port = start_server(fs).await;
    let mut conn = connect(port, CommandTimeout::from_secs(5)).await;

    let dir = tempfile::tempdir().expect("tmp");
    // Trailing slash: upstream builds "<local>/<name>.<hostname>".
    let local = format!("{}/", dir.path().to_string_lossy());
    conn.sftp_get_folder(std::path::Path::new("/logs"), std::path::Path::new(&local))
        .await
        .expect("get_folder");

    // The per-host suffix is the workflow contract.
    let expected = dir.path().join("out.txt.127.0.0.1");
    let got = tokio::fs::read(&expected)
        .await
        .expect("suffixed file must exist");
    assert_eq!(got, b"data");
}

/// Many entries download under bounded concurrency: every file lands with the
/// correct per-host suffix and content regardless of completion order. Guards
/// the `buffer_unordered` folder path against dropping or misnaming entries.
#[tokio::test]
async fn sftp_get_folder_downloads_many_entries_with_correct_suffixes() {
    let fs = SharedFs::default();
    let names: Vec<String> = (0..12).map(|i| format!("f{i}.log")).collect();
    {
        let mut g = fs.lock().await;
        g.dirs.insert("/logs".to_owned(), names.clone());
        for (i, n) in names.iter().enumerate() {
            g.files
                .insert(format!("/logs/{n}"), format!("body-{i}").into_bytes());
        }
    }
    let port = start_server(fs).await;
    let mut conn = connect(port, CommandTimeout::from_secs(5)).await;

    let dir = tempfile::tempdir().expect("tmp");
    let local = format!("{}/", dir.path().to_string_lossy());
    conn.sftp_get_folder(std::path::Path::new("/logs"), std::path::Path::new(&local))
        .await
        .expect("get_folder");

    for (i, n) in names.iter().enumerate() {
        let expected = dir.path().join(format!("{n}.127.0.0.1"));
        let got = tokio::fs::read(&expected)
            .await
            .unwrap_or_else(|e| panic!("{} must exist: {e}", expected.display()));
        assert_eq!(got, format!("body-{i}").into_bytes());
    }
}

#[tokio::test]
async fn connect_to_unreachable_host_maps_to_connect_error() {
    // Port 1 on localhost: nothing listens -> connection refused. A short
    // timeout keeps the test fast.
    let err = SshConnection::connect(
        "127.0.0.1",
        1,
        HostKeyPolicy::AutoAdd,
        CommandTimeout::new(Duration::from_millis(500)),
        Some(test_known_hosts()),
    )
    .await
    .expect_err("should fail to connect");
    assert!(
        matches!(&err, mtui_hosts::HostError::Connect { host, .. } if host == "127.0.0.1"),
        "unexpected error: {err:?}"
    );
}

/// Regression for the known_hosts test-isolation bug: `AutoAdd` must land the
/// fixture's host key in the per-test temp file, and must never touch the
/// developer's real `~/.ssh/known_hosts`.
#[tokio::test]
async fn connect_isolates_known_hosts_from_real_home() {
    // Snapshot the real file's contents (or absence) before connecting.
    let real = std::env::var_os("HOME")
        .map(PathBuf::from)
        .map(|h| h.join(".ssh").join("known_hosts"));
    let before = real.as_ref().and_then(|p| std::fs::read(p).ok());

    let port = start_server(SharedFs::default()).await;
    let mut conn = connect(port, CommandTimeout::from_secs(5)).await;
    assert!(conn.is_active());
    conn.close().await.expect("close");

    // AutoAdd persisted the loopback key into the temp file, not the home file.
    let temp = std::fs::read_to_string(test_known_hosts())
        .expect("temp known_hosts should exist after AutoAdd");
    assert!(
        temp.contains("127.0.0.1"),
        "temp known_hosts missing loopback entry: {temp:?}"
    );

    // The real file is byte-for-byte unchanged (still present/absent as before).
    let after = real.as_ref().and_then(|p| std::fs::read(p).ok());
    assert_eq!(
        before, after,
        "connect() must not modify the real ~/.ssh/known_hosts"
    );
}

#[tokio::test]
async fn fire_and_forget_dispatches_and_closes() {
    let port = start_server(SharedFs::default()).await;
    let mut conn = connect(port, CommandTimeout::from_secs(5)).await;
    assert!(conn.is_active());
    conn.fire_and_forget("reboot").await.expect("dispatch");
    // The local link is torn down after dispatch, matching upstream.
    assert!(!conn.is_active());
}

#[tokio::test]
async fn reconnect_reestablishes_after_close() {
    let port = start_server(SharedFs::default()).await;
    let mut conn = connect(port, CommandTimeout::from_secs(5)).await;
    conn.close().await.expect("close");
    assert!(!conn.is_active());
    conn.reconnect(0, false).await.expect("reconnect");
    assert!(conn.is_active());
    // Still usable after reconnect.
    let log = conn.run("echo hello").await.expect("run after reconnect");
    assert_eq!(log.stdout, "hello\n");
}

#[tokio::test]
async fn run_reconnects_when_link_dropped() {
    let port = start_server(SharedFs::default()).await;
    let mut conn = connect(port, CommandTimeout::from_secs(5)).await;
    // Simulate a dropped link: close, then run() must transparently reconnect
    // (its open->reconnect retry loop) and still return a result.
    conn.close().await.expect("close");
    let log = conn.run("echo hello").await.expect("run should recover");
    assert_eq!(log.stdout, "hello\n");
}

#[tokio::test]
async fn sftp_rmdir_removes_children_then_dir() {
    let fs = SharedFs::default();
    {
        let mut g = fs.lock().await;
        g.dirs
            .insert("/d".to_owned(), vec!["c1".to_owned(), "c2".to_owned()]);
        g.files.insert("/d/c1".to_owned(), b"1".to_vec());
        g.files.insert("/d/c2".to_owned(), b"2".to_vec());
    }
    let port = start_server(fs.clone()).await;
    let mut conn = connect(port, CommandTimeout::from_secs(5)).await;

    conn.sftp_rmdir(std::path::Path::new("/d"))
        .await
        .expect("rmdir");
    let g = fs.lock().await;
    assert!(!g.dirs.contains_key("/d"));
    assert!(!g.files.contains_key("/d/c1"));
    assert!(!g.files.contains_key("/d/c2"));
}

#[tokio::test]
async fn sftp_remove_and_readlink() {
    let fs = SharedFs::default();
    {
        let mut g = fs.lock().await;
        g.files.insert("/tmp/f".to_owned(), b"x".to_vec());
        g.links
            .insert("/tmp/link".to_owned(), "/tmp/target".to_owned());
    }
    let port = start_server(fs.clone()).await;
    let mut conn = connect(port, CommandTimeout::from_secs(5)).await;

    conn.sftp_remove(std::path::Path::new("/tmp/f"))
        .await
        .expect("remove");
    assert!(!fs.lock().await.files.contains_key("/tmp/f"));

    let target = conn
        .sftp_readlink(std::path::Path::new("/tmp/link"))
        .await
        .expect("readlink");
    assert_eq!(target, "/tmp/target");
}

#[cfg(feature = "shell")]
#[tokio::test]
async fn shell_spawns_pty_and_bridges_bytes() {
    use mtui_hosts::ShellChannel;

    let fs = SharedFs::default();
    let port = start_server(fs).await;
    let mut conn = connect(port, CommandTimeout::from_secs(5)).await;

    let mut ch: Box<dyn ShellChannel> = conn.shell(80, 24).await.expect("shell spawns");

    // Read the server greeting through a buffer smaller than the frame, forcing
    // the leftover-carryover path: no bytes may be lost across the two reads.
    let mut small = [0u8; 4];
    let n = ch.read(&mut small).await.expect("read greeting part 1");
    assert_eq!(&small[..n], b"welc");
    let n = ch.read(&mut small).await.expect("read greeting part 2");
    assert_eq!(&small[..n], b"ome\n");

    let mut buf = [0u8; 64];

    // Keystrokes are echoed back.
    ch.write(b"hi").await.expect("write");
    let n = ch.read(&mut buf).await.expect("read echo");
    assert_eq!(&buf[..n], b"hi");

    // A resize forwards a window-change without error.
    ch.resize(120, 40).await.expect("resize");

    // Ctrl-D closes the remote shell; the next read observes EOF (0).
    ch.write(&[0x04]).await.expect("write ctrl-d");
    let n = ch.read(&mut buf).await.expect("read eof");
    assert_eq!(n, 0, "channel close surfaces as EOF");
}

// ----------------------------------------------------------------------------
// HostsGroup fan-out over real SSH targets (P2.5 DoD).
//
// The colocated `hostgroup.rs` unit tests drive fan-out over `MockConnection`.
// These prove the same fan-out end-to-end over the in-process russh server:
// `run` reaches every enabled member across ≥2 real SSH sessions, in both
// parallel and serial modes, and per-host `TargetState` gating is honoured
// against a live transport — the epic's "run across ≥2 hosts against a local
// sshd fixture" line.
// ----------------------------------------------------------------------------

/// The scripted stdout the fixture returns for an otherwise-unknown command
/// (see `scripted_command`'s `other` arm) — asserted on to prove a command
/// actually reached the remote host.
fn ran(cmd: &str) -> String {
    format!("ran: {cmd}\n")
}

#[tokio::test]
async fn hostsgroup_run_parallel_reaches_every_member() {
    // Two enabled targets in Parallel mode share one fixture server.
    let port = start_server(SharedFs::default()).await;
    let targets = vec![
        connect_target("h1", port, TargetState::Enabled, ExecutionMode::Parallel).await,
        connect_target("h2", port, TargetState::Enabled, ExecutionMode::Parallel).await,
    ];
    let mut group = HostsGroup::new(targets, false);

    group.run("whoami-probe").await;

    // Every member ran the command and recorded the fixture's scripted stdout.
    for name in ["h1", "h2"] {
        let t = group.get(name).expect("member present");
        assert_eq!(t.lastout(), ran("whoami-probe"), "{name} ran the command");
        assert_eq!(t.lastexit(), Some(0), "{name} recorded exit 0");
    }
}

#[tokio::test]
async fn hostsgroup_run_serial_reaches_every_member() {
    // Same as above but Serial mode: the fan-out runs hosts one at a time; the
    // observable outcome (every host ran, results recorded) is identical.
    let port = start_server(SharedFs::default()).await;
    let targets = vec![
        connect_target("s1", port, TargetState::Enabled, ExecutionMode::Serial).await,
        connect_target("s2", port, TargetState::Enabled, ExecutionMode::Serial).await,
    ];
    let mut group = HostsGroup::new(targets, false);

    group.run("serial-probe").await;

    for name in ["s1", "s2"] {
        let t = group.get(name).expect("member present");
        assert_eq!(t.lastout(), ran("serial-probe"), "{name} ran the command");
        assert_eq!(t.lastexit(), Some(0), "{name} recorded exit 0");
    }
}

#[tokio::test]
async fn hostsgroup_run_honours_per_host_state_end_to_end() {
    // A mixed-state group over real SSH: enabled runs for real, disabled
    // records nothing.
    let port = start_server(SharedFs::default()).await;
    let targets = vec![
        connect_target("on", port, TargetState::Enabled, ExecutionMode::Parallel).await,
        connect_target("off", port, TargetState::Disabled, ExecutionMode::Parallel).await,
    ];
    let mut group = HostsGroup::new(targets, false);

    group.run("gated").await;

    // Enabled: real remote output.
    let on = group.get("on").expect("on present");
    assert_eq!(on.lastout(), ran("gated"));
    assert_eq!(on.lastexit(), Some(0));

    // Disabled: an empty entry — nothing ran.
    let off = group.get("off").expect("off present");
    assert_eq!(off.lastout(), "");
    assert!(
        off.lastexit().is_none() || off.lastexit() == Some(0),
        "disabled host did not execute the command"
    );
}

// ----------------------------------------------------------------------------
// Remote lock lifecycle over the fixture's real SFTP (P2.6 DoD).
//
// `locks.rs` unit-tests the protocol over `MockConnection`. These prove the
// same protocol over the in-process server's real SFTP subsystem — crucially
// the atomic `O_EXCL` create the free-host claim relies on (the fixture's
// `open` honours `OpenFlags::EXCLUDE`), plus release and stale-reap round-trips
// through actual SFTP write/open/remove.
// ----------------------------------------------------------------------------

/// Builds a [`Config`] with a fixed session user and reaping enabled, so the
/// lock's identity and stale-age policy are deterministic in tests.
fn lock_config(user: &str) -> mtui_config::Config {
    let mut c = mtui_config::Config::default();
    c.session_user = user.to_owned();
    c.lock_reap_stale = true;
    c.lock_stale_age = 86_400;
    c.lock_wait = 0; // fail-fast on a live foreign lock (no real sleeps in tests)
    c
}

#[tokio::test]
async fn lock_acquire_release_round_trips_over_sftp() {
    let fs = SharedFs::default();
    let port = start_server(fs.clone()).await;
    let conn = connect(port, CommandTimeout::from_secs(5)).await;
    let mut lock = TargetLock::new(Box::new(conn), &lock_config("alice"));

    // Free host: the atomic exclusive create wins and the lockfile lands.
    assert!(!lock.is_locked().await.expect("is_locked"));
    lock.lock("mtui operation").await.expect("acquire");
    let contents = fs
        .lock()
        .await
        .files
        .get(TARGET_LOCK_PATH)
        .cloned()
        .expect("lockfile written over sftp");
    let line = String::from_utf8(contents).expect("utf8");
    // Wire format is `timestamp:user:pid:comment`; assert the fields the claim
    // controls (user + comment) survived the real SFTP write.
    let fields: Vec<&str> = line.splitn(4, ':').collect();
    assert_eq!(fields.get(1), Some(&"alice"), "lock owned by alice: {line}");
    assert_eq!(
        fields.get(3),
        Some(&"mtui operation"),
        "comment kept: {line}"
    );

    // Release removes the lockfile through real SFTP `remove`.
    lock.unlock(false).await.expect("release");
    assert!(
        !fs.lock().await.files.contains_key(TARGET_LOCK_PATH),
        "lockfile removed on unlock"
    );
    assert!(!lock.is_locked().await.expect("is_locked after unlock"));
}

#[tokio::test]
async fn lock_refuses_fresh_foreign_lock_over_sftp() {
    // Seed a fresh foreign lock directly in the fixture FS, then prove a
    // different owner cannot take it (fail-fast, wait=0) — the atomic create
    // loses to the existing file and reconciliation refuses.
    let fs = SharedFs::default();
    let recent = "9999999999"; // far-future timestamp => never stale
    fs.lock().await.files.insert(
        TARGET_LOCK_PATH.to_owned(),
        format!("{recent}:bob:4242:mtui operation").into_bytes(),
    );
    let port = start_server(fs.clone()).await;
    let conn = connect(port, CommandTimeout::from_secs(5)).await;
    let mut lock = TargetLock::new(Box::new(conn), &lock_config("alice"));

    assert!(lock.is_locked().await.expect("is_locked"));
    let err = lock.lock("mine").await.expect_err("foreign lock refused");
    assert!(matches!(err, mtui_hosts::HostError::TargetLocked(_)));
    // The foreign lockfile is untouched.
    let line = String::from_utf8(
        fs.lock()
            .await
            .files
            .get(TARGET_LOCK_PATH)
            .cloned()
            .expect("foreign lock still present"),
    )
    .unwrap();
    assert!(line.contains(":bob:"), "foreign owner preserved: {line}");
}

#[tokio::test]
async fn lock_reaps_stale_foreign_lock_over_sftp() {
    // A very old foreign lock is reaped, then re-taken by us — proving the
    // stale-reap `remove` + fresh exclusive create both work over real SFTP.
    let fs = SharedFs::default();
    let stale = "1"; // 1970 => older than any stale-age
    fs.lock().await.files.insert(
        TARGET_LOCK_PATH.to_owned(),
        format!("{stale}:bob:4242").into_bytes(),
    );
    let port = start_server(fs.clone()).await;
    let conn = connect(port, CommandTimeout::from_secs(5)).await;
    let mut lock = TargetLock::new(Box::new(conn), &lock_config("alice"));

    // try_claim reaps the stale lock and then claims it for us.
    assert!(
        lock.try_claim("mtui operation").await.expect("try_claim"),
        "stale foreign lock reaped and re-claimed"
    );
    let line = String::from_utf8(
        fs.lock()
            .await
            .files
            .get(TARGET_LOCK_PATH)
            .cloned()
            .expect("re-claimed lockfile present"),
    )
    .unwrap();
    assert!(line.contains(":alice:"), "now owned by alice: {line}");
}
