//! Serialised interactive prompter for concurrent host fan-outs.
//!
//! Ported from upstream `mtui/cli/prompter.py`. When many targets run a command
//! in parallel, a worker that needs to ask the user something (e.g. the
//! command-timeout "keep waiting?" prompt in
//! [`SshConnection`](crate::connection::SshConnection)) must not race a sibling
//! for `stdin`: with two workers reading at once the prompt text interleaves
//! with other output and two workers can consume the same line.
//!
//! [`Prompter`] serialises those reads behind a single lock — only one worker
//! reads `stdin` at a time; the others queue on the lock until the current
//! prompt returns. During the read it holds a
//! [`suspend_async`](crate::target::suspend_async) guard so a live TTY spinner
//! erases its frame and stops repainting over the prompt until the user answers.
//!
//! ## Async, not threads
//!
//! Upstream uses a `threading.Lock` because its workers are OS threads. mtui
//! fans out with `tokio` tasks, so the lock is a [`tokio::sync::Mutex`] and
//! [`ask`](Prompter::ask) is async — held across the reader's `.await` soundly
//! (unlike a `std::sync::Mutex`, which clippy's `await_holding_lock` rightly
//! rejects and which would block the runtime).
//!
//! ## Injectable reader
//!
//! The reader is injected so the class is unit-testable without a real terminal
//! or `stdin`. The production reader is a blocking `stdin().read_line` bridged
//! onto the runtime with [`spawn_blocking`](tokio::task::spawn_blocking); the
//! composition root (`mtui-cli`) supplies it. Under `mtui-mcp` (no TTY) no
//! prompter is constructed at all — the timeout branch degrades to a silent
//! abort instead (upstream `prompter=None`).

use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::Arc;

use tokio::sync::Mutex;

use crate::target::suspend_async;

/// A boxed async reader: called with the prompt text, resolves to the user's
/// typed response.
pub type Reader =
    Arc<dyn Fn(String) -> Pin<Box<dyn Future<Output = io::Result<String>> + Send>> + Send + Sync>;

/// Serialises interactive prompts across concurrent host tasks.
///
/// Holds one async lock; [`ask`](Prompter::ask) acquires it for the duration of
/// the read and suspends any live spinner, so callers from any task observe
/// strictly sequential prompts in lock-acquisition order.
#[derive(Clone)]
pub struct Prompter {
    lock: Arc<Mutex<()>>,
    reader: Reader,
}

impl Prompter {
    /// Builds a prompter reading through `reader`.
    ///
    /// `reader` is invoked with the prompt text and must return the user's
    /// response. Injected so tests never touch a real terminal.
    #[must_use]
    pub fn new(reader: Reader) -> Self {
        Self {
            lock: Arc::new(Mutex::new(())),
            reader,
        }
    }

    /// Builds a prompter reading a line from the real `stdin` via
    /// [`spawn_blocking`](tokio::task::spawn_blocking), printing `text` to
    /// `stdout` first (no trailing newline, like `input`).
    #[must_use]
    pub fn stdin() -> Self {
        Self::new(Arc::new(|text: String| {
            Box::pin(async move {
                tokio::task::spawn_blocking(move || {
                    use std::io::Write as _;
                    let mut out = io::stdout();
                    let _ = out.write_all(text.as_bytes());
                    let _ = out.flush();
                    let mut line = String::new();
                    io::stdin().read_line(&mut line)?;
                    // Match `input`: strip the trailing newline only.
                    let trimmed = line.trim_end_matches(['\n', '\r']).to_owned();
                    Ok(trimmed)
                })
                .await
                .unwrap_or_else(|e| Err(io::Error::other(e)))
            }) as Pin<Box<dyn Future<Output = io::Result<String>> + Send>>
        }))
    }

    /// Prompts the user with `text` and returns the typed response.
    ///
    /// Acquires the prompter's lock for the whole read so sibling tasks cannot
    /// race for `stdin`, and holds a [`suspend_async`] guard so a live spinner
    /// erases its frame and stays quiet until the user answers.
    ///
    /// # Errors
    ///
    /// Propagates any I/O error from the injected reader.
    pub async fn ask(&self, text: &str) -> io::Result<String> {
        let _serialise = self.lock.lock().await;
        let _quiet = suspend_async();
        (self.reader)(text.to_owned()).await
    }

    /// Asks a yes/no question, returning whether the user confirmed.
    ///
    /// The Rust analogue of upstream `mtui.cli.term.prompt_user(text, ["yes",
    /// "y"], interactive=True, default)`: reads a line via [`ask`](Prompter::ask),
    /// lowercases it, and returns `true` for an empty response iff `default` is
    /// `true`, or for a response in `{"yes", "y"}`; anything else (including an
    /// I/O error, the analogue of upstream's Ctrl-C/Ctrl-D swallow) is `false`.
    ///
    /// Callers gate this behind an `interactive` check: upstream's
    /// non-interactive mode never requests input and always returns `false` (so
    /// a defaulted destructive prompt never auto-confirms unattended), so a
    /// headless caller must not call `confirm` at all — see the load-time
    /// stale-hash handling in `mtui-testreport`.
    pub async fn confirm(&self, text: &str, default: bool) -> bool {
        match self.ask(text).await {
            Ok(response) => {
                let response = response.trim().to_ascii_lowercase();
                if response.is_empty() {
                    default
                } else {
                    matches!(response.as_str(), "yes" | "y")
                }
            }
            // Upstream swallows KeyboardInterrupt / EOFError and returns False.
            Err(_) => false,
        }
    }

    /// Builds a [`TimeoutPrompt`](crate::connection::TimeoutPrompt) that routes
    /// the SSH command-timeout question through this prompter's serialised
    /// [`ask`](Prompter::ask).
    ///
    /// The single place the boxed-closure shape lives, so the composition root
    /// (`Session` → [`Target::set_timeout_prompt`]) wires the same serialised
    /// prompt (spinner-suspend + cross-task lock) instead of a bare closure.
    /// Upstream `Target.connect` passes `self._prompter.ask`; this is the Rust
    /// analogue.
    ///
    /// [`Target::set_timeout_prompt`]: crate::Target::set_timeout_prompt
    #[must_use]
    pub fn as_timeout_prompt(&self) -> crate::connection::TimeoutPrompt {
        let this = self.clone();
        Arc::new(move |text: String| {
            let this = this.clone();
            Box::pin(async move { this.ask(&text).await })
                as Pin<Box<dyn Future<Output = io::Result<String>> + Send>>
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    /// A reader that records its call order and returns a fixed answer.
    fn recording_reader(
        answer: &'static str,
        concurrent: Arc<AtomicUsize>,
        max_seen: Arc<AtomicUsize>,
    ) -> Reader {
        Arc::new(move |_text: String| {
            let concurrent = Arc::clone(&concurrent);
            let max_seen = Arc::clone(&max_seen);
            Box::pin(async move {
                let now = concurrent.fetch_add(1, Ordering::SeqCst) + 1;
                max_seen.fetch_max(now, Ordering::SeqCst);
                // Hold the "read" open briefly so any overlap would be observed.
                tokio::time::sleep(Duration::from_millis(20)).await;
                concurrent.fetch_sub(1, Ordering::SeqCst);
                Ok(answer.to_owned())
            }) as Pin<Box<dyn Future<Output = io::Result<String>> + Send>>
        })
    }

    #[tokio::test]
    async fn ask_returns_reader_response() {
        let _serial = crate::target::spinner::TEST_SERIAL.lock().await;
        let p = Prompter::new(Arc::new(|text: String| {
            Box::pin(async move { Ok(format!("echo:{text}")) })
                as Pin<Box<dyn Future<Output = io::Result<String>> + Send>>
        }));
        assert_eq!(p.ask("hi").await.unwrap(), "echo:hi");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_asks_are_serialised() {
        let _serial = crate::target::spinner::TEST_SERIAL.lock().await;
        let concurrent = Arc::new(AtomicUsize::new(0));
        let max_seen = Arc::new(AtomicUsize::new(0));
        let p = Prompter::new(recording_reader(
            "ok",
            Arc::clone(&concurrent),
            Arc::clone(&max_seen),
        ));
        // Fire several asks at once; the lock must keep the reader single-entry.
        let mut handles = Vec::new();
        for _ in 0..8 {
            let p = p.clone();
            handles.push(tokio::spawn(async move { p.ask("q").await.unwrap() }));
        }
        for h in handles {
            assert_eq!(h.await.unwrap(), "ok");
        }
        assert_eq!(
            max_seen.load(Ordering::SeqCst),
            1,
            "reader saw overlapping prompts"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn ask_suspends_active_spinner_during_read() {
        use crate::target::spinner;
        use std::io::Write;
        use std::sync::Mutex as StdMutex;

        let _serial = spinner::TEST_SERIAL.lock().await;

        // Shared in-memory sink so we can inspect what the spinner painted.
        let store = Arc::new(StdMutex::new(Vec::<u8>::new()));
        struct SharedBuf(Arc<StdMutex<Vec<u8>>>);
        impl Write for SharedBuf {
            fn write(&mut self, b: &[u8]) -> io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(b);
                Ok(b.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let sink: spinner::Sink = Arc::new(StdMutex::new(SharedBuf(Arc::clone(&store))));
        let mut s = spinner::TtySpinner::with_sink("busy", true, sink);
        s.start();
        tokio::time::sleep(Duration::from_millis(150)).await;

        // A reader that, mid-read, records the sink contents so we can prove no
        // frame is painted while the prompt is open.
        let probe = Arc::clone(&store);
        let p = Prompter::new(Arc::new(move |_t: String| {
            let probe = Arc::clone(&probe);
            Box::pin(async move {
                let before = String::from_utf8_lossy(&probe.lock().unwrap()).into_owned();
                tokio::time::sleep(Duration::from_millis(150)).await;
                let after = String::from_utf8_lossy(&probe.lock().unwrap()).into_owned();
                assert_eq!(before, after, "spinner repainted during prompt read");
                assert!(before.ends_with("\r\x1b[K"), "frame not erased: {before:?}");
                Ok("answer".to_owned())
            }) as Pin<Box<dyn Future<Output = io::Result<String>> + Send>>
        }));
        assert_eq!(p.ask("q").await.unwrap(), "answer");
        s.stop();
    }

    #[tokio::test]
    async fn as_timeout_prompt_routes_through_ask() {
        let _serial = crate::target::spinner::TEST_SERIAL.lock().await;
        let p = Prompter::new(Arc::new(|text: String| {
            Box::pin(async move { Ok(format!("via-ask:{text}")) })
                as Pin<Box<dyn Future<Output = io::Result<String>> + Send>>
        }));
        let tp = p.as_timeout_prompt();
        assert_eq!(tp("q".to_owned()).await.unwrap(), "via-ask:q");
    }

    #[tokio::test]
    async fn ask_propagates_reader_error() {
        let _serial = crate::target::spinner::TEST_SERIAL.lock().await;
        let p = Prompter::new(Arc::new(|_text: String| {
            Box::pin(async move { Err(io::Error::other("boom")) })
                as Pin<Box<dyn Future<Output = io::Result<String>> + Send>>
        }));
        let err = p.ask("x").await.unwrap_err();
        assert_eq!(err.to_string(), "boom");
    }

    /// Builds a prompter that always answers `answer`.
    fn fixed_prompter(answer: &'static str) -> Prompter {
        Prompter::new(Arc::new(move |_text: String| {
            Box::pin(async move { Ok(answer.to_owned()) })
                as Pin<Box<dyn Future<Output = io::Result<String>> + Send>>
        }))
    }

    #[tokio::test]
    async fn confirm_yes_tokens_are_true() {
        let _serial = crate::target::spinner::TEST_SERIAL.lock().await;
        for ans in ["y", "Y", "yes", "YES", " yes "] {
            assert!(
                fixed_prompter(ans).confirm("go? ", false).await,
                "{ans:?} should confirm"
            );
        }
    }

    #[tokio::test]
    async fn confirm_other_input_is_false() {
        let _serial = crate::target::spinner::TEST_SERIAL.lock().await;
        for ans in ["n", "no", "nope", "maybe"] {
            assert!(
                !fixed_prompter(ans).confirm("go? ", true).await,
                "{ans:?} should decline"
            );
        }
    }

    #[tokio::test]
    async fn confirm_empty_uses_default() {
        let _serial = crate::target::spinner::TEST_SERIAL.lock().await;
        assert!(fixed_prompter("").confirm("go? ", true).await);
        assert!(!fixed_prompter("").confirm("go? ", false).await);
    }

    #[tokio::test]
    async fn confirm_reader_error_is_false() {
        let _serial = crate::target::spinner::TEST_SERIAL.lock().await;
        let p = Prompter::new(Arc::new(|_text: String| {
            Box::pin(async move { Err(io::Error::other("eof")) })
                as Pin<Box<dyn Future<Output = io::Result<String>> + Send>>
        }));
        // Upstream swallows Ctrl-C / Ctrl-D and returns False, even with a
        // default of true.
        assert!(!p.confirm("go? ", true).await);
    }
}
