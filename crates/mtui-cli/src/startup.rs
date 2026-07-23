//! Startup seeding: the pre-REPL work the `mtui` binary does before entering the
//! interactive loop.
//!
//! Port of the argv-driven half of upstream `mtui.main.run_mtui` — the part that
//! runs *before* `prompt.cmdloop(...)`:
//!
//! 1. **`-a`/`-k` update** → [`Session::load_update`] (upstream
//!    `prompt.load_update`). An explicitly requested update that resolves to a
//!    null report exits the process (upstream returns `1` rather than dropping
//!    into an empty session).
//! 2. **`--sut` hosts** → the `add_host` command dispatched through the shared
//!    engine (upstream `prompt.do_add_host(x.print_args())`), best-effort: a bad
//!    host is logged and skipped.
//!
//! There is **no single-command / non-interactive mode**: upstream `mtui` has
//! only two surfaces — the REPL and `mtui-mcp` — and neither takes a positional
//! command. This module therefore only *seeds* the session; the binary always
//! enters the REPL afterwards. (The `run_once`/`ExitStatus` primitive in
//! `mtui-core` exists for `mtui-mcp`/embedding, not a CLI headless mode.)
//!
//! [`seed_session`] is the testable seam: it takes an already-built [`Session`]
//! and the parsed [`Args`], performs the seeding, and returns [`ControlFlow`] so
//! `main` can map a fatal outcome to a process exit without this module ever
//! calling [`std::process::exit`] itself.

use std::ops::ControlFlow;

use mtui_core::{Args, Registry, Session, dispatch_line};
use mtui_testreport::UpdateKind;
use mtui_types::Workflow;

/// Seeds `session` from the top-level `args` before the REPL starts.
///
/// Mirrors the pre-`cmdloop` body of upstream `run_mtui`:
///
/// * If `args.update()` is set (`-a`/`-k`), loads it via
///   [`Session::load_update`]. Autoconnect is requested iff no `--sut` override
///   was given (upstream `autoconnect=not bool(args.sut)`). If the load yields an
///   empty RRID — a null report for an *explicitly requested* update — the
///   user-facing "does not exist" message has already been logged, so this
///   returns [`ControlFlow::Break`] with exit code `1` rather than entering an
///   empty REPL.
/// * For each `--sut` entry, dispatches `add_host <fragment>` through the shared
///   engine (upstream `do_add_host(x.print_args())`), logging any failure and
///   continuing — one bad host never aborts startup.
///
/// Returns [`ControlFlow::Continue`] when the session is ready for the REPL, or
/// [`ControlFlow::Break(code)`] when the process should exit with `code` instead.
pub async fn seed_session(
    registry: &Registry,
    session: &mut Session,
    args: &Args,
) -> ControlFlow<i32> {
    if let Some(update) = args.update() {
        let autoconnect = args.sut.is_empty();
        let kind = match update.workflow {
            Workflow::Kernel => UpdateKind::Kernel,
            // `Args::update()` only ever yields `Auto` or `Kernel`; treat anything
            // else as the automatic default.
            _ => UpdateKind::Auto,
        };
        let rrid = session.load_update(&update.id, autoconnect, kind).await;
        if rrid.is_empty() {
            // Null report for an explicitly requested update: the "does not
            // exist" message was already logged by the load path. Exit rather
            // than drop into an empty interactive session (upstream parity).
            tracing::error!(update = %update.id, "requested update could not be loaded");
            return ControlFlow::Break(1);
        }
    }

    for sut in &args.sut {
        let line = format!("add_host {}", sut.print_args());
        if let Err(err) = dispatch_line(registry, session, &line).await {
            // Best-effort, upstream `do_add_host` + `logger.error`: log and keep
            // going so one malformed `--sut` never blocks the REPL.
            tracing::error!(%err, sut = ?sut.hosts(), "failed to add SUT host(s)");
        }
    }

    ControlFlow::Continue(())
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::{Arc, Mutex};

    use mtui_config::Config;
    use mtui_core::args::ColorArg;
    use mtui_core::{ColorMode, CommandPromptDisplay, register_all};

    /// Default top-level args with everything unset.
    fn args() -> Args {
        Args {
            template_dir: None,
            sut: Vec::new(),
            connection_timeout: None,
            reboot_timeout: None,
            reboot_retries: None,
            debug: false,
            config: None,
            color: ColorArg::Never,
            gitea_token: None,
            ssl_verify: None,
            auto_review_id: None,
            kernel_review_id: None,
        }
    }

    /// A `Write` sink backed by a shared buffer so a test can read the output.
    struct SharedBuf(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for SharedBuf {
        fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(data);
            Ok(data.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// Builds a session whose SVN checkout is guaranteed to fail **offline**:
    /// `svn_path` points at a bogus `file://` repo and `template_dir` at a
    /// non-existent temp dir, so a missing template triggers `svn co file://…`
    /// which fails instantly with no network — keeping the update-load test
    /// hermetic and fast (the `AGENTS.md` "unit tests run offline" rule).
    fn session_with_buffer() -> (Session, Arc<Mutex<Vec<u8>>>) {
        let buf = Arc::new(Mutex::new(Vec::new()));
        let display = CommandPromptDisplay::with_sink(
            Box::new(SharedBuf(Arc::clone(&buf))),
            ColorMode::Never,
        );
        let mut config = Config::default();
        config.svn_path = "file:///nonexistent/mtui-p67-offline-repo".to_owned();
        config.template_dir =
            std::env::temp_dir().join("mtui-p67-empty-template-dir-that-does-not-exist");
        (Session::with_display(config, true, display), buf)
    }

    /// With no `-a/-k` and no `--sut`, seeding is a no-op that continues to the
    /// REPL.
    #[tokio::test]
    async fn no_update_no_sut_continues() {
        let registry = register_all();
        let (mut session, _buf) = session_with_buffer();
        let flow = seed_session(&registry, &mut session, &args()).await;
        assert_eq!(flow, ControlFlow::Continue(()));
    }

    /// An explicitly requested update that cannot be loaded (offline `svn`, no
    /// on-disk template) yields a null report → exit 1, not an empty REPL.
    #[tokio::test]
    async fn explicit_update_that_fails_to_load_exits_one() {
        let registry = register_all();
        let (mut session, _buf) = session_with_buffer();
        let mut a = args();
        a.auto_review_id = Some("SUSE:Maintenance:99999:99999".parse().unwrap());
        let flow = seed_session(&registry, &mut session, &a).await;
        assert_eq!(flow, ControlFlow::Break(1));
    }

    /// A `--sut` host that cannot connect is logged and skipped; seeding still
    /// continues to the REPL (best-effort, one bad host never aborts startup).
    #[tokio::test]
    async fn sut_host_failure_is_skipped_and_continues() {
        let registry = register_all();
        let (mut session, _buf) = session_with_buffer();
        let mut a = args();
        a.sut = vec!["unreachable.invalid".parse().unwrap()];
        let flow = seed_session(&registry, &mut session, &a).await;
        assert_eq!(flow, ControlFlow::Continue(()));
        // The unreachable host could not connect, so it was not added.
        assert!(
            !session
                .targets()
                .names()
                .contains(&"unreachable.invalid".to_owned())
        );
    }
}
