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
fn contains_bounded(lower: &str, kw: &str) -> bool {
    let hay = lower.as_bytes();
    let nd = kw.as_bytes();
    if nd.is_empty() || nd.len() > hay.len() {
        return false;
    }
    let mut i = 0;
    while i + nd.len() <= hay.len() {
        if &hay[i..i + nd.len()] == nd
            && (i == 0 || !is_word_char(hay[i - 1]))
            && (i + nd.len() == hay.len() || !is_word_char(hay[i + nd.len()]))
        {
            return true;
        }
        i += 1;
    }
    false
}

fn is_foldable(line: &str) -> bool {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return false;
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
    if line.contains("completed on") || line.contains("rebooted") {
        return false;
    }
    if line.contains("FAILED") || line.contains("stderr:") {
        return false;
    }
    let lower = line.to_ascii_lowercase();
    for kw in [
        "failed",
        "error",
        "warning",
        "warn",
        "trace",
        "panic",
        "fatal",
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
        if contains_bounded(&lower, kw) {
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
            "",
        ] {
            let lines = vec![line.to_owned(); 5];
            assert_eq!(fold_output(&lines), lines, "must not fold {line:?}");
        }
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
