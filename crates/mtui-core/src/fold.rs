//! Deterministic folding for parallel fan-out outputs.
//!
//! `run` over many hosts repeats the same success spam once per host; without
//! folding a 50-host `zypper lr` dwarfs the MCP context. Folding is output-only
//! and dependency-free: consecutive identical lines collapse, and identical
//! per-host blocks share one body with a combined banner.

/// Marker for a collapsed run of identical lines; `n` is the folded-away count.
#[must_use]
pub(crate) fn fold_marker(n: usize) -> String {
    format!("…[{n} identical lines folded]")
}

/// Marker for a per-host block shared by `n` hosts.
#[must_use]
pub(crate) fn hosts_marker(n: usize) -> String {
    format!("…[output identical on {n} hosts folded]")
}

/// Whether `line` may be folded away (success spam only).
///
/// Verdicts, banners, errors, warnings and traces always survive: folding must
/// never hide the signal `cap_output`'s head-keeping is meant to preserve.
fn is_word_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'-'
}

/// Substring `kw` bounded by non-word chars, so `liberror` never matches `error`.
/// A CamelCase transition counts as a boundary, so `IndexError` matches `error`.
fn contains_bounded(lower: &str, kw: &str, orig: &str) -> bool {
    let hay = lower.as_bytes();
    let needle = kw.as_bytes();
    if needle.is_empty() || needle.len() > hay.len() {
        return false;
    }
    let orig = orig.as_bytes();
    let cased = orig.len() == hay.len();
    let mut i = 0;
    while i + needle.len() <= hay.len() {
        if &hay[i..i + needle.len()] == needle {
            let start_ok = i == 0
                || !is_word_char(hay[i - 1])
                || (cased && orig[i].is_ascii_uppercase() && hay[i - 1].is_ascii_alphanumeric());
            let end_ok = i + needle.len() == hay.len()
                || !is_word_char(hay[i + needle.len()])
                || (cased && orig[i + needle.len()].is_ascii_uppercase());
            if start_ok && end_ok {
                return true;
            }
        }
        i += 1;
    }
    false
}

/// Drops ANSI escape sequences so colored signals still block folding.
///
/// Both the diagnostics' own highlighting (`\x1b[33mwarning\x1b[39m`) and
/// colored remote output (`\x1b[31merror\x1b[0m`) end the opener in `m`, a
/// word char that would otherwise defeat [`contains_bounded`]'s start-boundary
/// check and let warnings fold away. Output keeps its escapes; only the scan
/// sees the stripped form.
fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        // CSI (`\x1b[` … final byte `@`..=`~`) covers the SGR colors both the
        // diagnostics' highlighting and remote output use; a bare ESC just
        // drops without eating the text that follows it.
        if c == '\x1b' && chars.peek() == Some(&'[') {
            chars.next();
            for c in chars.by_ref() {
                if ('@'..='~').contains(&c) {
                    break;
                }
            }
        } else if c == '\x1b' && chars.peek() == Some(&']') {
            // OSC (`\x1b]` … `BEL`/`ESC \`) carries metadata, never visible text.
            chars.next();
            loop {
                match chars.next() {
                    None | Some('\x07') => break,
                    Some('\x1b') if chars.peek() == Some(&'\\') => {
                        chars.next();
                        break;
                    }
                    Some(_) => {}
                }
            }
        } else if c != '\x1b' {
            out.push(c);
        }
    }
    out
}

fn is_foldable(line: &str) -> bool {
    let stripped = strip_ansi(line);
    let trimmed = stripped.trim();
    // Blank runs fold (len >= 3 below); singles pass through, empty output keeps its shared banner.
    if trimmed.is_empty() {
        return true;
    }
    if trimmed.starts_with("…[") {
        return false;
    }
    if trimmed.starts_with("===") {
        return false;
    }
    if line.contains(":->") {
        return false;
    }
    if stripped.contains("completed on") || stripped.contains("rebooted") {
        return false;
    }
    if stripped.contains("FAILED") || stripped.contains("stderr:") {
        return false;
    }
    let lower = stripped.to_ascii_lowercase();
    // Lowercase compounds bounded matching misses stay listed; CamelCase ones trip the case-boundary rule, so liberror/strace still fold.
    for kw in [
        "failed",
        "error",
        "errors",
        "keyerror",
        "assertionerror",
        "valueerror",
        "typeerror",
        "runtimeerror",
        "warning",
        "warnings",
        "warn",
        "trace",
        "traceback",
        "stacktrace",
        "panic",
        "panicked",
        "panics",
        "fatal",
        "critical",
        "exception",
        "timeout",
        "degradation",
        "cancelled",
        "canceled",
        "stopped after",
        "skipped",
        "no refhosts",
        "not connected",
    ] {
        if contains_bounded(&lower, kw, &stripped) {
            return false;
        }
    }
    true
}

/// Whether a per-host block may be shared across hosts.
///
/// Only clean success: exit 0, empty stderr; empty stdout folds (quiet `true`).
#[must_use]
pub(crate) fn can_fold_block(stdout: &str, stderr: &str, exit: Option<i16>) -> bool {
    if exit != Some(0) || !stderr.trim().is_empty() {
        return false;
    }
    for line in stdout.split('\n') {
        if line.trim().is_empty() {
            continue;
        }
        if !is_foldable(line) {
            return false;
        }
    }
    true
}

/// Folds runs of identical consecutive lines.
///
/// A run of `len >= 3` identical foldable lines emits one exemplar plus
/// [`fold_marker`]; shorter runs and significant lines pass through unchanged,
/// preserving order and the head verdict.
#[must_use]
pub(crate) fn fold_output(lines: &[String]) -> Vec<String> {
    let mut out = Vec::with_capacity(lines.len());
    let mut i = 0;
    while i < lines.len() {
        let mut j = i + 1;
        while j < lines.len() && lines[j] == lines[i] {
            j += 1;
        }
        let len = j - i;
        if len >= 3 && is_foldable(&lines[i]) {
            out.push(lines[i].clone());
            out.push(fold_marker(len - 1));
        } else {
            out.extend_from_slice(&lines[i..j]);
        }
        i = j;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_runs_pass_through() {
        let lines = ["a".to_owned(), "a".to_owned()];
        assert_eq!(fold_output(&lines), lines);
    }

    #[test]
    fn three_identical_success_lines_fold() {
        let lines = vec!["ok".to_owned(); 5];
        assert_eq!(
            fold_output(&lines),
            vec!["ok".to_owned(), "…[4 identical lines folded]".to_owned()]
        );
    }

    #[test]
    fn verdicts_and_errors_never_fold() {
        for line in [
            "run completed on h1 (exit 0)",
            "h1:-> true [0]",
            "FAILED on h2 (exit 1)",
            "stderr:",
            "error: boom",
            "warning: extra rpm output",
            "trace: entering",
            "=== SUSE:Maintenance:1:1 ===",
            "h1: rebooted & reconnected",
        ] {
            let lines = vec![line.to_owned(); 5];
            assert_eq!(fold_output(&lines), lines, "must not fold {line:?}");
        }
    }

    #[test]
    fn traceback_compounds_never_fold() {
        // Word-boundary matching misses error/trace inside these compounds,
        // so they are listed explicitly; a Python traceback with exit 0 and
        // empty stderr must still trip the fold-safety net.
        for line in [
            "Traceback (most recent call last):",
            "traceback (most recent call last):",
            "KeyError: 'foo'",
            "AssertionError",
            "assertionerror: boom",
            "StackTrace: boom",
            "stacktrace: boom",
        ] {
            let lines = vec![line.to_owned(); 5];
            assert_eq!(fold_output(&lines), lines, "must not fold {line:?}");
            assert!(
                !can_fold_block(line, "", Some(0)),
                "traceback signal must block sharing {line:?}"
            );
        }
    }

    #[test]
    fn ansi_wrapped_signals_never_fold() {
        // SGR openers end in `m`, a word char: without the strip the bounded
        // match misses and colored warnings fold away under Always/Auto.
        for line in [
            "\u{1b}[33mwarning\u{1b}[39m: extra rpm output",
            "\u{1b}[31merror\u{1b}[0m: boom",
            "\u{1b}[1;31merror\u{1b}[0m: boom",
            "got \u{1b}[33mwarn\u{1b}[0m: disk low",
            "\u{1b}warning: stray esc drops without eating text",
        ] {
            let lines = vec![line.to_owned(); 5];
            assert_eq!(fold_output(&lines), lines, "must not fold {line:?}");
            assert!(
                !can_fold_block(line, "", Some(0)),
                "colored signal must block sharing {line:?}"
            );
        }
        // An escapes-only line is effectively blank, so it still folds.
        assert!(can_fold_block("\u{1b}[0m", "", Some(0)));
    }

    #[test]
    fn bare_python_exceptions_never_fold() {
        // Single-line formatters print `ValueError: ...` with no traceback
        // header; the inner `error` is preceded by a word char, so these
        // compounds are listed explicitly.
        for line in [
            "ValueError: bad value",
            "TypeError: bad type",
            "RuntimeError: boom",
        ] {
            let lines = vec![line.to_owned(); 5];
            assert_eq!(fold_output(&lines), lines, "must not fold {line:?}");
            assert!(
                !can_fold_block(line, "", Some(0)),
                "exception must block sharing {line:?}"
            );
        }
    }

    #[test]
    fn blank_runs_fold_but_separators_survive() {
        let blanks = vec!["".to_owned(); 5];
        assert_eq!(
            fold_output(&blanks),
            vec!["".to_owned(), "…[4 identical lines folded]".to_owned()]
        );
        let single = vec!["a".to_owned(), "".to_owned(), "b".to_owned()];
        assert_eq!(fold_output(&single), single);
        let pair = vec!["a".to_owned(), "".to_owned(), "".to_owned(), "b".to_owned()];
        assert_eq!(fold_output(&pair), pair);
        assert!(can_fold_block("ok\n\n\n\nok", "", Some(0)));
    }

    #[test]
    fn non_consecutive_duplicates_do_not_fold() {
        let lines = vec![
            "a".to_owned(),
            "b".to_owned(),
            "a".to_owned(),
            "a".to_owned(),
        ];
        assert_eq!(fold_output(&lines), lines);
    }

    #[test]
    fn markers_do_not_fold_further() {
        let lines = vec!["…[4 identical lines folded]".to_owned(); 5];
        assert_eq!(fold_output(&lines), lines);
    }

    #[test]
    fn signal_keywords_never_fold() {
        for line in [
            "warn: disk low",
            "WARN: disk low",
            "panic: boom",
            "fatal: boom",
            "exception in thread",
            "timeout after 30s",
            "canceled by user",
            "cancelled by user",
            "degradation reported",
            "skipped, held by alice",
            "No refhosts defined",
            "Host 'h1' is not connected",
            "stopped after 1 of 4 templates",
            "failed to start",
        ] {
            let lines = vec![line.to_owned(); 5];
            assert_eq!(fold_output(&lines), lines, "must not fold {line:?}");
        }
    }

    #[test]
    fn inflected_signals_never_fold() {
        for line in [
            "3 errors found",
            "warnings: 2 issues",
            "thread 'main' panicked at 'boom', src/main.rs:1",
            "panics: boom",
        ] {
            let lines = vec![line.to_owned(); 5];
            assert_eq!(fold_output(&lines), lines, "must not fold {line:?}");
            assert!(
                !can_fold_block(line, "", Some(0)),
                "inflected signal must block sharing {line:?}"
            );
        }
    }

    #[test]
    fn camelcase_error_compounds_never_fold() {
        for line in [
            "IndexError: list index out of range",
            "AttributeError: no attribute 'foo'",
            "ImportError: no module named 'foo'",
            "FileNotFoundError: [Errno 2] No such file",
            "PermissionError: [Errno 13] Permission denied",
            "ConnectionError: failed to connect",
            "NotImplementedError: not implemented",
            "ZeroDivisionError: division by zero",
            "NameError: name 'foo' is not defined",
            "SyntaxError: invalid syntax",
            "ModuleNotFoundError: No module named 'foo'",
            "OSError: [Errno 5] Input/output error",
            "LookupError: lookup failed",
        ] {
            let lines = vec![line.to_owned(); 5];
            assert_eq!(fold_output(&lines), lines, "must not fold {line:?}");
            assert!(
                !can_fold_block(line, "", Some(0)),
                "compound must block sharing {line:?}"
            );
        }
    }

    #[test]
    fn critical_never_folds() {
        for line in ["critical: disk failing", "CRITICAL: disk failing"] {
            let lines = vec![line.to_owned(); 5];
            assert_eq!(fold_output(&lines), lines, "must not fold {line:?}");
            assert!(
                !can_fold_block(line, "", Some(0)),
                "critical must block sharing {line:?}"
            );
        }
    }

    #[test]
    fn osc_sequences_stripped_before_scan() {
        for line in [
            "\u{1b}]8;;http://example.com\u{1b}\\warning: extra rpm output",
            "got \u{1b}]8;;http://example.com\u{1b}\\error\u{1b}]8;;\u{1b}\\: boom",
            "\u{1b}]0;title\u{07}error: boom",
        ] {
            let lines = vec![line.to_owned(); 5];
            assert_eq!(fold_output(&lines), lines, "must not fold {line:?}");
            assert!(
                !can_fold_block(line, "", Some(0)),
                "osc-wrapped signal must block sharing {line:?}"
            );
        }
        assert!(can_fold_block(
            "\u{1b}]8;;http://example.com\u{1b}\\",
            "",
            Some(0)
        ));
        assert!(can_fold_block("ok\u{1b}]0;error\u{07}", "", Some(0)));
    }

    #[test]
    fn block_needs_clean_success() {
        assert!(can_fold_block("ok\nok", "", Some(0)));
        assert!(!can_fold_block("ok", "boom", Some(0)));
        assert!(!can_fold_block("ok", "", Some(1)));
        assert!(!can_fold_block("error: boom", "", Some(0)));
        assert!(!can_fold_block("warning: x", "", Some(0)));
        assert!(!can_fold_block("warn: x", "", Some(0)));
        assert!(!can_fold_block("panic: x", "", Some(0)));
        assert!(!can_fold_block("fatal: x", "", Some(0)));
        assert!(!can_fold_block("exception: x", "", Some(0)));
        assert!(!can_fold_block("timeout: x", "", Some(0)));
        assert!(!can_fold_block("canceled: x", "", Some(0)));
        assert!(!can_fold_block("cancelled: x", "", Some(0)));
        assert!(!can_fold_block("degradation: x", "", Some(0)));
        assert!(!can_fold_block("skipped: x", "", Some(0)));
        assert!(!can_fold_block("No refhosts defined", "", Some(0)));
        assert!(!can_fold_block("Host 'h1' is not connected", "", Some(0)));
        assert!(!can_fold_block("stopped after 1 of 4", "", Some(0)));
    }

    #[test]
    fn substrings_inside_words_still_fold() {
        for line in [
            "liberror",
            "liberror0 installed",
            "perl-Error installed",
            "strace: attached",
            "my timeouts are fine",
            "warningsummary: 3 packages",
        ] {
            let lines = vec![line.to_owned(); 5];
            assert_eq!(
                fold_output(&lines),
                vec![line.to_owned(), "…[4 identical lines folded]".to_owned()],
                "must fold {line:?}"
            );
            assert!(
                can_fold_block(line, "", Some(0)),
                "clean block must share {line:?}"
            );
        }
    }

    #[test]
    fn empty_block_shares_banner_when_clean() {
        assert!(can_fold_block("", "", Some(0)));
        assert!(can_fold_block("  \n ", "", Some(0)));
        assert!(!can_fold_block("", "", None));
        assert!(!can_fold_block("", "boom", Some(0)));
        assert!(!can_fold_block("", "", Some(1)));
    }
}
