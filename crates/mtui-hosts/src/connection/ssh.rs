//! The russh-backed [`SshConnection`] — the production [`Connection`] impl.
//!
//! Ported from upstream `mtui/hosts/connection/connection.py` (paramiko). This
//! is the async-native re-expression of that blocking wrapper on top of
//! [`russh`] (SSH transport) and [`russh_sftp`] (SFTP subsystem).
//!
//! ## Behavioural parity with upstream
//!
//! * **Pubkey/agent only.** Authentication tries SSH-agent keys (via
//!   `SSH_AUTH_SOCK`) first, then any identity files from `~/.ssh/config`, then
//!   the default `~/.ssh/id_*` keys — mirroring paramiko, which tries agent
//!   then key files. There is deliberately **no password fallback** (MTUI is
//!   pubkey-only by design); a failed auth surfaces [`HostError::Auth`].
//! * **`~/.ssh/config`.** hostname / user (default `root`) / port (default 22)
//!   / identityfile are honoured via [`russh_config`], matching upstream's
//!   `paramiko.SSHConfig` lookup.
//! * **`run` timeout.** The per-command timeout bounds the *no-output* window,
//!   not total runtime — a command that keeps producing output runs as long as
//!   it likes, but one that goes silent for the whole window is treated as
//!   stuck and aborted with [`HostError::Timeout`]. This is the non-interactive
//!   contract (upstream's `interactive=False` branch); the async model has no
//!   TTY prompt to loop on.
//! * **`run` output/lifetime bounds (th4o.6, no upstream port target).** Beyond
//!   the inactivity window, `run` additionally (a) caps the captured
//!   stdout/stderr at [`MAX_STREAM_BYTES`] per stream / [`MAX_TOTAL_BYTES`]
//!   combined, discarding the overflow instead of buffering it and flagging the
//!   resulting [`CommandLog`] `truncated`; and (b) in **non-interactive** runs
//!   enforces an *absolute* execution deadline
//!   (`connection_timeout * COMMAND_DEADLINE_FACTOR`) so a command that trickles
//!   output forever — which never trips the inactivity window — cannot hang a
//!   headless / `mtui-mcp` run. Upstream's blocking loop has neither bound (it
//!   appends `recv(1024)` chunks unbounded and only guards inactivity), so these
//!   are deliberate DoS hardening, not parity. In the REPL a human may answer
//!   the keep-waiting prompt indefinitely, so no absolute deadline is imposed
//!   there. An aborted/deadlined command's channel is closed before returning so
//!   no orphaned remote process/channel leaks (upstream's `close_session`).
//! * **`fire_and_forget`.** Dispatches on a fresh channel and closes the local
//!   link without awaiting completion — for reboot-style commands that tear
//!   down the transport; callers follow up with [`reconnect`](SshConnection).
//!
//! ## Deviations
//!
//! * **ProxyCommand** is not yet executed (russh needs a spawned-process
//!   stream); a host that relies on it degrades to a direct connect and is a
//!   documented follow-up. Upstream supports it via `paramiko.ProxyCommand`.
//! * **`sftp_open`** returns the file's bytes rather than a live file handle
//!   (the object-safe trait surface); this covers every current caller.
//! * The interactive PTY `shell` (feature `shell`, P2.10) returns an
//!   object-safe [`ShellChannel`] duplex over the PTY; the raw-`termios` local
//!   terminal bridge that consumes it is a CLI concern (Phase 6).

use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use mtui_types::hostlog::CommandLog;
use russh::client::{self, Handle};
use russh::keys::agent::AgentIdentity;
use russh::keys::agent::client::AgentClient;
use russh::keys::{HashAlg, PrivateKey, PrivateKeyWithHashAlg, PublicKey, load_secret_key};
use russh::{ChannelMsg, client::Config as ClientConfig};
use russh_sftp::client::SftpSession;
use tokio::time::{Duration, timeout};

#[cfg(feature = "shell")]
use super::ShellChannel;
use super::timeout::{CommandTimeout, HostKeyPolicy};
use super::{Connection, DEFAULT_USER};
use crate::error::{HostError, Result};

/// Number of reconnect+retry attempts before giving up, matching upstream
/// `RETRIES`.
const RETRIES: usize = 5;

/// The exit-code sentinel upstream uses when a command produced no exit status
/// (killed / channel lost). Kept in sync with [`CommandLog`]'s `-1` convention.
const NO_EXIT_CODE: i16 = -1;

/// Maximum bytes captured **per stream** (stdout, stderr) for one command.
///
/// A command that emits more has its excess for that stream discarded (not
/// buffered) and the resulting [`CommandLog`] is flagged
/// [`truncated`](CommandLog::truncated). Bounds the memory a single hostile or
/// runaway command (`yes`, `cat /dev/urandom`) can force mtui to hold — a DoS
/// vector upstream's unbounded `recv` loop leaves open. 16 MiB is generous for
/// legitimate `zypper`/`rpm` output while capping the blast radius under
/// host/template fan-out.
pub const MAX_STREAM_BYTES: usize = 16 * 1024 * 1024;

/// Maximum bytes captured **across both streams combined** for one command.
///
/// Enforced in addition to [`MAX_STREAM_BYTES`] so a command that splits a flood
/// evenly across stdout and stderr still cannot exceed a fixed total. Set to
/// twice the per-stream cap so each stream may independently reach its own limit
/// while the combined memory ceiling stays fixed and bounded.
pub const MAX_TOTAL_BYTES: usize = 2 * MAX_STREAM_BYTES;

/// Absolute wall-clock ceiling multiplier applied to the connection timeout to
/// derive a command's hard execution deadline in **non-interactive** runs.
///
/// A command that keeps producing output never trips the inactivity window, so a
/// headless / `mtui-mcp` run would otherwise hang forever on a command that
/// trickles output (`while true; do echo .; sleep 1; done`). The deadline is
/// `connection_timeout * COMMAND_DEADLINE_FACTOR`; it is enforced **only** when
/// there is no interactive user to answer the keep-waiting prompt (a REPL user
/// who chooses to keep waiting is never force-aborted — upstream parity). The
/// factor keeps the ceiling well above the inactivity window so legitimately
/// long, chatty commands (large `zypper` transactions) still complete.
const COMMAND_DEADLINE_FACTOR: u32 = 12;

/// Accumulates a command's stdout/stderr under fixed per-stream and combined
/// byte caps, discarding overflow instead of buffering it.
///
/// Each `push_*` copies only up to the remaining per-stream **and** remaining
/// combined budget; once either is reached the rest of the chunk is dropped and
/// [`truncated`](Self::truncated) latches `true`. This is the bounded
/// re-expression of upstream's unbounded `stdout += recv(1024)` accumulation.
#[derive(Debug, Default)]
struct CaptureBuf {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    /// Combined bytes captured so far (`stdout.len() + stderr.len()`), tracked
    /// explicitly so the combined cap binds regardless of the per-stream split.
    total: usize,
    truncated: bool,
}

impl CaptureBuf {
    fn push_stdout(&mut self, data: &[u8]) {
        let room = MAX_STREAM_BYTES.saturating_sub(self.stdout.len());
        let take = self.take(room, data);
        self.stdout.extend_from_slice(&data[..take]);
    }

    fn push_stderr(&mut self, data: &[u8]) {
        let room = MAX_STREAM_BYTES.saturating_sub(self.stderr.len());
        let take = self.take(room, data);
        self.stderr.extend_from_slice(&data[..take]);
    }

    /// Returns how many leading bytes of `data` fit under both the per-stream
    /// `stream_room` and the remaining combined budget, advancing the running
    /// total and latching [`truncated`](Self::truncated) if any byte is dropped.
    fn take(&mut self, stream_room: usize, data: &[u8]) -> usize {
        let combined_room = MAX_TOTAL_BYTES.saturating_sub(self.total);
        let room = stream_room.min(combined_room);
        let take = data.len().min(room);
        if take < data.len() {
            self.truncated = true;
        }
        self.total += take;
        take
    }
}

/// An async prompt invoked when a command hits its no-output timeout window.
///
/// Called with the prompt text; resolves to the user's answer (empty / `y` to
/// keep waiting, `n` to abort). The composition root (`mtui-cli`) wires a
/// [`Prompter::ask`](crate::prompter::Prompter::ask) here so the prompt is
/// serialised across parallel host tasks and suspends any live spinner. `None`
/// (headless / `mtui-mcp`) leaves the timeout an immediate abort (upstream's
/// `timeout_prompt=None`).
pub type TimeoutPrompt = Arc<
    dyn Fn(String) -> Pin<Box<dyn Future<Output = std::io::Result<String>> + Send>> + Send + Sync,
>;

/// The outcome of a command-timeout: resume the wait loop or abort the run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TimeoutDecision {
    /// Keep waiting for output (user answered empty / `y`).
    KeepWaiting,
    /// Abort with [`HostError::Timeout`] (user answered `n`, or headless).
    Abort,
}

/// Decides what to do when a command hits its no-output timeout window.
///
/// Extracted from [`SshConnection::run`] so the wait/abort/headless-WARN policy
/// is unit-testable without a live SSH channel. Mirrors upstream
/// `connection.py`'s timeout branch: interactive + a prompt → ask (empty / `y`
/// keep waiting, `n` abort); otherwise abort immediately and emit one WARN so
/// the non-interactive silence is observable.
async fn on_command_timeout(
    hostname: &str,
    command: &str,
    is_repl: bool,
    prompt: Option<&TimeoutPrompt>,
) -> TimeoutDecision {
    if is_repl && let Some(prompt) = prompt {
        let text = format!("command '{command}' timed out on {hostname}; keep waiting? [Y/n] ");
        let answer = prompt(text).await.unwrap_or_default();
        if answer.trim().eq_ignore_ascii_case("n") {
            return TimeoutDecision::Abort;
        }
        // Empty / `y` / anything else: keep waiting (upstream Enter/Y default).
        return TimeoutDecision::KeepWaiting;
    }
    tracing::warn!(
        host = %hostname,
        command,
        "command timed out with no output; aborting (non-interactive)",
    );
    TimeoutDecision::Abort
}

/// The russh client handler: it verifies the server's host key against
/// `known_hosts` first, then applies the [`HostKeyPolicy`] only to keys that
/// are *not already recorded* — mirroring paramiko's `load_system_host_keys()`
/// + `MissingHostKeyPolicy` layering.
///
/// A key that matches an existing `known_hosts` entry is accepted regardless of
/// policy; a key that *differs* from a recorded one (`BadHostKeyException` in
/// paramiko) is rejected under every policy and reported distinctly. Only an
/// unknown host falls through to the policy: `auto_add` accepts and persists the
/// key atomically, `warn` accepts without persisting, and `reject` refuses.
struct ClientHandler {
    hostname: String,
    /// The resolved connect host (post `~/.ssh/config`) used as the
    /// `known_hosts` lookup key, so config aliases/`HostName` match.
    connect_host: String,
    /// The resolved port, so a non-22 host matches its `[host]:port` entry.
    port: u16,
    policy: HostKeyPolicy,
    /// The `known_hosts` file to consult/append. `None` uses russh's default
    /// (`~/.ssh/known_hosts`); tests point it at a temp file.
    known_hosts_path: Option<PathBuf>,
}

impl client::Handler for ClientHandler {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        server_public_key: &PublicKey,
    ) -> std::result::Result<bool, Self::Error> {
        Ok(self.verify(server_public_key))
    }
}

impl ClientHandler {
    /// Verifies `server_public_key` against `known_hosts`, then applies the
    /// [`HostKeyPolicy`] to unknown hosts. Returns whether to accept the key.
    ///
    /// Never logs raw key material — only fingerprints.
    fn verify(&self, server_public_key: &PublicKey) -> bool {
        use russh::keys::Error as KeyError;
        use russh::keys::known_hosts::check_known_hosts_path;

        let fingerprint = server_public_key.fingerprint(Default::default());
        let path = self.known_hosts();

        match check_known_hosts_path(&self.connect_host, self.port, server_public_key, &path) {
            // Recorded and matching: accept regardless of policy.
            Ok(true) => {
                tracing::debug!(
                    host = %self.hostname,
                    %fingerprint,
                    "host key matches known_hosts",
                );
                true
            }
            // Recorded but *different*: a changed key (paramiko
            // BadHostKeyException). Reject under every policy and report it
            // distinctly — never silently auto-add over a changed key.
            Err(KeyError::KeyChanged { line }) => {
                tracing::error!(
                    host = %self.hostname,
                    %fingerprint,
                    line,
                    "host key CHANGED from the one recorded in known_hosts; \
                     rejecting (possible MITM). Verify the host and remove the \
                     stale line if the change is expected.",
                );
                false
            }
            // Unknown host: apply the policy.
            Ok(false) => self.apply_policy(server_public_key, &fingerprint, &path),
            // Any other lookup failure (no home dir, parse error, I/O): the key
            // is *not verified*. Under `reject` refuse; otherwise fall through
            // to the unknown-host policy (matching paramiko's empty-store
            // behaviour when known_hosts is missing/unreadable).
            Err(e) => {
                tracing::warn!(
                    host = %self.hostname,
                    %fingerprint,
                    "known_hosts lookup failed: {e}; treating host as unknown",
                );
                self.apply_policy(server_public_key, &fingerprint, &path)
            }
        }
    }

    /// The `known_hosts` path to use: the test override, else russh's default
    /// `~/.ssh/known_hosts`.
    fn known_hosts(&self) -> PathBuf {
        self.known_hosts_path.clone().unwrap_or_else(|| {
            dirs_home()
                .map(|h| h.join(".ssh").join("known_hosts"))
                .unwrap_or_default()
        })
    }

    /// Applies the [`HostKeyPolicy`] to an unknown host key.
    fn apply_policy(
        &self,
        server_public_key: &PublicKey,
        fingerprint: &impl std::fmt::Display,
        path: &Path,
    ) -> bool {
        match self.policy {
            HostKeyPolicy::AutoAdd => {
                tracing::debug!(host = %self.hostname, %fingerprint, "auto-adding host key");
                persist_host_key(&self.connect_host, self.port, server_public_key, path);
                true
            }
            HostKeyPolicy::Warn => {
                tracing::warn!(
                    host = %self.hostname,
                    %fingerprint,
                    "accepting unknown host key (warn policy); not persisting",
                );
                true
            }
            HostKeyPolicy::Reject => {
                tracing::error!(
                    host = %self.hostname,
                    %fingerprint,
                    "rejecting unknown host key (reject policy)",
                );
                false
            }
        }
    }
}

/// Resolved connection parameters after `~/.ssh/config` lookup.
#[derive(Debug, Clone)]
struct Resolved {
    /// The address to dial (config `HostName`, else the requested hostname).
    connect_host: String,
    /// The port to dial (config `Port`, else the requested port, else 22).
    port: u16,
    /// The login user (config `User`, else `root`).
    user: String,
    /// Identity files to try, in order (config `IdentityFile`s + defaults).
    identity_files: Vec<PathBuf>,
}

/// One russh-backed SSH/SFTP connection to a single host.
///
/// Construct with [`SshConnection::connect`]; then drive it through the
/// [`Connection`] trait. Holds the live russh [`Handle`] plus the parameters
/// needed to re-establish it on [`reconnect`](Connection::reconnect).
pub struct SshConnection {
    hostname: String,
    resolved: Resolved,
    policy: HostKeyPolicy,
    timeout: CommandTimeout,
    handle: Option<Handle<ClientHandler>>,
    /// Whether a TTY-backed user can answer the command-timeout prompt. `false`
    /// (the default, and always under `mtui-mcp`) makes a no-output timeout
    /// abort instead of asking. Mirrors upstream `Connection.interactive`.
    is_repl: bool,
    /// Optional serialised prompt for the command-timeout branch. Wired from the
    /// composition root; `None` keeps the timeout an immediate abort.
    timeout_prompt: Option<TimeoutPrompt>,
}

impl std::fmt::Debug for SshConnection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SshConnection")
            .field("hostname", &self.hostname)
            .field("port", &self.resolved.port)
            .field("user", &self.resolved.user)
            .field("connected", &self.handle.is_some())
            .finish()
    }
}

impl SshConnection {
    /// Connects to `hostname` on `port` (0 means "use `~/.ssh/config` / 22"),
    /// applying `policy` to the host key and `timeout` to the handshake.
    ///
    /// # Errors
    ///
    /// * [`HostError::Connect`] — the host is unreachable or the SSH handshake
    ///   failed (banner/timeout/protocol).
    /// * [`HostError::Auth`] — pubkey/agent authentication was rejected (there
    ///   is no password fallback).
    pub async fn connect(
        hostname: impl Into<String>,
        port: u16,
        policy: HostKeyPolicy,
        timeout: CommandTimeout,
    ) -> Result<Self> {
        let hostname = hostname.into();
        let resolved = resolve(&hostname, port);
        let handle = establish(&hostname, &resolved, policy, timeout).await?;
        Ok(Self {
            hostname,
            resolved,
            policy,
            timeout,
            handle: Some(handle),
            is_repl: false,
            timeout_prompt: None,
        })
    }

    /// Enables the interactive command-timeout prompt on this connection.
    ///
    /// When set, a no-output timeout asks the user (via `prompt`, typically a
    /// [`Prompter::ask`](crate::prompter::Prompter::ask) bound closure) whether
    /// to keep waiting (empty / `y`) or abort (`n`), instead of aborting
    /// immediately. Builder-style so the composition root can wire it after
    /// `connect` without widening the object-safe [`Connection`] trait.
    #[must_use]
    pub fn with_timeout_prompt(mut self, prompt: TimeoutPrompt) -> Self {
        self.is_repl = true;
        self.timeout_prompt = Some(prompt);
        self
    }

    /// Overrides the per-command (no-output window) timeout after connecting.
    ///
    /// [`connect`](Self::connect) applies one [`CommandTimeout`] to *both* the
    /// SSH handshake and the per-command wait; this builder lets a caller keep a
    /// normal handshake timeout while setting a different command timeout — the
    /// two concerns are otherwise conflated. Builder-style for the same reason as
    /// [`with_timeout_prompt`](Self::with_timeout_prompt): it stays off the
    /// object-safe [`Connection`] trait.
    #[must_use]
    pub fn with_command_timeout(mut self, timeout: CommandTimeout) -> Self {
        self.timeout = timeout;
        self
    }

    /// Returns the live handle or a [`HostError::Transport`] "not connected".
    fn handle(&self) -> Result<&Handle<ClientHandler>> {
        self.handle.as_ref().ok_or_else(|| HostError::Transport {
            host: self.hostname.clone(),
            reason: "not connected".to_owned(),
        })
    }

    /// Opens the SFTP subsystem on a fresh channel, reconnecting first if the
    /// link has dropped. Mirrors upstream `_sftp` (open per operation).
    async fn sftp(&mut self) -> Result<SftpSession> {
        if !self.is_active() {
            self.reconnect().await?;
        }
        let channel = self
            .handle()?
            .channel_open_session()
            .await
            .map_err(|e| self.sftp_err(e))?;
        channel
            .request_subsystem(true, "sftp")
            .await
            .map_err(|e| self.sftp_err(e))?;
        SftpSession::new(channel.into_stream())
            .await
            .map_err(|e| self.sftp_err(e))
    }

    fn sftp_err(&self, e: impl std::fmt::Display) -> HostError {
        HostError::Sftp {
            host: self.hostname.clone(),
            reason: e.to_string(),
        }
    }

    /// Maps a russh-sftp client error to [`HostError`], routing the
    /// `SSH_FX_NO_SUCH_FILE` status to the dedicated
    /// [`HostError::SftpNotFound`] variant so the host-system parser can branch
    /// on "not found" the way upstream branches on `FileNotFoundError`.
    fn sftp_err_at(&self, e: russh_sftp::client::error::Error, path: &Path) -> HostError {
        use russh_sftp::client::error::Error as SftpError;
        use russh_sftp::protocol::StatusCode;

        if let SftpError::Status(status) = &e
            && status.status_code == StatusCode::NoSuchFile
        {
            return HostError::SftpNotFound {
                host: self.hostname.clone(),
                path: path.to_string_lossy().into_owned(),
            };
        }
        HostError::Sftp {
            host: self.hostname.clone(),
            reason: e.to_string(),
        }
    }

    /// Categorizes the error from an **atomic exclusive create**
    /// ([`sftp_write`](Connection::sftp_write) with `exclusive = true`).
    ///
    /// SFTPv3 has no dedicated "file exists" status, so an `O_EXCL` collision
    /// surfaces as the generic [`StatusCode::Failure`]. That is the only status
    /// mapped to [`HostError::AlreadyExists`] (so the lock protocol reconciles
    /// the race). Every other case fails **closed** — it propagates as a real
    /// error rather than being mistaken for lost contention:
    ///
    /// * [`StatusCode::NoSuchFile`] → [`HostError::SftpNotFound`] (a missing
    ///   parent directory, not a collision),
    /// * every other status (`PermissionDenied`, `OpUnsupported`,
    ///   `NoConnection`, `ConnectionLost`, …) → [`HostError::Sftp`],
    /// * a non-status (transport/IO) error → [`HostError::Transport`].
    fn exclusive_create_err(
        &self,
        e: russh_sftp::client::error::Error,
        path_str: &str,
    ) -> HostError {
        exclusive_create_err(&self.hostname, e, path_str)
    }

    fn transport_err(&self, e: impl std::fmt::Display) -> HostError {
        HostError::Transport {
            host: self.hostname.clone(),
            reason: e.to_string(),
        }
    }
}

/// Categorizes the error from an **atomic exclusive create**
/// ([`Connection::sftp_write`](crate::Connection::sftp_write) with
/// `exclusive = true`).
///
/// SFTPv3 has no dedicated "file exists" status, so an `O_EXCL` collision
/// surfaces as the generic [`StatusCode::Failure`]. That is the only status
/// mapped to [`HostError::AlreadyExists`] (so the lock protocol reconciles the
/// race). Every other case fails **closed** — it propagates as a real error
/// rather than being mistaken for lost contention:
///
/// * [`StatusCode::NoSuchFile`] → [`HostError::SftpNotFound`] (a missing parent
///   directory, not a collision),
/// * every other status (`PermissionDenied`, `OpUnsupported`, `NoConnection`,
///   `ConnectionLost`, …) → [`HostError::Sftp`],
/// * a non-status (transport/IO) error → [`HostError::Transport`].
///
/// [`StatusCode::Failure`]: russh_sftp::protocol::StatusCode::Failure
/// [`StatusCode::NoSuchFile`]: russh_sftp::protocol::StatusCode::NoSuchFile
fn exclusive_create_err(
    hostname: &str,
    e: russh_sftp::client::error::Error,
    path_str: &str,
) -> HostError {
    use russh_sftp::client::error::Error as SftpError;
    use russh_sftp::protocol::StatusCode;

    if let SftpError::Status(status) = &e {
        match status.status_code {
            StatusCode::Failure => {
                tracing::debug!(
                    host = %hostname, path = %path_str, error = %e,
                    "exclusive sftp create did not win the race"
                );
                return HostError::AlreadyExists {
                    host: hostname.to_owned(),
                    path: path_str.to_owned(),
                };
            }
            StatusCode::NoSuchFile => {
                return HostError::SftpNotFound {
                    host: hostname.to_owned(),
                    path: path_str.to_owned(),
                };
            }
            _ => {}
        }
        tracing::debug!(
            host = %hostname, path = %path_str, error = %e,
            "exclusive sftp create failed (not contention)"
        );
        return HostError::Sftp {
            host: hostname.to_owned(),
            reason: e.to_string(),
        };
    }
    tracing::debug!(
        host = %hostname, path = %path_str, error = %e,
        "exclusive sftp create failed at transport"
    );
    HostError::Transport {
        host: hostname.to_owned(),
        reason: e.to_string(),
    }
}

/// Resolves `~/.ssh/config` for `hostname`, falling back to sensible defaults.
fn resolve(hostname: &str, port: u16) -> Resolved {
    let cfg = russh_config::parse_home(hostname).ok();

    let (cfg_host, cfg_user, cfg_port, cfg_identities) = match cfg {
        Some(ref c) => (
            c.host().to_owned(),
            c.host_config.user.clone(),
            c.host_config.port,
            c.host_config.identity_file.clone().unwrap_or_default(),
        ),
        None => (hostname.to_owned(), None, None, Vec::new()),
    };

    let mut identity_files = cfg_identities;
    if identity_files.is_empty() {
        identity_files = default_identity_files();
    }

    Resolved {
        connect_host: cfg_host,
        port: cfg_port
            .or(if port == 0 { None } else { Some(port) })
            .unwrap_or(22),
        user: cfg_user.unwrap_or_else(|| DEFAULT_USER.to_owned()),
        identity_files,
    }
}

/// The default private keys to try when config names none, mirroring the common
/// paramiko/ssh defaults.
fn default_identity_files() -> Vec<PathBuf> {
    let Some(home) = dirs_home() else {
        return Vec::new();
    };
    ["id_ed25519", "id_ecdsa", "id_rsa"]
        .into_iter()
        .map(|name| home.join(".ssh").join(name))
        .filter(|p| p.exists())
        .collect()
}

/// Best-effort `$HOME`.
fn dirs_home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

/// Best-effort atomic append of `host[:port] <openssh-pubkey>` to `known_hosts`.
///
/// Reads any existing content, then hands the full buffer to
/// [`mtui_config::atomic::write`] — the single secure temp-file + rename
/// implementation (unique `create_new` + `0o600` temp, fsync, rename) shared
/// across the workspace (the file-safety contract from th4o.11) — so a
/// concurrent reader never sees a half-written file and no predictable-name temp
/// can be pre-created by an attacker.
///
/// This is advisory, mirroring paramiko's `save_host_keys`: any failure is
/// logged and swallowed so a fresh host still connects under `auto_add`. Never
/// logs raw key material.
fn persist_host_key(host: &str, port: u16, pubkey: &PublicKey, path: &Path) {
    if let Err(e) = persist_host_key_inner(host, port, pubkey, path) {
        tracing::warn!(host, "failed to persist host key to known_hosts: {e}");
    }
}

fn persist_host_key_inner(
    host: &str,
    port: u16,
    pubkey: &PublicKey,
    path: &Path,
) -> std::io::Result<()> {
    let openssh = pubkey
        .to_openssh()
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?;
    let entry = if port == 22 {
        format!("{host} {openssh}\n")
    } else {
        format!("[{host}]:{port} {openssh}\n")
    };

    // Preserve existing entries: rewrite the whole file (existing + new).
    let mut contents = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(e) => return Err(e),
    };
    if !contents.is_empty() && !contents.ends_with(b"\n") {
        contents.push(b'\n');
    }
    contents.extend_from_slice(entry.as_bytes());

    // Delegate the secure temp-file + rename to the shared helper.
    mtui_config::atomic::write(&contents, path)
}

/// Establishes the transport and authenticates. Shared by `connect` and
/// `reconnect`.
async fn establish(
    hostname: &str,
    resolved: &Resolved,
    policy: HostKeyPolicy,
    ctimeout: CommandTimeout,
) -> Result<Handle<ClientHandler>> {
    let config = Arc::new(ClientConfig {
        inactivity_timeout: Some(Duration::from_secs(60)),
        ..ClientConfig::default()
    });
    let handler = ClientHandler {
        hostname: hostname.to_owned(),
        connect_host: resolved.connect_host.clone(),
        port: resolved.port,
        policy,
        known_hosts_path: None,
    };

    let addr = (resolved.connect_host.as_str(), resolved.port);
    let connect_fut = client::connect(config, addr, handler);
    let mut handle = match timeout(ctimeout.as_duration(), connect_fut).await {
        Ok(Ok(handle)) => handle,
        Ok(Err(e)) => {
            return Err(HostError::Connect {
                host: hostname.to_owned(),
                reason: e.to_string(),
            });
        }
        Err(_) => {
            return Err(HostError::Connect {
                host: hostname.to_owned(),
                reason: format!("connection timed out after {}s", ctimeout.as_secs()),
            });
        }
    };

    if authenticate(&mut handle, hostname, resolved).await? {
        Ok(handle)
    } else {
        Err(HostError::Auth {
            host: hostname.to_owned(),
        })
    }
}

/// Tries agent keys, then identity files. Returns `Ok(true)` on the first
/// success. Pubkey/agent only — no password path exists.
async fn authenticate(
    handle: &mut Handle<ClientHandler>,
    hostname: &str,
    resolved: &Resolved,
) -> Result<bool> {
    // 1. SSH agent (SSH_AUTH_SOCK), if present.
    if let Ok(mut agent) = AgentClient::connect_env().await
        && let Ok(identities) = agent.request_identities().await
    {
        for identity in identities {
            // russh 0.62 yields `AgentIdentity` (plain key or certificate);
            // pubkey auth only takes a bare `PublicKey`, so skip certificates
            // (paramiko parity: agent pubkey auth only).
            let AgentIdentity::PublicKey { key, .. } = identity else {
                continue;
            };
            match handle
                .authenticate_publickey_with(&resolved.user, key, best_hash(), &mut agent)
                .await
            {
                Ok(res) if res.success() => return Ok(true),
                Ok(_) => {}
                Err(e) => tracing::debug!(host = %hostname, "agent auth attempt failed: {e}"),
            }
        }
    }

    // 2. Identity files from config / defaults.
    for path in &resolved.identity_files {
        let key = match load_secret_key(path, None) {
            Ok(key) => key,
            Err(e) => {
                tracing::debug!(host = %hostname, path = %path.display(), "skipping unreadable key: {e}");
                continue;
            }
        };
        let key = Arc::new(key);
        if try_key(handle, &resolved.user, &key).await? {
            return Ok(true);
        }
    }

    Ok(false)
}

/// Attempts pubkey auth with one loaded key, trying an RSA SHA-2 hash where
/// applicable.
async fn try_key(
    handle: &mut Handle<ClientHandler>,
    user: &str,
    key: &Arc<PrivateKey>,
) -> Result<bool> {
    let with_alg = PrivateKeyWithHashAlg::new(key.clone(), best_hash());
    match handle.authenticate_publickey(user, with_alg).await {
        Ok(res) => Ok(res.success()),
        Err(e) => {
            tracing::debug!("pubkey auth attempt errored: {e}");
            Ok(false)
        }
    }
}

/// Preferred RSA hash (ignored for non-RSA keys by russh).
fn best_hash() -> Option<HashAlg> {
    Some(HashAlg::Sha512)
}

/// Validates that a server-supplied SFTP directory entry name is a single,
/// ordinary path component before it is used to build a local write path.
///
/// The remote peer controls directory-entry names; concatenating one verbatim
/// into a local path (`{local}{name}.{host}`) lets a hostile/compromised host
/// escape the download destination via `../`, an absolute path, a nested
/// `a/b`, or a Windows-style separator, and overwrite arbitrary local files.
/// Accept `name` iff it is exactly one [`std::path::Component::Normal`] equal to
/// itself and free of separators / control bytes; otherwise return
/// [`HostError::UnsafeSftpName`].
pub(crate) fn validate_sftp_component<'a>(name: &'a str, host: &str) -> Result<&'a str> {
    let reject = || HostError::UnsafeSftpName {
        host: host.to_owned(),
        name: name.to_owned(),
    };
    // Fast rejects: empty, dot components, separators (both platforms), and any
    // control byte (NUL, newline, etc.). `\` is rejected regardless of host OS
    // because the *local* side may be Windows.
    if name.is_empty()
        || name == "."
        || name == ".."
        || name.contains('/')
        || name.contains('\\')
        || name.chars().any(char::is_control)
    {
        return Err(reject());
    }
    // Defensive structural check: the name must resolve to exactly one normal
    // component identical to the input (catches drive/root prefixes and any
    // separator form the byte checks above might miss on other platforms).
    let mut comps = Path::new(name).components();
    match (comps.next(), comps.next()) {
        (Some(std::path::Component::Normal(c)), None) if c == name => Ok(name),
        _ => Err(reject()),
    }
}

#[async_trait]
impl Connection for SshConnection {
    fn hostname(&self) -> &str {
        &self.hostname
    }

    fn clone_box(&self) -> Box<dyn Connection> {
        // russh 0.62's `Handle` is neither `Clone` nor cheaply shareable across
        // the reconnect-swap that `reconnect`/`close` perform, so we cannot
        // hand out the *same* live channel here. Instead we clone the connection
        // *identity* (host/policy/timeout) with an empty handle; the first SFTP
        // op the clone performs opens its own session via `sftp()`'s
        // `reconnect`-if-inactive path. This means a `TargetLock` built from the
        // clone uses a second short-lived session to the same host for its
        // (rare) force-unlock safeguard — functionally correct, at the cost of
        // one extra channel on that path only. The mock double shares state via
        // `Arc`, so offline unit tests still observe the lock's SFTP ops.
        Box::new(Self {
            hostname: self.hostname.clone(),
            resolved: self.resolved.clone(),
            policy: self.policy,
            timeout: self.timeout,
            handle: None,
            is_repl: self.is_repl,
            timeout_prompt: self.timeout_prompt.clone(),
        })
    }

    async fn run(&mut self, command: &str) -> Result<CommandLog> {
        let started = Instant::now();

        // Open a channel, reconnecting + retrying on a lost link (upstream
        // loops open->reconnect up to RETRIES then raises ReConnectFailed).
        let mut attempt = 0;
        let mut channel = loop {
            if !self.is_active() {
                self.reconnect().await?;
            }
            match self.handle()?.channel_open_session().await {
                Ok(ch) => break ch,
                Err(e) => {
                    attempt += 1;
                    if attempt >= RETRIES {
                        return Err(HostError::ReconnectFailed {
                            host: self.hostname.clone(),
                        });
                    }
                    tracing::debug!(host = %self.hostname, "channel open failed ({e}); retrying");
                    self.reconnect().await?;
                }
            }
        };

        channel
            .exec(true, command)
            .await
            .map_err(|e| self.transport_err(e))?;
        // run() never feeds stdin: send EOF so a command that reads input gets
        // it and proceeds instead of blocking (upstream shutdown_write).
        let _ = channel.eof().await;

        let mut capture = CaptureBuf::default();
        let mut exitcode: i16 = NO_EXIT_CODE;
        let window = self.timeout.as_duration();
        // Absolute execution ceiling for non-interactive runs (headless /
        // `mtui-mcp`), which have no user to answer the keep-waiting prompt: a
        // command trickling output forever never trips the inactivity window, so
        // without this it would hang the run indefinitely. In the REPL there is a
        // human who may legitimately choose to keep waiting, so no absolute
        // deadline is imposed there (upstream parity).
        let deadline = (!self.is_repl)
            .then(|| Instant::now() + window.saturating_mul(COMMAND_DEADLINE_FACTOR));

        loop {
            // Enforce the absolute (non-interactive) deadline up front: continuous
            // output keeps `channel.wait()` returning data so the inactivity
            // branch never fires — the deadline must be checked every iteration,
            // not only on a wait timeout.
            if let Some(d) = deadline
                && Instant::now() >= d
            {
                tracing::warn!(
                    host = %self.hostname,
                    command,
                    "command exceeded absolute deadline; aborting (non-interactive)",
                );
                let _ = channel.close().await;
                return Err(HostError::Timeout {
                    command: command.to_owned(),
                });
            }

            // Bound each wait so the absolute deadline is honoured even under
            // continuous output (which would otherwise keep resetting `window`).
            // Interactive runs use the plain inactivity window.
            let wait_for = match deadline {
                Some(d) => window.min(d.saturating_duration_since(Instant::now())),
                None => window,
            };
            match timeout(wait_for, channel.wait()).await {
                // No message within the wait budget: either the absolute deadline
                // elapsed (non-interactive hard cap) or the no-output inactivity
                // window did.
                Err(_) => {
                    if let Some(d) = deadline
                        && Instant::now() >= d
                    {
                        // Non-interactive hard cap reached: abort. Close the
                        // channel so the remote process/channel is not orphaned.
                        tracing::warn!(
                            host = %self.hostname,
                            command,
                            "command exceeded absolute deadline; aborting (non-interactive)",
                        );
                        let _ = channel.close().await;
                        return Err(HostError::Timeout {
                            command: command.to_owned(),
                        });
                    }
                    // Inactivity window. Interactive: ask the user whether to keep
                    // waiting. Empty / `y` resumes the wait loop (upstream's
                    // Enter/Y default); `n` aborts. Headless: abort immediately,
                    // emitting one WARN so the silence is observable (upstream's
                    // `timeout_prompt=None` branch).
                    let decision = on_command_timeout(
                        &self.hostname,
                        command,
                        self.is_repl,
                        self.timeout_prompt.as_ref(),
                    )
                    .await;
                    match decision {
                        TimeoutDecision::KeepWaiting => continue,
                        TimeoutDecision::Abort => {
                            // Close the channel so the abandoned command's remote
                            // process/channel is reclaimed (upstream close_session
                            // on BaseException).
                            let _ = channel.close().await;
                            return Err(HostError::Timeout {
                                command: command.to_owned(),
                            });
                        }
                    }
                }
                // Channel closed cleanly.
                Ok(None) => break,
                Ok(Some(msg)) => match msg {
                    ChannelMsg::Data { data } => capture.push_stdout(&data),
                    ChannelMsg::ExtendedData { data, .. } => capture.push_stderr(&data),
                    ChannelMsg::ExitStatus { exit_status } => {
                        exitcode = i16::try_from(exit_status).unwrap_or(NO_EXIT_CODE);
                    }
                    ChannelMsg::Eof => {}
                    ChannelMsg::Close => break,
                    _ => {}
                },
            }
        }

        if capture.truncated {
            tracing::warn!(
                host = %self.hostname,
                command,
                "command output exceeded capture caps; truncated",
            );
        }
        let runtime = i64::try_from(started.elapsed().as_secs()).unwrap_or(i64::MAX);
        Ok(CommandLog::new(
            command,
            String::from_utf8_lossy(&capture.stdout).into_owned(),
            String::from_utf8_lossy(&capture.stderr).into_owned(),
            exitcode,
            runtime,
        )
        .with_flags(capture.truncated, false))
    }

    fn is_active(&self) -> bool {
        self.handle.as_ref().is_some_and(|h| !h.is_closed())
    }

    async fn close(&mut self) -> Result<()> {
        if let Some(handle) = self.handle.take() {
            let _ = handle
                .disconnect(russh::Disconnect::ByApplication, "", "")
                .await;
        }
        Ok(())
    }

    async fn reconnect(&mut self) -> Result<()> {
        if self.is_active() {
            return Ok(());
        }
        let mut last_err = None;
        for attempt in 0..=RETRIES {
            match establish(&self.hostname, &self.resolved, self.policy, self.timeout).await {
                Ok(handle) => {
                    self.handle = Some(handle);
                    return Ok(());
                }
                Err(e) => {
                    tracing::debug!(host = %self.hostname, attempt, "reconnect attempt failed: {e}");
                    last_err = Some(e);
                    // brief backoff between attempts
                    tokio::time::sleep(Duration::from_millis(200)).await;
                }
            }
        }
        tracing::debug!(host = %self.hostname, "reconnect gave up: {last_err:?}");
        Err(HostError::ReconnectFailed {
            host: self.hostname.clone(),
        })
    }

    async fn fire_and_forget(&mut self, command: &str) -> Result<()> {
        let channel = self
            .handle()?
            .channel_open_session()
            .await
            .map_err(|e| self.transport_err(e))?;
        // Dispatch without awaiting completion; a link dropped afterward is
        // expected (e.g. reboot). Then tear down the local connection.
        channel
            .exec(false, command)
            .await
            .map_err(|e| self.transport_err(e))?;
        self.close().await
    }

    async fn sftp_put(&mut self, local: &Path, remote: &Path) -> Result<()> {
        let data = tokio::fs::read(local).await.map_err(|e| HostError::Sftp {
            host: self.hostname.clone(),
            reason: format!("read {}: {e}", local.display()),
        })?;
        let sftp = self.sftp().await?;

        // Create parent directories (best-effort; "already exists" is success).
        let remote_str = remote.to_string_lossy();
        let parts: Vec<&str> = remote_str.split('/').collect();
        let mut path = String::new();
        for subdir in &parts[..parts.len().saturating_sub(1)] {
            if subdir.is_empty() {
                path.push('/');
                continue;
            }
            path.push_str(subdir);
            path.push('/');
            let _ = sftp.create_dir(path.clone()).await;
        }

        sftp.write(remote_str.to_string(), &data)
            .await
            .map_err(|e| self.sftp_err(e))?;
        // Make executable (0770), matching upstream chmod after put.
        if let Ok(mut meta) = sftp.metadata(remote_str.to_string()).await {
            meta.permissions = Some(0o770);
            let _ = sftp.set_metadata(remote_str.to_string(), meta).await;
        }
        let _ = sftp.close().await;
        Ok(())
    }

    async fn sftp_get(&mut self, remote: &Path, local: &Path) -> Result<()> {
        let sftp = self.sftp().await?;
        let data = sftp
            .read(remote.to_string_lossy().to_string())
            .await
            .map_err(|e| self.sftp_err(e))?;
        let _ = sftp.close().await;
        tokio::fs::write(local, &data)
            .await
            .map_err(|e| HostError::Sftp {
                host: self.hostname.clone(),
                reason: format!("write {}: {e}", local.display()),
            })
    }

    async fn sftp_get_folder(&mut self, remote: &Path, local: &Path) -> Result<()> {
        let sftp = self.sftp().await?;
        let remote_str = remote.to_string_lossy().to_string();
        let dir = sftp
            .read_dir(remote_str.clone())
            .await
            .map_err(|e| self.sftp_err(e))?;
        for entry in dir {
            let name = entry.file_name();
            // The peer controls entry names; a crafted name (`../x`, `/etc/x`,
            // `a/b`) would escape the download destination. Reject non-component
            // names and skip them — a hostile entry must not abort the transfer
            // of the legitimate ones (best-effort transfer contract). The name
            // is logged quoted, and no local path is emitted, so the diagnostic
            // cannot leak the attacker's chosen target.
            if let Err(e) = validate_sftp_component(&name, &self.hostname) {
                tracing::warn!(host = %self.hostname, error = %e, "skipping unsafe SFTP entry");
                continue;
            }
            // Stream remote -> local rather than buffering the whole file.
            let mut src = sftp
                .open(format!("{remote_str}/{name}"))
                .await
                .map_err(|e| self.sftp_err(e))?;
            // Per-host suffix contract: <local><name>.<hostname>
            let target = format!("{}{}.{}", local.to_string_lossy(), name, self.hostname);
            let mut dst = tokio::fs::File::create(&target)
                .await
                .map_err(|e| HostError::Sftp {
                    host: self.hostname.clone(),
                    reason: format!("create {target}: {e}"),
                })?;
            tokio::io::copy(&mut src, &mut dst)
                .await
                .map_err(|e| HostError::Sftp {
                    host: self.hostname.clone(),
                    reason: format!("write {target}: {e}"),
                })?;
        }
        let _ = sftp.close().await;
        Ok(())
    }

    async fn sftp_listdir(&mut self, path: &Path) -> Result<Vec<String>> {
        let sftp = self.sftp().await?;
        let dir = sftp
            .read_dir(path.to_string_lossy().to_string())
            .await
            .map_err(|e| self.sftp_err_at(e, path))?;
        let entries = dir.map(|e| e.file_name()).collect();
        let _ = sftp.close().await;
        Ok(entries)
    }

    async fn sftp_open(&mut self, path: &Path) -> Result<Vec<u8>> {
        let sftp = self.sftp().await?;
        let data = sftp
            .read(path.to_string_lossy().to_string())
            .await
            .map_err(|e| self.sftp_err_at(e, path))?;
        let _ = sftp.close().await;
        Ok(data)
    }

    async fn sftp_write(&mut self, path: &Path, data: &[u8], exclusive: bool) -> Result<()> {
        use russh_sftp::protocol::OpenFlags;
        use tokio::io::AsyncWriteExt;

        let sftp = self.sftp().await?;
        let path_str = path.to_string_lossy().to_string();

        if exclusive {
            // Atomic exclusive create (paramiko mode "x" -> O_CREAT | O_EXCL).
            // SFTPv3 has no dedicated "file exists" status, so an O_EXCL
            // collision surfaces as the generic `Failure` status — that (and
            // only that) is mapped to `AlreadyExists` so the lock protocol
            // reconciles the race. Every *other* category (permission denied,
            // operation unsupported, connection lost, non-status transport/IO)
            // must propagate: mapping them to `AlreadyExists` would fail *open*
            // (silently reconcile a genuinely-failed create). The true reason
            // is logged at debug for diagnosis.
            let flags =
                OpenFlags::CREATE | OpenFlags::WRITE | OpenFlags::TRUNCATE | OpenFlags::EXCLUDE;
            let mut file = match sftp.open_with_flags(path_str.clone(), flags).await {
                Ok(f) => f,
                Err(e) => {
                    let err = self.exclusive_create_err(e, &path_str);
                    let _ = sftp.close().await;
                    return Err(err);
                }
            };
            file.write_all(data).await.map_err(|e| self.sftp_err(e))?;
            file.shutdown().await.map_err(|e| self.sftp_err(e))?;
        } else {
            // Truncating overwrite (paramiko mode "w+").
            sftp.write(path_str, data)
                .await
                .map_err(|e| self.sftp_err(e))?;
        }
        let _ = sftp.close().await;
        Ok(())
    }

    async fn sftp_remove(&mut self, path: &Path) -> Result<()> {
        let sftp = self.sftp().await?;
        sftp.remove_file(path.to_string_lossy().to_string())
            .await
            .map_err(|e| self.sftp_err(e))?;
        let _ = sftp.close().await;
        Ok(())
    }

    async fn sftp_rmdir(&mut self, path: &Path) -> Result<()> {
        let sftp = self.sftp().await?;
        let path_str = path.to_string_lossy().to_string();
        if let Ok(dir) = sftp.read_dir(path_str.clone()).await {
            for entry in dir {
                let child = format!("{path_str}/{}", entry.file_name());
                let _ = sftp.remove_file(child).await;
            }
        }
        sftp.remove_dir(path_str)
            .await
            .map_err(|e| self.sftp_err(e))?;
        let _ = sftp.close().await;
        Ok(())
    }

    async fn sftp_readlink(&mut self, path: &Path) -> Result<String> {
        let sftp = self.sftp().await?;
        let target = sftp
            .read_link(path.to_string_lossy().to_string())
            .await
            .map_err(|e| self.sftp_err_at(e, path))?;
        let _ = sftp.close().await;
        Ok(target)
    }

    #[cfg(feature = "shell")]
    async fn shell(&mut self, cols: u32, rows: u32) -> Result<Box<dyn ShellChannel>> {
        // Open a channel, reconnecting + retrying on a lost link, mirroring the
        // open->reconnect loop in `run` (and upstream's `while not session:
        // reconnect()` in `shell`).
        let mut attempt = 0;
        let channel = loop {
            if !self.is_active() {
                self.reconnect().await?;
            }
            match self.handle()?.channel_open_session().await {
                Ok(ch) => break ch,
                Err(e) => {
                    attempt += 1;
                    if attempt >= RETRIES {
                        return Err(HostError::ReconnectFailed {
                            host: self.hostname.clone(),
                        });
                    }
                    tracing::debug!(host = %self.hostname, "shell channel open failed ({e}); retrying");
                    self.reconnect().await?;
                }
            }
        };

        // Request an `xterm` PTY sized cols x rows (no pixel dims, no special
        // terminal modes) then invoke the remote shell — upstream
        // `get_pty("xterm", width, height)` + `invoke_shell()`. On failure,
        // explicitly close the half-initialised channel (upstream's
        // `close_session` in the `except` arm) rather than relying on drop.
        if let Err(e) = channel
            .request_pty(true, "xterm", cols, rows, 0, 0, &[])
            .await
        {
            let _ = channel.close().await;
            return Err(self.transport_err(e));
        }
        if let Err(e) = channel.request_shell(true).await {
            let _ = channel.close().await;
            return Err(self.transport_err(e));
        }

        Ok(Box::new(SshShellChannel {
            host: self.hostname.clone(),
            channel,
            leftover: Vec::new(),
        }))
    }
}

/// A russh-backed [`ShellChannel`]: the interactive PTY duplex returned by
/// [`SshConnection::shell`].
///
/// Reads drain [`ChannelMsg::Data`]/[`ChannelMsg::ExtendedData`] (the PTY
/// merges stdout+stderr, so extended data is folded into the same stream a
/// terminal sees); writes send channel data; resize forwards `window-change`.
#[cfg(feature = "shell")]
struct SshShellChannel {
    host: String,
    channel: russh::Channel<russh::client::Msg>,
    /// Payload bytes received in excess of a previous `read`'s buffer, served
    /// before the next `wait()`. Mirrors paramiko's `recv(n)`, which leaves
    /// unconsumed bytes buffered in the transport rather than dropping them —
    /// without this, a server frame larger than the caller's buffer would lose
    /// its tail and corrupt interactive output.
    leftover: Vec<u8>,
}

#[cfg(feature = "shell")]
impl SshShellChannel {
    /// Copies up to `buf.len()` bytes of `data` into `buf`, stashing any excess
    /// in `self.leftover` for the next `read`. Returns the count copied.
    fn serve(&mut self, data: &[u8], buf: &mut [u8]) -> usize {
        let n = data.len().min(buf.len());
        buf[..n].copy_from_slice(&data[..n]);
        if n < data.len() {
            self.leftover.extend_from_slice(&data[n..]);
        }
        n
    }
}

#[cfg(feature = "shell")]
#[async_trait]
impl ShellChannel for SshShellChannel {
    async fn read(&mut self, buf: &mut [u8]) -> Result<usize> {
        // Drain any bytes carried over from a previous short read first.
        if !self.leftover.is_empty() {
            let carried = std::mem::take(&mut self.leftover);
            return Ok(self.serve(&carried, buf));
        }
        loop {
            match self.channel.wait().await {
                // Channel closed cleanly: the remote shell exited.
                None => return Ok(0),
                Some(ChannelMsg::Data { data }) | Some(ChannelMsg::ExtendedData { data, .. }) => {
                    return Ok(self.serve(&data, buf));
                }
                Some(ChannelMsg::Eof) | Some(ChannelMsg::Close) => return Ok(0),
                // Ignore control messages (window adjust, exit status, ...) and
                // keep waiting for payload or close.
                Some(_) => {}
            }
        }
    }

    async fn write(&mut self, data: &[u8]) -> Result<()> {
        self.channel
            .data(data)
            .await
            .map_err(|e| HostError::Transport {
                host: self.host.clone(),
                reason: e.to_string(),
            })
    }

    async fn resize(&mut self, cols: u32, rows: u32) -> Result<()> {
        self.channel
            .window_change(cols, rows, 0, 0)
            .await
            .map_err(|e| HostError::Transport {
                host: self.host.clone(),
                reason: e.to_string(),
            })
    }

    async fn close(&mut self) -> Result<()> {
        // Best-effort, idempotent close (upstream `close_session`): a channel
        // the remote already tore down is treated as success per the trait
        // contract, so a double-close never surfaces an error.
        if let Err(e) = self.channel.close().await {
            tracing::debug!(host = %self.host, error = %e, "shell channel already closed");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sftp_status(code: russh_sftp::protocol::StatusCode) -> russh_sftp::client::error::Error {
        russh_sftp::client::error::Error::Status(russh_sftp::protocol::Status {
            id: 0,
            status_code: code,
            error_message: "x".to_owned(),
            language_tag: String::new(),
        })
    }

    // --- CaptureBuf: bounded output capture (th4o.6). ---

    #[test]
    fn capture_small_output_is_not_truncated() {
        let mut c = CaptureBuf::default();
        c.push_stdout(b"hello");
        c.push_stderr(b"warn");
        assert_eq!(c.stdout, b"hello");
        assert_eq!(c.stderr, b"warn");
        assert!(!c.truncated);
    }

    #[test]
    fn capture_caps_stdout_at_per_stream_limit() {
        let mut c = CaptureBuf::default();
        // One oversized chunk: only MAX_STREAM_BYTES is kept, the rest dropped.
        let data = vec![b'a'; MAX_STREAM_BYTES + 4096];
        c.push_stdout(&data);
        assert_eq!(c.stdout.len(), MAX_STREAM_BYTES);
        assert!(c.truncated);
        // Further pushes to the full stream keep dropping.
        c.push_stdout(b"more");
        assert_eq!(c.stdout.len(), MAX_STREAM_BYTES);
    }

    #[test]
    fn capture_caps_stderr_at_per_stream_limit() {
        let mut c = CaptureBuf::default();
        let data = vec![b'e'; MAX_STREAM_BYTES + 1];
        c.push_stderr(&data);
        assert_eq!(c.stderr.len(), MAX_STREAM_BYTES);
        assert!(c.truncated);
    }

    #[test]
    fn capture_exact_per_stream_fit_is_not_truncated() {
        let mut c = CaptureBuf::default();
        let data = vec![b'a'; MAX_STREAM_BYTES];
        c.push_stdout(&data);
        assert_eq!(c.stdout.len(), MAX_STREAM_BYTES);
        assert!(!c.truncated);
    }

    #[test]
    fn capture_enforces_combined_cap_across_streams() {
        let mut c = CaptureBuf::default();
        // Each stream fills its own per-stream cap; together they reach exactly
        // MAX_TOTAL_BYTES (= 2 * MAX_STREAM_BYTES) with nothing dropped.
        c.push_stdout(&vec![b'a'; MAX_STREAM_BYTES]);
        c.push_stderr(&vec![b'e'; MAX_STREAM_BYTES]);
        assert_eq!(c.total, MAX_TOTAL_BYTES);
        assert!(!c.truncated);
        // Any further byte on either stream is over both caps and dropped.
        c.push_stdout(b"x");
        assert_eq!(c.total, MAX_TOTAL_BYTES);
        assert!(c.truncated);
    }

    #[test]
    fn capture_partial_chunk_copies_prefix_then_truncates() {
        let mut c = CaptureBuf::default();
        // Prime the stream near its cap, then push a chunk straddling the limit:
        // only the fitting prefix is copied, and truncated latches.
        c.push_stdout(&vec![b'a'; MAX_STREAM_BYTES - 2]);
        assert!(!c.truncated);
        c.push_stdout(b"XYZ");
        assert_eq!(c.stdout.len(), MAX_STREAM_BYTES);
        assert_eq!(&c.stdout[MAX_STREAM_BYTES - 2..], b"XY");
        assert!(c.truncated);
    }

    #[test]
    fn exclusive_create_failure_is_contention() {
        use russh_sftp::protocol::StatusCode;
        let err =
            exclusive_create_err("h", sftp_status(StatusCode::Failure), "/var/lock/mtui.lock");
        assert!(matches!(err, HostError::AlreadyExists { .. }));
    }

    #[test]
    fn exclusive_create_no_such_file_is_not_found() {
        use russh_sftp::protocol::StatusCode;
        let err = exclusive_create_err(
            "h",
            sftp_status(StatusCode::NoSuchFile),
            "/var/lock/mtui.lock",
        );
        assert!(matches!(err, HostError::SftpNotFound { .. }));
    }

    #[test]
    fn exclusive_create_permission_denied_propagates_as_sftp() {
        use russh_sftp::protocol::StatusCode;
        // Fail closed: a permission error is NOT mistaken for lost contention.
        let err = exclusive_create_err(
            "h",
            sftp_status(StatusCode::PermissionDenied),
            "/var/lock/mtui.lock",
        );
        assert!(matches!(err, HostError::Sftp { .. }));
    }

    #[test]
    fn exclusive_create_io_error_propagates_as_transport() {
        let err = exclusive_create_err(
            "h",
            russh_sftp::client::error::Error::IO("broken pipe".to_owned()),
            "/var/lock/mtui.lock",
        );
        assert!(matches!(err, HostError::Transport { .. }));
    }

    /// A prompt that always returns `answer`, recording whether it was called.
    fn fixed_prompt(
        answer: &'static str,
        called: Arc<std::sync::atomic::AtomicBool>,
    ) -> TimeoutPrompt {
        Arc::new(move |_text: String| {
            let called = Arc::clone(&called);
            Box::pin(async move {
                called.store(true, std::sync::atomic::Ordering::SeqCst);
                Ok(answer.to_owned())
            }) as Pin<Box<dyn Future<Output = std::io::Result<String>> + Send>>
        })
    }

    #[tokio::test]
    async fn timeout_headless_aborts_without_prompting() {
        // No prompt + not interactive: abort (and, in practice, WARN).
        let decision = on_command_timeout("h", "sleep 999", false, None).await;
        assert_eq!(decision, TimeoutDecision::Abort);
    }

    #[tokio::test]
    async fn timeout_interactive_but_no_prompt_aborts() {
        // interactive=true but prompt=None still degrades to abort.
        let decision = on_command_timeout("h", "sleep 999", true, None).await;
        assert_eq!(decision, TimeoutDecision::Abort);
    }

    #[tokio::test]
    async fn timeout_prompt_empty_keeps_waiting() {
        let called = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let p = fixed_prompt("", Arc::clone(&called));
        let decision = on_command_timeout("h", "sleep 999", true, Some(&p)).await;
        assert_eq!(decision, TimeoutDecision::KeepWaiting);
        assert!(called.load(std::sync::atomic::Ordering::SeqCst));
    }

    #[tokio::test]
    async fn timeout_prompt_y_keeps_waiting() {
        let called = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let p = fixed_prompt("Y\n", Arc::clone(&called));
        let decision = on_command_timeout("h", "sleep 999", true, Some(&p)).await;
        assert_eq!(decision, TimeoutDecision::KeepWaiting);
    }

    #[tokio::test]
    async fn timeout_prompt_n_aborts() {
        let called = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let p = fixed_prompt("n", Arc::clone(&called));
        let decision = on_command_timeout("h", "sleep 999", true, Some(&p)).await;
        assert_eq!(decision, TimeoutDecision::Abort);
        assert!(called.load(std::sync::atomic::Ordering::SeqCst));
    }

    #[tokio::test]
    async fn timeout_prompt_reader_error_keeps_waiting() {
        // A read error is treated as the Enter/Y default (keep waiting), never a
        // spurious abort.
        let p: TimeoutPrompt = Arc::new(|_t: String| {
            Box::pin(async move { Err(std::io::Error::other("eof")) })
                as Pin<Box<dyn Future<Output = std::io::Result<String>> + Send>>
        });
        let decision = on_command_timeout("h", "sleep 999", true, Some(&p)).await;
        assert_eq!(decision, TimeoutDecision::KeepWaiting);
    }

    #[test]
    fn resolve_uses_explicit_port_and_defaults_for_unknown_host() {
        // A host that cannot appear in any real ~/.ssh/config: the resolver
        // must fall back to the requested port and the default `root` user.
        // (We avoid mutating $HOME — that is racy under the test harness and
        // trips the workspace's unsafe-code lint.)
        let r = resolve("this-host-does-not-exist.invalid", 2222);
        assert_eq!(r.port, 2222);
        assert_eq!(r.user, "root");
        assert_eq!(r.connect_host, "this-host-does-not-exist.invalid");
    }

    #[test]
    fn resolve_defaults_port_to_22_when_zero() {
        let r = resolve("another-nonexistent.invalid", 0);
        assert_eq!(r.port, 22);
        assert_eq!(r.user, "root");
    }

    #[test]
    fn best_hash_is_sha512() {
        assert_eq!(best_hash(), Some(HashAlg::Sha512));
    }

    #[test]
    fn dirs_home_reads_home_env() {
        // HOME is virtually always set in the test environment; assert the
        // accessor returns it when present.
        match std::env::var_os("HOME") {
            Some(h) => assert_eq!(dirs_home(), Some(PathBuf::from(h))),
            None => assert_eq!(dirs_home(), None),
        }
    }

    #[test]
    fn default_identity_files_only_returns_existing_paths() {
        // Whatever the environment, every returned path must exist and live
        // under ~/.ssh — the filter guarantees it.
        for p in default_identity_files() {
            assert!(p.exists(), "returned nonexistent key path: {}", p.display());
            assert!(p.to_string_lossy().contains(".ssh"));
        }
    }

    #[test]
    fn debug_impl_shows_host_and_disconnected_state() {
        // Build a disconnected SshConnection directly to exercise the Debug
        // impl without any network.
        let conn = SshConnection {
            hostname: "example.host".to_owned(),
            resolved: Resolved {
                connect_host: "example.host".to_owned(),
                port: 2222,
                user: "root".to_owned(),
                identity_files: Vec::new(),
            },
            policy: HostKeyPolicy::AutoAdd,
            timeout: CommandTimeout::default(),
            handle: None,
            is_repl: false,
            timeout_prompt: None,
        };
        let s = format!("{conn:?}");
        assert!(s.contains("example.host"), "{s}");
        assert!(s.contains("2222"), "{s}");
        assert!(s.contains("root"), "{s}");
        assert!(s.contains("connected: false"), "{s}");
        // A disconnected connection reports inactive and errors on handle().
        assert!(!conn.is_active());
        assert!(conn.handle().is_err());
    }

    // --- host-key verification (th4o.4) ---

    fn gen_pubkey() -> PublicKey {
        PrivateKey::random(&mut rand::rng(), russh::keys::Algorithm::Ed25519)
            .expect("gen key")
            .public_key()
            .clone()
    }

    fn handler(host: &str, port: u16, policy: HostKeyPolicy, kh: &Path) -> ClientHandler {
        ClientHandler {
            hostname: host.to_owned(),
            connect_host: host.to_owned(),
            port,
            policy,
            known_hosts_path: Some(kh.to_path_buf()),
        }
    }

    #[test]
    fn unknown_host_follows_policy() {
        let key = gen_pubkey();
        for (policy, expect) in [
            (HostKeyPolicy::Reject, false),
            (HostKeyPolicy::AutoAdd, true),
            (HostKeyPolicy::Warn, true),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let kh = dir.path().join("known_hosts");
            let h = handler("h", 22, policy, &kh);
            assert_eq!(h.verify(&key), expect, "policy {policy:?}");
        }
    }

    #[test]
    fn missing_known_hosts_file_treated_as_unknown() {
        let key = gen_pubkey();
        // A fresh dir per policy: auto_add would otherwise create the file and
        // make the key "known" for the later reject check.
        for (policy, expect) in [
            (HostKeyPolicy::AutoAdd, true),
            (HostKeyPolicy::Warn, true),
            (HostKeyPolicy::Reject, false),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let kh = dir.path().join("does-not-exist/known_hosts");
            assert_eq!(
                handler("h", 22, policy, &kh).verify(&key),
                expect,
                "policy {policy:?}"
            );
        }
    }

    #[test]
    fn known_matching_key_accepts_under_every_policy() {
        let key = gen_pubkey();
        let dir = tempfile::tempdir().unwrap();
        let kh = dir.path().join("known_hosts");
        std::fs::write(&kh, format!("h {}\n", key.to_openssh().unwrap())).unwrap();
        for policy in [
            HostKeyPolicy::Reject,
            HostKeyPolicy::AutoAdd,
            HostKeyPolicy::Warn,
        ] {
            assert!(
                handler("h", 22, policy, &kh).verify(&key),
                "policy {policy:?}"
            );
        }
    }

    #[test]
    fn changed_key_rejected_under_every_policy() {
        let recorded = gen_pubkey();
        let presented = gen_pubkey();
        let dir = tempfile::tempdir().unwrap();
        let kh = dir.path().join("known_hosts");
        std::fs::write(&kh, format!("h {}\n", recorded.to_openssh().unwrap())).unwrap();
        for policy in [
            HostKeyPolicy::AutoAdd,
            HostKeyPolicy::Warn,
            HostKeyPolicy::Reject,
        ] {
            assert!(
                !handler("h", 22, policy, &kh).verify(&presented),
                "policy {policy:?} must reject a changed key"
            );
        }
        // The stale entry is untouched (no silent auto-add over a changed key).
        let after = std::fs::read_to_string(&kh).unwrap();
        assert_eq!(after.lines().count(), 1);
    }

    #[test]
    fn auto_add_persists_key_atomically() {
        let key = gen_pubkey();
        let dir = tempfile::tempdir().unwrap();
        let kh = dir.path().join("known_hosts");
        assert!(handler("h", 22, HostKeyPolicy::AutoAdd, &kh).verify(&key));

        // The key is now recorded and re-verifies as known.
        assert!(kh.exists());
        assert!(handler("h", 22, HostKeyPolicy::Reject, &kh).verify(&key));
        // No leftover temp files in the directory.
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "temp file left behind");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&kh).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "known_hosts must be 0o600");
        }
    }

    #[test]
    fn warn_accepts_without_persisting() {
        let key = gen_pubkey();
        let dir = tempfile::tempdir().unwrap();
        let kh = dir.path().join("known_hosts");
        assert!(handler("h", 22, HostKeyPolicy::Warn, &kh).verify(&key));
        assert!(!kh.exists(), "warn policy must not persist the key");
    }

    #[test]
    fn ported_host_matches_bracket_port_entry() {
        let key = gen_pubkey();
        let dir = tempfile::tempdir().unwrap();
        let kh = dir.path().join("known_hosts");
        std::fs::write(&kh, format!("[h]:2222 {}\n", key.to_openssh().unwrap())).unwrap();
        // Matches on the ported entry.
        assert!(handler("h", 2222, HostKeyPolicy::Reject, &kh).verify(&key));
        // But the same host on the default port is *not* covered by it.
        assert!(!handler("h", 22, HostKeyPolicy::Reject, &kh).verify(&key));
    }

    #[test]
    fn auto_add_persists_ported_host_with_bracket_form() {
        let key = gen_pubkey();
        let dir = tempfile::tempdir().unwrap();
        let kh = dir.path().join("known_hosts");
        assert!(handler("h", 2222, HostKeyPolicy::AutoAdd, &kh).verify(&key));
        let recorded = std::fs::read_to_string(&kh).unwrap();
        assert!(recorded.starts_with("[h]:2222 "), "got {recorded:?}");
        assert!(handler("h", 2222, HostKeyPolicy::Reject, &kh).verify(&key));
    }

    #[test]
    fn hashed_host_entry_matches() {
        let key = gen_pubkey();
        let dir = tempfile::tempdir().unwrap();
        let kh = dir.path().join("known_hosts");
        // russh's learn_known_hosts writes a plain entry; a hashed entry uses
        // the `|1|salt|hash` form. Verify our reader (russh's matcher) accepts a
        // hashed line by generating one deterministically.
        let line = hashed_known_hosts_line("h", &key);
        std::fs::write(&kh, format!("{line}\n")).unwrap();
        assert!(handler("h", 22, HostKeyPolicy::Reject, &kh).verify(&key));
    }

    /// Builds an OpenSSH `|1|salt|hash` hashed known_hosts line for `host`,
    /// using the same HMAC-SHA1 + `BASE64_MIME` scheme russh's matcher expects.
    fn hashed_known_hosts_line(host: &str, key: &PublicKey) -> String {
        use data_encoding::BASE64_MIME;
        use hmac::{Hmac, KeyInit, Mac};
        use sha1::Sha1;

        let salt: [u8; 20] = rand::random();
        let mut mac = Hmac::<Sha1>::new_from_slice(&salt).unwrap();
        mac.update(host.as_bytes());
        let hash = mac.finalize().into_bytes();
        format!(
            "|1|{}|{} {}",
            BASE64_MIME.encode(&salt).trim_end(),
            BASE64_MIME.encode(&hash).trim_end(),
            key.to_openssh().unwrap()
        )
    }

    #[test]
    fn persist_failure_leaves_connection_working() {
        let key = gen_pubkey();
        let dir = tempfile::tempdir().unwrap();
        // Point known_hosts at a path whose parent is a *file*, so create_dir_all
        // and the temp open both fail — persistence errors, but verify() still
        // accepts under auto_add.
        let blocker = dir.path().join("blocker");
        std::fs::write(&blocker, b"x").unwrap();
        let kh = blocker.join("known_hosts");
        assert!(
            handler("h", 22, HostKeyPolicy::AutoAdd, &kh).verify(&key),
            "auto_add must still accept when persistence fails"
        );
        assert!(!kh.exists());
    }

    #[test]
    fn sftp_component_accepts_ordinary_names() {
        for name in ["app.log", ".hidden", "Ünïcode.txt", "a b c", "file-1_2.log"] {
            assert_eq!(
                validate_sftp_component(name, "h").expect("should accept"),
                name,
                "expected {name:?} to be accepted"
            );
        }
    }

    #[test]
    fn sftp_component_rejects_traversal_and_absolute() {
        for name in [
            "",
            ".",
            "..",
            "../evil",
            "../../etc/passwd",
            "/etc/passwd",
            "a/b",
            "sub/../x",
            r"C:\evil",
            r"\\srv\share",
            r"dir\file",
            "foo\0bar",
            "line\nbreak",
        ] {
            let err = validate_sftp_component(name, "h").expect_err("should reject");
            assert!(
                matches!(err, HostError::UnsafeSftpName { .. }),
                "expected {name:?} rejected as UnsafeSftpName, got {err:?}"
            );
        }
    }

    #[test]
    fn sftp_component_error_quotes_name_without_local_path() {
        let err = validate_sftp_component("../evil", "badhost").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("badhost"));
        assert!(msg.contains("\"../evil\""));
    }
}
