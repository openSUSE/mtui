//! Integration tests for the russh-backed [`SshConnection`].
//!
//! These run **offline** against an ephemeral in-process russh server with a
//! freshly generated host key — no Docker, no external sshd — implementing just
//! enough `exec` (scripted stdout/stderr/exit per command) and SFTP (over an
//! in-memory filesystem) to exercise every `Connection` method. Auth accepts
//! any offered public key, proving the pubkey path end-to-end without a real
//! key store.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use russh::keys::{Algorithm, PrivateKey};
use russh::server::{Auth, Msg, Server as _, Session};
use russh::{Channel, ChannelId};
use russh_sftp::protocol::{
    Data, File, FileAttributes, Handle, Name, OpenFlags, Packet, Status, StatusCode, Version,
};
use tokio::net::TcpListener;
use tokio::sync::Mutex;

use mtui_hosts::{
    CommandTimeout, Connection, HostKeyPolicy, HostsGroup, MAX_STREAM_BYTES, SshConnection,
    TARGET_LOCK_PATH, Target, TargetLock,
};
use mtui_types::enums::TargetState;

/// How long the `quiet-then-exit` scripted command stays silent.
const QUIET_COMMAND_DURATION: Duration = Duration::from_millis(2500);

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
    /// path -> size attribute `fstat` reports instead of the stored length;
    /// `None` omits the attribute.
    size_attrs: HashMap<String, Option<u64>>,
    /// path -> modification time `stat` reports; absent omits the attribute.
    mtimes: HashMap<String, u32>,
    /// `(offset, len)` of every `read` request received.
    reads: Vec<(u64, u32)>,
    /// Total bytes served by `read`.
    read_bytes: usize,
    /// Paths whose `read` requests fail with a generic status.
    failing_reads: std::collections::HashSet<String>,
    /// Number of `open` requests received per path, across every subsystem.
    open_counts: HashMap<String, usize>,
    /// Paths whose `open` is answered with this status, before any effect.
    open_status: HashMap<String, StatusCode>,
    /// Queued holds per path, one consumed per `open`: it applies its effects,
    /// then withholds its reply.
    open_holds: HashMap<String, std::collections::VecDeque<OpenHold>>,
    /// Session handle and channel id of the most recent SFTP subsystem, so a
    /// test can close that channel from the server side.
    sftp_channel: Option<(russh::server::Handle, ChannelId)>,
    /// Permission bits per path, from the `open` and `fsetstat` attributes.
    modes: HashMap<String, u32>,
    /// `(path, flags, permissions)` of every `open`, in arrival order.
    opens: Vec<(String, OpenFlags, Option<u32>)>,
    /// `(path, permissions)` of every `mkdir`, in arrival order.
    mkdirs: Vec<(String, Option<u32>)>,
    /// `(from, to)` of every `posix-rename@openssh.com` received.
    posix_renames: Vec<(String, String)>,
    /// Leaves `posix-rename@openssh.com` out of the INIT reply.
    no_posix_rename: bool,
    /// How the server answers `posix-rename@openssh.com`.
    rename_reply: RenameReply,
}

/// The server's answer to `posix-rename@openssh.com`.
#[derive(Default, Clone, Copy)]
enum RenameReply {
    /// Rename, then report success.
    #[default]
    Apply,
    /// Not implemented: the handler's default `OpUnsupported`.
    Unsupported,
    /// Rename, then never reply, as a link that dies after the effect.
    ApplyThenSilence,
}

/// Lets a test observe an `open`'s filesystem effects while its reply is still
/// withheld, so a lost reply can be modelled after the file has appeared.
#[derive(Clone)]
struct OpenHold {
    /// Signalled once the `open`'s effects are visible in the filesystem.
    landed: Arc<tokio::sync::Notify>,
    /// The `open` replies only after this is signalled.
    release: Arc<tokio::sync::Notify>,
    /// Applies only to a non-exclusive truncating open, so reads and the
    /// exclusive create of the same path pass through.
    truncating_only: bool,
}

impl OpenHold {
    fn new() -> Self {
        Self {
            landed: Arc::new(tokio::sync::Notify::new()),
            release: Arc::new(tokio::sync::Notify::new()),
            truncating_only: false,
        }
    }
}

type SharedFs = Arc<Mutex<FakeFs>>;

/// Per-request delay knob for the SFTP handler, armable after the server is up.
/// One-shot: `open` consumes it the instant it reads a nonzero value, *before*
/// sleeping, so a retry right behind a timed-out request is not also delayed.
type SharedDelay = Arc<Mutex<Duration>>;

/// Every `exec` request the server received, verbatim: what the client really
/// sent, independent of what it chose to log.
static EXEC_LOG: LazyLock<std::sync::Mutex<Vec<String>>> =
    LazyLock::new(|| std::sync::Mutex::new(Vec::new()));

/// Counts `subsystem_request(name == "sftp")` calls server-side, so a test can
/// assert exactly how many SFTP handshakes a sequence of client verbs caused.
type SharedSubsystemCount = Arc<AtomicUsize>;

// ----------------------------------------------------------------------------
// SSH server: exec + sftp subsystem.
// ----------------------------------------------------------------------------

#[derive(Clone)]
struct TestServer {
    fs: SharedFs,
    delay: SharedDelay,
    subsystem_count: SharedSubsystemCount,
}

impl russh::server::Server for TestServer {
    type Handler = TestSshSession;

    fn new_client(&mut self, _: Option<SocketAddr>) -> Self::Handler {
        TestSshSession {
            fs: self.fs.clone(),
            delay: self.delay.clone(),
            subsystem_count: self.subsystem_count.clone(),
            channels: Arc::new(Mutex::new(HashMap::new())),
            #[cfg(feature = "shell")]
            shell_channels: Arc::new(Mutex::new(std::collections::HashSet::new())),
        }
    }
}

struct TestSshSession {
    fs: SharedFs,
    delay: SharedDelay,
    subsystem_count: SharedSubsystemCount,
    channels: Arc<Mutex<HashMap<ChannelId, Channel<Msg>>>>,
    /// Channels that requested an interactive shell, so `data` echoes keystrokes
    /// for shells only and leaves SFTP-subsystem data untouched.
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
        EXEC_LOG
            .lock()
            .expect("exec log lock")
            .push(command.clone());

        // Never produces output and never exits: the client's no-output timeout
        // is the only thing that can end it.
        if command == "sleep-forever" || command.starts_with("sleep-forever ") {
            return Ok(());
        }

        // Silent for a while, then exits cleanly. The wait runs in a spawned
        // task so this session loop stays free to answer keepalives.
        if command == "quiet-then-exit" {
            let handle = session.handle();
            tokio::spawn(async move {
                tokio::time::sleep(QUIET_COMMAND_DURATION).await;
                let _ = handle.exit_status_request(channel_id, 0).await;
                let _ = handle.eof(channel_id).await;
                let _ = handle.close(channel_id).await;
            });
            return Ok(());
        }

        // Writes an unterminated stderr line, then goes silent without an exit
        // status or Close, as a link dying mid-output does.
        if command == "partial-stderr-then-silent"
            || command.starts_with("partial-stderr-then-silent ")
        {
            session.extended_data(channel_id, 1, b"partial".to_vec())?;
            return Ok(());
        }

        // Reports an exit status, then goes silent without Close.
        if command == "exit-then-silent" {
            session.exit_status_request(channel_id, 3)?;
            return Ok(());
        }

        // Reports death by signal, then goes silent without Close.
        if command == "signal-then-silent" {
            session.exit_signal_request(channel_id, russh::Sig::KILL, false, "", "")?;
            return Ok(());
        }

        // Floods stdout past the client's capture cap, then exits cleanly.
        if command == "flood-stdout" || command.starts_with("flood-stdout ") {
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

        // Trickles forever: the continuous output keeps the inactivity window
        // from firing, so only the absolute deadline can stop it.
        if command == "trickle-forever" {
            loop {
                session.data(channel_id, b".".to_vec())?;
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }

        // Like `trickle-forever`, but the output is pushed from a spawned task
        // so it is actually delivered while the command runs. `flood-async`
        // never pauses, so the client always has a message ready.
        if command.starts_with("trickle-async ") || command.starts_with("flood-async ") {
            let pause = command
                .starts_with("trickle-async ")
                .then_some(Duration::from_millis(20));
            let handle = session.handle();
            tokio::spawn(async move {
                while handle.data(channel_id, b".".to_vec()).await.is_ok() {
                    if let Some(pause) = pause {
                        tokio::time::sleep(pause).await;
                    }
                }
            });
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
            self.subsystem_count.fetch_add(1, Ordering::SeqCst);
            self.fs.lock().await.sftp_channel = Some((session.handle(), channel_id));
            let channel = self.channels.lock().await.remove(&channel_id).unwrap();
            session.channel_success(channel_id)?;
            let handler = SftpHandler {
                fs: self.fs.clone(),
                delay: self.delay.clone(),
                listed: HashMap::new(),
                append: std::collections::HashSet::new(),
            };
            russh_sftp::server::run(channel.into_stream(), handler).await;
        } else {
            session.channel_failure(channel_id)?;
        }
        Ok(())
    }

    // A minimal interactive shell: greet, then echo keystrokes. The sentinel
    // `\x04` (Ctrl-D) closes the channel so the client read loop sees EOF.
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
        // SFTP-subsystem data is routed by russh_sftp's own runner and must not
        // be intercepted here.
        if !self.shell_channels.lock().await.contains(&channel_id) {
            return Ok(());
        }
        if data.contains(&0x04) {
            session.eof(channel_id)?;
            session.close(channel_id)?;
        } else {
            session.data(channel_id, data.to_vec())?;
        }
        Ok(())
    }
}

/// Decodes two SSH `string`s (u32 big-endian length, then bytes).
fn decode_two_strings(mut data: &[u8]) -> Option<(String, String)> {
    let mut next = || {
        let (len, rest) = data.split_first_chunk::<4>()?;
        let len = usize::try_from(u32::from_be_bytes(*len)).ok()?;
        let (text, rest) = rest.split_at_checked(len)?;
        data = rest;
        String::from_utf8(text.to_vec()).ok()
    };
    let first = next()?;
    let second = next()?;
    data.is_empty().then_some((first, second))
}

/// Maps a command string to a scripted `(stdout, stderr, exit_code)`.
fn scripted_command(cmd: &str) -> (String, String, u32) {
    match cmd {
        "id -u" => ("0\n".to_owned(), String::new(), 0),
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
    /// Slept before answering `open`, simulating a high-latency link.
    delay: SharedDelay,
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
        let mut version = Version::new();
        if !self.fs.lock().await.no_posix_rename {
            version
                .extensions
                .insert("posix-rename@openssh.com".to_owned(), "1".to_owned());
        }
        Ok(version)
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
        attrs: FileAttributes,
    ) -> Result<Handle, Self::Error> {
        self.fs
            .lock()
            .await
            .opens
            .push((filename.clone(), pflags, attrs.permissions));
        // Consumed before sleeping, so a retry issued while this call is still
        // asleep is not also delayed.
        *self
            .fs
            .lock()
            .await
            .open_counts
            .entry(filename.clone())
            .or_default() += 1;
        let delay = std::mem::replace(&mut *self.delay.lock().await, Duration::ZERO);
        if !delay.is_zero() {
            tokio::time::sleep(delay).await;
        }
        // Effects run under one lock acquisition so the O_EXCL presence check
        // and the create cannot interleave with another open.
        let hold = {
            let mut fs = self.fs.lock().await;
            if let Some(code) = fs.open_status.get(&filename) {
                return Err(*code);
            }
            if !pflags.contains(OpenFlags::CREATE) && !fs.files.contains_key(&filename) {
                return Err(StatusCode::NoSuchFile);
            }
            // O_EXCL against an existing file fails as a real sshd would — what
            // the lock protocol relies on.
            if pflags.contains(OpenFlags::EXCLUDE) && fs.files.contains_key(&filename) {
                return Err(StatusCode::Failure);
            }
            // A create materialises the file so a later exclusive create sees it
            // and a read observes the write; TRUNCATE resets existing contents.
            if pflags.contains(OpenFlags::CREATE) {
                if let (false, Some(mode)) = (fs.files.contains_key(&filename), attrs.permissions) {
                    fs.modes.insert(filename.clone(), mode);
                }
                if pflags.contains(OpenFlags::TRUNCATE) {
                    fs.files.insert(filename.clone(), Vec::new());
                } else {
                    fs.files.entry(filename.clone()).or_default();
                }
            }
            fs.open_holds
                .get_mut(&filename)
                .and_then(|q| match q.front() {
                    Some(h)
                        if h.truncating_only
                            && !(pflags.contains(OpenFlags::TRUNCATE)
                                && !pflags.contains(OpenFlags::EXCLUDE)) =>
                    {
                        None
                    }
                    Some(_) => q.pop_front(),
                    None => None,
                })
        };
        if let Some(hold) = hold {
            hold.landed.notify_one();
            hold.release.notified().await;
        }
        // russh-sftp's client does not seek to EOF, so O_APPEND has to be
        // tracked per handle here.
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
        let mut fs = self.fs.lock().await;
        fs.reads.push((offset, len));
        if fs.failing_reads.contains(&handle) {
            return Err(StatusCode::Failure);
        }
        let contents = fs.files.get(&handle).cloned().unwrap_or_default();
        let start = offset as usize;
        if start >= contents.len() {
            return Err(StatusCode::Eof);
        }
        let end = (start + len as usize).min(contents.len());
        fs.read_bytes += end - start;
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
        attrs: FileAttributes,
    ) -> Result<Status, Self::Error> {
        let mut fs = self.fs.lock().await;
        fs.mkdirs.push((path.clone(), attrs.permissions));
        if let Some(mode) = attrs.permissions {
            fs.modes.insert(path.clone(), mode);
        }
        fs.dirs.entry(path).or_default();
        Ok(Self::ok(id))
    }

    async fn fsetstat(
        &mut self,
        id: u32,
        handle: String,
        attrs: FileAttributes,
    ) -> Result<Status, Self::Error> {
        if let Some(mode) = attrs.permissions {
            self.fs.lock().await.modes.insert(handle, mode);
        }
        Ok(Self::ok(id))
    }

    async fn extended(
        &mut self,
        id: u32,
        request: String,
        data: Vec<u8>,
    ) -> Result<Packet, Self::Error> {
        let reply = self.fs.lock().await.rename_reply;
        if request != "posix-rename@openssh.com" || matches!(reply, RenameReply::Unsupported) {
            return Err(self.unimplemented());
        }
        let (from, to) = decode_two_strings(&data).ok_or(StatusCode::BadMessage)?;
        {
            let mut fs = self.fs.lock().await;
            fs.posix_renames.push((from.clone(), to.clone()));
            let moved = fs.files.remove(&from).ok_or(StatusCode::NoSuchFile)?;
            fs.files.insert(to.clone(), moved);
            if let Some(mode) = fs.modes.remove(&from) {
                fs.modes.insert(to, mode);
            }
        }
        if matches!(reply, RenameReply::ApplyThenSilence) {
            tokio::time::sleep(Duration::from_secs(3600)).await;
        }
        Ok(Packet::Status(Self::ok(id)))
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
                attrs: FileAttributes {
                    mtime: fs.mtimes.get(&path).copied(),
                    ..FileAttributes::default()
                },
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
        let fs = self.fs.lock().await;
        let permissions = if path == "/" || fs.dirs.contains_key(&path) {
            0o040_000 | fs.modes.get(&path).copied().unwrap_or(0o755)
        } else if fs.files.contains_key(&path) {
            0o100_000 | fs.modes.get(&path).copied().unwrap_or(0o644)
        } else {
            return Err(StatusCode::NoSuchFile);
        };
        Ok(russh_sftp::protocol::Attrs {
            id,
            attrs: FileAttributes {
                permissions: Some(permissions),
                uid: Some(0),
                ..FileAttributes::default()
            },
        })
    }

    async fn fstat(
        &mut self,
        id: u32,
        handle: String,
    ) -> Result<russh_sftp::protocol::Attrs, Self::Error> {
        let fs = self.fs.lock().await;
        let contents = fs.files.get(&handle).ok_or(StatusCode::NoSuchFile)?;
        let size = fs
            .size_attrs
            .get(&handle)
            .copied()
            .unwrap_or(Some(contents.len() as u64));
        Ok(russh_sftp::protocol::Attrs {
            id,
            attrs: FileAttributes {
                size,
                uid: Some(0),
                permissions: Some(0o100_000 | fs.modes.get(&handle).copied().unwrap_or(0o644)),
                ..FileAttributes::default()
            },
        })
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
/// One key, not one per server: all tests share a `known_hosts` file and the
/// kernel recycles ephemeral loopback ports within a run, so a recycled port
/// presenting a *different* key would read as a changed key — rejected under
/// every policy, failing whichever test drew the reused port.
static HOST_KEY: LazyLock<PrivateKey> =
    LazyLock::new(|| PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).expect("host key"));

/// Starts the in-process server, returning its bound port and the shared FS.
async fn start_server(fs: SharedFs) -> u16 {
    start_server_with_delay(fs).await.0
}

/// Like [`start_server`], but also returns the [`SharedDelay`] handle so a
/// test can arm a per-`open`-request delay after the server is up.
async fn start_server_with_delay(fs: SharedFs) -> (u16, SharedDelay) {
    let (port, delay, _count) = start_server_with_delay_and_counter(fs).await;
    (port, delay)
}

/// Like [`start_server_with_delay`], additionally returning the
/// [`SharedSubsystemCount`] so a test can assert how many SFTP handshakes a
/// sequence of client verbs caused.
async fn start_server_with_delay_and_counter(
    fs: SharedFs,
) -> (u16, SharedDelay, SharedSubsystemCount) {
    let delay: SharedDelay = Arc::new(Mutex::new(Duration::ZERO));
    let subsystem_count: SharedSubsystemCount = Arc::new(AtomicUsize::new(0));
    let config = Arc::new(russh::server::Config {
        auth_rejection_time: Duration::from_millis(1),
        keys: vec![HOST_KEY.clone()],
        ..Default::default()
    });
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.expect("bind");
    let port = listener.local_addr().expect("addr").port();

    let server_delay = delay.clone();
    let server_count = subsystem_count.clone();
    tokio::spawn(async move {
        let mut server = TestServer {
            fs,
            delay: server_delay,
            subsystem_count: server_count,
        };
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

    (port, delay, subsystem_count)
}

/// Per-process temp directory holding this test binary's `known_hosts`.
///
/// The fixture generates a fresh host key each run and loopback ports get
/// reused, so `AutoAdd` against the real `~/.ssh/known_hosts` would accumulate
/// `[127.0.0.1]:<port>` entries and eventually collide. A temp file keeps the
/// developer's real file untouched.
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
        timeout,
        Some(test_known_hosts()),
    )
    .await
    .expect("connect")
}

/// Builds a [`Target`] whose live connection is a real [`SshConnection`] to the
/// in-process fixture on `port`.
///
/// The multi-target seam: several `Target`s point at one shared server, so
/// `HostsGroup::run` and the remote-lock protocol run over real SSH rather than
/// a `MockConnection`.
async fn connect_target(name: &str, port: u16, state: TargetState) -> Target {
    let conn = connect(port, CommandTimeout::from_secs(5)).await;
    Target::with_connection(name, state, Box::new(conn))
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
async fn keepalive_keeps_a_quiet_command_alive() {
    let port = start_server(SharedFs::default()).await;
    let mut conn = connect(port, CommandTimeout::from_secs(5)).await;
    conn.close().await.expect("close");
    let mut conn = conn.with_transport_timers(
        Some(Duration::from_millis(200)),
        Some(Duration::from_secs(1)),
    );
    conn.reconnect(0, false).await.expect("reconnect");
    let log = tokio::time::timeout(Duration::from_secs(10), conn.run("quiet-then-exit"))
        .await
        .expect("run must return before the test-level timeout")
        .expect("run");
    assert_eq!(log.exitcode, 0, "stderr: {}", log.stderr);
    assert!(!log.stderr.contains("lost after"), "stderr: {}", log.stderr);
}

/// Runs `command` on a link whose inactivity timeout fires with no keepalives
/// to prevent it, so the session ends without a Close.
async fn run_on_a_dying_link(command: &str) -> mtui_types::hostlog::CommandLog {
    let port = start_server(SharedFs::default()).await;
    let mut conn = connect(port, CommandTimeout::from_secs(5)).await;
    conn.close().await.expect("close");
    let mut conn = conn.with_transport_timers(None, Some(Duration::from_secs(1)));
    conn.reconnect(0, false).await.expect("reconnect");
    tokio::time::timeout(Duration::from_secs(10), conn.run(command))
        .await
        .expect("run must return before the test-level timeout")
        .expect("run")
}

#[tokio::test]
async fn run_reports_a_session_lost_mid_command() {
    let log = run_on_a_dying_link("partial-stderr-then-silent").await;
    assert_eq!(log.exitcode, -1);
    let (output, note) = log
        .stderr
        .split_once('\n')
        .unwrap_or_else(|| panic!("note must start on its own line: {:?}", log.stderr));
    assert_eq!(output, "partial");
    assert!(
        note.starts_with("mtui: SSH session to 127.0.0.1 lost after ")
            && note.ends_with("s; the command may still be running\n"),
        "note: {note:?}"
    );
    assert!(!log.truncated);
}

#[tokio::test]
async fn run_does_not_claim_a_finished_command_may_still_be_running() {
    let log = run_on_a_dying_link("exit-then-silent").await;
    assert_eq!(log.exitcode, 3);
    assert!(!log.stderr.contains("still be running"), "{}", log.stderr);
}

#[tokio::test]
async fn run_does_not_claim_a_signalled_command_may_still_be_running() {
    let log = run_on_a_dying_link("signal-then-silent").await;
    assert!(!log.stderr.contains("still be running"), "{}", log.stderr);
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
    // Headless (no timeout prompt), so a short inactivity window makes the
    // absolute deadline small and stops the trickling command. Only the
    // *command* window is narrowed: the short value must not also bound the
    // loopback handshake, which can exceed 50ms on a loaded CI runner.
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

/// A put to a *fresh* (non-existent) remote path must succeed: opening
/// WRITE-only without CREATE makes a real sshd return SSH_FX_NO_SUCH_FILE on
/// every first upload. The >64KB payload also exercises multi-chunk writes.
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

    let remote = std::path::Path::new("/fresh/dir/payload.bin");
    conn.sftp_put(&local_src, remote)
        .await
        .expect("put to fresh path");

    // Intact through the multi-chunk write path.
    let stored = fs
        .lock()
        .await
        .files
        .get("/fresh/dir/payload.bin")
        .cloned()
        .expect("fresh file created on the remote");
    assert_eq!(stored, payload);
}

/// What the client really sends for an upload: an exclusive create inside a
/// private stage, mode set on the handle, then a POSIX rename onto the
/// destination, which itself is never opened.
#[tokio::test]
async fn sftp_put_stages_privately_and_publishes_with_posix_rename() {
    let fs = SharedFs::default();
    let dest = "/pub/upload.bin";
    fs.lock()
        .await
        .files
        .insert(dest.to_owned(), b"OLD".to_vec());
    let port = start_server(fs.clone()).await;
    let mut conn = connect(port, CommandTimeout::from_secs(5)).await;

    conn.sftp_put_bytes(b"NEW", std::path::Path::new(dest))
        .await
        .expect("put");

    let state = fs.lock().await;
    assert_eq!(state.files.get(dest).map(Vec::as_slice), Some(&b"NEW"[..]));
    assert_eq!(state.modes.get(dest), Some(&0o770));
    let [(path, flags, perms)] = state.opens.as_slice() else {
        panic!("exactly one open expected: {:?}", state.opens);
    };
    assert!(
        path.starts_with("/pub/.mtui-upload-") && path.ends_with("/payload"),
        "{path}"
    );
    assert_eq!(
        flags.bits(),
        (OpenFlags::CREATE | OpenFlags::WRITE | OpenFlags::EXCLUDE).bits()
    );
    assert_eq!(*perms, Some(0o600));
    let stage = path.trim_end_matches("/payload");
    assert!(
        state.mkdirs.contains(&(stage.to_owned(), Some(0o700))),
        "{:?}",
        state.mkdirs
    );
    assert_eq!(state.posix_renames, vec![(path.clone(), dest.to_owned())]);
    assert!(
        !state.dirs.contains_key(stage),
        "stage removed after publication"
    );
    assert!(state.files.keys().all(|k| !k.contains("/payload")));
    assert!(EXEC_LOG.lock().unwrap().iter().any(|c| c == "id -u"));
}

#[tokio::test]
async fn sftp_put_refuses_a_server_without_posix_rename_before_any_effect() {
    let fs = SharedFs::default();
    fs.lock().await.no_posix_rename = true;
    fs.lock()
        .await
        .files
        .insert("/pub/upload.bin".to_owned(), b"OLD".to_vec());
    let port = start_server(fs.clone()).await;
    let mut conn = connect(port, CommandTimeout::from_secs(5)).await;

    let err = conn
        .sftp_put_bytes(b"NEW", std::path::Path::new("/pub/upload.bin"))
        .await
        .expect_err("must fail closed");

    assert!(
        matches!(
            &err,
            mtui_hosts::HostError::UploadRefused {
                reason: mtui_hosts::UploadRefusal::PosixRenameUnavailable,
                host,
                ..
            } if host == "127.0.0.1"
        ),
        "{err:?}"
    );
    let state = fs.lock().await;
    assert!(
        state.opens.is_empty() && state.mkdirs.is_empty(),
        "no side effects"
    );
    assert_eq!(
        state.files.get("/pub/upload.bin").map(Vec::as_slice),
        Some(&b"OLD"[..])
    );
}

#[tokio::test]
async fn sftp_put_does_not_fall_back_when_the_rename_is_unsupported() {
    let fs = SharedFs::default();
    fs.lock().await.rename_reply = RenameReply::Unsupported;
    fs.lock()
        .await
        .files
        .insert("/pub/upload.bin".to_owned(), b"OLD".to_vec());
    let port = start_server(fs.clone()).await;
    let mut conn = connect(port, CommandTimeout::from_secs(5)).await;

    let err = conn
        .sftp_put_bytes(b"NEW", std::path::Path::new("/pub/upload.bin"))
        .await
        .expect_err("must fail closed");

    assert!(
        matches!(err, mtui_hosts::HostError::UploadFailed { .. }),
        "{err:?}"
    );
    let state = fs.lock().await;
    assert_eq!(
        state.files.get("/pub/upload.bin").map(Vec::as_slice),
        Some(&b"OLD"[..])
    );
    assert!(
        state
            .opens
            .iter()
            .all(|(p, ..)| p.contains(".mtui-upload-"))
    );
    assert!(
        state.files.keys().all(|k| !k.contains("/payload")),
        "stage cleaned"
    );
}

/// A rename whose reply never arrives may already have taken effect, so it is
/// reported as unknown and never sent again.
#[tokio::test]
async fn sftp_put_reports_an_unanswered_rename_as_indeterminate_without_retrying() {
    let fs = SharedFs::default();
    fs.lock().await.rename_reply = RenameReply::ApplyThenSilence;
    let port = start_server(fs.clone()).await;
    let mut conn = connect(port, CommandTimeout::from_secs(1)).await;

    let err = conn
        .sftp_put_bytes(b"NEW", std::path::Path::new("/pub/upload.bin"))
        .await
        .expect_err("no verdict");

    assert!(
        matches!(&err, mtui_hosts::HostError::UploadIndeterminate { path, .. } if path == "/pub/upload.bin"),
        "{err:?}"
    );
    let state = fs.lock().await;
    assert_eq!(state.posix_renames.len(), 1, "never repeated");
    assert_eq!(
        state.files.get("/pub/upload.bin").map(Vec::as_slice),
        Some(&b"NEW"[..])
    );
}

/// Every host's refusal reaches the group's results under its own name.
#[tokio::test]
async fn sftp_put_refusals_stay_attributed_per_host_through_the_group() {
    let refusing = SharedFs::default();
    refusing.lock().await.no_posix_rename = true;
    let good = SharedFs::default();
    let bad_port = start_server(refusing).await;
    let good_port = start_server(good.clone()).await;
    let targets = vec![
        connect_target("bad", bad_port, TargetState::Enabled).await,
        connect_target("good", good_port, TargetState::Enabled).await,
    ];
    let mut group = HostsGroup::new(targets, false);

    let results = group
        .sftp_put_bytes(b"NEW", std::path::Path::new("/pub/upload.bin"))
        .await;

    assert_eq!(results["good"], Ok(()));
    let bad = results["bad"].as_ref().expect_err("refused");
    assert!(
        bad.contains("refused on 127.0.0.1") && bad.contains("posix-rename"),
        "{bad}"
    );
    assert_eq!(
        good.lock()
            .await
            .files
            .get("/pub/upload.bin")
            .map(Vec::as_slice),
        Some(&b"NEW"[..])
    );
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

/// A budgeted read keeps and transfers only a bounded prefix of a large file:
/// the fixture counts what the server actually served.
#[tokio::test]
async fn sftp_read_capped_bounds_retention_and_transfer() {
    const FILE_LEN: usize = 8 * 1024 * 1024;
    let fs = SharedFs::default();
    let content: Vec<u8> = (0..FILE_LEN).map(|i| (i % 251) as u8).collect();
    fs.lock()
        .await
        .files
        .insert("/big.bin".to_owned(), content.clone());
    let port = start_server(fs.clone()).await;
    let mut conn = connect(port, CommandTimeout::from_secs(5)).await;

    let got = conn
        .sftp_read_capped(std::path::Path::new("/big.bin"), 32)
        .await
        .expect("capped read");

    assert_eq!(got.bytes, content[..32]);
    assert_eq!(got.size, FILE_LEN as u64);
    assert!(got.truncated);
    let served = fs.lock().await.read_bytes;
    assert!(
        served <= 256 * 1024,
        "a 32-byte budget must not pull the file across the link: served {served} of {FILE_LEN}"
    );
}

#[tokio::test]
async fn sftp_read_capped_reports_truncation_at_every_boundary() {
    let fs = SharedFs::default();
    {
        let mut g = fs.lock().await;
        for (name, bytes) in [
            ("/empty", &b""[..]),
            ("/short", b"ab"),
            ("/equal", b"abcd"),
            ("/long", b"abcde"),
        ] {
            g.files.insert(name.to_owned(), bytes.to_vec());
        }
    }
    let port = start_server(fs).await;
    let mut conn = connect(port, CommandTimeout::from_secs(5)).await;

    for (name, bytes, size, truncated) in [
        ("/empty", &b""[..], 0, false),
        ("/short", b"ab", 2, false),
        ("/equal", b"abcd", 4, false),
        ("/long", b"abcd", 5, true),
    ] {
        let got = conn
            .sftp_read_capped(std::path::Path::new(name), 4)
            .await
            .expect("capped read");
        assert_eq!(
            (got.bytes.as_slice(), got.size, got.truncated),
            (bytes, size, truncated),
            "{name}"
        );
    }
}

/// `size` is the server's handle metadata, not the length of what was kept.
#[tokio::test]
async fn sftp_read_capped_reports_the_server_size_snapshot() {
    let fs = SharedFs::default();
    {
        let mut g = fs.lock().await;
        g.files.insert("/f".to_owned(), b"abcde".to_vec());
        g.size_attrs.insert("/f".to_owned(), Some(100));
    }
    let port = start_server(fs).await;
    let mut conn = connect(port, CommandTimeout::from_secs(5)).await;

    let got = conn
        .sftp_read_capped(std::path::Path::new("/f"), 4)
        .await
        .expect("capped read");

    assert_eq!(got.bytes, b"abcd");
    assert_eq!(got.size, 100);
    assert!(got.truncated);
}

/// Without a size the budget cannot be reported honestly: fail, and read nothing.
#[tokio::test]
async fn sftp_read_capped_without_size_attr_fails_before_reading() {
    let fs = SharedFs::default();
    {
        let mut g = fs.lock().await;
        g.files.insert("/f".to_owned(), b"abcde".to_vec());
        g.size_attrs.insert("/f".to_owned(), None);
    }
    let port = start_server(fs.clone()).await;
    let mut conn = connect(port, CommandTimeout::from_secs(5)).await;

    let err = conn
        .sftp_read_capped(std::path::Path::new("/f"), 4)
        .await
        .expect_err("a budgeted read needs a size");

    assert!(
        matches!(err, mtui_hosts::HostError::Sftp { .. }),
        "expected Sftp, got {err:?}"
    );
    assert!(
        fs.lock().await.reads.is_empty(),
        "no read may be issued once the size is unknown"
    );
}

#[tokio::test]
async fn sftp_read_capped_zero_budget_reads_the_whole_file() {
    let fs = SharedFs::default();
    let content = vec![b'x'; 300 * 1024];
    fs.lock()
        .await
        .files
        .insert("/f".to_owned(), content.clone());
    let port = start_server(fs).await;
    let mut conn = connect(port, CommandTimeout::from_secs(5)).await;

    let got = conn
        .sftp_read_capped(std::path::Path::new("/f"), 0)
        .await
        .expect("unlimited read");

    assert_eq!(got.bytes, content);
    assert_eq!(got.size, content.len() as u64);
    assert!(!got.truncated);
}

#[tokio::test]
async fn sftp_read_capped_read_failure_is_an_sftp_error_in_both_modes() {
    let fs = SharedFs::default();
    {
        let mut g = fs.lock().await;
        g.files.insert("/f".to_owned(), b"abcde".to_vec());
        g.failing_reads.insert("/f".to_owned());
    }
    let port = start_server(fs).await;
    let mut conn = connect(port, CommandTimeout::from_secs(5)).await;

    for max_bytes in [0, 4] {
        let err = conn
            .sftp_read_capped(std::path::Path::new("/f"), max_bytes)
            .await
            .expect_err("a failing read must not look like an empty file");
        assert!(
            matches!(err, mtui_hosts::HostError::Sftp { .. }),
            "max_bytes={max_bytes}: expected Sftp, got {err:?}"
        );
    }
}

#[tokio::test]
async fn sftp_read_capped_missing_file_is_not_found() {
    let port = start_server(SharedFs::default()).await;
    let mut conn = connect(port, CommandTimeout::from_secs(5)).await;

    let err = conn
        .sftp_read_capped(std::path::Path::new("/nope"), 4)
        .await
        .expect_err("missing file");

    assert!(
        matches!(err, mtui_hosts::HostError::SftpNotFound { .. }),
        "expected SftpNotFound, got {err:?}"
    );
}

#[tokio::test]
async fn sftp_write_exclusive_then_overwrite_and_read_back() {
    let fs = SharedFs::default();
    let port = start_server(fs).await;
    let mut conn = connect(port, CommandTimeout::from_secs(5)).await;

    let lock = std::path::Path::new("/var/lock/mtui.lock");

    // Atomic exclusive create wins on a free path.
    conn.sftp_write(lock, b"1700000000:alice:42", true)
        .await
        .expect("exclusive create wins on free path");

    // A second exclusive create loses the race.
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

    // A non-exclusive overwrite replaces the contents.
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

    // A first append against a missing path creates the file.
    conn.sftp_append(log, b"1700000000:alice:install\n")
        .await
        .expect("first append creates the file");
    // Subsequent appends land at EOF, preserving prior entries: no
    // read-modify-write.
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
    // Trailing slash: builds "<local>/<name>.<hostname>".
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
    // Port 1: nothing listens, so the connection is refused immediately.
    let err = SshConnection::connect(
        "127.0.0.1",
        1,
        HostKeyPolicy::AutoAdd,
        CommandTimeout::new(Duration::from_millis(500)),
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
    let real = std::env::var_os("HOME")
        .map(PathBuf::from)
        .map(|h| h.join(".ssh").join("known_hosts"));
    let before = real.as_ref().and_then(|p| std::fs::read(p).ok());

    let port = start_server(SharedFs::default()).await;
    let mut conn = connect(port, CommandTimeout::from_secs(5)).await;
    assert!(conn.is_active());
    conn.close().await.expect("close");

    // AutoAdd wrote to the temp file, not the home file.
    let temp = std::fs::read_to_string(test_known_hosts())
        .expect("temp known_hosts should exist after AutoAdd");
    assert!(
        temp.contains("127.0.0.1"),
        "temp known_hosts missing loopback entry: {temp:?}"
    );

    // The real file is byte-for-byte unchanged.
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
    // The local link is torn down after dispatch.
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
    // A dropped link: run() must transparently reconnect and still answer.
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

    // A buffer smaller than the frame forces the leftover-carryover path: no
    // bytes may be lost across the two reads.
    let mut small = [0u8; 4];
    let n = ch.read(&mut small).await.expect("read greeting part 1");
    assert_eq!(&small[..n], b"welc");
    let n = ch.read(&mut small).await.expect("read greeting part 2");
    assert_eq!(&small[..n], b"ome\n");

    let mut buf = [0u8; 64];

    ch.write(b"hi").await.expect("write");
    let n = ch.read(&mut buf).await.expect("read echo");
    assert_eq!(&buf[..n], b"hi");

    ch.resize(120, 40).await.expect("resize");

    // Ctrl-D closes the remote shell, so the next read observes EOF.
    ch.write(&[0x04]).await.expect("write ctrl-d");
    let n = ch.read(&mut buf).await.expect("read eof");
    assert_eq!(n, 0, "channel close surfaces as EOF");
}

// ----------------------------------------------------------------------------
// HostsGroup fan-out over real SSH targets.
//
// `hostgroup.rs` unit-tests fan-out over `MockConnection`; these prove the same
// end-to-end over ≥2 real SSH sessions, with per-host `TargetState` gating
// honoured against a live transport.
// ----------------------------------------------------------------------------

/// The fixture's stdout for an otherwise-unknown command (`scripted_command`'s
/// `other` arm), asserted on to prove a command reached the remote host.
fn ran(cmd: &str) -> String {
    format!("ran: {cmd}\n")
}

#[tokio::test]
async fn hostsgroup_run_parallel_reaches_every_member() {
    let port = start_server(SharedFs::default()).await;
    let targets = vec![
        connect_target("h1", port, TargetState::Enabled).await,
        connect_target("h2", port, TargetState::Enabled).await,
    ];
    let mut group = HostsGroup::new(targets, false);

    group.run("whoami-probe").await;

    for name in ["h1", "h2"] {
        let t = group.get(name).expect("member present");
        assert_eq!(t.lastout(), ran("whoami-probe"), "{name} ran the command");
        assert_eq!(t.lastexit(), Some(0), "{name} recorded exit 0");
    }
}

#[tokio::test]
async fn hostsgroup_run_honours_per_host_state_end_to_end() {
    let port = start_server(SharedFs::default()).await;
    let targets = vec![
        connect_target("on", port, TargetState::Enabled).await,
        connect_target("off", port, TargetState::Disabled).await,
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
// Remote lock lifecycle over the fixture's real SFTP.
//
// `locks.rs` unit-tests the protocol over `MockConnection`; these run it over
// the fixture's real SFTP subsystem — crucially the atomic `O_EXCL` create the
// free-host claim relies on, plus release and stale-reap round-trips.
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
    // Wire format `timestamp:user:pid:comment`: assert the fields the claim
    // controls survived the real SFTP write.
    let fields: Vec<&str> = line.splitn(4, ':').collect();
    assert_eq!(fields.get(1), Some(&"alice"), "lock owned by alice: {line}");
    assert_eq!(
        fields.get(3),
        Some(&"mtui operation"),
        "comment kept: {line}"
    );

    lock.unlock(false).await.expect("release");
    assert!(
        !fs.lock().await.files.contains_key(TARGET_LOCK_PATH),
        "lockfile removed on unlock"
    );
    assert!(!lock.is_locked().await.expect("is_locked after unlock"));
}

#[tokio::test]
async fn lock_refuses_fresh_foreign_lock_over_sftp() {
    // A fresh foreign lock seeded straight into the fixture FS: the atomic
    // create loses to it and reconciliation refuses (fail-fast, wait=0).
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
    // A very old foreign lock: stale-reap `remove` plus a fresh exclusive
    // create, both over real SFTP.
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

#[tokio::test]
async fn sftp_mtime_reports_the_servers_modification_time() {
    let fs = SharedFs::default();
    {
        let mut fs = fs.lock().await;
        fs.files.insert("/dated".to_owned(), b"x".to_vec());
        fs.files.insert("/undated".to_owned(), b"x".to_vec());
        fs.mtimes.insert("/dated".to_owned(), 1_700_000_000);
    }
    let port = start_server(fs).await;
    let mut conn = connect(port, CommandTimeout::from_secs(5)).await;

    assert_eq!(
        conn.sftp_mtime(std::path::Path::new("/dated"))
            .await
            .expect("dated"),
        Some(1_700_000_000)
    );
    assert_eq!(
        conn.sftp_mtime(std::path::Path::new("/undated"))
            .await
            .expect("undated"),
        None
    );
    let err = conn
        .sftp_mtime(std::path::Path::new("/missing"))
        .await
        .expect_err("missing");
    assert!(
        matches!(err, mtui_hosts::HostError::SftpNotFound { .. }),
        "{err:?}"
    );
}

#[tokio::test]
async fn an_empty_lock_file_is_invalid_and_reaped_by_its_modification_time_over_sftp() {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as u32;
    let fs = SharedFs::default();
    {
        let mut fs = fs.lock().await;
        fs.files.insert(TARGET_LOCK_PATH.to_owned(), Vec::new());
        fs.mtimes.insert(TARGET_LOCK_PATH.to_owned(), now - 60);
    }
    let port = start_server(fs.clone()).await;
    let conn = connect(port, CommandTimeout::from_secs(5)).await;
    let mut lock = TargetLock::new(Box::new(conn), &lock_config("alice"));

    let err = lock.try_claim("mtui operation").await.expect_err("young");
    assert!(
        matches!(err, mtui_hosts::HostError::InvalidLockFile { .. }),
        "{err:?}"
    );
    assert_eq!(fs.lock().await.files[TARGET_LOCK_PATH], b"");

    fs.lock()
        .await
        .mtimes
        .insert(TARGET_LOCK_PATH.to_owned(), now - 200_000);
    assert!(lock.try_claim("mtui operation").await.expect("old"));
    let line = String::from_utf8(fs.lock().await.files[TARGET_LOCK_PATH].clone()).unwrap();
    assert!(line.contains(":alice:"), "reaped and re-claimed: {line}");
}

#[tokio::test]
async fn sftp_request_budget_follows_connect_timeout_not_the_hardcoded_ten_seconds() {
    // A 2s server-side `open` delay against a 1s `connect_timeout`: the
    // request must fail well under russh-sftp's fixed 10s default, proving the
    // per-request timeout is derived. The 1.5s bound absorbs scheduling jitter
    // while staying far below both 2s and 10s.
    let fs = SharedFs::default();
    let (port, delay) = start_server_with_delay(fs).await;
    *delay.lock().await = Duration::from_secs(2);
    let mut conn = connect(port, CommandTimeout::from_secs(1)).await;

    let start = std::time::Instant::now();
    conn.sftp_open(std::path::Path::new("/some/path"))
        .await
        .expect_err("2s server delay must exceed the 1s connect_timeout-derived budget");
    let elapsed = start.elapsed();
    assert!(
        elapsed < Duration::from_millis(1500),
        "request budget did not follow connect_timeout: elapsed {elapsed:?}"
    );
}

#[tokio::test]
async fn sftp_verbs_over_one_connection_share_a_single_subsystem() {
    // Several SFTP verbs over one connection must open the subsystem exactly
    // once. `close()` tears down the whole SSH session, not just the cached
    // subsystem, so the next verb reconnects and re-handshakes.
    let fs = SharedFs::default();
    let (port, _delay, count) = start_server_with_delay_and_counter(fs).await;
    let mut conn = connect(port, CommandTimeout::from_secs(5)).await;

    conn.sftp_write(std::path::Path::new("/a"), b"1", false)
        .await
        .expect("write a");
    conn.sftp_open(std::path::Path::new("/a"))
        .await
        .expect("open a");
    conn.sftp_listdir(std::path::Path::new("/"))
        .await
        .expect("listdir");
    assert_eq!(
        count.load(Ordering::SeqCst),
        1,
        "three verbs must share one subsystem"
    );

    conn.close().await.expect("close");
    conn.sftp_write(std::path::Path::new("/b"), b"2", false)
        .await
        .expect("write b after close");
    assert_eq!(
        count.load(Ordering::SeqCst),
        2,
        "close() must force a fresh subsystem on the next verb"
    );
}

#[tokio::test]
async fn sftp_transparently_rehandshakes_after_a_stale_cached_sessions_first_request_times_out() {
    // A cached subsystem the peer silently dropped: its first request on a
    // later verb times out, and must be invalidated, re-handshaked and retried
    // exactly once, so the caller sees success rather than the timeout.
    let fs = SharedFs::default();
    let (port, delay, count) = start_server_with_delay_and_counter(fs).await;
    let mut conn = connect(port, CommandTimeout::from_secs(1)).await;

    conn.sftp_write(std::path::Path::new("/a"), b"1", false)
        .await
        .expect("first write opens the subsystem");
    assert_eq!(count.load(Ordering::SeqCst), 1);

    // A one-shot delay past the 1s derived budget times out the verb's first
    // `open`; being one-shot, the retry behind it is not also delayed.
    *delay.lock().await = Duration::from_secs(2);
    conn.sftp_open(std::path::Path::new("/a"))
        .await
        .expect("transparently retried after re-handshaking, not surfaced as a timeout");

    assert_eq!(
        count.load(Ordering::SeqCst),
        2,
        "the timed-out cached session must be invalidated and re-handshaked exactly once"
    );
}

// ----------------------------------------------------------------------------
// SFTP first-request retry: only a replay-safe request on a cached session,
// and only after a session-fatal failure, may be replayed.
// ----------------------------------------------------------------------------

/// Connects with a 1s request budget and warms the SFTP subsystem with an
/// unrelated write, so the verb under test runs against a cached session.
async fn warm_sftp_connection(fs: &SharedFs) -> (SshConnection, SharedSubsystemCount) {
    let (port, _delay, count) = start_server_with_delay_and_counter(fs.clone()).await;
    let mut conn = connect(port, CommandTimeout::from_secs(1)).await;
    conn.sftp_write(std::path::Path::new("/warm"), b"w", false)
        .await
        .expect("warm-up write opens the subsystem");
    assert_eq!(
        count.load(Ordering::SeqCst),
        1,
        "warm-up opens one subsystem"
    );
    (conn, count)
}

async fn open_count(fs: &SharedFs, path: &str) -> usize {
    fs.lock().await.open_counts.get(path).copied().unwrap_or(0)
}

async fn file_bytes(fs: &SharedFs, path: &str) -> Option<Vec<u8>> {
    fs.lock().await.files.get(path).cloned()
}

async fn hold_next_open(fs: &SharedFs, path: &str) -> OpenHold {
    let hold = OpenHold::new();
    fs.lock()
        .await
        .open_holds
        .entry(path.to_owned())
        .or_default()
        .push_back(hold.clone());
    hold
}

#[tokio::test]
async fn sftp_retry_fixture_exposes_an_open_effect_before_its_reply() {
    let fs = SharedFs::default();
    let (port, _delay, count) = start_server_with_delay_and_counter(fs.clone()).await;
    let mut conn = connect(port, CommandTimeout::from_secs(5)).await;
    let hold = hold_next_open(&fs, "/held").await;

    let task = tokio::spawn(async move {
        conn.sftp_write(std::path::Path::new("/held"), b"payload", true)
            .await
    });
    hold.landed.notified().await;

    assert_eq!(
        file_bytes(&fs, "/held").await,
        Some(Vec::new()),
        "the create's effect must be visible while its reply is withheld"
    );
    assert_eq!(open_count(&fs, "/held").await, 1);
    assert!(!task.is_finished(), "the reply is still withheld");

    hold.release.notify_one();
    task.await
        .expect("join")
        .expect("write completes once released");
    assert_eq!(file_bytes(&fs, "/held").await, Some(b"payload".to_vec()));
    assert_eq!(open_count(&fs, "/held").await, 1);
    assert_eq!(count.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn sftp_retry_status_not_found_keeps_cached_subsystem() {
    let fs = SharedFs::default();
    let (mut conn, count) = warm_sftp_connection(&fs).await;
    {
        let mut guard = fs.lock().await;
        guard.files.insert("/target".to_owned(), b"data".to_vec());
        guard
            .open_status
            .insert("/target".to_owned(), StatusCode::NoSuchFile);
    }

    let err = conn
        .sftp_open(std::path::Path::new("/target"))
        .await
        .expect_err("scripted NoSuchFile");

    assert!(
        matches!(err, mtui_hosts::HostError::SftpNotFound { .. }),
        "unexpected error: {err:?}"
    );
    assert_eq!(
        open_count(&fs, "/target").await,
        1,
        "the open is not replayed"
    );
    assert_eq!(count.load(Ordering::SeqCst), 1, "no re-handshake");

    fs.lock().await.open_status.clear();
    let bytes = conn
        .sftp_open(std::path::Path::new("/target"))
        .await
        .expect("follow-up read");
    assert_eq!(bytes, b"data", "the status override changed no bytes");
    assert_eq!(
        count.load(Ordering::SeqCst),
        1,
        "the cached subsystem survives an ordinary status failure"
    );
}

#[tokio::test]
async fn sftp_retry_permission_denied_is_not_replayed() {
    let fs = SharedFs::default();
    let (mut conn, count) = warm_sftp_connection(&fs).await;
    fs.lock()
        .await
        .open_status
        .insert("/locked".to_owned(), StatusCode::PermissionDenied);

    let err = conn
        .sftp_write(std::path::Path::new("/locked"), b"x", false)
        .await
        .expect_err("scripted PermissionDenied");

    assert!(
        matches!(&err, mtui_hosts::HostError::Sftp { reason, .. } if reason.contains("Permission denied")),
        "unexpected error: {err:?}"
    );
    assert_eq!(open_count(&fs, "/locked").await, 1);
    assert_eq!(count.load(Ordering::SeqCst), 1);
    assert_eq!(file_bytes(&fs, "/locked").await, None);

    conn.sftp_open(std::path::Path::new("/warm"))
        .await
        .expect("the cached session is still usable");
    assert_eq!(count.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn sftp_retry_exclusive_collision_is_not_replayed() {
    let fs = SharedFs::default();
    let (mut conn, count) = warm_sftp_connection(&fs).await;
    fs.lock()
        .await
        .files
        .insert("/taken".to_owned(), b"mine".to_vec());

    let err = conn
        .sftp_write(std::path::Path::new("/taken"), b"theirs", true)
        .await
        .expect_err("exclusive create over an existing file");

    assert!(
        matches!(err, mtui_hosts::HostError::AlreadyExists { .. }),
        "unexpected error: {err:?}"
    );
    assert_eq!(file_bytes(&fs, "/taken").await, Some(b"mine".to_vec()));
    assert_eq!(open_count(&fs, "/taken").await, 1);
    assert_eq!(count.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn sftp_retry_exclusive_timeout_does_not_replay_open() {
    let fs = SharedFs::default();
    let (mut conn, count) = warm_sftp_connection(&fs).await;
    let hold = hold_next_open(&fs, "/claim").await;

    let err = conn
        .sftp_write(std::path::Path::new("/claim"), b"owner", true)
        .await
        .expect_err("the create reply never arrives");
    hold.landed.notified().await;

    assert!(
        matches!(err, mtui_hosts::HostError::SftpTimeout { .. }),
        "unexpected error: {err:?}"
    );
    assert_eq!(
        open_count(&fs, "/claim").await,
        1,
        "the create is not replayed"
    );
    assert_eq!(file_bytes(&fs, "/claim").await, Some(Vec::new()));
    assert_eq!(
        count.load(Ordering::SeqCst),
        1,
        "no second subsystem was opened by the failed call"
    );
    hold.release.notify_one();

    conn.sftp_open(std::path::Path::new("/warm"))
        .await
        .expect("the next verb re-handshakes");
    assert_eq!(
        count.load(Ordering::SeqCst),
        2,
        "the timed-out session was dropped from the cache"
    );
}

#[tokio::test]
async fn sftp_retry_truncating_timeout_replays_the_open() {
    let fs = SharedFs::default();
    let (mut conn, count) = warm_sftp_connection(&fs).await;
    fs.lock()
        .await
        .files
        .insert("/config".to_owned(), b"old contents".to_vec());
    let hold = hold_next_open(&fs, "/config").await;

    conn.sftp_write(std::path::Path::new("/config"), b"new contents", false)
        .await
        .expect("the lost truncating open is replayed on a fresh subsystem");
    hold.landed.notified().await;

    assert_eq!(
        open_count(&fs, "/config").await,
        2,
        "one attempt plus one replay"
    );
    assert_eq!(
        file_bytes(&fs, "/config").await,
        Some(b"new contents".to_vec()),
        "the payload is written once, after the replayed open"
    );
    assert_eq!(count.load(Ordering::SeqCst), 2);
    hold.release.notify_one();
}

#[tokio::test]
async fn sftp_retry_append_timeout_replays_the_open() {
    let fs = SharedFs::default();
    let (mut conn, count) = warm_sftp_connection(&fs).await;
    fs.lock()
        .await
        .files
        .insert("/history".to_owned(), b"row1\n".to_vec());
    let hold = hold_next_open(&fs, "/history").await;

    conn.sftp_append(std::path::Path::new("/history"), b"row2\n")
        .await
        .expect("the lost append open is replayed on a fresh subsystem");
    hold.landed.notified().await;

    assert_eq!(
        open_count(&fs, "/history").await,
        2,
        "one attempt plus one replay"
    );
    assert_eq!(
        file_bytes(&fs, "/history").await,
        Some(b"row1\nrow2\n".to_vec()),
        "the history row is written exactly once"
    );
    assert_eq!(count.load(Ordering::SeqCst), 2);
    hold.release.notify_one();
}

#[tokio::test]
async fn sftp_retry_lost_reply_on_a_lock_restamp_keeps_the_new_stamp() {
    let fs = SharedFs::default();
    let (port, _delay, count) = start_server_with_delay_and_counter(fs.clone()).await;
    let conn = connect(port, CommandTimeout::from_secs(1)).await;
    let mut lock = TargetLock::new(Box::new(conn), &lock_config("alice"));
    lock.lock("first").await.expect("acquire");
    let mut hold = OpenHold::new();
    hold.truncating_only = true;
    fs.lock()
        .await
        .open_holds
        .entry(TARGET_LOCK_PATH.to_owned())
        .or_default()
        .push_back(hold.clone());

    lock.lock("second")
        .await
        .expect("the re-stamp's lost open is replayed");

    let line = String::from_utf8(
        file_bytes(&fs, TARGET_LOCK_PATH)
            .await
            .expect("lockfile present"),
    )
    .unwrap();
    assert!(
        line.ends_with(":second"),
        "the lockfile must hold the new stamp, not an empty truncation: {line:?}"
    );
    assert_eq!(count.load(Ordering::SeqCst), 2);
    hold.release.notify_one();
}

/// Closes the most recent SFTP channel from the server side and waits for the
/// client's SFTP runner to notice, so its next request is never sent.
async fn close_sftp_channel(fs: &SharedFs) {
    let (handle, id) = fs
        .lock()
        .await
        .sftp_channel
        .clone()
        .expect("an sftp subsystem is open");
    handle.eof(id).await.expect("eof");
    handle.close(id).await.expect("close");
    // The client observes the close asynchronously and exposes no signal for
    // it; a request sent before it does would be queued, not refused.
    tokio::time::sleep(Duration::from_millis(250)).await;
}

#[tokio::test]
async fn sftp_retry_never_sent_exclusive_create_is_replayed_on_a_closed_session() {
    let fs = SharedFs::default();
    let (mut conn, count) = warm_sftp_connection(&fs).await;
    close_sftp_channel(&fs).await;

    conn.sftp_write(std::path::Path::new("/claim"), b"owner", true)
        .await
        .expect("a request that was never sent is replayed on a fresh subsystem");

    assert_eq!(
        open_count(&fs, "/claim").await,
        1,
        "exactly one OPEN reached the server"
    );
    assert_eq!(file_bytes(&fs, "/claim").await, Some(b"owner".to_vec()));
    assert_eq!(count.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn sftp_retry_never_sent_append_is_replayed_on_a_closed_session() {
    let fs = SharedFs::default();
    let (mut conn, count) = warm_sftp_connection(&fs).await;
    close_sftp_channel(&fs).await;

    conn.sftp_append(std::path::Path::new("/history"), b"row1\n")
        .await
        .expect("a request that was never sent is replayed on a fresh subsystem");

    assert_eq!(open_count(&fs, "/history").await, 1);
    assert_eq!(file_bytes(&fs, "/history").await, Some(b"row1\n".to_vec()));
    assert_eq!(count.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn sftp_retry_read_on_a_closed_session_is_replayed() {
    let fs = SharedFs::default();
    let (mut conn, count) = warm_sftp_connection(&fs).await;
    close_sftp_channel(&fs).await;

    let bytes = conn
        .sftp_open(std::path::Path::new("/warm"))
        .await
        .expect("read replayed on a fresh subsystem");

    assert_eq!(bytes, b"w");
    assert_eq!(count.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn sftp_retry_replay_failure_evicts_the_session_even_when_the_mapper_does_not() {
    let fs = SharedFs::default();
    let (mut conn, count) = warm_sftp_connection(&fs).await;
    fs.lock()
        .await
        .files
        .insert("/slow".to_owned(), b"data".to_vec());
    let first = hold_next_open(&fs, "/slow").await;
    let replay = hold_next_open(&fs, "/slow").await;
    let local = tempfile::tempdir().expect("tempdir");

    let err = conn
        .sftp_get(std::path::Path::new("/slow"), &local.path().join("out"))
        .await
        .expect_err("both attempts time out");
    assert!(
        matches!(err, mtui_hosts::HostError::Sftp { .. }),
        "get maps a timeout to a generic sftp error: {err:?}"
    );
    assert_eq!(count.load(Ordering::SeqCst), 2);

    // Released first, so a session that stayed cached would answer the next
    // verb instead of timing it out into a replay of its own.
    first.release.notify_one();
    replay.release.notify_one();
    conn.sftp_open(std::path::Path::new("/warm"))
        .await
        .expect("the next verb re-handshakes");
    assert_eq!(
        count.load(Ordering::SeqCst),
        3,
        "the replay's timed-out session was dropped from the cache"
    );
}

#[tokio::test]
async fn sftp_retry_append_adds_exactly_one_row_on_a_cached_session() {
    let fs = SharedFs::default();
    let (mut conn, count) = warm_sftp_connection(&fs).await;

    conn.sftp_append(std::path::Path::new("/history"), b"row1\n")
        .await
        .expect("append");

    assert_eq!(file_bytes(&fs, "/history").await, Some(b"row1\n".to_vec()));
    assert_eq!(open_count(&fs, "/history").await, 1);
    assert_eq!(count.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn sftp_retry_read_only_persistent_timeout_stops_after_one_replay() {
    let fs = SharedFs::default();
    let (mut conn, count) = warm_sftp_connection(&fs).await;
    fs.lock()
        .await
        .files
        .insert("/slow".to_owned(), b"data".to_vec());
    let first = hold_next_open(&fs, "/slow").await;
    let replay = hold_next_open(&fs, "/slow").await;

    let err = conn
        .sftp_open(std::path::Path::new("/slow"))
        .await
        .expect_err("both attempts time out");

    assert!(
        matches!(err, mtui_hosts::HostError::SftpTimeout { .. }),
        "unexpected error: {err:?}"
    );
    assert_eq!(
        open_count(&fs, "/slow").await,
        2,
        "one attempt plus one replay"
    );
    assert_eq!(count.load(Ordering::SeqCst), 2);
    first.release.notify_one();
    replay.release.notify_one();
}

#[tokio::test]
async fn sftp_retry_read_only_timeout_on_a_fresh_session_is_not_replayed() {
    let fs = SharedFs::default();
    let (port, _delay, count) = start_server_with_delay_and_counter(fs.clone()).await;
    let mut conn = connect(port, CommandTimeout::from_secs(1)).await;
    fs.lock()
        .await
        .files
        .insert("/slow".to_owned(), b"data".to_vec());
    let hold = hold_next_open(&fs, "/slow").await;

    let err = conn
        .sftp_open(std::path::Path::new("/slow"))
        .await
        .expect_err("the only attempt times out");

    assert!(
        matches!(err, mtui_hosts::HostError::SftpTimeout { .. }),
        "unexpected error: {err:?}"
    );
    assert_eq!(open_count(&fs, "/slow").await, 1);
    assert_eq!(count.load(Ordering::SeqCst), 1);
    hold.release.notify_one();
}

// ----------------------------------------------------------------------------
// Repository URL credentials: executed verbatim, never logged or stored.
// ----------------------------------------------------------------------------

fn server_received(command: &str) -> bool {
    EXEC_LOG
        .lock()
        .expect("exec log lock")
        .iter()
        .any(|c| c == command)
}

#[tokio::test]
async fn run_sends_credentials_verbatim_but_stores_and_logs_them_masked() {
    use crate::repository_redaction::{URL, assert_no_credentials, capture_events};

    let port = start_server(SharedFs::default()).await;
    let mut conn = connect(port, CommandTimeout::from_secs(5)).await;
    let command = format!("zypper ar x {URL} x");

    let mut log = None;
    let events = capture_events(|| async {
        log = Some(conn.run(&command).await.expect("run"));
    })
    .await;
    let log = log.expect("ran");

    assert!(
        server_received(&command),
        "the server must receive the exact credential-bearing command"
    );
    // The scripted server echoes the command back on stdout, so the output
    // really did carry the credentials before being stored.
    assert_no_credentials("log command", &log.command);
    assert_no_credentials("log stdout", &log.stdout);
    assert!(
        log.stdout
            .starts_with("ran: zypper ar x https://***@repo.example/")
    );
    assert_no_credentials("tracing events", &events);
    assert_eq!(log.exitcode, 0);
}

#[tokio::test]
async fn run_timeout_error_and_log_mask_the_command_credentials() {
    use crate::repository_redaction::{URL, assert_no_credentials, capture_events};

    let port = start_server(SharedFs::default()).await;
    let mut conn = connect(port, CommandTimeout::new(Duration::from_millis(300))).await;
    let command = format!("sleep-forever {URL}");

    let mut err = None;
    let events = capture_events(|| async {
        err = Some(conn.run(&command).await.expect_err("should time out"));
    })
    .await;
    let err = err.expect("ran");

    assert!(
        events.contains("command timed out with no output"),
        "the timeout warning was not captured; the secrecy assertions would be vacuous"
    );
    assert_no_credentials("tracing events", &events);
    assert!(
        matches!(&err, mtui_hosts::HostError::Timeout { .. }),
        "unexpected error: {err:?}"
    );
    assert_no_credentials("timeout error display", &err.to_string());
    assert_no_credentials("timeout error debug", &format!("{err:?}"));
    assert!(
        server_received(&command),
        "the server must still receive the exact command"
    );
}

#[tokio::test]
async fn run_absolute_deadline_error_and_log_mask_the_command_credentials() {
    use crate::repository_redaction::{URL, assert_no_credentials, capture_events};

    // `trickle-async` ends at the deadline-bounded wait; `flood-async` at the
    // deadline check ahead of each message.
    for verb in ["trickle-async", "flood-async"] {
        let port = start_server(SharedFs::default()).await;
        let mut conn = connect(port, CommandTimeout::from_secs(5))
            .await
            .with_command_timeout(CommandTimeout::new(Duration::from_millis(150)));
        let command = format!("{verb} {URL}");

        let mut err = None;
        let events = capture_events(|| async {
            err = Some(
                conn.run(&command)
                    .await
                    .expect_err("should hit the deadline"),
            );
        })
        .await;
        let err = err.expect("ran");

        assert!(
            events.contains("command exceeded absolute deadline"),
            "the deadline warning was not captured; the secrecy assertions would be vacuous"
        );
        assert_no_credentials("tracing events", &events);
        assert_no_credentials("timeout error", &format!("{err} {err:?}"));
    }
}

#[tokio::test]
async fn run_session_loss_diagnostics_mask_the_command_credentials() {
    use crate::repository_redaction::{URL, assert_no_credentials, capture_events};

    let mut log = None;
    let events = capture_events(|| async {
        log = Some(run_on_a_dying_link(&format!("partial-stderr-then-silent {URL}")).await);
    })
    .await;
    let log = log.expect("ran");

    assert!(
        events.contains("SSH session lost mid-command"),
        "the session-loss warning was not captured; the secrecy assertions would be vacuous"
    );
    assert_no_credentials("tracing events", &events);
    assert_no_credentials("log command", &log.command);
    assert_no_credentials("log stderr", &log.stderr);
}

#[tokio::test]
async fn run_truncation_warning_masks_the_command_credentials() {
    use crate::repository_redaction::{URL, assert_no_credentials, capture_events};

    let port = start_server(SharedFs::default()).await;
    let mut conn = connect(port, CommandTimeout::from_secs(5)).await;

    let mut log = None;
    let events = capture_events(|| async {
        log = Some(conn.run(&format!("flood-stdout {URL}")).await.expect("run"));
    })
    .await;
    let log = log.expect("ran");

    assert!(log.truncated);
    assert!(
        events.contains("command output exceeded capture caps"),
        "the truncation warning was not captured; the secrecy assertions would be vacuous"
    );
    assert_no_credentials("tracing events", &events);
    assert_no_credentials("log command", &log.command);
}

#[tokio::test]
async fn target_over_ssh_stores_timeout_and_output_masked() {
    use crate::repository_redaction::{URL, assert_no_credentials};

    let port = start_server(SharedFs::default()).await;
    let conn = connect(port, CommandTimeout::new(Duration::from_millis(300))).await;
    let target = Target::with_connection("h1", TargetState::Enabled, Box::new(conn));
    let mut group = HostsGroup::new(vec![target], false);

    group.run(format!("sleep-forever {URL}").as_str()).await;
    group.run(format!("zypper rr {URL}").as_str()).await;

    let t = group.get("h1").expect("member present");
    assert_eq!(t.out().len(), 2);
    for entry in t.out().iter() {
        assert_no_credentials("stored command", &entry.command);
        assert_no_credentials("stored stdout", &entry.stdout);
        assert_no_credentials("stored stderr", &entry.stderr);
    }
    assert_eq!(t.out()[0].exitcode, -1, "the timeout is still recorded");
    assert_eq!(t.lastexit(), Some(0));
}
