//! The `checkers` command — list the build-check results for the loaded update.

use async_trait::async_trait;
use clap::{Arg, ArgAction, ArgMatches};
use mtui_datasources::CheckerRun;

use crate::command::{Command, Scope};
use crate::commands::apicall::teregen_client;
use crate::commands::support::{require_update, template_completion};
use crate::display::{CommandPromptDisplay, sanitize_external};
use crate::error::CommandResult;
use crate::session::Session;

/// Indent of a `--full-output` continuation line: `"  "` + the status column +
/// `' '`. It clears the status column rather than aligning with the first
/// output line, which is printed inline after `check_type` so a one-line finding
/// costs one row.
const OUTPUT_INDENT: usize = 13;

/// Characters of server-supplied text a single row may show before it is cut
/// and marked.
///
/// The whole payload is untrusted and only capped at `MAX_API_BODY` (16 MiB), so
/// `output.lines().next()` on a newline-free body would put all of it on one row
/// — flooding scrollback and the MCP transcript, and holding the worker for the
/// duration of the write. A real summary is one diagnostic (an rpmlint verdict,
/// an install-check `requires … but none of the providers can be installed`),
/// 80–140 characters, so 200 keeps every genuine one whole while bounding the
/// row at roughly two 80-column lines. `--full-output` lifts it for the check
/// *output* only; a label is an identifier and stays bounded there too.
const ROW_LIMIT: usize = 200;

/// How a checker status renders.
///
/// The vocabulary is the one [`CheckerRun`]'s `*_count` fields enumerate, but
/// the classification is deliberately **open**: anything else is
/// [`Verdict::Other`] and prints verbatim, so a status the server adds degrades
/// to visible text instead of the `?` this command used to emit (#522).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Verdict {
    /// `pass`/`skip` — green, and the only verdict whose output is suppressed.
    Pass,
    /// `warn` — yellow.
    Warn,
    /// `fail`/`error`/`recerror` — red.
    Fail,
    /// `running`/`wait`/`unknown` — dim; the run has not decided yet.
    Pending,
    /// Unrecognised — uncolored, rendered as sent.
    Other,
}

impl Verdict {
    /// Classifies `status` case-insensitively.
    fn classify(status: &str) -> Self {
        match status.to_lowercase().as_str() {
            "pass" | "skip" => Self::Pass,
            "warn" => Self::Warn,
            "fail" | "error" | "recerror" => Self::Fail,
            "running" | "wait" | "unknown" => Self::Pending,
            _ => Self::Other,
        }
    }

    /// Paints `text` in this verdict's color.
    fn paint(self, display: &CommandPromptDisplay, text: &str) -> String {
        match self {
            Self::Pass => display.green(text),
            Self::Warn => display.yellow(text),
            Self::Fail => display.red(text),
            Self::Pending => display.dim(text),
            Self::Other => text.to_owned(),
        }
    }
}

/// The per-run header: the run's label and its non-zero counts.
fn run_header(rrid: &str, run: &CheckerRun) -> String {
    let counts = [
        ("pass", run.pass_count),
        ("fail", run.fail_count),
        ("warn", run.warn_count),
        ("skip", run.skip_count),
        ("error", run.error_count),
        ("recerror", run.recerror_count),
        ("running", run.running_count),
        ("wait", run.wait_count),
        ("unknown", run.unknown_count),
    ];
    let summary: Vec<String> = counts
        .iter()
        .filter(|(_, n)| *n > 0)
        .map(|(name, n)| format!("{n} {name}"))
        .collect();
    let label = if run.checker_type.is_empty() {
        "unnamed".to_owned()
    } else {
        sanitize_external(&run.checker_type, Some(ROW_LIMIT))
    };
    let mut header = format!("Checker results for {rrid} — {label} run");
    if !summary.is_empty() {
        header.push_str(&format!(", {}", summary.join(" / ")));
    }
    header.push(':');
    header
}

/// Prints one run: its header, then a `<status> <check_type>` row per result.
///
/// A non-passing result also carries its output — its first line inline, the
/// rest only under `full_output`, since some checks emit long diffs.
///
/// **Every field printed here comes from the TeReGen payload**, so all of them
/// go through [`sanitize_external`]: status and label included, not just the
/// output body. `full_output` waives the [`ROW_LIMIT`] cut but never the
/// filtering — a `--full-output` body is more escape surface, not less.
fn print_run(display: &mut CommandPromptDisplay, rrid: &str, run: &CheckerRun, full_output: bool) {
    let header = run_header(rrid, run);
    display.println(&header);
    let summary_limit = if full_output { None } else { Some(ROW_LIMIT) };
    for result in &run.results {
        // Classify the *sanitized* status, so the verdict and the text the user
        // reads are decided by the same string.
        let status = sanitize_external(&result.status, Some(ROW_LIMIT));
        let verdict = Verdict::classify(&status);
        let status = verdict.paint(display, &format!("{status:<10}"));
        let check_type = sanitize_external(&result.check_type, Some(ROW_LIMIT));
        let mut row = format!("  {status} {check_type}");
        let body = if verdict == Verdict::Pass {
            ""
        } else {
            result.output.as_str()
        };
        let mut lines = body.lines();
        let summary = lines.next().map(|l| sanitize_external(l, summary_limit));
        if let Some(summary) = summary.filter(|s| !s.is_empty()) {
            row.push_str("  ");
            row.push_str(&summary);
        }
        display.println(&row);
        if full_output {
            for line in lines {
                display.println(&format!(
                    "{:OUTPUT_INDENT$}{}",
                    "",
                    sanitize_external(line, None)
                ));
            }
        }
    }
}

/// Lists the build-check (checker) result runs for the loaded update.
///
/// Fetches the live checker results from the TeReGen report API
/// (`GET /reports/{id}/checkers`) and prints, per run, a header with the run's
/// non-zero counts followed by one colored `<status> <check_type>` row per
/// result. Requires a loaded update.
///
/// The payload is external, so every field it contributes is filtered of
/// terminal control sequences and bounded to 200 characters per row before
/// printing; `--full-output` waives the bound on the check output alone.
pub struct Checkers;

#[async_trait]
impl Command for Checkers {
    fn name(&self) -> &'static str {
        "checkers"
    }

    fn about(&self) -> Option<&'static str> {
        Some("Lists the build-check (checker) result runs for the loaded update.")
    }

    fn scope(&self) -> Scope {
        Scope::Fanout
    }

    fn configure(&self, cmd: clap::Command) -> clap::Command {
        cmd.arg(
            Arg::new("full_output")
                .long("full-output")
                .action(ArgAction::SetTrue)
                .help(
                    "print every line of a non-passing check's output, untruncated, \
                     instead of a bounded first-line summary (some checks emit long diffs)",
                ),
        )
    }

    fn complete(&self, session: &Session, text: &str, _line: &str) -> Vec<String> {
        template_completion(session, text)
    }

    async fn call(&self, session: &mut Session, args: &ArgMatches) -> CommandResult {
        let rrid = require_update(session)?;
        let teregen = teregen_client(session)?;
        let full_output = args.get_flag("full_output");

        let runs = teregen
            .checkers(&rrid.to_string())
            .await
            .unwrap_or_default();
        if runs.is_empty() {
            session
                .display
                .println(&format!("No checker results for {rrid}"));
            return Ok(());
        }

        let rrid = rrid.to_string();
        for run in &runs {
            print_run(&mut session.display, &rrid, run, full_output);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::testkit::{Buffer, empty_session, matches, session_with_hosts};
    use crate::display::{ColorMode, TRUNCATION_MARK};
    use crate::error::CommandError;
    use mtui_config::Config;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const RRID: &str = "SUSE:Maintenance:1:1";

    /// ANSI SGR introducers the row colors must actually emit.
    const GREEN: &str = "\u{1b}[32m";
    const YELLOW: &str = "\u{1b}[33m";
    const RED: &str = "\u{1b}[31m";
    const DIM: &str = "\u{1b}[2m";
    /// owo-colors' foreground reset, closing a `red()`/`yellow()` span.
    const RESET: &str = "\u{1b}[39m";

    /// A session pointed at a wiremock TeReGen serving `body`, with color forced
    /// on: with the default `ColorMode::Never` the green/red assertions below
    /// could not fail.
    async fn session_serving(body: serde_json::Value) -> (MockServer, Session, Buffer) {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(format!("/reports/{RRID}/checkers")))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(&server)
            .await;

        let (mut session, buf) = session_with_hosts(RRID, &["h1"], "ok");
        let mut config = Config::default();
        config.teregen_api = server.uri();
        session.config = config;
        session.display.set_color(ColorMode::Always);
        (server, session, buf)
    }

    /// Every `  `-indented line, i.e. one per rendered result row (a wrapped
    /// `--full-output` line is indented further).
    fn rows(out: &str) -> Vec<&str> {
        out.lines()
            .filter(|l| l.starts_with("  ") && !l.starts_with("   "))
            .collect()
    }

    /// One synthetic run: 12 `PASS` + 3 `WARN` results. Only two warns carry
    /// output, so the third pins that an empty `output` adds no separator.
    ///
    /// The timestamp and `run_id` keys are served but unmodelled; they stay in
    /// the fixture to pin that an unknown key is ignored rather than fatal.
    fn nested_body() -> serde_json::Value {
        let mut results: Vec<serde_json::Value> = (1..=12)
            .map(|i| {
                serde_json::json!({
                    "check_type": format!("check-pass-{i:02}"),
                    "status": "PASS",
                    "output": "synthetic passing output that must stay hidden",
                    "created": "2026-01-01T00:00:00Z",
                    "changed": "2026-01-01T00:01:00Z",
                })
            })
            .collect();
        for (i, output) in [
            (1, "synthetic warn 1 line one\nsynthetic warn 1 line two"),
            (2, "synthetic warn 2 line one\nsynthetic warn 2 line two"),
            (3, ""),
        ] {
            results.push(serde_json::json!({
                "check_type": format!("check-warn-{i:02}"),
                "status": "WARN",
                "output": output,
                "created": "2026-01-01T00:00:00Z",
                "changed": "2026-01-01T00:01:00Z",
            }));
        }
        serde_json::json!({"checkers": [{
            "checker_type": "checker-alpha",
            "run_id": "run-0001",
            "started": "2026-01-01T00:00:00Z",
            "finished": "2026-01-01T00:05:00Z",
            "pass_count": 12,
            "warn_count": 3,
            "fail_count": 0,
            "error_count": 0,
            "skip_count": 0,
            "recerror_count": 0,
            "running_count": 0,
            "wait_count": 0,
            "unknown_count": 0,
            "results": results,
        }]})
    }

    /// A single-result run with `status`, for the vocabulary regressions.
    fn run_with_status(status: &str, output: &str) -> serde_json::Value {
        serde_json::json!({"checkers": [{
            "checker_type": "checker-beta",
            "results": [{"check_type": "check-99", "status": status, "output": output}],
        }]})
    }

    #[test]
    fn name_and_fanout_scope() {
        assert_eq!(Checkers.name(), "checkers");
        assert_eq!(Checkers.scope(), Scope::Fanout);
    }

    /// Catches a case-sensitive match (`PASS` is what the server sends) and a
    /// closed vocabulary that would swallow an unseen status.
    #[test]
    fn classify_is_case_insensitive_and_open() {
        for s in ["PASS", "pass", "Skip"] {
            assert!(matches!(Verdict::classify(s), Verdict::Pass), "{s}");
        }
        assert!(matches!(Verdict::classify("WARN"), Verdict::Warn));
        for s in ["FAIL", "error", "RecError"] {
            assert!(matches!(Verdict::classify(s), Verdict::Fail), "{s}");
        }
        for s in ["RUNNING", "wait", "UNKNOWN"] {
            assert!(matches!(Verdict::classify(s), Verdict::Pending), "{s}");
        }
        for s in ["QUARANTINED", ""] {
            assert!(matches!(Verdict::classify(s), Verdict::Other), "{s:?}");
        }
    }

    #[tokio::test]
    async fn errors_when_no_report_loaded() {
        let (mut session, _buf) = empty_session();
        let args = matches(&Checkers, &[]);
        let err = Checkers.call(&mut session, &args).await.unwrap_err();
        assert!(matches!(err, CommandError::Other(_)));
    }

    #[tokio::test]
    async fn renders_nested_run_rows_with_counts_and_colors() {
        let (_server, mut session, buf) = session_serving(nested_body()).await;
        let args = matches(&Checkers, &[]);
        Checkers.call(&mut session, &args).await.unwrap();
        let out = buf.contents();

        // The header names the run and its non-zero counts, not the run count.
        assert!(
            out.contains(&format!(
                "Checker results for {RRID} — checker-alpha run, 12 pass / 3 warn:"
            )),
            "{out}"
        );
        // One row per result, not per run.
        assert_eq!(rows(&out).len(), 15, "{out}");
        // The bug's signature: a flat-shape parse produced `?` for both columns.
        assert!(!out.contains('?'), "{out}");
        // Fails on the old PASSING list, which lacked "pass" and painted red.
        assert!(out.contains(&format!("{GREEN}PASS")), "{out:?}");
        assert!(!out.contains(&format!("{RED}PASS")), "{out:?}");
        assert!(out.contains(&format!("{YELLOW}WARN")), "{out:?}");
        // A non-passing result carries its first output line, and only that.
        assert!(
            out.contains("check-warn-01  synthetic warn 1 line one"),
            "{out}"
        );
        assert!(
            out.contains("check-warn-02  synthetic warn 2 line one"),
            "{out}"
        );
        assert!(!out.contains("line two"), "{out}");
        // A passing result's output stays out of the summary.
        assert!(!out.contains("must stay hidden"), "{out}");
        // An empty output leaves no dangling separator.
        assert!(out.contains("check-warn-03\n"), "{out}");
    }

    #[tokio::test]
    async fn full_output_flag_prints_every_output_line() {
        let (_server, mut session, buf) = session_serving(nested_body()).await;
        let args = matches(&Checkers, &["--full-output"]);
        Checkers.call(&mut session, &args).await.unwrap();
        let out = buf.contents();
        assert!(out.contains("synthetic warn 1 line two"), "{out}");
        assert!(out.contains("synthetic warn 2 line two"), "{out}");
        // Still suppressed for a passing result.
        assert!(!out.contains("must stay hidden"), "{out}");
        // A continuation line clears the status column: 2 + the 10-wide status
        // + 1. The width is spelled out rather than read from OUTPUT_INDENT,
        // which would make the assertion move with the constant it pins.
        assert!(
            out.contains(&format!("\n{}synthetic warn 1 line two\n", " ".repeat(13))),
            "{out:?}"
        );
    }

    /// Regression (#522): the statuses a reviewer acts on must reach `paint`,
    /// and a status shorter than the column must be padded to it — the verbatim
    /// test above uses an 11-char status, so it pins the *un*-padded path.
    #[tokio::test]
    async fn failing_status_is_red_and_padded_to_the_column() {
        let (_server, mut session, buf) =
            session_serving(run_with_status("FAIL", "synthetic failure")).await;
        let args = matches(&Checkers, &[]);
        Checkers.call(&mut session, &args).await.unwrap();
        let out = buf.contents();
        assert!(out.contains(&format!("{RED}FAIL      ")), "{out:?}");
        assert!(out.contains("check-99  synthetic failure"), "{out}");
    }

    /// Regression (#522): a run whose `checker_type` is absent still gets a
    /// header — the counts are the useful half of it.
    #[tokio::test]
    async fn nameless_run_is_labelled_unnamed() {
        let body = serde_json::json!({"checkers": [{"pass_count": 1}]});
        let (_server, mut session, buf) = session_serving(body).await;
        let args = matches(&Checkers, &[]);
        Checkers.call(&mut session, &args).await.unwrap();
        assert!(
            buf.contents().contains(&format!(
                "Checker results for {RRID} — unnamed run, 1 pass:"
            )),
            "{}",
            buf.contents()
        );
    }

    /// Regression (#522): `null` is the wire form of an in-progress run's
    /// timestamps and a drifted verdict is a single bad row — neither may cost
    /// the run, its header or its good siblings, which would render as
    /// "No checker results", byte-identical to an unreachable TeReGen.
    #[tokio::test]
    async fn null_and_drifted_keys_cost_at_most_one_row() {
        let body = serde_json::json!({"checkers": [{
            "checker_type": "checker-alpha",
            "run_id": 1234,
            "started": "2026-01-01T00:00:00Z",
            "finished": null,
            "pass_count": 1,
            "running_count": 1,
            "results": [
                {"check_type": "check-ok", "status": "PASS", "created": null},
                {"check_type": "check-drifted", "status": 7},
            ],
        }]});
        let (_server, mut session, buf) = session_serving(body).await;
        let args = matches(&Checkers, &[]);
        Checkers.call(&mut session, &args).await.unwrap();
        let out = buf.contents();
        assert!(!out.contains("No checker results"), "{out}");
        assert!(
            out.contains(&format!(
                "Checker results for {RRID} — checker-alpha run, 1 pass / 1 running:"
            )),
            "{out}"
        );
        assert_eq!(rows(&out).len(), 1, "{out}");
        assert!(out.contains(&format!("{GREEN}PASS")), "{out:?}");
        assert!(out.contains("check-ok"), "{out}");
        assert!(!out.contains("check-drifted"), "{out}");
    }

    /// Regression (#522): an unseen status must reach the user as itself.
    #[tokio::test]
    async fn unrecognised_status_renders_verbatim() {
        let (_server, mut session, buf) =
            session_serving(run_with_status("QUARANTINED", "synthetic quarantine note")).await;
        let args = matches(&Checkers, &[]);
        Checkers.call(&mut session, &args).await.unwrap();
        let out = buf.contents();
        assert!(!out.contains('?'), "{out}");
        // Verbatim and uncolored — no color introducer precedes the status.
        assert!(
            out.contains("  QUARANTINED check-99  synthetic quarantine note"),
            "{out:?}"
        );
    }

    /// Regression (#522): a run with no `results` key is an empty run, not a
    /// parse failure and not a `?` row.
    #[tokio::test]
    async fn run_without_results_renders_header_and_no_rows() {
        let body = serde_json::json!({"checkers": [{"checker_type": "checker-beta"}]});
        let (_server, mut session, buf) = session_serving(body).await;
        let args = matches(&Checkers, &[]);
        Checkers.call(&mut session, &args).await.unwrap();
        let out = buf.contents();
        assert!(
            out.contains(&format!("Checker results for {RRID} — checker-beta run:")),
            "{out}"
        );
        assert!(rows(&out).is_empty(), "{out}");
        assert!(!out.contains('?'), "{out}");
    }

    #[tokio::test]
    async fn pending_status_is_dimmed() {
        let (_server, mut session, buf) = session_serving(run_with_status("RUNNING", "")).await;
        let args = matches(&Checkers, &[]);
        Checkers.call(&mut session, &args).await.unwrap();
        let out = buf.contents();
        assert!(out.contains(&format!("{DIM}RUNNING")), "{out:?}");
    }

    /// A checker output with no newline in it is a whole 16 MiB response body on
    /// one row unless the summary is bounded. Also pins the mark, so a cut row
    /// cannot read as the complete finding.
    #[tokio::test]
    async fn summary_is_bounded_and_marked() {
        let output = format!("{}TAIL", "x".repeat(10_000));
        let (_server, mut session, buf) = session_serving(run_with_status("FAIL", &output)).await;
        let args = matches(&Checkers, &[]);
        Checkers.call(&mut session, &args).await.unwrap();
        let out = buf.contents();
        assert!(!out.contains("TAIL"), "unbounded summary");
        assert!(out.contains(&"x".repeat(ROW_LIMIT)), "{out}");
        assert!(!out.contains(&"x".repeat(ROW_LIMIT + 1)), "{out}");
        assert!(out.contains(TRUNCATION_MARK), "{out}");
    }

    /// A multibyte body must cut on a `char`: `&s[..200]` panics mid-UTF-8.
    #[tokio::test]
    async fn multibyte_summary_cuts_without_panicking() {
        let (_server, mut session, buf) =
            session_serving(run_with_status("FAIL", &"é".repeat(10_000))).await;
        let args = matches(&Checkers, &[]);
        Checkers.call(&mut session, &args).await.unwrap();
        let out = buf.contents();
        assert!(
            out.contains(&format!("{}{TRUNCATION_MARK}", "é".repeat(ROW_LIMIT))),
            "{out}"
        );
    }

    /// `--full-output` is the escape-*worse* path, so it waives the cut on the
    /// output body and nothing else.
    #[tokio::test]
    async fn full_output_waives_the_cut_for_the_body_only() {
        let output = format!("{}TAIL", "x".repeat(1_000));
        let (_server, mut session, buf) = session_serving(run_with_status("FAIL", &output)).await;
        let args = matches(&Checkers, &["--full-output"]);
        Checkers.call(&mut session, &args).await.unwrap();
        let out = buf.contents();
        assert!(out.contains("TAIL"), "{out}");
        assert!(!out.contains(TRUNCATION_MARK), "{out}");

        // A label is an identifier, not output: the flag does not unbound it.
        let body = serde_json::json!({"checkers": [{
            "checker_type": "c".repeat(1_000),
            "results": [{"check_type": "n".repeat(1_000), "status": "FAIL"}],
        }]});
        let (_server, mut session, buf) = session_serving(body).await;
        let args = matches(&Checkers, &["--full-output"]);
        Checkers.call(&mut session, &args).await.unwrap();
        let out = buf.contents();
        assert!(!out.contains(&"c".repeat(ROW_LIMIT + 1)), "{out}");
        assert!(!out.contains(&"n".repeat(ROW_LIMIT + 1)), "{out}");
        assert_eq!(out.matches(TRUNCATION_MARK).count(), 2, "{out}");
    }

    /// Regression: TeReGen's payload is external. A control sequence in it must
    /// never reach the terminal — OSC 52 writes the user's clipboard, CSI
    /// repaints the screen, CR overwrites the row's own `<status> <check_type>`
    /// prefix — on either the summary or the `--full-output` path.
    #[tokio::test]
    async fn control_sequences_never_reach_the_terminal() {
        let output = concat!(
            "start\u{1b}[2J\u{1b}]52;c;cGF5bG9hZA==\u{7}mid\r  PASS       forged\n",
            "second\u{9b}31m\u{9d}52;c;bQ==\u{9c}line\u{202e}rtl",
        );
        for flags in [vec![], vec!["--full-output"]] {
            let (_server, mut session, buf) =
                session_serving(run_with_status("FAIL", output)).await;
            let args = matches(&Checkers, &flags);
            Checkers.call(&mut session, &args).await.unwrap();
            let out = buf.contents();
            // Strip the row color mtui itself emits, then nothing may remain.
            let external: String = out.replace(RED, "").replace(RESET, "");
            for bad in ['\u{1b}', '\u{7}', '\r', '\u{9b}', '\u{9d}', '\u{202e}'] {
                assert!(!external.contains(bad), "{flags:?} leaked {bad:?}: {out:?}");
            }
            // The text around the sequences survives; only the control does not.
            assert!(out.contains("startmid  PASS       forged"), "{out:?}");
            if !flags.is_empty() {
                assert!(out.contains("secondlinertl"), "{out:?}");
            }
        }
    }

    /// The label, status and check name come from the same untrusted payload as
    /// the output body, and are printed outside it.
    #[tokio::test]
    async fn label_status_and_check_name_are_sanitized() {
        let body = serde_json::json!({"checkers": [{
            "checker_type": "alpha\u{1b}]0;pwned\u{7}beta",
            "results": [{
                "check_type": "check\u{1b}[2J-99",
                "status": "WARN\u{9b}31m",
                "output": "",
            }],
        }]});
        let (_server, mut session, buf) = session_serving(body).await;
        let args = matches(&Checkers, &[]);
        Checkers.call(&mut session, &args).await.unwrap();
        let out = buf.contents();
        assert!(out.contains("alphabeta run:"), "{out}");
        assert!(out.contains("check-99"), "{out}");
        assert!(!out.contains("pwned"), "{out:?}");
        assert!(!out.contains('\u{9b}'), "{out:?}");
        // The sanitized status still classifies, so the row keeps its color.
        assert!(out.contains(&format!("{YELLOW}WARN")), "{out:?}");
    }

    /// Ordinary whitespace is not a control sequence: a tab-aligned or indented
    /// finding must survive the filter unchanged.
    #[tokio::test]
    async fn ordinary_whitespace_survives() {
        let (_server, mut session, buf) =
            session_serving(run_with_status("FAIL", "  col\tone\tcol two")).await;
        let args = matches(&Checkers, &[]);
        Checkers.call(&mut session, &args).await.unwrap();
        assert!(
            buf.contents().contains("check-99    col\tone\tcol two"),
            "{}",
            buf.contents()
        );
    }

    #[tokio::test]
    async fn reports_empty_when_no_checkers() {
        let (_server, mut session, buf) =
            session_serving(serde_json::json!({"checkers": []})).await;
        let args = matches(&Checkers, &[]);
        Checkers.call(&mut session, &args).await.unwrap();
        assert!(
            buf.contents()
                .contains(&format!("No checker results for {RRID}"))
        );
    }
}
