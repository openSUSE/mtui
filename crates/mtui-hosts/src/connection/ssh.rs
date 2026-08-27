//! The russh-backed [`SshConnection`] — the production [`Connection`] impl,
//! built on [`russh`] (SSH transport) and [`russh_sftp`] (SFTP subsystem).
//!
//! * **Pubkey/agent only.** SSH-agent keys (`SSH_AUTH_SOCK`), then
//!   `~/.ssh/config` identity files, then the default `~/.ssh/id_*`. There is
//!   deliberately **no password fallback** (MTUI is pubkey-only by design); a
//!   failed auth surfaces [`HostError::Auth`].
//! * **`~/.ssh/config`.** hostname / user (default `root`) / port (default 22)
//!   / identityfile are honoured via [`russh_config`]. **ProxyCommand is not**
//!   (russh needs a spawned-process stream); such a host degrades to a direct
//!   connect.
//! * **`run` timeout.** The per-command timeout bounds the *no-output* window,
//!   not total runtime, and a command silent for the whole window is aborted
//!   with [`HostError::Timeout`].
//! * **`run` bounds (th4o.6) — deliberate DoS hardening.** Output is capped at
//!   [`MAX_STREAM_BYTES`] per stream / [`MAX_TOTAL_BYTES`] combined, the
//!   overflow discarded and the [`CommandLog`] flagged `truncated`.
//!   **Non-interactive** runs additionally get an absolute deadline
//!   (`connection_timeout * COMMAND_DEADLINE_FACTOR`), since a command that
//!   trickles output forever never trips the inactivity window; the REPL has a
//!   human who may keep answering the prompt, so it gets none. An aborted
//!   command's channel is closed so no remote process is orphaned.
//! * **`fire_and_forget`.** Dispatches on a fresh channel and closes the local
//!   link without awaiting completion — for reboot-style commands that tear
//!   down the transport; callers follow up with [`reconnect`](SshConnection).
//! * **`sftp_open`** returns bytes rather than a live file handle (the
//!   object-safe trait surface); that covers every current caller. The `shell`
//!   feature's `ShellChannel` likewise carries the transport only — the
//!   raw-`termios` local terminal bridge is a CLI concern.

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
use russh::keys::{
    HashAlg, PrivateKey, PrivateKeyWithHashAlg, PublicKey, PublicKeyOrCertificate, load_secret_key,
};
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

/// Bound on concurrent per-entry transfers *within* one folder download.
/// Host-level fan-out is already bounded by the fleet `max_parallel`.
const FOLDER_DOWNLOAD_CONCURRENCY: usize = 4;

/// The exit-code sentinel used when a command produced no exit status
/// (killed / channel lost). Kept in sync with [`CommandLog`]'s `-1` convention.
const NO_EXIT_CODE: i16 = -1;

/// Maximum bytes captured **per stream** (stdout, stderr) for one command.
///
/// Excess is discarded (not buffered) and the [`CommandLog`] flagged
/// [`truncated`](CommandLog::truncated), closing the DoS vector an unbounded
/// output loop (`yes`, `cat /dev/urandom`) would leave open. 16 MiB is generous
/// for legitimate `zypper`/`rpm` output.
pub const MAX_STREAM_BYTES: usize = 16 * 1024 * 1024;

/// Maximum bytes captured **across both streams combined** for one command.
///
/// Bounds a flood split evenly across stdout and stderr. Twice the per-stream
/// cap, so each stream may independently reach its own limit.
pub const MAX_TOTAL_BYTES: usize = 2 * MAX_STREAM_BYTES;

/// Multiplier on the connection timeout giving a command's hard execution
/// deadline in **non-interactive** runs. Kept well above the inactivity window
/// so long, chatty `zypper` transactions still complete.
const COMMAND_DEADLINE_FACTOR: u32 = 12;

/// The standard SSH port, used when neither `~/.ssh/config` nor the refhost
/// entry names one.
const DEFAULT_SSH_PORT: u16 = 22;

/// Accumulates a command's stdout/stderr under fixed per-stream and combined
/// byte caps, discarding overflow instead of buffering it.
///
/// Once either budget is reached the rest of the chunk is dropped and
/// [`truncated`](Self::truncated) latches `true`.
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

    /// Returns how many leading bytes of `data` fit under both `stream_room`
    /// and the remaining combined budget, advancing the running total and
    /// latching [`truncated`](Self::truncated) if any byte is dropped.
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
/// Resolves to the user's answer (empty / `y` to keep waiting, `n` to abort).
/// `mtui-cli` wires a [`Prompter::ask`](crate::prompter::Prompter::ask) here so
/// the prompt is serialised across parallel host tasks and suspends any live
/// spinner. `None` (headless / `mtui-mcp`) leaves the timeout an immediate
/// abort.
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
/// Split out of [`SshConnection::run`] so the policy is unit-testable without a
/// live SSH channel. Without an interactive prompt it aborts immediately, with
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

/// The russh client handler: verifies the server's host key against
/// `known_hosts`, applying the [`HostKeyPolicy`] only to keys not already
/// recorded.
///
/// A matching recorded key is accepted regardless of policy; a *changed* one is
/// rejected under every policy and never auto-added over. Only an unknown host
/// reaches the policy: `auto_add` accepts and persists atomically, `warn`
/// accepts without persisting, `reject` refuses.
///
/// A server-presented certificate is always rejected, fail-closed. mtui never
/// sets `Preferred::host_key_certificates`, so the arm is unreachable today,
/// but unwrapping a cert to its embedded key would silently downgrade a CA
/// trust decision into a TOFU one the day advertisement is enabled.
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
        server_public_key: &PublicKeyOrCertificate,
    ) -> std::result::Result<bool, Self::Error> {
        let key = match server_public_key {
            PublicKeyOrCertificate::PublicKey { key, .. } => key,
            PublicKeyOrCertificate::Certificate(cert) => {
                let ca_fingerprint = cert.signature_key().fingerprint(Default::default());
                tracing::error!(
                    host = %self.hostname,
                    %ca_fingerprint,
                    "server presented a host certificate; rejecting (fail-closed, \
                     certificate advertisement is not enabled)",
                );
                return Ok(false);
            }
        };
        Ok(self.verify(key))
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
            Ok(true) => {
                tracing::debug!(
                    host = %self.hostname,
                    %fingerprint,
                    "host key matches known_hosts",
                );
                true
            }
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
            Ok(false) => self.apply_policy(server_public_key, &fingerprint, &path),
            // An unverifiable key (no home dir, parse error, I/O) falls through
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
/// Holds the live russh [`Handle`] plus the parameters needed to re-establish
/// it on [`reconnect`](Connection::reconnect).
pub struct SshConnection {
    hostname: String,
    resolved: Resolved,
    policy: HostKeyPolicy,
    timeout: CommandTimeout,
    /// SSH connect handshake budget (TCP connect, banner, auth), distinct from
    /// [`timeout`](Self::timeout), the per-command no-output window. Also sizes
    /// each SFTP session's per-request timeout (see [`sftp`](Self::sftp)): a
    /// WAN/VPN refhost needs the handshake and the SFTP round trip raised
    /// together, so one key covers both.
    connect_timeout: CommandTimeout,
    handle: Option<Handle<ClientHandler>>,
    /// Whether a TTY-backed user can answer the command-timeout prompt. `false`
    /// (the default, and always under `mtui-mcp`) makes a no-output timeout
    /// abort instead of asking.
    is_repl: bool,
    /// Optional serialised prompt for the command-timeout branch. Wired from the
    /// composition root; `None` keeps the timeout an immediate abort.
    timeout_prompt: Option<TimeoutPrompt>,
    /// The `known_hosts` file consulted during the handshake (`None` = russh's
    /// default), retained so [`reconnect`](Connection::reconnect) re-verifies
    /// against the same file [`connect`](Self::connect) used.
    known_hosts: Option<PathBuf>,
    /// Backoff base for [`reconnect`](Connection::reconnect)'s post-reboot
    /// budget (config `reboot_timeout`, default 10s). Only consulted when the
    /// caller passes `backoff = true`; set via [`with_reboot_budget`].
    ///
    /// [`with_reboot_budget`]: Self::with_reboot_budget
    reconnect_backoff_base: Duration,
    /// The SFTP subsystem, opened lazily and reused across every `sftp_*` verb
    /// instead of one channel+handshake per call. `RusshSftpSession` routes
    /// request/reply by atomically allocated id, so concurrent verbs (e.g.
    /// `sftp_get_folder`'s fan-out) safely share one `Arc` clone each. Cleared
    /// by [`close`](Connection::close), a successful
    /// [`reconnect`](Connection::reconnect), and
    /// [`invalidate_sftp_if_fatal`](Self::invalidate_sftp_if_fatal), so the next
    /// call re-handshakes rather than reusing a dead session.
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
    /// applying `connect_timeout` to the whole handshake and `timeout` to the
    /// later per-command no-output window.
    ///
    /// `known_hosts` selects the file consulted (and, under
    /// [`AutoAdd`](HostKeyPolicy::AutoAdd), appended); `None` uses russh's
    /// default. It is a `connect` argument rather than a post-connect builder
    /// because it is applied *during* the handshake; tests pass a temp path to
    /// stay off the developer's file.
    ///
    /// # Errors
    ///
    /// [`HostError::Connect`] if the host is unreachable or the handshake
    /// failed; [`HostError::Auth`] if pubkey/agent auth was rejected (there is
    /// no password fallback).
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
    /// A no-output timeout then asks the user (via `prompt`, typically a
    /// [`Prompter::ask`](crate::prompter::Prompter::ask) bound closure) whether
    /// to keep waiting. Builder-style so the composition root can wire it after
    /// `connect` without widening the object-safe [`Connection`] trait.
    #[must_use]
    pub(crate) fn with_timeout_prompt(mut self, prompt: TimeoutPrompt) -> Self {
        self.is_repl = true;
        self.timeout_prompt = Some(prompt);
        self
    }

    /// Overrides the per-command (no-output window) timeout after connecting.
    ///
    /// Lets a caller keep a normal handshake timeout while setting a different
    /// command one. Builder-style to stay off the object-safe [`Connection`]
    /// trait.
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
    /// The whole open sequence is bounded by `connect_timeout`, since the
    /// channel/subsystem steps have no timeout of their own. The session's
    /// per-request budget (russh-sftp's `Config::request_timeout_secs`) derives
    /// from `connect_timeout` too, or its fixed 10s default would bound every
    /// SFTP op regardless of link latency; it must go through
    /// `new_with_config`, since a post-`new` `set_timeout` lands after INIT and
    /// leaves the handshake itself pinned at 10s. `.max(1)` guards a zero
    /// duration (fires instantly) — config validates `connect_timeout > 0`, but
    /// a test `CommandTimeout` can still be built at zero.
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

    /// Opens a fresh SFTP subsystem channel. Split out of [`sftp`](Self::sftp)
    /// so the whole open sequence can be wrapped in one `tokio::time::timeout`.
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
    /// [`HostError::SftpTimeout`] and [`HostError::Transport`] are where a
    /// non-`Status` russh-sftp error lands (see [`sftp_err_at_for`] /
    /// [`exclusive_create_err`]) and both mean the shared channel is suspect. A
    /// `Status`-based error is a normal per-request outcome and leaves the
    /// session cached.
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

    /// Maps a russh-sftp client error to [`HostError`], routing
    /// `SSH_FX_NO_SUCH_FILE` to [`HostError::SftpNotFound`] so the host-system
    /// parser can branch distinctly on "not found".
    fn sftp_err_at(&self, e: russh_sftp::client::error::Error, path: &Path) -> HostError {
        sftp_err_at_for(&self.hostname, e, path)
    }

    /// Categorizes the error from an **atomic exclusive create**
    /// ([`sftp_write`](Connection::sftp_write) with `exclusive = true`); see
    /// the free [`exclusive_create_err`] for the mapping and why it fails
    /// closed.
    fn exclusive_create_err(
        &self,
        e: russh_sftp::client::error::Error,
        path_str: &str,
    ) -> HostError {
        exclusive_create_err(&self.hostname, e, path_str)
    }

    /// Like [`sftp_err`](Self::sftp_err), but also invalidates the cached
    /// subsystem on a session-fatal error. Used by every `sftp_*` verb, unlike
    /// [`sftp`](Self::sftp)'s own handshake which has nothing cached yet.
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
    /// A long-lived shared subsystem can be silently closed by the peer (idle
    /// timeout, restarted sshd), and its *first* request is safe to retry
    /// because nothing has been written yet. A session opened fresh in this
    /// same call is not retried (a different, likely permanent problem), and
    /// neither is any request past the first: retrying a write/append could
    /// duplicate a remote-history row (the append-only contract).
    ///
    /// Returns the session alongside the result so the caller can issue further
    /// requests against the same subsystem.
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
/// Shared with the batched [`SshSftpSession`] so both paths map SFTP failures
/// identically.
fn sftp_err_for(host: &str, e: impl std::fmt::Display) -> HostError {
    HostError::Sftp {
        host: host.to_owned(),
        reason: e.to_string(),
    }
}

/// Maps a russh-sftp client error to [`HostError`] for `host`/`path`, routing
/// the `SSH_FX_NO_SUCH_FILE` status to [`HostError::SftpNotFound`].
///
/// Shared with the batched [`SshSftpSession`] so both preserve the "not found"
/// branch the host-system parser relies on.
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
/// ([`Connection::sftp_write`] with `exclusive = true`).
///
/// SFTPv3 has no dedicated "file exists" status, so an `O_EXCL` collision
/// surfaces as the generic [`StatusCode::Failure`]; that is the only status
/// mapped to [`HostError::AlreadyExists`] (so the lock protocol reconciles the
/// race). Every other case fails **closed** rather than being mistaken for lost
/// contention: a request timeout → [`HostError::SftpTimeout`] (the create may
/// have landed server-side despite the client never seeing the reply, so the
/// lock protocol re-reads to check), [`StatusCode::NoSuchFile`] →
/// [`HostError::SftpNotFound`] (a missing parent directory), any other status →
/// [`HostError::Sftp`], and a non-status transport/IO error →
/// [`HostError::Transport`].
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

/// The common OpenSSH default keys, tried when config names none.
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
/// Rewrites the whole file through [`mtui_config::atomic::write`] — the
/// workspace's single secure temp-file + rename implementation (th4o.11) — so a
/// concurrent reader never sees a half-written file and no predictable-name
/// temp can be pre-created.
///
/// Advisory: any failure is logged and swallowed so a fresh host still connects
/// under `auto_add`. Never logs raw key material.
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

    mtui_config::atomic::write(&contents, path)
}

/// The next `reconnect` backoff sleep after `count` attempts:
/// `2 * (timeout + 5 * count)`.
fn reconnect_delay(count: usize, base: Duration) -> Duration {
    (base + Duration::from_secs(5 * count as u64)) * 2
}

/// Establishes the transport and authenticates. `connect_timeout` bounds the
/// TCP connect / banner wait **and** the authentication — one budget for the
/// whole handshake.
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
    if let Ok(mut agent) = AgentClient::connect_env().await
        && let Ok(identities) = agent.request_identities().await
    {
        for identity in identities {
            // Pubkey auth only takes a bare `PublicKey`, so skip certificates.
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
/// The peer controls entry names, and concatenating one verbatim into
/// `{local}{name}.{host}` would let a hostile host escape the destination and
/// overwrite arbitrary local files. Accepted iff `name` is exactly one
/// [`std::path::Component::Normal`] equal to itself and free of separators /
/// control bytes; otherwise [`HostError::UnsafeSftpName`].
pub(crate) fn validate_sftp_component<'a>(name: &'a str, host: &str) -> Result<&'a str> {
    let reject = || HostError::UnsafeSftpName {
        host: host.to_owned(),
        name: name.to_owned(),
    };
    // `\` is rejected regardless of host OS because the *local* side may be
    // Windows.
    if name.is_empty()
        || name == "."
        || name == ".."
        || name.contains('/')
        || name.contains('\\')
        || name.chars().any(char::is_control)
    {
        return Err(reject());
    }
    // Defensive: catches drive/root prefixes and any separator form the byte
    // checks above might miss on other platforms.
    let mut comps = Path::new(name).components();
    match (comps.next(), comps.next()) {
        (Some(std::path::Component::Normal(c)), None) if c == name => Ok(name),
        _ => Err(reject()),
    }
}

/// A batched SFTP session over one russh channel+subsystem, returned by
/// [`SshConnection::sftp_session`].
///
/// Every read verb runs against the *same* subsystem — no per-op handshake —
/// and routes failures through the shared
/// [`sftp_err_for`]/[`sftp_err_at_for`] mappers so the error surface is
/// identical to the per-op [`Connection`] path.
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
        // The subsystem is shared, so dropping this `Arc` clone releases only
        // this handle's share; real teardown is `SshConnection::close` /
        // `reconnect` or invalidation on a session-fatal error.
        Ok(())
    }
}

#[async_trait]
impl Connection for SshConnection {
    fn hostname(&self) -> &str {
        &self.hostname
    }

    fn clone_box(&self) -> Box<dyn Connection> {
        // russh 0.62's `Handle` is neither `Clone` nor shareable across the
        // reconnect-swap `reconnect`/`close` perform, so only the connection
        // *identity* is cloned; the clone's first SFTP op opens its own
        // subsystem via `sftp()`'s reconnect-if-inactive path. A `TargetLock`
        // built from the clone therefore costs one extra channel on its (rare)
        // force-unlock path. The mock shares state via `Arc`, so offline unit
        // tests still observe the lock's SFTP ops.
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

        // Open a channel, reconnecting + retrying on a lost link.
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
        // run() never feeds stdin: EOF keeps a command that reads input from
        // blocking forever.
        let _ = channel.eof().await;

        let mut capture = CaptureBuf::default();
        let mut exitcode: i16 = NO_EXIT_CODE;
        let window = self.timeout.as_duration();
        // Non-interactive runs (headless / `mtui-mcp`) have no user to answer
        // the keep-waiting prompt, and a command trickling output forever never
        // trips the inactivity window — so they alone get an absolute ceiling.
        let deadline = (!self.is_repl)
            .then(|| Instant::now() + window.saturating_mul(COMMAND_DEADLINE_FACTOR));

        loop {
            // Checked every iteration, not only on a wait timeout: continuous
            // output keeps `channel.wait()` returning data, so the inactivity
            // branch below would never fire.
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

            // Bound each wait so the deadline is honoured even under continuous
            // output, which would otherwise keep resetting `window`.
            let wait_for = match deadline {
                Some(d) => window.min(d.saturating_duration_since(Instant::now())),
                None => window,
            };
            match timeout(wait_for, channel.wait()).await {
                // No message within the wait budget: either the absolute
                // deadline or the no-output inactivity window elapsed.
                Err(_) => {
                    if let Some(d) = deadline
                        && Instant::now() >= d
                    {
                        // Close the channel so the remote process is not
                        // orphaned.
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
                            // Reclaim the abandoned command's remote process.
                            let _ = channel.close().await;
                            return Err(HostError::Timeout {
                                command: command.to_owned(),
                            });
                        }
                    }
                }
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
        // Dropping the last `Arc` ends russh-sftp's `run()` task.
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
            // The pre-sleep is gated on `backoff`: non-reboot callers pass
            // `(0, false)` and must fail fast on a dead link mid-command, so
            // only the reboot-recovery budget pays the wait.
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
        // Revive an idle-dropped link before dispatching, as `run` does. The
        // only caller is the post-operation reboot, which follows a
        // `transactional-update` long enough for the server to have closed an
        // idle session. A failed dispatch with a *successful* reconnect is the
        // signature `update` routes its group-wide rollback on, so without this
        // an idle TCP session would downgrade every host in the group.
        if !self.is_active() {
            self.reconnect(0, false).await?;
        }
        let channel = self
            .handle()?
            .channel_open_session()
            .await
            .map_err(|e| self.transport_err(e))?;
        // Dispatch without awaiting completion; a link dropped afterward is
        // expected (e.g. reboot).
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

        // Explicit CREATE: russh-sftp's `write` convenience opens WRITE-only,
        // which returns SSH_FX_NO_SUCH_FILE for a not-yet-existing file.
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
        if let Ok(mut meta) = sftp.metadata(remote_str.clone()).await {
            meta.permissions = Some(0o770);
            let _ = sftp.set_metadata(remote_str, meta).await;
        }
        Ok(())
    }

    async fn sftp_get(&mut self, remote: &Path, local: &Path) -> Result<()> {
        // Streamed, not buffered whole in memory.
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

        // A hostile entry name is skipped, not fatal: it must not abort the
        // transfer of the legitimate ones. Logged quoted and without any local
        // path, so the diagnostic cannot leak the attacker's chosen target.
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
        // The per-entry futures run concurrently and so must not borrow
        // `&mut self`; they build errors via the free `sftp_err_for`.
        let host = self.hostname.clone();
        let sftp = &sftp;
        // The shared `sftp` session accepts concurrent `open`s (`&self`).
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

        // Those futures cannot self-invalidate the session, so a fatal failure
        // among them is handled here once the results are back.
        if let Some(Err(e)) = results.iter().find(|r| r.is_err()) {
            self.invalidate_sftp_if_fatal(e);
        }
        // Fail on the first transfer error.
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
            // Atomic exclusive create; `exclusive_create_err` explains why only
            // the generic `Failure` status maps to `AlreadyExists`.
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
            // Truncating overwrite. Explicit CREATE: the `write` convenience
            // opens WRITE-only and fails with NO_SUCH_FILE on a missing file.
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

        // O_APPEND: every write lands at the current EOF, so concurrent
        // appenders extend the file without a read-modify-write race. Only the
        // open is retried on a stale cached session — retrying the
        // write/shutdown would duplicate a remote-history row.
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
        // `sftp()` reconnects at entry if the link dropped; mid-session errors
        // then propagate.
        let sftp = self.sftp().await?;
        Ok(Box::new(SshSftpSession {
            sftp,
            hostname: self.hostname.clone(),
        }))
    }

    #[cfg(feature = "shell")]
    async fn shell(&mut self, cols: u32, rows: u32) -> Result<Box<dyn ShellChannel>> {
        // Mirrors the open->reconnect loop in `run`.
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

        // On failure, close the half-initialised channel explicitly rather than
        // relying on drop.
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
/// Reads fold [`ChannelMsg::ExtendedData`] into the same stream as
/// [`ChannelMsg::Data`], since the PTY merges stdout+stderr the way a terminal
/// sees them.
#[cfg(feature = "shell")]
struct SshShellChannel {
    host: String,
    channel: russh::Channel<russh::client::Msg>,
    /// Payload bytes received in excess of a previous `read`'s buffer, served
    /// before the next `wait()`. Dropping them instead would lose the tail of
    /// any server frame larger than the caller's buffer.
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
        if !self.leftover.is_empty() {
            let carried = std::mem::take(&mut self.leftover);
            return Ok(self.serve(&carried, buf));
        }
        loop {
            match self.channel.wait().await {
                None => return Ok(0),
                Some(ChannelMsg::Data { data }) | Some(ChannelMsg::ExtendedData { data, .. }) => {
                    return Ok(self.serve(&data, buf));
                }
                Some(ChannelMsg::Eof) | Some(ChannelMsg::Close) => return Ok(0),
                // Control messages: keep waiting for payload or close.
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
        // Idempotent per the trait contract: an already-torn-down channel is
        // success, so a double-close never surfaces an error.
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
        // Both streams at their own cap reach exactly MAX_TOTAL_BYTES.
        c.push_stdout(&vec![b'a'; MAX_STREAM_BYTES]);
        c.push_stderr(&vec![b'e'; MAX_STREAM_BYTES]);
        assert_eq!(c.total, MAX_TOTAL_BYTES);
        assert!(!c.truncated);
        c.push_stdout(b"x");
        assert_eq!(c.total, MAX_TOTAL_BYTES);
        assert!(c.truncated);
    }

    #[test]
    fn capture_partial_chunk_copies_prefix_then_truncates() {
        let mut c = CaptureBuf::default();
        // A chunk straddling the limit: only the fitting prefix is copied.
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
        let decision = on_command_timeout("h", "sleep 999", false, None).await;
        assert_eq!(decision, TimeoutDecision::Abort);
    }

    #[tokio::test]
    async fn timeout_interactive_but_no_prompt_aborts() {
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
        // A read error takes the Enter/Y default, never a spurious abort.
        let p: TimeoutPrompt = Arc::new(|_t: String| {
            Box::pin(async move { Err(std::io::Error::other("eof")) })
                as Pin<Box<dyn Future<Output = std::io::Result<String>> + Send>>
        });
        let decision = on_command_timeout("h", "sleep 999", true, Some(&p)).await;
        assert_eq!(decision, TimeoutDecision::KeepWaiting);
    }

    #[test]
    fn resolve_uses_explicit_port_and_defaults_for_unknown_host() {
        // A host no real ~/.ssh/config can name, so the resolver must fall back.
        // ($HOME is not mutated: racy under the harness, and it trips the
        // workspace's unsafe-code lint.)
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
        match std::env::var_os("HOME") {
            Some(h) => assert_eq!(dirs_home(), Some(PathBuf::from(h))),
            None => assert_eq!(dirs_home(), None),
        }
    }

    #[test]
    fn default_identity_files_only_returns_existing_paths() {
        for p in default_identity_files() {
            assert!(p.exists(), "returned nonexistent key path: {}", p.display());
            assert!(p.to_string_lossy().contains(".ssh"));
        }
    }

    #[test]
    fn debug_impl_shows_host_and_disconnected_state() {
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
        assert!(!conn.is_active());
        assert!(conn.handle().is_err());
    }

    // --- reconnect budget (fix-reboot-reconnect-window) ---

    /// A disconnected `SshConnection` on a port nothing listens on
    /// (127.0.0.1:1 refuses immediately), so `establish()` fails fast and the
    /// reconnect loop's own sleeps are isolated from network latency.
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
        // The non-reboot call shape: one immediate probe, no pre-sleep.
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
        // the listen backlog, so the socket connects but no banner arrives. The
        // short connect_timeout must bound the hang; the much larger
        // per-command budget must not leak into it.
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
        // The reboot-recovery call shape. A paused clock resolves each
        // pre-attempt sleep instantly while still advancing virtual time by the
        // scheduled amount, so the elapsed total proves both the attempt count
        // and the backoff formula in no wall-clock time.
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
        // Slack: the dead-port connect attempts' real latency accrues on the
        // paused clock alongside the auto-advanced sleeps.
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
        // No silent auto-add over a changed key.
        let after = std::fs::read_to_string(&kh).unwrap();
        assert_eq!(after.lines().count(), 1);
    }

    #[test]
    fn auto_add_persists_key_atomically() {
        let key = gen_pubkey();
        let dir = tempfile::tempdir().unwrap();
        let kh = dir.path().join("known_hosts");
        assert!(handler("h", 22, HostKeyPolicy::AutoAdd, &kh).verify(&key));

        // Now recorded, and re-verifies as known.
        assert!(kh.exists());
        assert!(handler("h", 22, HostKeyPolicy::Reject, &kh).verify(&key));
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
        assert!(handler("h", 2222, HostKeyPolicy::Reject, &kh).verify(&key));
        // The same host on the default port is *not* covered by it.
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
        // russh writes plain entries, so a `|1|salt|hash` line has to be
        // generated to prove its matcher reads the hashed form too.
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
        // A known_hosts path whose parent is a *file*: create_dir_all and the
        // temp open both fail.
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
