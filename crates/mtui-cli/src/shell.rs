//! The interactive `shell` REPL command and its raw-mode TTY bridge.
//!
//! Ports upstream `mtui.commands.shell.Shell` + the `shell`/`__invoke_shell`
//! pair in `mtui.hosts.connection.connection` (the raw-`termios` `select()`
//! loop). The host-library half — spawning the remote PTY and exposing it as an
//! object-safe [`ShellChannel`] duplex — landed in P2.10 (`mtui-hosts` feature
//! `shell`); this module is the **CLI consumer** that owns the local terminal.
//!
//! ## Why this lives in the CLI, not the engine
//!
//! Attaching an interactive PTY needs a controlling terminal, which only the
//! `mtui` binary owns. `mtui-core`'s `shell` command therefore stays a
//! headless-error stub (correct for MCP / non-interactive), and the REPL
//! **intercepts** the `shell` line before dispatch (see [`is_shell_line`]) and
//! drives the bridge here. A host library has no business toggling the local TTY
//! into raw mode, so the raw-mode bridge is a terminal concern by construction.
//!
//! ## Testability
//!
//! The pump loop [`bridge`] is generic over a [`BridgeIo`] trait abstracting the
//! three terminal touch-points (async input, stdout write, flush), so it runs
//! entirely offline against `MockConnection`'s scriptable [`ShellChannel`] — the
//! same mocking doctrine as the rest of the host layer. The only untested sliver
//! is the thin [`TerminalIo`] / [`RawModeGuard`] wiring around a real TTY.

use std::io::Write as _;

use crossterm::event::{Event, EventStream, KeyCode, KeyEvent, KeyModifiers};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use futures::StreamExt as _;
use mtui_core::Session;
use mtui_core::commands::support::{add_hosts_arg, select_names};
use mtui_hosts::{ShellChannel, Target};

/// One event pumped from the local terminal into the bridge.
#[derive(Debug, PartialEq, Eq)]
pub enum InputEvent {
    /// Keystroke bytes to forward to the remote shell.
    Bytes(Vec<u8>),
    /// A terminal size change `(cols, rows)` to forward as SSH `window-change`.
    Resize(u32, u32),
}

/// The three local-terminal touch-points the [`bridge`] pump needs, abstracted
/// so tests drive it without a real TTY (mirrors the host layer's
/// `MockConnection`).
///
/// `#[async_trait]` (not a native `async fn` in trait) keeps it dyn-compatible so
/// the [`bridge`] pump can take `&mut dyn BridgeIo`.
#[async_trait::async_trait]
pub trait BridgeIo: Send {
    /// Awaits the next local input event, or `None` on local EOF / stream end
    /// (upstream `sys.stdin.read(1) == "" → break`).
    async fn next_input(&mut self) -> Option<InputEvent>;

    /// Writes remote shell output to the local terminal (upstream
    /// `sys.stdout.write`).
    fn write_out(&mut self, bytes: &[u8]);

    /// Flushes the local terminal (upstream `sys.stdout.flush`).
    fn flush(&mut self);
}

/// Pumps bytes between a spawned [`ShellChannel`] and the local terminal until
/// either side ends the session.
///
/// Ports upstream's `while True: select([session, stdin]) …` loop:
///
/// * remote output → [`BridgeIo::write_out`]; `read → Ok(0)` (remote shell
///   exited) or a channel error stops the loop;
/// * local [`InputEvent::Bytes`] → [`ShellChannel::write`]; a local `None`
///   (stdin EOF) stops the loop;
/// * local [`InputEvent::Resize`] → best-effort [`ShellChannel::resize`]
///   (SIGWINCH forwarding — an improvement over upstream, which fixes size at
///   spawn).
///
/// The channel is closed on the way out.
///
/// # Errors
///
/// Propagates a [`ShellChannel::write`] failure (a keystroke could not be sent).
/// A read error is treated as a clean stop, not an error, matching upstream's
/// `except TimeoutError: pass` / EOF handling.
pub async fn bridge(channel: &mut dyn ShellChannel, io: &mut dyn BridgeIo) -> anyhow::Result<()> {
    let mut buf = [0u8; 4096];
    loop {
        tokio::select! {
            read = channel.read(&mut buf) => match read {
                Ok(0) | Err(_) => break,          // remote shell exited / channel error
                Ok(n) => {
                    io.write_out(&buf[..n]);
                    io.flush();
                }
            },
            ev = io.next_input() => match ev {
                None => break,                    // local EOF
                Some(InputEvent::Bytes(b)) => channel.write(&b).await?,
                Some(InputEvent::Resize(cols, rows)) => {
                    // Best-effort: a resize failure must not tear down the session.
                    let _ = channel.resize(cols, rows).await;
                }
            },
        }
    }
    let _ = channel.close().await;
    Ok(())
}

/// Restores the terminal to cooked mode on drop — the RAII equivalent of
/// upstream's `finally: termios.tcsetattr(..., oldtty)`.
///
/// Enabling raw mode in [`new`](Self::new) and disabling it in [`Drop`]
/// guarantees restoration on normal return, `?`, or panic.
struct RawModeGuard;

impl RawModeGuard {
    /// Puts the terminal into raw mode (upstream `tty.setraw`/`setcbreak`).
    ///
    /// # Errors
    ///
    /// Propagates the underlying [`enable_raw_mode`] failure.
    fn new() -> anyhow::Result<Self> {
        enable_raw_mode()?;
        Ok(Self)
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        // Best-effort restore; nothing actionable if the terminal is already gone.
        let _ = disable_raw_mode();
    }
}

/// The production [`BridgeIo`]: a crossterm [`EventStream`] for input/resize and
/// stdout for output.
struct TerminalIo {
    events: EventStream,
    stdout: std::io::Stdout,
}

impl TerminalIo {
    fn new() -> Self {
        Self {
            events: EventStream::new(),
            stdout: std::io::stdout(),
        }
    }
}

#[async_trait::async_trait]
impl BridgeIo for TerminalIo {
    async fn next_input(&mut self) -> Option<InputEvent> {
        loop {
            match self.events.next().await {
                Some(Ok(Event::Key(key))) => {
                    if let Some(bytes) = encode_key(key) {
                        return Some(InputEvent::Bytes(bytes));
                    }
                    // A key with no byte encoding (e.g. a bare modifier) is
                    // ignored; keep waiting for the next event.
                }
                Some(Ok(Event::Paste(text))) => {
                    return Some(InputEvent::Bytes(text.into_bytes()));
                }
                Some(Ok(Event::Resize(cols, rows))) => {
                    return Some(InputEvent::Resize(u32::from(cols), u32::from(rows)));
                }
                // Focus/mouse events carry nothing for the shell; keep waiting.
                Some(Ok(_)) => {}
                // A read error or end-of-stream ends the local side.
                Some(Err(_)) | None => return None,
            }
        }
    }

    fn write_out(&mut self, bytes: &[u8]) {
        let _ = self.stdout.write_all(bytes);
    }

    fn flush(&mut self) {
        let _ = self.stdout.flush();
    }
}

/// Encodes a crossterm [`KeyEvent`] into the terminal byte sequence a remote
/// shell expects, covering the common cases (printable, Enter, Tab, Backspace,
/// Esc, Ctrl-letter, and CSI arrow keys).
///
/// Returns `None` for keys with no meaningful byte encoding (bare modifiers).
/// This is deliberately not exhaustive — uncommon keys are dropped rather than
/// mis-encoded; extend as needed.
fn encode_key(key: KeyEvent) -> Option<Vec<u8>> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    match key.code {
        KeyCode::Char(c) => {
            if ctrl {
                // Ctrl-letter → control byte (Ctrl-A = 0x01 … Ctrl-Z = 0x1a);
                // matches a terminal driver's control-char generation.
                let upper = c.to_ascii_uppercase();
                if upper.is_ascii_uppercase() {
                    return Some(vec![(upper as u8) - b'A' + 1]);
                }
                // Ctrl with a non-letter: fall through to the raw byte.
            }
            let mut b = [0u8; 4];
            Some(c.encode_utf8(&mut b).as_bytes().to_vec())
        }
        KeyCode::Enter => Some(vec![b'\r']),
        KeyCode::Tab => Some(vec![b'\t']),
        KeyCode::Backspace => Some(vec![0x7f]),
        KeyCode::Esc => Some(vec![0x1b]),
        KeyCode::Up => Some(b"\x1b[A".to_vec()),
        KeyCode::Down => Some(b"\x1b[B".to_vec()),
        KeyCode::Right => Some(b"\x1b[C".to_vec()),
        KeyCode::Left => Some(b"\x1b[D".to_vec()),
        KeyCode::Home => Some(b"\x1b[H".to_vec()),
        KeyCode::End => Some(b"\x1b[F".to_vec()),
        KeyCode::Delete => Some(b"\x1b[3~".to_vec()),
        _ => None,
    }
}

/// Spawns an interactive shell on `target` and runs the local↔remote [`bridge`]
/// under raw mode.
///
/// Reads the current terminal size, spawns via [`Target::shell`] (state-gated:
/// a disabled/dryrun/failed target returns `None`), then bridges until the
/// session ends, restoring cooked mode via [`RawModeGuard`] on the way out.
///
/// # Errors
///
/// * Returns an error if the terminal cannot enter raw mode.
/// * Returns an error if `target.shell(..)` yields `None` (no PTY to attach —
///   the caller reports it and moves on to the next host).
/// * Propagates a [`bridge`] failure.
async fn run_bridge_on(target: &mut Target) -> anyhow::Result<()> {
    let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
    // Enter raw mode *before* spawning so the guard restores it even if the
    // bridge fails partway; the spawn itself does not touch the local TTY.
    let _guard = RawModeGuard::new()?;
    let mut channel = target
        .shell(u32::from(cols), u32::from(rows))
        .await
        .ok_or_else(|| anyhow::anyhow!("could not open a shell on {}", target.hostname()))?;
    let mut io = TerminalIo::new();
    bridge(channel.as_mut(), &mut io).await
    // `_guard` drops here → cooked mode restored.
}

/// Peeks a REPL input line: if its first token is the `shell` command, returns
/// its argv (everything after the command word); otherwise `None`.
///
/// This is the pure seam the REPL uses to route `shell` to the CLI bridge
/// instead of the headless engine (kept off the reedline boundary so it is
/// unit-testable, mirroring the P6.2 `step` extraction).
#[must_use]
pub fn is_shell_line(line: &str) -> Option<Vec<String>> {
    let tokens = shlex_split(line)?;
    let (name, argv) = tokens.split_first()?;
    (name == "shell").then(|| argv.to_vec())
}

/// Shlex-splits a line the same way the engine does (upstream `shlex.split`),
/// returning `None` on unbalanced quotes.
fn shlex_split(line: &str) -> Option<Vec<String>> {
    shlex::split(line)
}

/// Runs the `shell` command: parse `-t/--target`, resolve the host selection,
/// then attach a shell on each selected host **sequentially** (upstream
/// `for target in targets: targets[target].shell()`).
///
/// A host with no attachable PTY (disabled / dryrun / spawn failure →
/// [`Target::shell`] `None`) is reported and skipped; the loop continues to the
/// next host. An empty selection is reported and nothing is spawned.
///
/// # Errors
///
/// Returns an error only for an argument-parse failure (clap usage) or an
/// unbalanced-quote split — the same class of error the engine would render.
/// Per-host attach failures are rendered to the session display and do not abort
/// the command.
pub async fn run_shell(session: &mut Session, argv: &[String]) -> anyhow::Result<()> {
    // Reuse mtui-core's exact `-t/--target` grammar so REPL `shell` and the
    // (headless-stub) engine `shell` stay in lock-step.
    let parser = add_hosts_arg(clap::Command::new("shell").no_binary_name(true));
    let matches = parser
        .try_get_matches_from(argv)
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    let names =
        select_names(session.targets(), &matches, true).map_err(|e| anyhow::anyhow!("{e}"))?;
    if names.is_empty() {
        session.display.println("no reference hosts defined");
        return Ok(());
    }

    for name in names {
        let Some(target) = session.targets_mut().get_mut(&name) else {
            // The name resolved from the live group a moment ago; a miss here is
            // a race we simply skip.
            continue;
        };
        if let Err(e) = run_bridge_on(target).await {
            let msg = session.display.red(&format!("shell on {name}: {e}"));
            session.display.println(&msg);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use mtui_hosts::{HostsGroup, MockConnection, Target};
    use mtui_types::enums::{ExecutionMode, TargetState};
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    /// A test [`ShellChannel`] that records writes/resizes and **blocks forever**
    /// on `read` (never EOF), so a test can assert input handling deterministically
    /// via a local-EOF (`next_input → None`) stop — without racing the mock's
    /// instant `Ok(0)`. (The `MockConnection` shell double, which serves canned
    /// output then EOF, covers the remote-EOF stop path in the `bridge_writes_*`
    /// test.)
    struct BlockingChannel {
        writes: Arc<Mutex<Vec<u8>>>,
        resizes: Arc<Mutex<Vec<(u32, u32)>>>,
    }

    #[async_trait::async_trait]
    impl ShellChannel for BlockingChannel {
        async fn read(&mut self, _buf: &mut [u8]) -> mtui_hosts::Result<usize> {
            std::future::pending().await
        }
        async fn write(&mut self, data: &[u8]) -> mtui_hosts::Result<()> {
            self.writes.lock().unwrap().extend_from_slice(data);
            Ok(())
        }
        async fn resize(&mut self, cols: u32, rows: u32) -> mtui_hosts::Result<()> {
            self.resizes.lock().unwrap().push((cols, rows));
            Ok(())
        }
        async fn close(&mut self) -> mtui_hosts::Result<()> {
            Ok(())
        }
    }

    /// A scripted [`BridgeIo`] double: serves queued input events, records
    /// everything written to "stdout".
    ///
    /// Once the queue is drained, [`next_input`](ScriptedIo::next_input) either
    /// returns `None` (signalling local EOF, the loop's stop condition) or stays
    /// pending forever (`eof_on_drain = false`), so the *remote* side (channel
    /// EOF) is what stops the loop — mirroring a real terminal, where stdin
    /// blocks rather than reporting EOF while the shell is alive. The pending
    /// mode lets a test assert that all remote output is flushed before the
    /// remote shell exits, without racing an instant local EOF.
    struct ScriptedIo {
        inputs: VecDeque<InputEvent>,
        out: Vec<u8>,
        eof_on_drain: bool,
    }

    impl ScriptedIo {
        /// Drives the given inputs, then reports local EOF (`None`).
        fn new(inputs: Vec<InputEvent>) -> Self {
            Self {
                inputs: inputs.into(),
                out: Vec::new(),
                eof_on_drain: true,
            }
        }

        /// Drives the given inputs, then blocks forever — so only the remote
        /// channel EOF stops the loop.
        fn blocking_after(inputs: Vec<InputEvent>) -> Self {
            Self {
                inputs: inputs.into(),
                out: Vec::new(),
                eof_on_drain: false,
            }
        }
    }

    #[async_trait::async_trait]
    impl BridgeIo for ScriptedIo {
        async fn next_input(&mut self) -> Option<InputEvent> {
            if let Some(ev) = self.inputs.pop_front() {
                return Some(ev);
            }
            if self.eof_on_drain {
                None
            } else {
                // Stay pending: let the channel-read branch of the select win.
                std::future::pending().await
            }
        }
        fn write_out(&mut self, bytes: &[u8]) {
            self.out.extend_from_slice(bytes);
        }
        fn flush(&mut self) {}
    }

    /// An enabled [`Target`] backed by a `MockConnection` with the given canned
    /// shell output chunks; returns the handle so tests can introspect it.
    fn target_with_output(host: &str, chunks: &[&[u8]]) -> (Target, MockConnection) {
        let mut conn = MockConnection::new(host);
        for c in chunks {
            conn = conn.with_shell_output(c.to_vec());
        }
        let handle = conn.clone();
        let target = Target::with_connection(
            host,
            TargetState::Enabled,
            ExecutionMode::Serial,
            Box::new(conn),
        );
        (target, handle)
    }

    #[tokio::test]
    async fn bridge_writes_remote_output_then_stops_on_eof() {
        let (mut target, _h) = target_with_output("h1", &[b"welcome\n", b"$ "]);
        let mut channel = target.shell(80, 24).await.expect("spawn");
        // Block on input so the remote channel EOF (all chunks drained) is what
        // stops the loop — asserts every output chunk is flushed first.
        let mut io = ScriptedIo::blocking_after(vec![]);
        bridge(channel.as_mut(), &mut io).await.unwrap();
        assert_eq!(io.out, b"welcome\n$ ");
    }

    #[tokio::test]
    async fn bridge_forwards_keystrokes_to_channel() {
        let writes = Arc::new(Mutex::new(Vec::new()));
        let mut channel = BlockingChannel {
            writes: Arc::clone(&writes),
            resizes: Arc::new(Mutex::new(Vec::new())),
        };
        // Input drains, then `next_input → None` stops the loop; the channel
        // never EOFs, so the write is guaranteed processed first.
        let mut io = ScriptedIo::new(vec![InputEvent::Bytes(b"ls\n".to_vec())]);
        bridge(&mut channel, &mut io).await.unwrap();
        assert_eq!(*writes.lock().unwrap(), b"ls\n");
    }

    #[tokio::test]
    async fn bridge_forwards_resize_to_channel() {
        let resizes = Arc::new(Mutex::new(Vec::new()));
        let mut channel = BlockingChannel {
            writes: Arc::new(Mutex::new(Vec::new())),
            resizes: Arc::clone(&resizes),
        };
        let mut io = ScriptedIo::new(vec![InputEvent::Resize(120, 40)]);
        bridge(&mut channel, &mut io).await.unwrap();
        assert_eq!(*resizes.lock().unwrap(), vec![(120, 40)]);
    }

    #[tokio::test]
    async fn bridge_records_spawn_size() {
        let (mut target, handle) = target_with_output("h1", &[]);
        let _channel = target.shell(90, 20).await.expect("spawn");
        assert_eq!(handle.shell_spawns(), vec![(90, 20)]);
    }

    #[tokio::test]
    async fn bridge_stops_on_local_eof_with_no_input() {
        // No scripted input and no output: `next_input` yields None → clean stop.
        let (mut target, _h) = target_with_output("h1", &[]);
        let mut channel = target.shell(80, 24).await.expect("spawn");
        let mut io = ScriptedIo::new(vec![]);
        // Even with a pending output chunk absent, the loop must terminate.
        bridge(channel.as_mut(), &mut io).await.unwrap();
        assert!(io.out.is_empty());
    }

    #[test]
    fn is_shell_line_matches_only_shell() {
        assert_eq!(is_shell_line("shell"), Some(vec![]));
        assert_eq!(
            is_shell_line("shell -t h1"),
            Some(vec!["-t".to_owned(), "h1".to_owned()])
        );
        assert_eq!(is_shell_line("run uname -a"), None);
        assert_eq!(is_shell_line(""), None);
        // Unbalanced quotes: not routed to the bridge (engine renders the error).
        assert_eq!(is_shell_line("shell \"unbalanced"), None);
    }

    #[test]
    fn encode_key_common_cases() {
        let k = |code, mods| encode_key(KeyEvent::new(code, mods));
        assert_eq!(
            k(KeyCode::Char('a'), KeyModifiers::NONE),
            Some(b"a".to_vec())
        );
        assert_eq!(k(KeyCode::Enter, KeyModifiers::NONE), Some(b"\r".to_vec()));
        assert_eq!(k(KeyCode::Tab, KeyModifiers::NONE), Some(b"\t".to_vec()));
        assert_eq!(k(KeyCode::Backspace, KeyModifiers::NONE), Some(vec![0x7f]));
        assert_eq!(k(KeyCode::Esc, KeyModifiers::NONE), Some(vec![0x1b]));
        assert_eq!(k(KeyCode::Up, KeyModifiers::NONE), Some(b"\x1b[A".to_vec()));
        // Ctrl-C → 0x03.
        assert_eq!(
            k(KeyCode::Char('c'), KeyModifiers::CONTROL),
            Some(vec![0x03])
        );
        // Ctrl-D → 0x04.
        assert_eq!(
            k(KeyCode::Char('d'), KeyModifiers::CONTROL),
            Some(vec![0x04])
        );
    }

    /// Builds a session with a group of enabled hosts, each backed by a
    /// `MockConnection`, returning the handles keyed by name for introspection.
    fn group(hosts: &[(&str, TargetState)]) -> HostsGroup {
        let targets = hosts
            .iter()
            .map(|(h, state)| {
                Target::with_connection(
                    *h,
                    *state,
                    ExecutionMode::Serial,
                    Box::new(MockConnection::new(*h)),
                )
            })
            .collect();
        HostsGroup::new(targets, true)
    }

    /// A session whose active report carries the given host group.
    fn session_with_group(g: HostsGroup) -> Session {
        use mtui_config::Config;
        use mtui_core::{ColorMode, CommandPromptDisplay};
        let display = CommandPromptDisplay::with_sink(Box::new(std::io::sink()), ColorMode::Never);
        let mut session = Session::with_display(Config::default(), true, display);
        *session.targets_mut() = g;
        session
    }

    #[tokio::test]
    async fn run_shell_no_hosts_does_not_error() {
        let mut session = session_with_group(group(&[]));
        // No hosts → prints a notice, returns Ok, spawns nothing.
        run_shell(&mut session, &[]).await.unwrap();
    }

    #[tokio::test]
    async fn run_shell_unknown_host_errors() {
        let mut session = session_with_group(group(&[("h1", TargetState::Enabled)]));
        let err = run_shell(&mut session, &["-t".to_owned(), "ghost".to_owned()])
            .await
            .unwrap_err();
        assert!(err.to_string().contains("ghost"));
    }

    #[tokio::test]
    async fn run_shell_bad_flag_errors() {
        let mut session = session_with_group(group(&[("h1", TargetState::Enabled)]));
        let err = run_shell(&mut session, &["--nope".to_owned()])
            .await
            .unwrap_err();
        assert!(!err.to_string().is_empty());
    }
}
