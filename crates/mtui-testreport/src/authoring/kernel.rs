//! Authoring for kernel jobs: `testing.regression` from the kernel
//! connectors.
//!
//! `Regression.verdict` has no exporter-computed source: unlike `auto.rs`'s
//! `install_status`, `export::kernel` never derives a pass/fail roll-up —
//! `REGRESSION TEST SUMMARY`'s verdict is tester-typed text no exporter
//! reads (step 1 gap analysis, `plans/phase4-authoring.md`). Only `comment`
//! is authored here, from the same per-test/per-module matrix the text
//! exporter already renders.

use mtui_datasources::openqa::kernel::KernelOpenQA;
use mtui_types::OpenQAResult;
use mtui_types::report_document::{Regression, Req};

const BLOCK_BEGIN: &str = "[mtui export: kernel openQA results - rewritten by every export]";
const BLOCK_END: &str = "[end of mtui export]";

/// Builds `testing.regression` from the kernel connectors' rendered result
/// matrices, wrapped in marker lines so a re-export can find and replace it
/// without touching the tester's own text.
///
/// `None` when no connector produced results — mirroring
/// `openqa_install_from_auto`'s "nothing to report yet" omission.
#[must_use]
pub fn regression_from_kernel(kernel: &[KernelOpenQA]) -> Option<Regression> {
    let lines: Vec<&str> = kernel
        .iter()
        .filter(|k| k.has_results())
        .flat_map(|k| k.pp())
        .map(String::as_str)
        .collect();
    if lines.is_empty() {
        return None;
    }
    Some(Regression {
        verdict: Req(None),
        comment: Req(Some(wrap_block(&lines.join("\n")))),
    })
}

pub(super) fn wrap_block(body: &str) -> String {
    format!("{BLOCK_BEGIN}\n{body}\n{BLOCK_END}")
}

/// Puts `block` into the tester's `existing` comment: replaces the marked
/// block in place, else appends after the text, else stands alone.
///
/// An unterminated begin marker counts as no block, so nothing is dropped.
pub(super) fn splice_block(existing: Option<&str>, block: &str) -> String {
    let Some(existing) = existing.filter(|t| !t.trim().is_empty()) else {
        return block.to_owned();
    };
    // Anchoring on the first end marker and the last begin before it keeps a
    // stray begin marker in the tester's text from swallowing what follows.
    let span = existing.find(BLOCK_END).and_then(|end| {
        let start = existing[..end].rfind(BLOCK_BEGIN)?;
        Some((start, end + BLOCK_END.len()))
    });
    match span {
        Some((start, end)) => format!("{}{block}{}", &existing[..start], &existing[end..]),
        None => format!("{}\n\n{block}", existing.trim_end()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // `KernelOpenQA::pp`/`has_results` are only populated by `run()` against
    // a live client (built from `ruoqa::Client` + an `IncidentName`, neither
    // mockable from outside `mtui-datasources`); `export::kernel`'s own test
    // suite has the same constraint and, likewise, never constructs a
    // populated connector. The no-connectors case needs no instance at all
    // and is the property this pure function actually adds.
    #[test]
    fn no_connectors_omits_the_object() {
        assert!(regression_from_kernel(&[]).is_none());
    }

    fn block(body: &str) -> String {
        wrap_block(body)
    }

    #[test]
    fn a_null_comment_becomes_just_the_block() {
        assert_eq!(splice_block(None, &block("m")), block("m"));
    }

    #[test]
    fn a_blank_comment_becomes_just_the_block() {
        assert_eq!(splice_block(Some(" \n"), &block("m")), block("m"));
    }

    #[test]
    fn the_block_is_appended_after_the_testers_text() {
        assert_eq!(
            splice_block(Some("looks fine\n"), &block("m")),
            format!("looks fine\n\n{}", block("m"))
        );
    }

    #[test]
    fn splicing_twice_leaves_exactly_one_block() {
        let once = splice_block(Some("looks fine"), &block("old"));
        let twice = splice_block(Some(&once), &block("new"));
        assert_eq!(twice, format!("looks fine\n\n{}", block("new")));
        assert_eq!(twice.matches(BLOCK_BEGIN).count(), 1);
    }

    #[test]
    fn text_on_both_sides_of_the_block_survives_a_replace() {
        let existing = format!("before\n{}\nafter", block("old"));
        assert_eq!(
            splice_block(Some(&existing), &block("new")),
            format!("before\n{}\nafter", block("new"))
        );
    }

    #[test]
    fn a_begin_marker_without_an_end_is_kept_and_the_block_appended() {
        let existing = format!("note\n{BLOCK_BEGIN}\nhalf-typed");
        let out = splice_block(Some(&existing), &block("m"));
        assert!(out.starts_with(&existing), "{out}");
        assert!(out.ends_with(&block("m")), "{out}");
    }

    #[test]
    fn a_stray_begin_marker_does_not_swallow_text_on_the_next_export() {
        let existing = format!("note\n{BLOCK_BEGIN}\nhalf-typed");
        let once = splice_block(Some(&existing), &block("old"));
        let twice = splice_block(Some(&once), &block("new"));
        assert!(twice.contains("half-typed"), "{twice}");
        assert!(twice.ends_with(&block("new")), "{twice}");
        assert!(!twice.contains("old"), "{twice}");
    }

    #[test]
    fn an_end_marker_without_a_begin_is_kept_and_the_block_appended() {
        let existing = format!("note {BLOCK_END}");
        let out = splice_block(Some(&existing), &block("m"));
        assert!(out.starts_with(&existing), "{out}");
        assert!(out.ends_with(&block("m")), "{out}");
    }
}
