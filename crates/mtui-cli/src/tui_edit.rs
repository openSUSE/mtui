//! The report-document editor's terminal handover.
//!
//! The form model and screen live in `mtui-tui`; this module owns the real
//! terminal around them (raw mode, the alternate screen, the event loop) and
//! applies a saved document to the session.

use std::io::{self, IsTerminal};
use std::panic;
use std::sync::Arc;

use anyhow::{Context, anyhow, bail};
use crossterm::cursor::{Hide, Show};
use crossterm::event::{self, Event};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use mtui_core::{ReportAccess, Session};
use mtui_tui::{App, Exit, Form, Schema};
use ratatui::Terminal;
use ratatui::backend::{Backend, CrosstermBackend};

/// Where the editor's events come from; `None` means the input ended.
pub(crate) trait EventSource {
    fn next_event(&mut self) -> io::Result<Option<Event>>;
}

struct TerminalEvents;

impl EventSource for TerminalEvents {
    fn next_event(&mut self) -> io::Result<Option<Event>> {
        event::read().map(Some)
    }
}

type PanicHook = Arc<dyn Fn(&panic::PanicHookInfo<'_>) + Send + Sync + 'static>;

/// Raw mode, the alternate screen and a hidden cursor, undone on drop and by a
/// panic hook chained in front of the previous one.
struct TerminalGuard {
    previous: PanicHook,
}

fn restore_terminal() {
    let _ = disable_raw_mode();
    let _ = execute!(io::stdout(), Show, LeaveAlternateScreen);
}

impl TerminalGuard {
    fn enter() -> io::Result<Self> {
        enable_raw_mode()?;
        if let Err(err) = execute!(io::stdout(), EnterAlternateScreen, Hide) {
            restore_terminal();
            return Err(err);
        }
        let previous: PanicHook = Arc::from(panic::take_hook());
        let chained = Arc::clone(&previous);
        panic::set_hook(Box::new(move |info| {
            restore_terminal();
            chained(info);
        }));
        Ok(Self { previous })
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        restore_terminal();
        // The hook cannot be swapped from a panicking thread; the chained one
        // then stays, and restoring twice is harmless.
        if !std::thread::panicking() {
            let _ = panic::take_hook();
            let previous = Arc::clone(&self.previous);
            panic::set_hook(Box::new(move |info| previous(info)));
        }
    }
}

/// Refuses to start without a terminal on both ends.
pub(crate) fn require_tty(stdin_is_tty: bool, stdout_is_tty: bool) -> anyhow::Result<()> {
    if stdin_is_tty && stdout_is_tty {
        Ok(())
    } else {
        bail!("the document editor needs an interactive terminal on stdin and stdout")
    }
}

/// Opens the editor on the active report's document.
///
/// # Errors
///
/// Fails without a terminal, without a loaded document, or when the terminal
/// cannot be driven.
pub(crate) fn edit_document(session: &mut Session) -> anyhow::Result<()> {
    require_tty(io::stdin().is_terminal(), io::stdout().is_terminal())?;
    let (rrid, mut app) = open(session)?;
    let exit = run_terminal(&mut app)?;
    apply(session, &rrid, exit)
}

/// Runs `app` on the real terminal; the screen is restored before this returns.
fn run_terminal(app: &mut App) -> anyhow::Result<Exit> {
    let _guard = TerminalGuard::enter().context("entering the editor screen")?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))
        .map_err(|e| anyhow!("starting the editor screen: {e}"))?;
    drive(&mut terminal, app, &mut TerminalEvents)
}

fn open(session: &Session) -> anyhow::Result<(String, App)> {
    let rrid = session
        .templates
        .active_rrid()
        .map(str::to_owned)
        .context("no report is loaded")?;
    let document = session
        .with_report(&rrid, |report| report.base().document.clone())
        .map_err(|access| access_error(access, &rrid))?
        .context("the loaded report has no document")?;
    let schema = Schema::load().context("loading the report schema")?;
    Ok((rrid, App::new(Form::new(&schema, &document))))
}

fn access_error(access: ReportAccess, rrid: &str) -> anyhow::Error {
    match access {
        ReportAccess::NotLoaded => anyhow!("template not loaded: {rrid}"),
        ReportAccess::Busy => anyhow!("template busy: {rrid}"),
    }
}

/// Runs the editor until it exits. A resize needs no handling of its own: every
/// event redraws, and the draw re-reads the terminal size.
pub(crate) fn drive<B: Backend>(
    terminal: &mut Terminal<B>,
    app: &mut App,
    events: &mut dyn EventSource,
) -> anyhow::Result<Exit> {
    loop {
        terminal
            .draw(|frame| app.draw(frame))
            .map_err(|e| anyhow!("drawing the editor: {e}"))?;
        match events.next_event().context("reading the keyboard")? {
            Some(Event::Key(key)) => {
                if let Some(exit) = app.handle(key) {
                    return Ok(exit);
                }
            }
            Some(_) => {}
            None => return Ok(Exit::Discarded),
        }
    }
}

/// Puts a saved document on the report and marks it authored; a discard, or a
/// save that changed nothing, leaves the report as it was.
pub(crate) fn apply(session: &mut Session, rrid: &str, exit: Exit) -> anyhow::Result<()> {
    let Exit::Saved(saved) = exit else {
        return Ok(());
    };
    if saved.touched.is_empty() {
        return Ok(());
    }
    let mtui_tui::Saved { document, touched } = *saved;
    session
        .with_report_mut(rrid, |report| {
            let base = report.base_mut();
            base.document = Some(document);
            let touched: Vec<&str> = touched.iter().map(String::as_str).collect();
            base.mark_document_authored(&touched);
        })
        .map_err(|access| access_error(access, rrid))?;
    let sections: Vec<&str> = touched.iter().map(|p| p.trim_start_matches('/')).collect();
    session.display.println(&format!(
        "document: {} edited (uncommitted)",
        sections.join(", ")
    ));
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::Mutex;

    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use mtui_config::Config;
    use mtui_core::{ColorMode, CommandPromptDisplay};
    use mtui_testreport::{ObsReport, TestReport};
    use mtui_types::RequestReviewID;
    use mtui_types::report_document::{ReportDocument, Verdict};
    use ratatui::backend::TestBackend;

    use super::*;

    const RRID: &str = "SUSE:Maintenance:1:1";
    const DOCUMENT: &str =
        include_str!("../../mtui-types/tests/fixtures/document/maintenance_openqa_l3.json");

    struct Script(VecDeque<Event>);

    impl Script {
        fn keys(keys: impl IntoIterator<Item = KeyEvent>) -> Self {
            Self(keys.into_iter().map(Event::Key).collect())
        }
    }

    impl EventSource for Script {
        fn next_event(&mut self) -> io::Result<Option<Event>> {
            Ok(self.0.pop_front())
        }
    }

    struct SharedBuf(Arc<Mutex<Vec<u8>>>);

    impl io::Write for SharedBuf {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ctrl_s() -> KeyEvent {
        KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL)
    }

    /// A session holding one active report, with `document` when given, and
    /// its captured display output.
    fn session(document: Option<&str>) -> (Session, Arc<Mutex<Vec<u8>>>) {
        let out = Arc::new(Mutex::new(Vec::new()));
        let display = CommandPromptDisplay::with_sink(
            Box::new(SharedBuf(Arc::clone(&out))),
            ColorMode::Never,
        );
        let mut session = Session::with_display(Config::default(), true, display);
        let mut report = ObsReport::new(session.config.clone());
        report.base_mut().rrid = Some(RequestReviewID::parse(RRID).unwrap());
        report.base_mut().document = document.map(|raw| raw.parse::<ReportDocument>().unwrap());
        session.templates.add(Box::new(report));
        session.templates.set_active(RRID);
        (session, out)
    }

    fn run(session: &mut Session, script: Script) -> Exit {
        let (_, mut app) = open(session).unwrap();
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        drive(&mut terminal, &mut app, &mut { script }).unwrap()
    }

    fn state(session: &Session) -> (Option<Verdict>, bool) {
        session
            .with_report(RRID, |r| {
                let base = r.base();
                (
                    base.document.as_ref().and_then(|d| *d.verdict),
                    base.document_dirty,
                )
            })
            .unwrap()
    }

    fn printed(out: &Arc<Mutex<Vec<u8>>>) -> String {
        String::from_utf8(out.lock().unwrap().clone()).unwrap()
    }

    #[test]
    fn a_saved_edit_replaces_the_document_and_marks_it_dirty() {
        let (mut session, out) = session(Some(DOCUMENT));
        assert_eq!(state(&session), (None, false));

        let exit = run(&mut session, Script::keys([key(KeyCode::Right), ctrl_s()]));
        apply(&mut session, RRID, exit).unwrap();

        assert_eq!(state(&session), (Some(Verdict::Passed), true));
        assert_eq!(printed(&out), "document: verdict edited (uncommitted)\n");
    }

    #[test]
    fn a_discard_leaves_the_document_and_the_flag_alone() {
        let (mut session, out) = session(Some(DOCUMENT));

        let exit = run(
            &mut session,
            Script::keys([
                key(KeyCode::Right),
                key(KeyCode::Char('q')),
                key(KeyCode::Char('y')),
            ]),
        );
        assert_eq!(exit, Exit::Discarded);
        apply(&mut session, RRID, exit).unwrap();

        assert_eq!(state(&session), (None, false));
        assert_eq!(printed(&out), "");
    }

    #[test]
    fn a_save_that_changed_nothing_is_not_an_edit() {
        let (mut session, out) = session(Some(DOCUMENT));

        let exit = run(&mut session, Script::keys([ctrl_s()]));
        apply(&mut session, RRID, exit).unwrap();

        assert_eq!(state(&session), (None, false));
        assert_eq!(printed(&out), "");
    }

    #[test]
    fn the_message_names_every_touched_section() {
        let (mut session, out) = session(Some(DOCUMENT));

        let exit = run(
            &mut session,
            Script::keys([
                key(KeyCode::Right),
                key(KeyCode::Down),
                key(KeyCode::Enter),
                key(KeyCode::Char('x')),
                ctrl_s(),
                ctrl_s(),
            ]),
        );
        apply(&mut session, RRID, exit).unwrap();

        assert_eq!(
            printed(&out),
            "document: verdict, comment edited (uncommitted)\n"
        );
    }

    #[test]
    fn an_exhausted_input_ends_the_editor_as_a_discard() {
        let (mut session, _) = session(Some(DOCUMENT));

        assert_eq!(run(&mut session, Script::keys([])), Exit::Discarded);
    }

    #[test]
    fn events_that_are_not_keys_only_redraw() {
        let (mut session, _) = session(Some(DOCUMENT));
        let script = Script(VecDeque::from([
            Event::Resize(80, 24),
            Event::FocusGained,
            Event::Key(key(KeyCode::Char('q'))),
        ]));

        assert_eq!(run(&mut session, script), Exit::Discarded);
    }

    #[test]
    fn a_report_without_a_document_cannot_be_opened() {
        let (session, _) = session(None);

        let err = open(&session).err().expect("refused");

        assert!(err.to_string().contains("no document"), "{err}");
    }

    #[test]
    fn nothing_loaded_cannot_be_opened() {
        let display = CommandPromptDisplay::with_sink(Box::new(io::sink()), ColorMode::Never);
        let session = Session::with_display(Config::default(), true, display);

        let err = open(&session).err().expect("refused");

        assert!(err.to_string().contains("no report is loaded"), "{err}");
    }

    #[test]
    fn a_missing_terminal_on_either_end_is_refused() {
        assert!(require_tty(true, true).is_ok());
        for (stdin, stdout) in [(false, true), (true, false), (false, false)] {
            let err = require_tty(stdin, stdout).unwrap_err();
            assert!(err.to_string().contains("interactive terminal"), "{err}");
        }
    }
}
