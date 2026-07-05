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
use std::sync::Arc;
use std::time::Duration;

use russh::keys::{Algorithm, PrivateKey};
use russh::server::{Auth, Msg, Server as _, Session};
use russh::{Channel, ChannelId};
use russh_sftp::protocol::{Data, File, FileAttributes, Handle, Name, Status, StatusCode, Version};
use tokio::net::TcpListener;
use tokio::sync::Mutex;

use mtui_hosts::{CommandTimeout, Connection, HostKeyPolicy, SshConnection};

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
        }
    }
}

struct TestSshSession {
    fs: SharedFs,
    channels: Arc<Mutex<HashMap<ChannelId, Channel<Msg>>>>,
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
            };
            russh_sftp::server::run(channel.into_stream(), handler).await;
        } else {
            session.channel_failure(channel_id)?;
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
        _pflags: russh_sftp::protocol::OpenFlags,
        _attrs: FileAttributes,
    ) -> Result<Handle, Self::Error> {
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
        let mut fs = self.fs.lock().await;
        let buf = fs.files.entry(handle).or_default();
        let start = offset as usize;
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

/// Starts the in-process server, returning its bound port and the shared FS.
async fn start_server(fs: SharedFs) -> u16 {
    let key = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).expect("host key");
    let config = Arc::new(russh::server::Config {
        auth_rejection_time: Duration::from_millis(1),
        keys: vec![key],
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

async fn connect(port: u16, timeout: CommandTimeout) -> SshConnection {
    SshConnection::connect("127.0.0.1", port, HostKeyPolicy::AutoAdd, timeout)
        .await
        .expect("connect")
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

#[tokio::test]
async fn connect_to_unreachable_host_maps_to_connect_error() {
    // Port 1 on localhost: nothing listens -> connection refused. A short
    // timeout keeps the test fast.
    let err = SshConnection::connect(
        "127.0.0.1",
        1,
        HostKeyPolicy::AutoAdd,
        CommandTimeout::new(Duration::from_millis(500)),
    )
    .await
    .expect_err("should fail to connect");
    assert!(
        matches!(&err, mtui_hosts::HostError::Connect { host, .. } if host == "127.0.0.1"),
        "unexpected error: {err:?}"
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
    conn.reconnect().await.expect("reconnect");
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
