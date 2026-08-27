//! Startup seeding: the pre-REPL work the `mtui` binary does before entering the
//! interactive loop.
//!
//! First an `-a`/`-k` update via [`Session::load_update`] — an explicitly
//! requested one resolving to a null report exits `1` rather than dropping into
//! an empty session — then the `--sut` hosts through the shared engine's
//! `add_host`, best-effort.
//!
//! This module only *seeds*; the binary always enters the REPL afterwards, since
//! mtui has no single-command mode. [`seed_session`] is the testable seam: it
//! returns [`ControlFlow`] so `main` maps a fatal outcome to a process exit and
//! this module never calls [`std::process::exit`] itself.

use std::ops::ControlFlow;

use mtui_core::{Args, ExitStatus, Registry, Session, dispatch_line};
use mtui_testreport::UpdateKind;
use mtui_types::Workflow;

/// Seeds `session` from the top-level `args` before the REPL starts.
///
/// `args.update()` (`-a`/`-k`) loads via [`Session::load_update`], autoconnecting
/// iff no `--sut` override was given; an empty RRID back means a null report for
/// an explicitly requested update, so it breaks with [`ExitStatus::Failure`]
/// rather than entering an empty REPL. Each `--sut` entry then dispatches
/// `add_host <fragment>`, logging any failure and continuing.
///
/// Returns [`ControlFlow::Continue`] when the session is ready for the REPL, or
/// [`ControlFlow::Break`] with the [`ExitStatus`] to exit with instead.
pub async fn seed_session(
    registry: &Registry,
    session: &mut Session,
    args: &Args,
) -> ControlFlow<ExitStatus> {
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
            // The load path already logged "does not exist".
            tracing::error!(update = %update.id, "requested update could not be loaded");
            return ControlFlow::Break(ExitStatus::Failure);
        }
    }

    for sut in &args.sut {
        let line = format!("add_host {}", sut.print_args());
        if let Err(err) = dispatch_line(registry, session, &line).await {
            // One malformed `--sut` must not block the REPL.
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

    /// A session whose SVN checkout fails instantly **offline**: `svn_path` is a
    /// bogus `file://` repo and `template_dir` does not exist, so the missing
    /// template triggers an `svn co` that fails with no network.
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

    /// An unloadable explicit update yields a null report and exit 1, not an
    /// empty REPL.
    #[tokio::test]
    async fn explicit_update_that_fails_to_load_exits_one() {
        let registry = register_all();
        let (mut session, _buf) = session_with_buffer();
        let mut a = args();
        a.auto_review_id = Some("SUSE:Maintenance:99999:99999".parse().unwrap());
        let flow = seed_session(&registry, &mut session, &a).await;
        assert_eq!(flow, ControlFlow::Break(ExitStatus::Failure));
    }

    /// A `--sut` host that cannot connect is logged and skipped; seeding still
    /// continues to the REPL.
    #[tokio::test]
    async fn sut_host_failure_is_skipped_and_continues() {
        let registry = register_all();
        let (mut session, _buf) = session_with_buffer();
        let mut a = args();
        a.sut = vec!["unreachable.invalid".parse().unwrap()];
        let flow = seed_session(&registry, &mut session, &a).await;
        assert_eq!(flow, ControlFlow::Continue(()));
        assert!(
            !session
                .targets()
                .names()
                .contains(&"unreachable.invalid".to_owned())
        );
    }
}
