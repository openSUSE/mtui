//! Formatted command output (`CommandPromptDisplay`) + color mode + pager.
//!
//! Port of upstream `mtui.cli.display.CommandPromptDisplay`, its color helpers
//! (`mtui.cli.colors`), and the pager (`mtui.cli.term.page`). The full `list_*`
//! family (bugs, history, host status, locks, sessions, timeout, versions,
//! products, update repos) plus [`show_log`](CommandPromptDisplay::show_log) is
//! ported here so the Phase-5 command bodies have their output seam.
//!
//! Output is captured through a boxed [`std::io::Write`] sink so tests can
//! snapshot it and the REPL/MCP can point it at stdout or a buffer.
//!
//! **Color** is a three-way [`ColorMode`] (`Auto`/`Always`/`Never`) resolved at
//! call time via [`ColorMode::resolve`], honouring the same precedence as
//! upstream `mtui.cli.colors.mode`: `Always` → `Never` → `NO_COLOR` →
//! `COLOR=never|always` → `stderr.is_terminal()`.
//!
//! **Deviation from upstream:** [`list_history`](CommandPromptDisplay::list_history)
//! formats timestamps in **UTC** rather than local time. Upstream uses
//! `datetime.fromtimestamp` (local), but that requires chrono's `clock` feature
//! (pulling `iana-time-zone`, against the no-runtime-deps goal) and makes
//! snapshot output timezone-dependent. UTC keeps the crate std-only and tests
//! deterministic; the upstream test only asserts substrings, not the exact date.

use std::io::{IsTerminal, Write};

use chrono::DateTime;
use mtui_types::{ExecutionMode, RPMVersion, System, SystemProduct, TargetState};
use owo_colors::OwoColorize;

/// One host and its resolved [`System`], as passed to
/// [`list_versions`](CommandPromptDisplay::list_versions).
pub(crate) type HostSystem = (String, System);

/// A package name paired with its observed versions (newest-first is applied by
/// the display), as passed to [`list_versions`](CommandPromptDisplay::list_versions).
pub(crate) type PackageVersions = (String, Vec<RPMVersion>);

/// A version-history group: the hosts it covers and their package versions.
pub(crate) type VersionGroup = (Vec<HostSystem>, Vec<PackageVersions>);

/// Already-resolved lock state for a host, as displayed by
/// [`list_locks`](CommandPromptDisplay::list_locks).
///
/// The upstream lock accessors are async `&mut self` in `mtui-hosts`; callers do
/// that I/O and hand the resolved values here so display stays sync and
/// snapshot-testable.
#[derive(Debug, Clone, Default)]
pub struct LockStatus {
    /// Whether the host is currently locked.
    pub(crate) is_locked: bool,
    /// Whether the lock belongs to the current user (renders as "me").
    pub(crate) is_mine: bool,
    /// The lock owner (ignored when `is_mine`).
    pub(crate) locked_by: String,
    /// Human-readable lock timestamp.
    pub(crate) time: String,
    /// Optional lock comment.
    pub(crate) comment: String,
}

/// Whether ANSI color escapes are emitted.
///
/// Mirrors upstream `mtui.cli.colors.mode.ColorMode` (`"auto" | "always" |
/// "never"`). The active decision is made by [`resolve`](ColorMode::resolve).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ColorMode {
    /// Emit color iff `stderr` is a TTY, unless overridden by `NO_COLOR` /
    /// `COLOR`. Upstream default.
    Auto,
    /// Always emit color escapes.
    Always,
    /// Never emit color escapes (plain text). The safe default for non-TTY
    /// sinks (buffers, MCP, redirected stdout); keeps snapshot tests stable.
    #[default]
    Never,
}

impl ColorMode {
    /// Resolves whether color escapes should be emitted right now.
    ///
    /// Precedence (highest first), mirroring upstream `colors_enabled`:
    /// 1. `Always` → `true`
    /// 2. `Never` → `false`
    /// 3. `NO_COLOR` set (any non-empty value) → `false` (per no-color.org)
    /// 4. `COLOR=never` → `false` (legacy mtui knob)
    /// 5. `COLOR=always` → `true` (legacy mtui knob)
    /// 6. `Auto` → `stderr.is_terminal()`
    #[must_use]
    pub fn resolve(self) -> bool {
        match self {
            Self::Always => true,
            Self::Never => false,
            Self::Auto => Self::resolve_auto(
                std::env::var_os("NO_COLOR").is_some_and(|v| !v.is_empty()),
                std::env::var("COLOR").ok().as_deref(),
                std::io::stderr().is_terminal(),
            ),
        }
    }

    /// Pure decision core for `Auto`, split out so the env/TTY precedence is
    /// unit-testable without touching process-global state.
    #[must_use]
    fn resolve_auto(no_color: bool, color: Option<&str>, is_tty: bool) -> bool {
        if no_color {
            return false;
        }
        match color {
            Some("never") => false,
            Some("always") => true,
            _ => is_tty,
        }
    }
}

/// Handles the display of formatted output in the command prompt.
///
/// Owns its output sink; construct with [`with_sink`](Self::with_sink) for tests
/// (a `Vec<u8>` buffer) or [`stdout`](Self::stdout) for the interactive REPL.
pub struct CommandPromptDisplay {
    output: Box<dyn Write + Send>,
    color: ColorMode,
}

impl CommandPromptDisplay {
    /// Builds a display over an arbitrary sink with an explicit color mode.
    #[must_use]
    pub fn with_sink(output: Box<dyn Write + Send>, color: ColorMode) -> Self {
        Self { output, color }
    }

    /// Builds a display writing to stdout.
    ///
    /// Color defaults to [`ColorMode::Never`]; the REPL flips it to the resolved
    /// [`ColorMode`] per the `--color` flag via [`set_color`](Self::set_color)
    /// right after building the session (`mtui-cli::main`).
    #[must_use]
    pub(crate) fn stdout() -> Self {
        Self {
            output: Box::new(std::io::stdout()),
            color: ColorMode::Never,
        }
    }

    /// The active color mode.
    #[must_use]
    pub const fn color(&self) -> ColorMode {
        self.color
    }

    /// Sets the color mode (e.g. the REPL enabling color per `--color`).
    pub fn set_color(&mut self, color: ColorMode) {
        self.color = color;
    }

    /// Writes `msg` followed by a newline to the output sink.
    ///
    /// Mirrors upstream `println(msg, eol="\n")`. Write errors are swallowed to
    /// match the Python surface, which never surfaces stdout write failures from
    /// display helpers.
    ///
    /// The write holds a [`mtui_hosts::suspend`] guard so a live TTY spinner
    /// erases its current frame first and the output lands on a clean line
    /// (upstream's `SpinnerAwareStreamHandler`). A strict no-op beyond taking the
    /// paint lock when no spinner is active (off a TTY, tests), so buffered /
    /// snapshot output is unaffected.
    pub fn println(&mut self, msg: &str) {
        let _quiet = mtui_hosts::suspend();
        let _ = writeln!(self.output, "{msg}");
    }

    /// Writes `msg` followed by an explicit end-of-line string.
    ///
    /// Suspends any live spinner for the write, like [`println`](Self::println).
    pub(crate) fn print_eol(&mut self, msg: &str, eol: &str) {
        let _quiet = mtui_hosts::suspend();
        let _ = write!(self.output, "{msg}{eol}");
    }

    /// Prints a per-template banner used to label fan-out output.
    ///
    /// Printed before each template's output block when a command fans out
    /// across more than one loaded template, so the user can tell which template
    /// produced which result. Upstream renders exactly `=== {rrid} ===`.
    pub(crate) fn template_banner(&mut self, rrid: &str) {
        self.println(&format!("=== {rrid} ==="));
    }

    /// Wraps `text` in green when color resolves on, else returns it unchanged.
    #[must_use]
    pub(crate) fn green(&self, text: &str) -> String {
        if self.color.resolve() {
            OwoColorize::green(&text).to_string()
        } else {
            text.to_owned()
        }
    }

    /// Wraps `text` in red when color resolves on, else returns it unchanged.
    #[must_use]
    pub fn red(&self, text: &str) -> String {
        if self.color.resolve() {
            OwoColorize::red(&text).to_string()
        } else {
            text.to_owned()
        }
    }

    /// Wraps `text` in yellow when color resolves on, else returns it unchanged.
    #[must_use]
    pub(crate) fn yellow(&self, text: &str) -> String {
        if self.color.resolve() {
            OwoColorize::yellow(&text).to_string()
        } else {
            text.to_owned()
        }
    }

    /// Wraps `text` in blue when color resolves on, else returns it unchanged.
    #[must_use]
    pub(crate) fn blue(&self, text: &str) -> String {
        if self.color.resolve() {
            OwoColorize::blue(&text).to_string()
        } else {
            text.to_owned()
        }
    }

    /// Displays a list of bugs and Jira issues.
    ///
    /// Mirrors upstream `list_bugs`: sorted ids, the `[""]` empty-sentinel
    /// ("No bugs…"/"No Jira issues…"), the `Buglist:` query URL, and per-item
    /// `Bug #{id}: {summary}` / `Jira #{id}: {summary}` blocks with tracker URLs.
    pub(crate) fn list_bugs(
        &mut self,
        bugs: &std::collections::BTreeMap<String, String>,
        jira: &std::collections::BTreeMap<String, String>,
        url: &str,
    ) {
        let ids: Vec<&String> = bugs.keys().collect();
        if ids.len() == 1 && ids[0].is_empty() {
            self.println("No bugs associated with Release Request.");
        } else {
            let joined: Vec<&str> = ids.iter().map(|s| s.as_str()).collect();
            self.println(&format!(
                "Buglist: {url}/buglist.cgi?bug_id={}",
                joined.join(",")
            ));
            for bug in &ids {
                let summary = &bugs[*bug];
                self.println("");
                self.println(&format!("Bug #{bug:5}: {summary}"));
                self.println(&format!("{url}/show_bug.cgi?id={bug}"));
            }
        }

        let jids: Vec<&String> = jira.keys().collect();
        if jids.is_empty() || (jids.len() == 1 && jids[0].is_empty()) {
            self.println("");
            self.println("No Jira issues associated with Release Request.");
        } else {
            for issue in &jids {
                let summary = &jira[*issue];
                self.println("");
                self.println(&format!("Jira #{issue:5}: {summary}"));
                self.println(&format!("https://jira.suse.com/browse/{issue}"));
            }
        }
    }

    /// Displays the command history for a host.
    ///
    /// Mirrors upstream `list_history`: reverses `lines`, splits each on the
    /// first two colons (`when:who:event`, colons preserved in `event`), skips
    /// malformed lines, and formats `when` (epoch seconds) as
    /// `%A, %d.%m.%Y %H:%M`. See the module doc for the UTC deviation.
    pub(crate) fn list_history(&mut self, hostname: &str, system: &System, lines: &[String]) {
        self.println(&format!("history from {hostname} ({system}):"));
        for line in lines.iter().rev() {
            let mut parts = line.splitn(3, ':');
            let (Some(when), Some(who), Some(event)) = (parts.next(), parts.next(), parts.next())
            else {
                continue;
            };
            let Ok(secs) = when.parse::<f64>() else {
                continue;
            };
            #[allow(clippy::cast_possible_truncation)]
            let Some(dt) = DateTime::from_timestamp(secs as i64, 0) else {
                continue;
            };
            self.println(&format!(
                "{}, {}: {}",
                dt.format("%A, %d.%m.%Y %H:%M"),
                who,
                event
            ));
        }
        self.println("");
    }

    /// Displays the status of a host.
    ///
    /// Mirrors upstream `list_host`: colored `state` label (green/red),
    /// `transactional`/`standard` label, and the fixed-width layout line.
    pub(crate) fn list_host(
        &mut self,
        hostname: &str,
        system: &System,
        transactional: bool,
        state: TargetState,
        mode: ExecutionMode,
    ) {
        let state_label = match state {
            TargetState::Enabled => Self::green(self, "Enabled"),
            TargetState::Disabled => Self::red(self, "Disabled"),
        };
        let trn = if transactional {
            Self::red(self, "transactional")
        } else {
            Self::green(self, "standard     ")
        };
        let sys = system.to_string();
        self.println(&format!(
            "{hostname:<20} ({sys:<28}): {state_label:<8} - {trn:<15} - ({mode})"
        ));
    }

    /// Displays the lock status of a host.
    ///
    /// Mirrors upstream `list_locks`, taking an already-resolved [`LockStatus`]
    /// (the upstream lock accessors are async `&mut self` in `mtui-hosts`;
    /// callers do the I/O and pass the resolved values in). When
    /// [`LockStatus::is_mine`] is set, "me" is shown in place of `locked_by`.
    pub(crate) fn list_locks(&mut self, hostname: &str, system: &System, lock: &LockStatus) {
        let sys = system.to_string();
        if lock.is_locked {
            let by = if lock.is_mine { "me" } else { &lock.locked_by };
            let since = Self::yellow(self, &format!("since {} by {by}", lock.time));
            self.print_eol(&format!("{hostname:20} {sys:20}: {since}"), "");
            if lock.comment.is_empty() {
                self.println("");
            } else {
                self.println(&format!(" : {}", lock.comment));
            }
        } else {
            let not_locked = Self::green(self, "not locked");
            self.println(&format!("{hostname:20} {sys:20}: {not_locked}"));
        }
    }

    /// Displays the active sessions on a host.
    ///
    /// Mirrors upstream `list_sessions`.
    pub(crate) fn list_sessions(&mut self, hostname: &str, system: &System, stdout: &str) {
        self.println(&format!("sessions on {hostname} ({system}):"));
        self.println(stdout);
    }

    /// Displays the command timeout for a host.
    ///
    /// Mirrors upstream `list_timeout`.
    pub(crate) fn list_timeout(&mut self, hostname: &str, system: &System, timeout: u64) {
        let sys = format!("({system})");
        self.println(&format!("{hostname:20} {sys:20}: {timeout}s"));
    }

    /// Displays the version history of packages.
    ///
    /// Mirrors upstream `list_versions`. `hosts_pvs` maps a group of hostnames
    /// (with their systems) to `(package, versions)` pairs; when more than one
    /// group is present, each is prefixed with a "version history from:" header.
    /// Versions are shown newest-first as an indented ladder.
    pub(crate) fn list_versions(&mut self, hosts_pvs: &[VersionGroup]) {
        let multi = hosts_pvs.len() > 1;
        for (hosts, pvs) in hosts_pvs {
            if multi {
                self.println("version history from:");
                for (hn, sys) in hosts {
                    self.println(&format!("  {hn} ({sys})"));
                }
                self.println("");
            }
            for (pkg, vers) in pvs {
                self.println(&format!("{pkg}:"));
                let mut sorted = vers.clone();
                sorted.sort_by(|a, b| b.cmp(a));
                for (indent, ver) in sorted.iter().enumerate() {
                    self.println(&format!("{}-> {ver}", "  ".repeat(indent)));
                }
                self.println("");
            }
        }
    }

    /// Displays the products of a reference host.
    ///
    /// Mirrors upstream `list_products`. Note: upstream literally prints
    /// "Referenece host" (sic) — preserved for byte-parity with existing tools.
    pub(crate) fn list_products(&mut self, hostname: &str, system: &System) {
        // sic: upstream typo "Referenece", kept for output parity.
        let label = Self::green(self, "Referenece host");
        let host = Self::yellow(self, hostname);
        self.println(&format!("{label}: {host}"));
        for x in system.pretty() {
            self.println(&x);
        }
        self.println("");
    }

    /// Displays the update repositories.
    ///
    /// Mirrors upstream `list_update_repos`. `repos` pairs a product with its
    /// repo URL/path string.
    pub(crate) fn list_update_repos(&mut self, repos: &[(SystemProduct, String)]) {
        for (p, r) in repos {
            let product = Self::green(self, "Product");
            let pname = Self::yellow(self, &p.name);
            let ver_l = Self::green(self, "version");
            let pver = Self::yellow(self, &p.version);
            let arch_l = Self::green(self, "arch");
            let parch = Self::yellow(self, &p.arch);
            self.println(&format!(
                "{product}: {pname} - {ver_l}: {pver} - {arch_l}: {parch}"
            ));
            self.println(&format!("    {r}"));
        }
    }

    /// Displays the command log for a host through an arbitrary `sink`.
    ///
    /// Mirrors upstream `show_log` (a `@staticmethod`). Each log entry is
    /// `(cmdline, stdout, stderr, exitcode)`; the sink is called once per output
    /// line (it appends its own newline, matching the upstream `Callable`).
    pub(crate) fn show_log(
        hostname: &str,
        hostlog: &[(String, String, String, i32)],
        sink: &mut dyn FnMut(&str),
    ) {
        sink(&format!("log from {hostname}:"));
        for (cmdline, stdout, stderr, exitcode) in hostlog {
            sink(&format!("{hostname}:~> {cmdline} [{exitcode}]"));
            sink("stdout:");
            for line in stdout.split('\n') {
                sink(line);
            }
            sink("stderr:");
            for line in stderr.split('\n') {
                sink(line);
            }
        }
    }
}

impl Default for CommandPromptDisplay {
    fn default() -> Self {
        Self::stdout()
    }
}

/// Displays long text, non-interactively.
///
/// Port of upstream `mtui.cli.term.page`'s non-interactive contract:
/// * `interactive == false` and `writer` is `None` → no-op (historical
///   behaviour: no output, no error).
/// * `interactive == false` and `writer` is `Some` → each line is forwarded to
///   the writer with trailing `\r`/`\n` stripped (the MCP / headless path).
///
/// `interactive == true` is **not** handled here — the REPL uses the async
/// [`page_interactive`] driver instead, which reads Enter/`q` through the
/// session's [`Prompter`](mtui_hosts::Prompter). For safety, if a caller still
/// passes `interactive == true` with a `writer`, this forwards every line
/// unpaged (never blocks on a read it cannot perform); with no `writer` it is a
/// no-op.
pub(crate) fn page(text: &[String], interactive: bool, writer: Option<&mut dyn FnMut(&str)>) {
    let _ = interactive;
    if let Some(w) = writer {
        for line in text {
            w(line.trim_end_matches(['\r', '\n']));
        }
    }
}

/// Removes the ANSI escape codes upstream `mtui.cli.term.filter_ansi` strips.
///
/// Verbatim port of the upstream substitutions: a bare `ESC` (`\x1b`), the
/// `[<params>m` / `[<params>A` SGR/cursor sequences, and the `[K` erase-line
/// sequence. Applied to each line before it is width-wrapped in the interactive
/// pager so escape bytes do not inflate the visible column count.
#[must_use]
fn filter_ansi(text: &str) -> String {
    use std::sync::OnceLock;

    use regex::Regex;

    static SGR: OnceLock<Regex> = OnceLock::new();
    static ERASE: OnceLock<Regex> = OnceLock::new();
    let sgr = SGR.get_or_init(|| Regex::new(r"\[[0-9;]*[mA]").unwrap());
    let erase = ERASE.get_or_init(|| Regex::new(r"\[K").unwrap());

    let no_esc = text.replace('\u{1b}', "");
    let no_sgr = sgr.replace_all(&no_esc, "");
    erase.replace_all(&no_sgr, "").into_owned()
}

/// Returns the terminal size as `(cols, rows)`.
///
/// Port of upstream `mtui.cli.term.termsize`: reads `TIOCGWINSZ` via `ioctl`,
/// falling back to the `ACCTEST_COLS`/`ACCTEST_ROWS` environment pair (used by
/// the acceptance harness and by unit tests, which have no controlling TTY).
///
/// The tuple order is **`(cols, rows)`** on every path — upstream once had a
/// transpose bug on the env fallback that swapped the geometry, so the order is
/// pinned by test.
// `ioctl(TIOCGWINSZ)` is the only portable way to read the tty geometry, mirroring
// upstream `mtui.cli.term.termsize`; the block below is the sole `unsafe` use.
#[allow(unsafe_code)]
#[must_use]
fn termsize() -> (usize, usize) {
    #[cfg(unix)]
    {
        // SAFETY: `winsize` is POD; `ioctl(TIOCGWINSZ)` fills it or fails, in
        // which case we fall through to the env/default path without reading it.
        unsafe {
            let mut ws: libc::winsize = std::mem::zeroed();
            if libc::ioctl(libc::STDIN_FILENO, libc::TIOCGWINSZ, &raw mut ws) == 0
                && ws.ws_col > 0
                && ws.ws_row > 0
            {
                return (ws.ws_col as usize, ws.ws_row as usize);
            }
        }
    }
    termsize_from_env().unwrap_or((80, 24))
}

/// Pure `ACCTEST_COLS`/`ACCTEST_ROWS` fallback, split out so the `(cols, rows)`
/// ordering is unit-testable without a TTY. Returns `None` unless both parse.
#[must_use]
fn termsize_from_env() -> Option<(usize, usize)> {
    let cols = std::env::var("ACCTEST_COLS").ok()?.parse().ok()?;
    let rows = std::env::var("ACCTEST_ROWS").ok()?.parse().ok()?;
    Some((cols, rows))
}

/// One screenful emitted by [`page_screen`], plus what remains.
#[derive(Debug, PartialEq, Eq)]
enum PageStep {
    /// A full screen was printed; these lines are still unshown.
    More(Vec<String>),
    /// All input was printed; nothing remains.
    Done,
}

/// Prints up to `height - 1` display rows (ANSI-filtered, wrapped to `width`
/// columns) from `text` via `emit`, and returns the unconsumed remainder.
///
/// Mirrors upstream `term.py`'s inner loop: each logical line is `filter_ansi`'d
/// then hard-wrapped into `width`-column chunks; an empty line still occupies one
/// row; the screen holds `height - 1` rows (one reserved for the prompt).
fn page_screen(
    text: &[String],
    width: usize,
    height: usize,
    emit: &mut dyn FnMut(&str),
) -> PageStep {
    let width = width.max(1);
    let mut rows_left = height.saturating_sub(1).max(1);
    let mut idx = 0;

    while idx < text.len() {
        let line = filter_ansi(text[idx].trim_end_matches(['\r', '\n']));
        // Wrap into width-column chunks by char boundary; an empty line is one row.
        let mut chunks: Vec<String> = Vec::new();
        let chars: Vec<char> = line.chars().collect();
        let mut c = 0;
        while c < chars.len() {
            let end = (c + width).min(chars.len());
            chunks.push(chars[c..end].iter().collect());
            c = end;
        }
        if chunks.is_empty() {
            chunks.push(String::new());
        }

        if chunks.len() > rows_left {
            // Print what fits; carry the rest of this same line into the remainder.
            for chunk in chunks.iter().take(rows_left) {
                emit(chunk);
            }
            let carried: String = chunks[rows_left..].concat();
            let mut rest = Vec::with_capacity(text.len() - idx);
            rest.push(carried);
            rest.extend_from_slice(&text[idx + 1..]);
            return PageStep::More(rest);
        }

        for chunk in &chunks {
            emit(chunk);
        }
        rows_left -= chunks.len();
        idx += 1;
        if rows_left == 0 {
            let rest = text[idx..].to_vec();
            return if rest.is_empty() {
                PageStep::Done
            } else {
                PageStep::More(rest)
            };
        }
    }
    PageStep::Done
}

/// Interactive TTY pager: prints `text` a screen at a time, blocking on
/// `Press Enter to continue... (q to quit)` between screens.
///
/// The async analogue of upstream `mtui.cli.term.page(..., interactive=True)`.
/// Reads the continue/quit answer through the session's serialised
/// [`Prompter`](mtui_hosts::Prompter) (so a live spinner is suspended and
/// concurrent host prompts stay serialised); typing `q` stops early. When no
/// prompter is available (should not happen in the REPL, but keeps the function
/// total) it prints everything without prompting.
pub(crate) async fn page_interactive(
    text: &[String],
    display: &mut CommandPromptDisplay,
    prompter: Option<&mtui_hosts::Prompter>,
) {
    let (width, height) = termsize();
    let mut remaining: Vec<String> = text.to_vec();

    loop {
        let mut batch: Vec<String> = Vec::new();
        let step = page_screen(&remaining, width, height, &mut |line| {
            batch.push(line.to_owned())
        });
        for line in &batch {
            display.println(line);
        }
        match step {
            PageStep::Done => return,
            PageStep::More(rest) => {
                remaining = rest;
                let Some(p) = prompter else {
                    // No TTY read available: dump the rest unpaged and stop.
                    for line in &remaining {
                        display.println(&filter_ansi(line.trim_end_matches(['\r', '\n'])));
                    }
                    return;
                };
                // `q` quits; Enter (or anything else) continues.
                let answer = p
                    .ask("Press Enter to continue... (q to quit)")
                    .await
                    .unwrap_or_default();
                if answer.trim().eq_ignore_ascii_case("q") {
                    return;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};
    use std::sync::{Arc, Mutex};

    use mtui_types::{ExecutionMode, RPMVersion, System, SystemProduct, TargetState};

    use super::*;

    /// A display over a shared buffer, returning the handle to inspect output.
    fn buffered(color: ColorMode) -> (CommandPromptDisplay, Arc<Mutex<Vec<u8>>>) {
        let buf = Arc::new(Mutex::new(Vec::new()));
        let sink = SharedSink(buf.clone());
        (CommandPromptDisplay::with_sink(Box::new(sink), color), buf)
    }

    struct SharedSink(Arc<Mutex<Vec<u8>>>);
    impl Write for SharedSink {
        fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(data);
            Ok(data.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn rendered(buf: &Arc<Mutex<Vec<u8>>>) -> String {
        String::from_utf8(buf.lock().unwrap().clone()).unwrap()
    }

    fn system(name: &str) -> System {
        System::new(
            SystemProduct::new(name, "15.5", "x86_64"),
            BTreeSet::new(),
            false,
        )
    }

    #[test]
    fn template_banner_matches_upstream() {
        let (mut d, buf) = buffered(ColorMode::Never);
        d.template_banner("SUSE:Maintenance:1:1");
        assert_eq!(rendered(&buf), "=== SUSE:Maintenance:1:1 ===\n");
    }

    #[test]
    fn println_appends_newline() {
        let (mut d, buf) = buffered(ColorMode::Never);
        d.println("hello");
        assert_eq!(rendered(&buf), "hello\n");
    }

    #[test]
    fn color_never_emits_no_escapes() {
        let d = CommandPromptDisplay::with_sink(Box::new(Vec::new()), ColorMode::Never);
        assert_eq!(d.green("ok"), "ok");
        assert!(!d.red("bad").contains('\u{1b}'));
    }

    #[test]
    fn color_always_emits_escapes() {
        let d = CommandPromptDisplay::with_sink(Box::new(Vec::new()), ColorMode::Always);
        assert!(d.green("ok").contains('\u{1b}'));
        assert!(d.red("bad").contains('\u{1b}'));
        assert!(d.yellow("warn").contains('\u{1b}'));
        assert!(d.blue("info").contains('\u{1b}'));
    }

    #[test]
    fn color_accessor_and_setter_roundtrip() {
        let mut d = CommandPromptDisplay::with_sink(Box::new(Vec::new()), ColorMode::Never);
        assert_eq!(d.color(), ColorMode::Never);
        d.set_color(ColorMode::Always);
        assert_eq!(d.color(), ColorMode::Always);
    }

    #[test]
    fn print_eol_uses_explicit_terminator() {
        let (mut d, buf) = buffered(ColorMode::Never);
        d.print_eol("a", "");
        d.print_eol("b", "|");
        assert_eq!(rendered(&buf), "ab|");
    }

    #[test]
    fn default_and_stdout_construct_without_panic() {
        let _ = CommandPromptDisplay::default();
        let d = CommandPromptDisplay::stdout();
        assert_eq!(d.color(), ColorMode::Never);
    }

    #[test]
    fn color_mode_default_is_never() {
        assert_eq!(ColorMode::default(), ColorMode::Never);
    }

    #[test]
    fn resolve_always_and_never_are_absolute() {
        assert!(ColorMode::Always.resolve());
        assert!(!ColorMode::Never.resolve());
    }

    #[test]
    fn resolve_auto_precedence_matrix() {
        // NO_COLOR wins over everything, even COLOR=always and a TTY.
        assert!(!ColorMode::resolve_auto(true, Some("always"), true));
        assert!(!ColorMode::resolve_auto(true, None, true));
        // COLOR=never / always override the TTY check.
        assert!(!ColorMode::resolve_auto(false, Some("never"), true));
        assert!(ColorMode::resolve_auto(false, Some("always"), false));
        // Fall through to the TTY check.
        assert!(ColorMode::resolve_auto(false, None, true));
        assert!(!ColorMode::resolve_auto(false, None, false));
        assert!(!ColorMode::resolve_auto(false, Some("bogus"), false));
    }

    #[test]
    fn list_bugs_populated_renders_ids_and_urls() {
        let (mut d, buf) = buffered(ColorMode::Never);
        let mut bugs = BTreeMap::new();
        bugs.insert("123".to_owned(), "Test bug".to_owned());
        let mut jira = BTreeMap::new();
        jira.insert("ABC-123".to_owned(), "Test Jira issue".to_owned());
        d.list_bugs(&bugs, &jira, "https://bugzilla.suse.com");
        let out = rendered(&buf);
        assert!(out.contains("Bug #123"));
        assert!(out.contains("Jira #ABC-123"));
        assert!(out.contains("https://bugzilla.suse.com/show_bug.cgi?id=123"));
        assert!(out.contains("https://jira.suse.com/browse/ABC-123"));
    }

    #[test]
    fn list_bugs_empty_sentinels() {
        let (mut d, buf) = buffered(ColorMode::Never);
        let mut bugs = BTreeMap::new();
        bugs.insert(String::new(), String::new());
        let jira = BTreeMap::new();
        d.list_bugs(&bugs, &jira, "https://bugzilla.suse.com");
        let out = rendered(&buf);
        assert!(out.contains("No bugs associated with Release Request."));
        assert!(out.contains("No Jira issues associated with Release Request."));
    }

    #[test]
    fn list_history_formats_and_skips_malformed() {
        let (mut d, buf) = buffered(ColorMode::Never);
        let lines = vec![
            "1678886400:user:test command".to_owned(),
            "malformed-line".to_owned(),
        ];
        d.list_history("test_host", &system("SLES"), &lines);
        let out = rendered(&buf);
        assert!(out.contains("history from test_host"));
        assert!(out.contains("test command"));
        assert!(out.contains("user"));
        // 1678886400 == 2023-03-15 13:20 UTC (Wednesday).
        assert!(out.contains("Wednesday, 15.03.2023 13:20"));
    }

    #[test]
    fn list_host_shows_state_label() {
        let (mut d, buf) = buffered(ColorMode::Never);
        d.list_host(
            "test_host",
            &system("SLES"),
            false,
            TargetState::Enabled,
            ExecutionMode::Parallel,
        );
        let out = rendered(&buf);
        assert!(out.contains("test_host"));
        assert!(out.contains("Enabled"));
        assert!(out.contains("standard"));
        assert!(out.contains("(parallel)"));
    }

    #[test]
    fn list_host_transactional_and_disabled() {
        let (mut d, buf) = buffered(ColorMode::Never);
        d.list_host(
            "h",
            &system("SLES"),
            true,
            TargetState::Disabled,
            ExecutionMode::Serial,
        );
        let out = rendered(&buf);
        assert!(out.contains("Disabled"));
        assert!(out.contains("transactional"));
        assert!(out.contains("(serial)"));
    }

    #[test]
    fn list_locks_locked_by_me_with_comment() {
        let (mut d, buf) = buffered(ColorMode::Never);
        let lock = LockStatus {
            is_locked: true,
            is_mine: true,
            locked_by: "someone".to_owned(),
            time: "now".to_owned(),
            comment: "test comment".to_owned(),
        };
        d.list_locks("test_host", &system("SLES"), &lock);
        let out = rendered(&buf);
        assert!(out.contains("since now by me"));
        assert!(out.contains(": test comment"));
    }

    #[test]
    fn list_locks_locked_by_other_no_comment() {
        let (mut d, buf) = buffered(ColorMode::Never);
        let lock = LockStatus {
            is_locked: true,
            is_mine: false,
            locked_by: "alice".to_owned(),
            time: "then".to_owned(),
            comment: String::new(),
        };
        d.list_locks("h", &system("SLES"), &lock);
        assert!(rendered(&buf).contains("since then by alice"));
    }

    #[test]
    fn list_locks_not_locked() {
        let (mut d, buf) = buffered(ColorMode::Never);
        d.list_locks("h", &system("SLES"), &LockStatus::default());
        assert!(rendered(&buf).contains("not locked"));
    }

    #[test]
    fn list_sessions_renders_header_and_body() {
        let (mut d, buf) = buffered(ColorMode::Never);
        d.list_sessions("test_host", &system("SLES"), "test session");
        let out = rendered(&buf);
        assert!(out.contains("sessions on test_host"));
        assert!(out.contains("test session"));
    }

    #[test]
    fn list_timeout_renders_seconds() {
        let (mut d, buf) = buffered(ColorMode::Never);
        d.list_timeout("test_host", &system("SLES"), 600);
        assert!(rendered(&buf).contains("600s"));
    }

    #[test]
    fn list_versions_single_host_ladder() {
        let (mut d, buf) = buffered(ColorMode::Never);
        let v1 = RPMVersion::parse("1.0-1").unwrap();
        let v2 = RPMVersion::parse("2.0-1").unwrap();
        let hosts_pvs = vec![(
            vec![("h1".to_owned(), system("SLES"))],
            vec![("pkg".to_owned(), vec![v1, v2])],
        )];
        d.list_versions(&hosts_pvs);
        let out = rendered(&buf);
        assert!(out.contains("pkg:"));
        // newest-first: 2.0 before 1.0, and 1.0 more indented.
        let idx2 = out.find("-> 2.0-1").unwrap();
        let idx1 = out.find("-> 1.0-1").unwrap();
        assert!(idx2 < idx1);
        assert!(out.contains("  -> 1.0-1"));
        assert!(!out.contains("version history from:"));
    }

    #[test]
    fn list_versions_multi_host_header() {
        let (mut d, buf) = buffered(ColorMode::Never);
        let v1 = RPMVersion::parse("1.0-1").unwrap();
        let hosts_pvs = vec![
            (
                vec![("h1".to_owned(), system("SLES"))],
                vec![("pkg".to_owned(), vec![v1.clone()])],
            ),
            (
                vec![("h2".to_owned(), system("SLED"))],
                vec![("pkg".to_owned(), vec![v1])],
            ),
        ];
        d.list_versions(&hosts_pvs);
        assert!(rendered(&buf).contains("version history from:"));
    }

    #[test]
    fn list_products_preserves_upstream_typo() {
        let (mut d, buf) = buffered(ColorMode::Never);
        d.list_products("refhost", &system("SLES"));
        let out = rendered(&buf);
        assert!(out.contains("Referenece host: refhost"));
        assert!(out.contains("Base product: SLES-15.5-x86_64"));
    }

    #[test]
    fn list_update_repos_renders_product_line() {
        let (mut d, buf) = buffered(ColorMode::Never);
        let repos = vec![(
            SystemProduct::new("SLES", "15.5", "x86_64"),
            "https://repo.example/path".to_owned(),
        )];
        d.list_update_repos(&repos);
        let out = rendered(&buf);
        assert!(out.contains("Product: SLES - version: 15.5 - arch: x86_64"));
        assert!(out.contains("    https://repo.example/path"));
    }

    #[test]
    fn show_log_forwards_to_sink() {
        let mut captured: Vec<String> = Vec::new();
        {
            let mut sink = |m: &str| captured.push(m.to_owned());
            CommandPromptDisplay::show_log(
                "test_host",
                &[("cmd".to_owned(), "out".to_owned(), "err".to_owned(), 0)],
                &mut sink,
            );
        }
        let joined = captured.join("\n");
        assert!(joined.contains("log from test_host"));
        assert!(joined.contains("test_host:~> cmd [0]"));
        assert!(joined.contains("out"));
        assert!(joined.contains("err"));
    }

    #[test]
    fn page_non_interactive_no_writer_is_noop() {
        let text = vec!["a".to_owned(), "b".to_owned(), "c".to_owned()];
        page(&text, false, None);
        // No panic, input untouched (it is borrowed immutably).
        assert_eq!(text, vec!["a", "b", "c"]);
    }

    #[test]
    fn page_non_interactive_writer_strips_line_endings() {
        let mut captured: Vec<String> = Vec::new();
        {
            let mut w = |m: &str| captured.push(m.to_owned());
            let text = vec![
                "alpha".to_owned(),
                "beta\n".to_owned(),
                "gamma\r\n".to_owned(),
            ];
            page(&text, false, Some(&mut w));
        }
        assert_eq!(captured, vec!["alpha", "beta", "gamma"]);
    }

    #[test]
    fn filter_ansi_strips_color_and_control_sequences() {
        // A colored string round-trips to its bare text (upstream test_filter_ansi).
        let colored = "err".red().to_string();
        assert!(colored.contains('\u{1b}'));
        assert_eq!(filter_ansi(&colored), "err");
        // Bare ESC, an SGR/cursor code, and the erase-line code are all removed.
        assert_eq!(filter_ansi("a\u{1b}b"), "ab");
        assert_eq!(filter_ansi("a\u{1b}[2Ab"), "ab");
        assert_eq!(filter_ansi("a\u{1b}[Kb"), "ab");
        // Plain text is untouched.
        assert_eq!(filter_ansi("plain"), "plain");
    }

    #[test]
    #[serial_test::serial(env)]
    #[allow(unsafe_code)]
    fn termsize_from_env_returns_cols_then_rows() {
        // The fallback must return (cols, rows) — never the transposed (rows, cols).
        // `set_var`/`remove_var` are `unsafe` in edition 2024; `#[serial(env)]`
        // makes the mutation exclusive.
        unsafe {
            std::env::set_var("ACCTEST_COLS", "80");
            std::env::set_var("ACCTEST_ROWS", "24");
        }
        assert_eq!(termsize_from_env(), Some((80, 24)));
        unsafe {
            std::env::remove_var("ACCTEST_COLS");
            std::env::remove_var("ACCTEST_ROWS");
        }
        assert_eq!(termsize_from_env(), None);
    }

    #[test]
    fn page_screen_wraps_to_width_and_stops_at_height() {
        // width=3, height=3 → 2 display rows per screen. "abcdef" wraps to
        // "abc"/"def" (2 rows), filling the screen; the second line spills over.
        let text = vec!["abcdef".to_owned(), "next".to_owned()];
        let mut out: Vec<String> = Vec::new();
        let step = page_screen(&text, 3, 3, &mut |l| out.push(l.to_owned()));
        assert_eq!(out, vec!["abc", "def"]);
        assert_eq!(step, PageStep::More(vec!["next".to_owned()]));
    }

    #[test]
    fn page_screen_empty_line_is_one_row_and_done_when_all_fit() {
        let text = vec!["a".to_owned(), String::new(), "b".to_owned()];
        let mut out: Vec<String> = Vec::new();
        // height=10 → plenty of room; all three rows print, nothing remains.
        let step = page_screen(&text, 80, 10, &mut |l| out.push(l.to_owned()));
        assert_eq!(out, vec!["a", "", "b"]);
        assert_eq!(step, PageStep::Done);
    }

    #[test]
    fn page_screen_exact_fill_is_done_not_more() {
        // Two lines exactly fill a 2-row screen with nothing left → Done, not
        // More(empty) (the rows_left==0 && rest.is_empty() edge).
        let text = vec!["a".to_owned(), "b".to_owned()];
        let mut out: Vec<String> = Vec::new();
        let step = page_screen(&text, 80, 3, &mut |l| out.push(l.to_owned()));
        assert_eq!(out, vec!["a", "b"]);
        assert_eq!(step, PageStep::Done);
    }

    #[test]
    fn page_screen_full_screen_with_remainder_is_more() {
        // Three lines, 2-row screen: fills exactly, one line remains → More.
        let text = vec!["a".to_owned(), "b".to_owned(), "c".to_owned()];
        let mut out: Vec<String> = Vec::new();
        let step = page_screen(&text, 80, 3, &mut |l| out.push(l.to_owned()));
        assert_eq!(out, vec!["a", "b"]);
        assert_eq!(step, PageStep::More(vec!["c".to_owned()]));
    }

    #[test]
    fn page_screen_carries_overflow_of_a_single_wrapped_line() {
        // A line longer than one screen's worth of rows carries its tail forward.
        // width=2, height=3 → 2 rows/screen. "abcdef" = ab|cd|ef → prints ab,cd;
        // carries "ef".
        let text = vec!["abcdef".to_owned()];
        let mut out: Vec<String> = Vec::new();
        let step = page_screen(&text, 2, 3, &mut |l| out.push(l.to_owned()));
        assert_eq!(out, vec!["ab", "cd"]);
        assert_eq!(step, PageStep::More(vec!["ef".to_owned()]));
    }

    /// A prompter whose reader always returns `answer`, for driving the pager.
    fn fixed_prompter(answer: &'static str) -> mtui_hosts::Prompter {
        mtui_hosts::Prompter::new(std::sync::Arc::new(move |_t: String| {
            Box::pin(async move { Ok(answer.to_owned()) })
                as std::pin::Pin<
                    Box<dyn std::future::Future<Output = std::io::Result<String>> + Send>,
                >
        }))
    }

    #[tokio::test]
    #[serial_test::serial(env)]
    #[allow(unsafe_code)]
    async fn page_interactive_quits_on_q_after_first_screen() {
        // Force a tiny screen so paging actually happens: 2 rows/screen.
        // `set_var`/`remove_var` are `unsafe` in edition 2024; `#[serial(env)]`
        // makes the mutation exclusive.
        unsafe {
            std::env::set_var("ACCTEST_COLS", "80");
            std::env::set_var("ACCTEST_ROWS", "3");
        }
        let (mut d, buf) = buffered(ColorMode::Never);
        let text: Vec<String> = (0..10).map(|i| format!("line{i}")).collect();
        page_interactive(&text, &mut d, Some(&fixed_prompter("q"))).await;
        unsafe {
            std::env::remove_var("ACCTEST_COLS");
            std::env::remove_var("ACCTEST_ROWS");
        }
        let out = rendered(&buf);
        // First screen (2 rows) shown, then `q` stopped it.
        assert!(out.contains("line0") && out.contains("line1"), "{out}");
        assert!(
            !out.contains("line2"),
            "should have quit after first screen: {out}"
        );
    }

    #[tokio::test]
    #[serial_test::serial(env)]
    #[allow(unsafe_code)]
    async fn page_interactive_enter_pages_to_end() {
        // `set_var`/`remove_var` are `unsafe` in edition 2024; serialised on `env`.
        unsafe {
            std::env::set_var("ACCTEST_COLS", "80");
            std::env::set_var("ACCTEST_ROWS", "3");
        }
        let (mut d, buf) = buffered(ColorMode::Never);
        let text: Vec<String> = (0..5).map(|i| format!("line{i}")).collect();
        // Empty answer (Enter) continues through every screen.
        page_interactive(&text, &mut d, Some(&fixed_prompter(""))).await;
        unsafe {
            std::env::remove_var("ACCTEST_COLS");
            std::env::remove_var("ACCTEST_ROWS");
        }
        let out = rendered(&buf);
        for i in 0..5 {
            assert!(out.contains(&format!("line{i}")), "missing line{i}: {out}");
        }
    }

    #[tokio::test]
    #[serial_test::serial(env)]
    #[allow(unsafe_code)]
    async fn page_interactive_without_prompter_dumps_all() {
        // `set_var`/`remove_var` are `unsafe` in edition 2024; serialised on `env`.
        unsafe {
            std::env::set_var("ACCTEST_COLS", "80");
            std::env::set_var("ACCTEST_ROWS", "3");
        }
        let (mut d, buf) = buffered(ColorMode::Never);
        let text: Vec<String> = (0..5).map(|i| format!("line{i}")).collect();
        page_interactive(&text, &mut d, None).await;
        unsafe {
            std::env::remove_var("ACCTEST_COLS");
            std::env::remove_var("ACCTEST_ROWS");
        }
        let out = rendered(&buf);
        for i in 0..5 {
            assert!(out.contains(&format!("line{i}")), "missing line{i}: {out}");
        }
    }
}
