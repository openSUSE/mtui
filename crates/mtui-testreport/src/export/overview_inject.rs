//! Inject the `openqa_overview` block into a testreport's `log` file.
//!
//! Ported from `mtui/update_workflow/export/overview_inject.py`. The block lives
//! under the existing `regression tests:` section. On first export it is
//! appended after any existing content in that section; on subsequent exports a
//! previously-inserted block is detected via the `OVERVIEW_*` markers and
//! replaced in place, so the file never accumulates duplicate copies
//! (**idempotent re-export** — a Phase 4 text-format contract).

use mtui_datasources::oqa_search::render::{
    OVERVIEW_BEGIN_MARKER, OVERVIEW_END_MARKER, render_overview,
};
use mtui_datasources::{BuildCheckResult, GroupResult, VersionResult};

/// The header line delimiting the section we own (verbatim, with the trailing
/// newline that [`FileList`](crate::support::FileList) stores).
const REGRESSION_HEADER: &str = "regression tests:\n";
/// The next section header. The inserted block is always kept strictly before
/// this line.
const NEXT_SECTION_HEADER: &str = "build log review:\n";

/// The begin-marker line as it appears inside the template (with newline).
fn begin_line() -> String {
    format!("{OVERVIEW_BEGIN_MARKER}\n")
}

/// The end-marker line as it appears inside the template (with newline).
fn end_line() -> String {
    format!("{OVERVIEW_END_MARKER}\n")
}

/// Finds the first index of `needle` in `template` at or after `from`.
fn index_from(template: &[String], needle: &str, from: usize) -> Option<usize> {
    template
        .iter()
        .skip(from)
        .position(|l| l == needle)
        .map(|i| i + from)
}

/// Inserts (or replaces) the openqa_overview block in `template`.
///
/// Returns `true` if the template was modified, `false` if it has no
/// `regression tests:` section (meaning the user is exporting against a file
/// that mtui's template machinery did not generate — leave it alone).
///
/// Any prior block between [`OVERVIEW_BEGIN_MARKER`] and [`OVERVIEW_END_MARKER`]
/// is removed before the new one is inserted, so re-exports stay idempotent.
pub fn inject_overview(
    template: &mut Vec<String>,
    single_incidents_rows: &[VersionResult],
    aggregated_updates_rows: &[GroupResult],
    build_checks_rows: &[BuildCheckResult],
    skip_aggregated: bool,
) -> bool {
    let Some(regression_idx) = index_from(template, REGRESSION_HEADER, 0) else {
        tracing::debug!("No 'regression tests:' header found; skipping overview injection");
        return false;
    };

    // 1. Strip any previous block, if present.
    remove_existing_block(template, regression_idx);

    // 2. Find the insertion point: end of the regression-tests section.
    let insert_at = section_end(template, regression_idx);

    // 2a. Remove existing trailing blank lines in the section so we control the
    //     gap ourselves (otherwise the block's trailing `\n` stacks with the
    //     section's existing trailing `\n`).
    let mut trim_end = insert_at;
    while trim_end < template.len() && template[trim_end] == "\n" {
        trim_end += 1;
    }
    if trim_end > insert_at {
        template.drain(insert_at..trim_end);
    }

    // 3. Build the new block (markers + rendered lines + trailing blank line for
    //    breathing room before the next section).
    let rendered = render_overview(
        single_incidents_rows,
        aggregated_updates_rows,
        build_checks_rows,
        skip_aggregated,
    );
    let mut block: Vec<String> = Vec::with_capacity(rendered.len() + 3);
    block.push(begin_line());
    block.extend(rendered.into_iter().map(|line| format!("{line}\n")));
    block.push(end_line());
    block.push("\n".to_string());

    // 4. Guarantee one blank line between the prior content (e.g. "(put your
    //    details here)") and our block: `section_end` strips trailing blanks
    //    before the next section, so without this the block would butt right up
    //    against the previous line.
    if insert_at > 0 && template[insert_at - 1] != "\n" {
        block.insert(0, "\n".to_string());
    }

    // 5. Splice into the template at the insertion point.
    template.splice(insert_at..insert_at, block);
    true
}

/// Removes a prior marker-bounded block, if one exists.
///
/// Also strips the single blank line inserted before the block (if any) and the
/// single blank line appended after it, so repeated re-exports do not slowly
/// grow the surrounding gap.
fn remove_existing_block(template: &mut Vec<String>, search_from: usize) {
    let begin_line = begin_line();
    let end_line = end_line();

    let Some(begin) = index_from(template, &begin_line, search_from) else {
        return;
    };
    let Some(end) = index_from(template, &end_line, begin) else {
        // Begin without matching end -- corrupt; leave it alone.
        tracing::warn!("Found {OVERVIEW_BEGIN_MARKER} without matching end marker; not removing");
        return;
    };

    let mut end_exclusive = end + 1;
    // Swallow the trailing blank line we appended last time.
    if end_exclusive < template.len() && template[end_exclusive] == "\n" {
        end_exclusive += 1;
    }
    // Swallow the leading blank line we prepended last time, but only if
    // removing it would not glue the markers to text we do not own (i.e. there
    // is still something non-blank above).
    let mut begin_inclusive = begin;
    if begin_inclusive > 0
        && template[begin_inclusive - 1] == "\n"
        && begin_inclusive >= 2
        && template[begin_inclusive - 2] != "\n"
    {
        begin_inclusive -= 1;
    }
    template.drain(begin_inclusive..end_exclusive);
}

/// Finds where the regression-tests section ends.
///
/// Returns the index of the next-section header line, or `template.len()` if
/// there isn't one. Trailing blank lines before the next section are trimmed so
/// the block is not sandwiched between two blank lines on re-export.
fn section_end(template: &[String], regression_idx: usize) -> usize {
    let Some(mut next_idx) = index_from(template, NEXT_SECTION_HEADER, regression_idx + 1) else {
        return template.len();
    };

    while next_idx - 1 > regression_idx && template[next_idx - 1] == "\n" {
        next_idx -= 1;
    }
    next_idx
}
