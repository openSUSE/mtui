//! The russh-backed [`SshConnection`] — the production [`Connection`] impl,
//! built on [`russh`] (SSH transport) and [`russh_sftp`] (SFTP subsystem).
//!
//! ## Behaviour
//!
//! * **Pubkey/agent only.** Authentication tries SSH-agent keys (via
//!   `SSH_AUTH_SOCK`) first, then any identity files from `~/.ssh/config`, then
//!   the default `~/.ssh/id_*` keys. There is deliberately **no password
//!   fallback** (MTUI is pubkey-only by design); a failed auth surfaces
//!   [`HostError::Auth`].
//! * **`~/.ssh/config`.** hostname / user (default `root`) / port (default 22)
//!   / identityfile are honoured via [`russh_config`].
//! * **`run` timeout.** The per-command timeout bounds the *no-output* window,
//!   not total runtime — a command that keeps producing output runs as long as
//!   it likes, but one that goes silent for the whole window is treated as
//!   stuck and aborted with [`HostError::Timeout`]. This is the non-interactive
//!   contract; the async model has no TTY prompt to loop on.
//! * **`run` output/lifetime bounds (th4o.6).** Beyond
//!   the inactivity window, `run` additionally (a) caps the captured
//!   stdout/stderr at [`MAX_STREAM_BYTES`] per stream / [`MAX_TOTAL_BYTES`]
//!   combined, discarding the overflow instead of buffering it and flagging the
//!   resulting [`CommandLog`] `truncated`; and (b) in **non-interactive** runs
//!   enforces an *absolute* execution deadline
//!   (`connection_timeout * COMMAND_DEADLINE_FACTOR`) so a command that trickles
//!   output forever — which never trips the inactivity window — cannot hang a
//!   headless / `mtui-mcp` run. These are deliberate DoS hardening. In the REPL
//!   a human may answer the keep-waiting prompt indefinitely, so no absolute
//!   deadline is imposed there. An aborted/deadlined command's channel is
//!   closed before returning so no orphaned remote process/channel leaks.
//! * **`fire_and_forget`.** Dispatches on a fresh channel and closes the local
//!   link without awaiting completion — for reboot-style commands that tear
//!   down the transport; callers follow up with [`reconnect`](SshConnection).
//!
//! ## Known limitations
//!
//! * **ProxyCommand** is not yet executed (russh needs a spawned-process
//!   stream); a host that relies on it degrades to a direct connect and is a
//!   documented follow-up.
//! * **`sftp_open`** returns the file's bytes rather than a live file handle
//!   (the object-safe trait surface); this covers every current caller.
//! * The interactive PTY `shell` (feature `shell`) returns an
//!   object-safe [`ShellChannel`] duplex over the PTY; the raw-`termios` local
//!   terminal bridge that consumes it is a CLI concern.

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
use russh_sftp::client::SftpSession as RusshSftpSession;
use tokio::time::{Duration, timeout};

#[cfg(feature = "shell")]
use super::ShellChannel;
use super::sftp_session::SftpSession;
use super::timeout::{CommandTimeout, HostKeyPolicy};
use super::{Connection, DEFAULT_USER};
use crate::error::{HostError, Result};

/// Number of reconnect+retry attempts before giving up.
const RETRIES: usize = 5;

/// Modest bound on concurrent per-entry transfers within a single folder
/// download. Host-level fan-out is already bounded by the fleet
/// `max_parallel`; this only caps parallelism *within* one host's folder so a
/// directory with many entries streams a few at a time rather than all at once.
const FOLDER_DOWNLOAD_CONCURRENCY: usize = 4;

/// The exit-code sentinel used when a command produced no exit status
/// (killed / channel lost). Kept in sync with [`CommandLog`]'s `-1` convention.
const NO_EXIT_CODE: i16 = -1;

/// Maximum bytes captured **per stream** (stdout, stderr) for one command.
///
/// A command that emits more has its excess for that stream discarded (not
/// buffered) and the resulting [`CommandLog`] is flagged
/// [`truncated`](CommandLog::truncated). Bounds the memory a single hostile or
/// runaway command (`yes`, `cat /dev/urandom`) can force mtui to hold, closing
/// off a DoS vector an unbounded output loop would leave open. 16 MiB is generous for
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
/// who chooses to keep waiting is never force-aborted). The
/// factor keeps the ceiling well above the inactivity window so legitimately
/// long, chatty commands (large `zypper` transactions) still complete.
const COMMAND_DEADLINE_FACTOR: u32 = 12;

/// The standard SSH port, used when neither `~/.ssh/config` nor the refhost
/// entry names one.
const DEFAULT_SSH_PORT: u16 = 22;

/// Accumulates a command's stdout/stderr under fixed per-stream and combined
/// byte caps, discarding overflow instead of buffering it.
///
/// Each `push_*` copies only up to the remaining per-stream **and** remaining
/// combined budget; once either is reached the rest of the chunk is dropped and
/// [`truncated`](Self::truncated) latches `true`. This keeps memory bounded
/// regardless of how much output a command produces.
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
/// (headless / `mtui-mcp`) leaves the timeout an immediate abort.
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
/// is unit-testable without a live SSH channel. Interactive + a prompt → ask
/// (empty / `y` keep waiting, `n` abort); otherwise abort immediately and emit
/// one WARN so the non-interactive silence is observable.
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
        // Empty / `y` / anything else: keep waiting (Enter/Y default).
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
/// are *not already recorded*.
///
/// A key that matches an existing `known_hosts` entry is accepted regardless of
/// policy; a key that *differs* from a recorded one is rejected under every
/// policy and reported distinctly. Only an
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
            // Recorded but *different*: a changed key. Reject under every
            // policy and report it distinctly — never silently auto-add over
            // a changed key.
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
            // to the unknown-host policy.
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
    /// SSH connect handshake budget (TCP connect, banner, and auth), applied
    /// both by [`connect`](Self::connect) and by [`reconnect`](Connection::reconnect).
    /// Distinct from [`timeout`](Self::timeout), which bounds the per-command
    /// no-output window only.
    ///
    /// Also sizes the per-request timeout of each SFTP session opened by
    /// [`sftp`](Self::sftp) (russh-sftp's `Config::request_timeout_secs`,
    /// otherwise pinned at its 10s default): a WAN/VPN refhost needs both the
    /// connect handshake and the SFTP round trip raised together, so one key
    /// covers both rather than adding a second.
    connect_timeout: CommandTimeout,
    handle: Option<Handle<ClientHandler>>,
    /// Whether a TTY-backed user can answer the command-timeout prompt. `false`
    /// (the default, and always under `mtui-mcp`) makes a no-output timeout
    /// abort instead of asking.
    is_repl: bool,
    /// Optional serialised prompt for the command-timeout branch. Wired from the
    /// composition root; `None` keeps the timeout an immediate abort.
    timeout_prompt: Option<TimeoutPrompt>,
    /// The `known_hosts` file consulted during the handshake; `None` uses
    /// russh's default (`~/.ssh/known_hosts`). Retained so
    /// [`reconnect`](Connection::reconnect) re-verifies against the same file
    /// the initial [`connect`](Self::connect) used (tests point it at a temp
    /// file to stay out of the developer's real store).
    known_hosts: Option<PathBuf>,
    /// Backoff base for [`reconnect`](Connection::reconnect)'s post-reboot
    /// budget (config `reboot_timeout`, default 10s). Only consulted when the
    /// caller passes `backoff = true`; set via [`with_reboot_budget`].
    ///
    /// [`with_reboot_budget`]: Self::with_reboot_budget
    reconnect_backoff_base: Duration,
    /// The SFTP subsystem for this connection, opened lazily and reused
    /// across every `sftp_*` verb instead of one channel+handshake per call.
    /// `RusshSftpSession`'s own request/reply routing is by atomically
    /// allocated id, so concurrent verbs (e.g. `sftp_get_folder`'s
    /// `buffer_unordered` fan-out) safely share one `Arc` clone each. Cleared
    /// by [`close`](Connection::close), a successful
    /// [`reconnect`](Connection::reconnect), and on a session-fatal error (see
    /// [`invalidate_sftp_if_fatal`](Self::invalidate_sftp_if_fatal)) so the
    /// next call re-handshakes rather than reusing a dead session.
    sftp: Option<Arc<RusshSftpSession>>,
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
    /// applying `policy` to the host key, `connect_timeout` to the handshake
    /// (TCP connect, banner, and auth), and `timeout` to the later per-command
    /// no-output window.
    ///
    /// `known_hosts` selects the file consulted (and, under
    /// [`AutoAdd`](HostKeyPolicy::AutoAdd), appended) during host-key
    /// verification; `None` uses russh's default (`~/.ssh/known_hosts`). Tests
    /// pass a per-test temp path so they never touch the developer's real file.
    /// The path is applied during the handshake, so it must be supplied here
    /// rather than via a post-connect builder.
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
        connect_timeout: CommandTimeout,
        timeout: CommandTimeout,
        known_hosts: Option<PathBuf>,
    ) -> Result<Self> {
        let hostname = hostname.into();
        let resolved = resolve(&hostname, port);
        let handle = establish(
            &hostname,
            &resolved,
            policy,
            connect_timeout,
            known_hosts.clone(),
        )
        .await?;
        Ok(Self {
            hostname,
            resolved,
            policy,
            timeout,
            connect_timeout,
            handle: Some(handle),
            is_repl: false,
            timeout_prompt: None,
            known_hosts,
            reconnect_backoff_base: Duration::from_secs(10),
            sftp: None,
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
    pub(crate) fn with_timeout_prompt(mut self, prompt: TimeoutPrompt) -> Self {
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
    /// `with_timeout_prompt`: it stays off the
    /// object-safe [`Connection`] trait.
    #[must_use]
    pub fn with_command_timeout(mut self, timeout: CommandTimeout) -> Self {
        self.timeout = timeout;
        self
    }

    /// Sets the backoff base [`reconnect`](Connection::reconnect) uses when a
    /// caller passes `backoff = true` (the post-reboot recovery path).
    /// Mirrors config `[connection] reboot_timeout`; unset callers keep the
    /// 10s default set in [`connect`](Self::connect).
    #[must_use]
    pub(crate) fn with_reboot_budget(mut self, base: Duration) -> Self {
        self.reconnect_backoff_base = base;
        self
    }

    /// Returns the live handle or a [`HostError::Transport`] "not connected".
    fn handle(&self) -> Result<&Handle<ClientHandler>> {
        self.handle.as_ref().ok_or_else(|| HostError::Transport {
            host: self.hostname.clone(),
            reason: "not connected".to_owned(),
        })
    }

    /// Returns the cached SFTP subsystem, opening one only when the cache is
    /// empty (reconnecting first if the link has dropped).
    ///
    /// Every `sftp_*` verb shares this one channel+handshake per connection
    /// instead of paying a fresh one per call — the `Arc` clone handed back is
    /// cheap, and `RusshSftpSession`'s own request/reply routing is by
    /// atomically allocated id, so concurrent verbs may safely hold their own
    /// clone. The whole open sequence (channel open, subsystem request, and
    /// the INIT/VERSION handshake) is bounded by `connect_timeout` — the
    /// channel/subsystem steps have no timeout of their own otherwise.
    ///
    /// The per-request budget of the opened session (russh-sftp's
    /// `Config::request_timeout_secs`, default 10s) is likewise derived from
    /// `connect_timeout` rather than left at the dependency default:
    /// `new_with_config` runs the INIT/VERSION handshake through the same
    /// `request` path, so a fixed 10s would bound every SFTP op — including
    /// the handshake — regardless of link latency. `new_with_config` must be
    /// used rather than a post-`new` `set_timeout`, which would land after
    /// INIT and leave the handshake pinned at 10s. `.max(1)` guards against a
    /// zero duration (fires instantly); `connect_timeout` is validated `> 0`
    /// in config but a test `CommandTimeout` can still be constructed at zero.
    async fn sftp(&mut self) -> Result<Arc<RusshSftpSession>> {
        if !self.is_active() {
            self.reconnect(0, false).await?;
        }
        if let Some(cached) = &self.sftp {
            return Ok(Arc::clone(cached));
        }
        let hostname = self.hostname.clone();
        let session = tokio::time::timeout(
            self.connect_timeout.as_duration(),
            self.open_sftp_subsystem(),
        )
        .await
        .map_err(|_| HostError::Transport {
            host: hostname,
            reason: "sftp subsystem handshake timed out".to_owned(),
        })??;
        let session = Arc::new(session);
        self.sftp = Some(Arc::clone(&session));
        Ok(session)
    }

    /// Opens a fresh SFTP subsystem channel: `channel_open_session` +
    /// `request_subsystem("sftp")` + the `RusshSftpSession` INIT/VERSION
    /// handshake. Split out of [`sftp`](Self::sftp) so the whole sequence can
    /// be wrapped in one `tokio::time::timeout`.
    async fn open_sftp_subsystem(&self) -> Result<RusshSftpSession> {
        let channel = self
            .handle()?
            .channel_open_session()
            .await
            .map_err(|e| self.sftp_err(e))?;
        channel
            .request_subsystem(true, "sftp")
            .await
            .map_err(|e| self.sftp_err(e))?;
        let config = russh_sftp::client::Config {
            request_timeout_secs: self.connect_timeout.as_secs().max(1),
            ..Default::default()
        };
        RusshSftpSession::new_with_config(channel.into_stream(), config)
            .await
            .map_err(|e| self.sftp_err(e))
    }

    /// Drops the cached SFTP subsystem so the next [`sftp`](Self::sftp) call
    /// re-handshakes, if `err` is session-fatal.
    ///
    /// [`HostError::SftpTimeout`] and [`HostError::Transport`] are the two
    /// buckets a non-`Status` russh-sftp error lands in (see
    /// [`sftp_err_at_for`] / [`exclusive_create_err`]): both mean the shared
    /// channel itself is suspect (wedged or gone), so continuing to hand it
    /// out to later verbs would just repeat the same failure. A `Status`-based
    /// error (`HostError::Sftp`/`SftpNotFound`/`AlreadyExists`) is a normal
    /// per-request outcome and leaves the session cached.
    fn invalidate_sftp_if_fatal(&mut self, err: &HostError) {
        if matches!(
            err,
            HostError::SftpTimeout { .. } | HostError::Transport { .. }
        ) {
            self.sftp = None;
        }
    }

    fn sftp_err(&self, e: impl std::fmt::Display) -> HostError {
        sftp_err_for(&self.hostname, e)
    }

    /// Maps a russh-sftp client error to [`HostError`], routing the
    /// `SSH_FX_NO_SUCH_FILE` status to the dedicated
    /// [`HostError::SftpNotFound`] variant so the host-system parser can branch
    /// distinctly on "not found".
    fn sftp_err_at(&self, e: russh_sftp::client::error::Error, path: &Path) -> HostError {
        sftp_err_at_for(&self.hostname, e, path)
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
    ///
    /// [`StatusCode::Failure`]: russh_sftp::protocol::StatusCode::Failure
    /// [`StatusCode::NoSuchFile`]: russh_sftp::protocol::StatusCode::NoSuchFile
    fn exclusive_create_err(
        &self,
        e: russh_sftp::client::error::Error,
        path_str: &str,
    ) -> HostError {
        exclusive_create_err(&self.hostname, e, path_str)
    }

    /// Like [`sftp_err`](Self::sftp_err), but also invalidates the cached
    /// subsystem on a session-fatal error. Used by every `sftp_*` verb (as
    /// opposed to [`sftp`](Self::sftp)'s own handshake, which has nothing
    /// cached yet to invalidate).
    fn sftp_verb_err(&mut self, e: impl std::fmt::Display) -> HostError {
        let err = self.sftp_err(e);
        self.invalidate_sftp_if_fatal(&err);
        err
    }

    /// Like [`sftp_err_at`](Self::sftp_err_at), invalidating on a session-fatal
    /// error.
    fn sftp_verb_err_at(&mut self, e: russh_sftp::client::error::Error, path: &Path) -> HostError {
        let err = self.sftp_err_at(e, path);
        self.invalidate_sftp_if_fatal(&err);
        err
    }

    /// Like [`exclusive_create_err`](Self::exclusive_create_err), invalidating
    /// on a session-fatal error.
    fn sftp_verb_exclusive_create_err(
        &mut self,
        e: russh_sftp::client::error::Error,
        path_str: &str,
    ) -> HostError {
        let err = self.exclusive_create_err(e, path_str);
        self.invalidate_sftp_if_fatal(&err);
        err
    }

    /// Runs a verb's *first* SFTP request (`open`/`read`/`read_dir`/
    /// `read_link`/`open_with_flags` — i.e. before anything has been written)
    /// with one retry against a freshly-handshaked session, but only when the
    /// failed attempt used a session pulled from the cache.
    ///
    /// A long-lived shared subsystem can be silently closed by the peer (an
    /// idle timeout, a restarted sshd) between calls; the *first* request on
    /// such a session is safe to retry because nothing has been written yet.
    /// A session opened fresh in this same call failing immediately is a
    /// different, likely permanent, problem and is not retried — and no
    /// request past this first one is retried by this helper, since retrying
    /// a write/append could duplicate a remote-history row (the append-only
    /// contract).
    ///
    /// Returns the session alongside the successful result so the caller can
    /// issue further requests (e.g. `write_all`/`shutdown` on the opened
    /// file) against the same subsystem without invalidation risk.
    async fn sftp_first_request<T, ReqFut>(
        &mut self,
        req: impl Fn(Arc<RusshSftpSession>) -> ReqFut,
        map_err: impl Fn(&mut Self, russh_sftp::client::error::Error) -> HostError,
    ) -> Result<(Arc<RusshSftpSession>, T)>
    where
        ReqFut: Future<Output = std::result::Result<T, russh_sftp::client::error::Error>>,
    {
        let from_cache = self.sftp.is_some();
        let sftp = self.sftp().await?;
        match req(Arc::clone(&sftp)).await {
            Ok(v) => Ok((sftp, v)),
            Err(e) if from_cache => {
                tracing::debug!(
                    host = %self.hostname, error = %e,
                    "stale cached sftp session on first request; re-handshaking and retrying once"
                );
                self.sftp = None;
                let sftp = self.sftp().await?;
                match req(Arc::clone(&sftp)).await {
                    Ok(v) => Ok((sftp, v)),
                    Err(e) => Err(map_err(self, e)),
                }
            }
            Err(e) => Err(map_err(self, e)),
        }
    }

    fn transport_err(&self, e: impl std::fmt::Display) -> HostError {
        HostError::Transport {
            host: self.hostname.clone(),
            reason: e.to_string(),
        }
    }
}

/// Builds a generic [`HostError::Sftp`] for `host` from a displayable error.
///
/// Shared by [`SshConnection::sftp_err`] and the batched [`SshSftpSession`] so
/// both paths map SFTP failures identically.
fn sftp_err_for(host: &str, e: impl std::fmt::Display) -> HostError {
    HostError::Sftp {
        host: host.to_owned(),
        reason: e.to_string(),
    }
}

/// Maps a russh-sftp client error to [`HostError`] for `host`/`path`, routing
/// the `SSH_FX_NO_SUCH_FILE` status to [`HostError::SftpNotFound`].
///
/// Shared by [`SshConnection::sftp_err_at`] and the batched [`SshSftpSession`]
/// so both paths preserve the "not found" branch the host-system parser relies
/// on.
fn sftp_err_at_for(host: &str, e: russh_sftp::client::error::Error, path: &Path) -> HostError {
    use russh_sftp::client::error::Error as SftpError;
    use russh_sftp::protocol::StatusCode;

    if matches!(e, SftpError::Timeout) {
        return HostError::SftpTimeout {
            host: host.to_owned(),
            path: path.to_string_lossy().into_owned(),
        };
    }
    if let SftpError::Status(status) = &e
        && status.status_code == StatusCode::NoSuchFile
    {
        return HostError::SftpNotFound {
            host: host.to_owned(),
            path: path.to_string_lossy().into_owned(),
        };
    }
    HostError::Sftp {
        host: host.to_owned(),
        reason: e.to_string(),
    }
}

/// Categorizes the error from an **atomic exclusive create**
/// ([`Connection::sftp_write`] with
/// `exclusive = true`).
///
/// SFTPv3 has no dedicated "file exists" status, so an `O_EXCL` collision
/// surfaces as the generic [`StatusCode::Failure`]. That is the only status
/// mapped to [`HostError::AlreadyExists`] (so the lock protocol reconciles the
/// race). Every other case fails **closed** — it propagates as a real error
/// rather than being mistaken for lost contention:
///
/// * a request timeout → [`HostError::SftpTimeout`] (the create may have
///   landed server-side despite the client never seeing the reply; the lock
///   protocol re-reads to check rather than assuming failure),
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

    if matches!(e, SftpError::Timeout) {
        return HostError::SftpTimeout {
            host: hostname.to_owned(),
            path: path_str.to_owned(),
        };
    }
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
            .unwrap_or(DEFAULT_SSH_PORT),
        user: cfg_user.unwrap_or_else(|| DEFAULT_USER.to_owned()),
        identity_files,
    }
}

/// The default private keys to try when config names none, the common
/// OpenSSH defaults.
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
/// This is advisory: any failure is logged and swallowed so a fresh host
/// still connects under `auto_add`. Never logs raw key material.
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
    let entry = if port == DEFAULT_SSH_PORT {
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

/// The next `reconnect` backoff sleep after `count` attempts:
/// `2 * (timeout + 5 * count)`.
fn reconnect_delay(count: usize, base: Duration) -> Duration {
    (base + Duration::from_secs(5 * count as u64)) * 2
}

/// Establishes the transport and authenticates. Shared by `connect` and
/// `reconnect`. `connect_timeout` bounds the TCP connect / banner wait
/// **and** the subsequent authentication — the whole handshake is one budget.
async fn establish(
    hostname: &str,
    resolved: &Resolved,
    policy: HostKeyPolicy,
    connect_timeout: CommandTimeout,
    known_hosts: Option<PathBuf>,
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
        known_hosts_path: known_hosts,
    };

    let addr = (resolved.connect_host.as_str(), resolved.port);
    let connect_fut = client::connect(config, addr, handler);
    let mut handle = match timeout(connect_timeout.as_duration(), connect_fut).await {
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
                reason: format!("connection timed out after {}s", connect_timeout.as_secs()),
            });
        }
    };

    let authenticated = match timeout(
        connect_timeout.as_duration(),
        authenticate(&mut handle, hostname, resolved),
    )
    .await
    {
        Ok(result) => result?,
        Err(_) => {
            return Err(HostError::Connect {
                host: hostname.to_owned(),
                reason: format!(
                    "authentication timed out after {}s",
                    connect_timeout.as_secs()
                ),
            });
        }
    };

    if authenticated {
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
            // pubkey auth only takes a bare `PublicKey`, so skip certificates.
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

/// A batched SFTP session over one russh channel+subsystem, returned by
/// [`SshConnection::sftp_session`].
///
/// Holds a clone of the connection's shared [`RusshSftpSession`] (see
/// [`SshConnection::sftp`]) and the hostname (for error context). Each read
/// verb runs against the *same* subsystem — no per-op handshake — and routes
/// failures through the shared [`sftp_err_for`]/[`sftp_err_at_for`] mappers so
/// the error surface is identical to the per-op [`Connection`] path.
struct SshSftpSession {
    sftp: Arc<RusshSftpSession>,
    hostname: String,
}

#[async_trait]
impl SftpSession for SshSftpSession {
    async fn open(&mut self, path: &Path) -> Result<Vec<u8>> {
        self.sftp
            .read(path.to_string_lossy().to_string())
            .await
            .map_err(|e| sftp_err_at_for(&self.hostname, e, path))
    }

    async fn listdir(&mut self, path: &Path) -> Result<Vec<String>> {
        let dir = self
            .sftp
            .read_dir(path.to_string_lossy().to_string())
            .await
            .map_err(|e| sftp_err_at_for(&self.hostname, e, path))?;
        Ok(dir.map(|e| e.file_name()).collect())
    }

    async fn readlink(&mut self, path: &Path) -> Result<String> {
        self.sftp
            .read_link(path.to_string_lossy().to_string())
            .await
            .map_err(|e| sftp_err_at_for(&self.hostname, e, path))
    }

    async fn close(&mut self) -> Result<()> {
        // The subsystem is shared with the owning `SshConnection`'s cache
        // (and possibly other in-flight `SshSftpSession`/verb handles);
        // dropping this `Arc` clone releases only this handle's share. Real
        // teardown happens via `SshConnection::close`/`reconnect`, or
        // invalidation on a session-fatal error.
        Ok(())
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
        // *identity* (host/policy/timeout) with an empty handle and no cached
        // SFTP subsystem; the first SFTP op the clone performs opens its own via
        // `sftp()`'s `reconnect`-if-inactive path. This means a `TargetLock`
        // built from the clone uses a second long-lived subsystem to the same
        // host for its (rare) force-unlock safeguard — functionally correct, at
        // the cost of one extra channel on that path only. The mock double
        // shares state via `Arc`, so offline unit tests still observe the
        // lock's SFTP ops.
        Box::new(Self {
            hostname: self.hostname.clone(),
            resolved: self.resolved.clone(),
            policy: self.policy,
            timeout: self.timeout,
            connect_timeout: self.connect_timeout,
            handle: None,
            is_repl: self.is_repl,
            timeout_prompt: self.timeout_prompt.clone(),
            known_hosts: self.known_hosts.clone(),
            reconnect_backoff_base: self.reconnect_backoff_base,
            sftp: None,
        })
    }

    async fn run(&mut self, command: &str) -> Result<CommandLog> {
        let started = Instant::now();

        // Open a channel, reconnecting + retrying on a lost link, up to
        // RETRIES attempts before giving up.
        let mut attempt = 0;
        let mut channel = loop {
            if !self.is_active() {
                self.reconnect(0, false).await?;
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
                    self.reconnect(0, false).await?;
                }
            }
        };

        channel
            .exec(true, command)
            .await
            .map_err(|e| self.transport_err(e))?;
        // run() never feeds stdin: send EOF so a command that reads input gets
        // it and proceeds instead of blocking.
        let _ = channel.eof().await;

        let mut capture = CaptureBuf::default();
        let mut exitcode: i16 = NO_EXIT_CODE;
        let window = self.timeout.as_duration();
        // Absolute execution ceiling for non-interactive runs (headless /
        // `mtui-mcp`), which have no user to answer the keep-waiting prompt: a
        // command trickling output forever never trips the inactivity window, so
        // without this it would hang the run indefinitely. In the REPL there is a
        // human who may legitimately choose to keep waiting, so no absolute
        // deadline is imposed there.
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
                    // waiting. Empty / `y` resumes the wait loop (Enter/Y
                    // default); `n` aborts. Headless: abort immediately,
                    // emitting one WARN so the silence is observable.
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
                            // process/channel is reclaimed.
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
        // Drop the cached SFTP subsystem along with the channel it lives on —
        // dropping the last `Arc` ends russh-sftp's `run()` task.
        self.sftp = None;
        if let Some(handle) = self.handle.take() {
            let _ = handle
                .disconnect(russh::Disconnect::ByApplication, "", "")
                .await;
        }
        Ok(())
    }

    async fn reconnect(&mut self, retry: usize, backoff: bool) -> Result<()> {
        if self.is_active() {
            return Ok(());
        }
        // A new SSH session invalidates any subsystem cached on the old one.
        self.sftp = None;
        let mut count = 0usize;
        let mut rtimeout = self.reconnect_backoff_base;
        let mut last_err = None;
        while !self.is_active() && count <= retry {
            count += 1;
            // Sleep before each probe, growing the wait when `backoff`. The
            // pre-sleep itself is gated on `backoff` — non-reboot callers pass
            // `(0, false)` and must fail fast (no multi-second pause) on a
            // genuinely dead link mid-command; only the reboot-recovery budget
            // (`backoff = true`) pays the wait.
            if backoff {
                tokio::time::sleep(rtimeout).await;
                rtimeout = reconnect_delay(count, self.reconnect_backoff_base);
            }
            match establish(
                &self.hostname,
                &self.resolved,
                self.policy,
                self.connect_timeout,
                self.known_hosts.clone(),
            )
            .await
            {
                Ok(handle) => {
                    self.handle = Some(handle);
                }
                Err(e) => {
                    tracing::debug!(host = %self.hostname, attempt = count, "reconnect attempt failed: {e}");
                    last_err = Some(e);
                }
            }
        }
        if self.is_active() {
            return Ok(());
        }
        tracing::debug!(host = %self.hostname, "reconnect gave up: {last_err:?}");
        Err(HostError::ReconnectFailed {
            host: self.hostname.clone(),
        })
    }

    async fn fire_and_forget(&mut self, command: &str) -> Result<()> {
        // Revive an idle-dropped link before dispatching, as [`run`] does.
        //
        // The only caller is the post-operation reboot, which fires after a
        // `transactional-update` that can run for minutes — long enough for the
        // server to have closed an idle session. Without this, that reboot
        // fails to dispatch against a perfectly healthy host, and because a
        // failed dispatch with a *successful* reconnect is precisely the
        // signature of "host is up but never got the command", `update` routes
        // its group-wide rollback on it: an idle TCP session would downgrade
        // every host in the group. One reconnect removes the trigger.
        if !self.is_active() {
            self.reconnect(0, false).await?;
        }
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
        self.sftp_put_bytes(&data, remote).await
    }

    async fn sftp_put_bytes(&mut self, data: &[u8], remote: &Path) -> Result<()> {
        use russh_sftp::protocol::OpenFlags;
        use tokio::io::AsyncWriteExt;

        // Create parent directories (best-effort; "already exists" is success).
        let remote_str = remote.to_string_lossy().to_string();
        {
            let sftp = self.sftp().await?;
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
        }

        // Open explicitly with CREATE so a fresh (non-existent) remote path is
        // created; the russh-sftp `write` convenience opens WRITE-only, which
        // returns SSH_FX_NO_SUCH_FILE for a not-yet-existing file.
        let flags = OpenFlags::CREATE | OpenFlags::WRITE | OpenFlags::TRUNCATE;
        let open_path = remote_str.clone();
        let (sftp, mut file) = self
            .sftp_first_request(
                move |sftp| {
                    let open_path = open_path.clone();
                    async move { sftp.open_with_flags(open_path, flags).await }
                },
                |s, e| s.sftp_verb_err_at(e, remote),
            )
            .await?;
        file.write_all(data)
            .await
            .map_err(|e| self.sftp_verb_err(e))?;
        file.shutdown().await.map_err(|e| self.sftp_verb_err(e))?;
        // Make executable (0770) after the transfer.
        if let Ok(mut meta) = sftp.metadata(remote_str.clone()).await {
            meta.permissions = Some(0o770);
            let _ = sftp.set_metadata(remote_str, meta).await;
        }
        Ok(())
    }

    async fn sftp_get(&mut self, remote: &Path, local: &Path) -> Result<()> {
        // Stream remote -> local rather than buffering the whole file in memory.
        let open_path = remote.to_string_lossy().to_string();
        let (_sftp, mut src) = self
            .sftp_first_request(
                move |sftp| {
                    let open_path = open_path.clone();
                    async move { sftp.open(open_path).await }
                },
                |s, e| s.sftp_verb_err(e),
            )
            .await?;
        let mut dst = tokio::fs::File::create(local)
            .await
            .map_err(|e| HostError::Sftp {
                host: self.hostname.clone(),
                reason: format!("create {}: {e}", local.display()),
            })?;
        tokio::io::copy(&mut src, &mut dst)
            .await
            .map_err(|e| HostError::Sftp {
                host: self.hostname.clone(),
                reason: format!("write {}: {e}", local.display()),
            })?;
        Ok(())
    }

    async fn sftp_get_folder(&mut self, remote: &Path, local: &Path) -> Result<()> {
        use futures::stream::{self, StreamExt};

        let remote_str = remote.to_string_lossy().to_string();
        let list_path = remote_str.clone();
        let (sftp, dir) = self
            .sftp_first_request(
                move |sftp| {
                    let list_path = list_path.clone();
                    async move { sftp.read_dir(list_path).await }
                },
                |s, e| s.sftp_verb_err(e),
            )
            .await?;

        // The peer controls entry names; a crafted name (`../x`, `/etc/x`,
        // `a/b`) would escape the download destination. Validate up front and
        // skip hostile names — a hostile entry must not abort the transfer of
        // the legitimate ones (best-effort transfer contract). The name is
        // logged quoted, and no local path is emitted, so the diagnostic cannot
        // leak the attacker's chosen target.
        let names: Vec<String> = dir
            .map(|entry| entry.file_name())
            .filter(
                |name| match validate_sftp_component(name.as_str(), &self.hostname) {
                    Ok(_) => true,
                    Err(e) => {
                        tracing::warn!(host = %self.hostname, error = %e, "skipping unsafe SFTP entry");
                        false
                    }
                },
            )
            .collect();

        let local_str = local.to_string_lossy();
        // Capture the host name once; the per-entry futures must not borrow
        // `&mut self` (they run concurrently), so build errors via the free
        // `sftp_err_for` rather than `self.sftp_err`.
        let host = self.hostname.clone();
        let sftp = &sftp;
        // Stream each entry remote -> local under modest bounded concurrency;
        // the shared `sftp` session accepts concurrent `open`s (`&self`).
        let results: Vec<Result<()>> = stream::iter(names)
            .map(|name| {
                let host = &host;
                // Per-host suffix contract: <local><name>.<hostname>
                let target = format!("{local_str}{name}.{host}");
                let remote_path = format!("{remote_str}/{name}");
                async move {
                    let mut src = sftp
                        .open(remote_path)
                        .await
                        .map_err(|e| sftp_err_for(host, e))?;
                    let mut dst =
                        tokio::fs::File::create(&target)
                            .await
                            .map_err(|e| HostError::Sftp {
                                host: host.clone(),
                                reason: format!("create {target}: {e}"),
                            })?;
                    tokio::io::copy(&mut src, &mut dst)
                        .await
                        .map_err(|e| HostError::Sftp {
                            host: host.clone(),
                            reason: format!("write {target}: {e}"),
                        })?;
                    Ok(())
                }
            })
            .buffer_unordered(FOLDER_DOWNLOAD_CONCURRENCY)
            .collect()
            .await;

        // The per-entry futures build errors via the free `sftp_err_for` (they
        // must not borrow `&mut self`, since they run concurrently), so a
        // session-fatal failure among them cannot self-invalidate; do it here
        // once results are back.
        if let Some(Err(e)) = results.iter().find(|r| r.is_err()) {
            self.invalidate_sftp_if_fatal(e);
        }
        // Surface the first transfer error, if any (matches the previous
        // fail-on-first-error semantics of the sequential loop).
        results.into_iter().collect::<Result<Vec<()>>>().map(|_| ())
    }

    async fn sftp_listdir(&mut self, path: &Path) -> Result<Vec<String>> {
        let list_path = path.to_string_lossy().to_string();
        let (_sftp, dir) = self
            .sftp_first_request(
                move |sftp| {
                    let list_path = list_path.clone();
                    async move { sftp.read_dir(list_path).await }
                },
                |s, e| s.sftp_verb_err_at(e, path),
            )
            .await?;
        Ok(dir.map(|e| e.file_name()).collect())
    }

    async fn sftp_open(&mut self, path: &Path) -> Result<Vec<u8>> {
        let read_path = path.to_string_lossy().to_string();
        let (_sftp, data) = self
            .sftp_first_request(
                move |sftp| {
                    let read_path = read_path.clone();
                    async move { sftp.read(read_path).await }
                },
                |s, e| s.sftp_verb_err_at(e, path),
            )
            .await?;
        Ok(data)
    }

    async fn sftp_write(&mut self, path: &Path, data: &[u8], exclusive: bool) -> Result<()> {
        use russh_sftp::protocol::OpenFlags;
        use tokio::io::AsyncWriteExt;

        let path_str = path.to_string_lossy().to_string();

        let mut file = if exclusive {
            // Atomic exclusive create (O_CREAT | O_EXCL).
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
            let open_path = path_str.clone();
            let (_sftp, file) = self
                .sftp_first_request(
                    move |sftp| {
                        let open_path = open_path.clone();
                        async move { sftp.open_with_flags(open_path, flags).await }
                    },
                    |s, e| s.sftp_verb_exclusive_create_err(e, &path_str),
                )
                .await?;
            file
        } else {
            // Truncating overwrite. Open explicitly with
            // CREATE so a fresh path is created; the `write` convenience opens
            // WRITE-only and fails with NO_SUCH_FILE on a missing file.
            let flags = OpenFlags::CREATE | OpenFlags::WRITE | OpenFlags::TRUNCATE;
            let open_path = path_str;
            let (_sftp, file) = self
                .sftp_first_request(
                    move |sftp| {
                        let open_path = open_path.clone();
                        async move { sftp.open_with_flags(open_path, flags).await }
                    },
                    |s, e| s.sftp_verb_err_at(e, path),
                )
                .await?;
            file
        };
        file.write_all(data)
            .await
            .map_err(|e| self.sftp_verb_err(e))?;
        file.shutdown().await.map_err(|e| self.sftp_verb_err(e))?;
        Ok(())
    }

    async fn sftp_append(&mut self, path: &Path, data: &[u8]) -> Result<()> {
        use russh_sftp::protocol::OpenFlags;
        use tokio::io::AsyncWriteExt;

        // Open at end-of-file (O_APPEND), creating the file if it is
        // missing. Each write lands at the current EOF, so
        // concurrent appenders extend the file without a read-modify-write race.
        // The open is the verb's first request and is safe to retry on a stale
        // cached session (nothing written yet); the write/shutdown that follow
        // are not — a retried append would duplicate a remote-history row.
        let flags = OpenFlags::CREATE | OpenFlags::WRITE | OpenFlags::APPEND;
        let open_path = path.to_string_lossy().to_string();
        let (_sftp, mut file) = self
            .sftp_first_request(
                move |sftp| {
                    let open_path = open_path.clone();
                    async move { sftp.open_with_flags(open_path, flags).await }
                },
                |s, e| s.sftp_verb_err_at(e, path),
            )
            .await?;
        file.write_all(data)
            .await
            .map_err(|e| self.sftp_verb_err(e))?;
        file.shutdown().await.map_err(|e| self.sftp_verb_err(e))?;
        Ok(())
    }

    async fn sftp_remove(&mut self, path: &Path) -> Result<()> {
        let sftp = self.sftp().await?;
        sftp.remove_file(path.to_string_lossy().to_string())
            .await
            .map_err(|e| self.sftp_verb_err(e))?;
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
            .map_err(|e| self.sftp_verb_err(e))?;
        Ok(())
    }

    async fn sftp_readlink(&mut self, path: &Path) -> Result<String> {
        let link_path = path.to_string_lossy().to_string();
        let (_sftp, target) = self
            .sftp_first_request(
                move |sftp| {
                    let link_path = link_path.clone();
                    async move { sftp.read_link(link_path).await }
                },
                |s, e| s.sftp_verb_err_at(e, path),
            )
            .await?;
        Ok(target)
    }

    async fn sftp_session(&mut self) -> Result<Box<dyn SftpSession + '_>> {
        // A clone of the connection's shared subsystem, reused across the
        // returned handle's reads. `sftp()` already reconnects at entry if the
        // link dropped; mid-session errors then propagate.
        let sftp = self.sftp().await?;
        Ok(Box::new(SshSftpSession {
            sftp,
            hostname: self.hostname.clone(),
        }))
    }

    #[cfg(feature = "shell")]
    async fn shell(&mut self, cols: u32, rows: u32) -> Result<Box<dyn ShellChannel>> {
        // Open a channel, reconnecting + retrying on a lost link, mirroring the
        // open->reconnect loop in `run`.
        let mut attempt = 0;
        let channel = loop {
            if !self.is_active() {
                self.reconnect(0, false).await?;
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
                    self.reconnect(0, false).await?;
                }
            }
        };

        // Request an `xterm` PTY sized cols x rows (no pixel dims, no special
        // terminal modes) then invoke the remote shell. On failure,
        // explicitly close the half-initialised channel rather than relying
        // on drop.
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
    /// before the next `wait()`. Unconsumed bytes are buffered rather than
    /// dropped — without this, a server frame larger than the caller's buffer
    /// would lose its tail and corrupt interactive output.
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
        // Best-effort, idempotent close: a channel the remote already tore
        // down is treated as success per the trait contract, so a
        // double-close never surfaces an error.
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
            connect_timeout: CommandTimeout::default(),
            handle: None,
            is_repl: false,
            timeout_prompt: None,
            known_hosts: None,
            reconnect_backoff_base: Duration::from_secs(10),
            sftp: None,
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

    // --- reconnect budget (fix-reboot-reconnect-window) ---

    /// A disconnected `SshConnection` pointed at a port nothing listens on
    /// (127.0.0.1:1 refuses immediately), so `establish()` fails fast with no
    /// timer involved — isolating the reconnect loop's own sleeps from real
    /// network latency.
    fn dead_port_connection(base: Duration) -> SshConnection {
        SshConnection {
            hostname: "127.0.0.1".to_owned(),
            resolved: Resolved {
                connect_host: "127.0.0.1".to_owned(),
                port: 1,
                user: "root".to_owned(),
                identity_files: Vec::new(),
            },
            policy: HostKeyPolicy::AutoAdd,
            timeout: CommandTimeout::new(Duration::from_millis(200)),
            connect_timeout: CommandTimeout::new(Duration::from_millis(200)),
            handle: None,
            is_repl: false,
            timeout_prompt: None,
            known_hosts: None,
            reconnect_backoff_base: base,
            sftp: None,
        }
    }

    #[test]
    fn reconnect_delay_matches_formula() {
        // Backoff formula: `2 * (timeout + 5 * count)`.
        let base = Duration::from_secs(10);
        assert_eq!(reconnect_delay(1, base), Duration::from_secs(2 * (10 + 5)));
        assert_eq!(reconnect_delay(2, base), Duration::from_secs(2 * (10 + 10)));
        assert_eq!(reconnect_delay(3, base), Duration::from_secs(2 * (10 + 15)));
        assert_eq!(
            reconnect_delay(10, base),
            Duration::from_secs(2 * (10 + 50))
        );
    }

    #[tokio::test]
    async fn reconnect_fast_path_probes_once_and_fails_fast() {
        // retry=0, backoff=false: the non-reboot (run/shell/sftp) call shape.
        // Must not pay the backoff base's pre-sleep — a single immediate probe.
        let mut conn = dead_port_connection(Duration::from_secs(10));
        let started = std::time::Instant::now();
        let err = conn.reconnect(0, false).await.expect_err("dead port fails");
        assert!(matches!(err, HostError::ReconnectFailed { host } if host == "127.0.0.1"));
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "fast path must not sleep by the reboot backoff base: took {:?}",
            started.elapsed()
        );
    }

    #[tokio::test]
    async fn connect_against_a_black_hole_host_bounds_on_connect_timeout() {
        // Bind but never accept(): the kernel completes the TCP handshake from
        // the listen backlog, so the socket connects but no SSH banner ever
        // arrives -- a synthetic black hole with no network dependency. A
        // short connect_timeout must still bound the hang; a much larger
        // connection_timeout (the per-command budget) must not leak into it.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("local_addr").port();

        let result = tokio::time::timeout(
            Duration::from_secs(5),
            SshConnection::connect(
                "127.0.0.1",
                port,
                HostKeyPolicy::AutoAdd,
                CommandTimeout::from_secs(1),
                CommandTimeout::from_secs(300),
                None,
            ),
        )
        .await
        .expect("connect() must return within the 5s test wrapper, not hang");

        assert!(
            matches!(result, Err(HostError::Connect { .. })),
            "expected HostError::Connect, got {result:?}"
        );
        drop(listener);
    }

    #[tokio::test(start_paused = true)]
    async fn reconnect_backoff_makes_retry_plus_one_attempts_then_gives_up() {
        // retry=3, backoff=true: the reboot-recovery call shape. Every attempt
        // is preceded by a sleep (base, then the grown `reconnect_delay`); with
        // a paused clock the sleeps resolve instantly while still advancing
        // tokio's virtual clock by the exact scheduled amount, so the elapsed
        // virtual time proves both the attempt count and the backoff formula
        // without the test taking minutes of wall-clock time.
        let base = Duration::from_secs(10);
        let mut conn = dead_port_connection(base);
        let started = tokio::time::Instant::now();
        let err = conn
            .reconnect(3, true)
            .await
            .expect_err("dead port fails after exhausting the budget");
        assert!(matches!(err, HostError::ReconnectFailed { host } if host == "127.0.0.1"));
        let expected =
            base + reconnect_delay(1, base) + reconnect_delay(2, base) + reconnect_delay(3, base);
        // Paused virtual time still accrues the dead-port connect attempts'
        // small real wall-clock latency alongside the auto-advanced sleeps, so
        // allow a little slack rather than requiring byte-exact equality.
        let elapsed = started.elapsed();
        assert!(
            elapsed.abs_diff(expected) < Duration::from_secs(5),
            "expected ~4 attempts (retry+1) with sleeps base, then reconnect_delay(1..=3): \
             elapsed={elapsed:?}, expected={expected:?}"
        );
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
