//! Row-budget crush for unbounded listing outputs (SmartCrusher-lite).
//!
//! Row budget: keep first-40 + last-10 + all anomaly rows, exact-dedup identical rows, hard cap 100.
//! Row-cap is not byte-cap: MCP `max_output_bytes` can still cut mid-array on huge rows.
//!
//! Shared truncation-notice convention (converged for #610/#611/#620):
//! - Emit a truncation notice only on a real row-budget cut
//!   (`crushed.truncated > 0`); user `--limit`/`--offset` windowing alone
//!   never notices.
//! - Count anomalies over the full deduped dataset (`0..total`), not just the
//!   middle slice, so a `0/0` notice never shows while anomalies are visible.
//! - Human output appends one trailing plaintext notice via [`row_notice`];
//!   it omits the `(K/L anomalies kept)` parenthetical when `L == 0`.
//! - `--json` output stays parseable: over-cap stdout is a JSON envelope
//!   (`{"rows": [...], "truncation": {...}}`, plus `"probe"` where a survey
//!   was windowed) instead of array plus trailing plaintext; small outputs
//!   stay a plain array.
//! - Partial-survey probe notes (e.g. `--free` windowing leaves hosts
//!   unprobed) are decoupled from truncation: [`probe_notice`] (human) and
//!   the envelope `"probe"` object (JSON) never fabricate anomaly counts over
//!   unprobed rows.

use std::collections::HashSet;
use std::hash::Hash;

use serde_json::{Value, json};

/// Head rows always kept.
pub(crate) const ROW_HEAD: usize = 40;
/// Tail rows always kept.
pub(crate) const ROW_TAIL: usize = 10;
/// Hard cap on kept rows, anomalies included.
pub(crate) const ROW_CAP: usize = 100;
const _: () = assert!(ROW_HEAD + ROW_TAIL <= ROW_CAP);

/// Outcome of [`crush`]: the kept items plus truncation counts for the notice.
pub(crate) struct CrushOutcome<T> {
    /// Kept items in original order.
    pub kept: Vec<T>,
    /// Post-dedup total the kept subset was drawn from.
    pub total: usize,
    /// `total - kept.len()`; zero means nothing was dropped.
    pub truncated: usize,
    /// Anomalies kept, counted over the full dataset (head + selected middle + tail).
    pub anomaly_kept: usize,
    /// Anomalies found over the full dataset (`0..total`), head/tail included.
    pub anomaly_total: usize,
}

/// Crush `items` to budget, preserving order.
///
/// Exact-dedups on `key`, then keeps head + tail + top-severity middle
/// anomalies up to [`ROW_CAP`]. `severity_of` returns 0 for routine rows,
/// higher for more severe anomalies; ties keep file order (stable).
pub(crate) fn crush<T, K: Eq + Hash>(
    items: Vec<T>,
    mut key_of: impl FnMut(&T) -> K,
    mut severity_of: impl FnMut(&T) -> u8,
) -> CrushOutcome<T> {
    // Dedup first so identical rows never consume budget twice.
    let mut seen = HashSet::new();
    let items: Vec<T> = items
        .into_iter()
        .filter(|it| seen.insert(key_of(it)))
        .collect();
    let total = items.len();
    if total <= ROW_CAP {
        let n = items.iter().filter(|it| severity_of(it) > 0).count();
        return CrushOutcome {
            kept: items,
            total,
            truncated: 0,
            anomaly_kept: n,
            anomaly_total: n,
        };
    }
    let tail_start = total - ROW_TAIL;
    let head_anomalies = items[..ROW_HEAD]
        .iter()
        .filter(|it| severity_of(it) > 0)
        .count();
    let tail_anomalies = items[tail_start..]
        .iter()
        .filter(|it| severity_of(it) > 0)
        .count();
    let mut scored: Vec<(u8, usize)> = (ROW_HEAD..tail_start)
        .map(|i| (severity_of(&items[i]), i))
        .filter(|(s, _)| *s > 0)
        .collect();
    let middle_total = scored.len();
    let anomaly_total = head_anomalies + middle_total + tail_anomalies;
    // Stable severity-desc: severe survives, ties keep positional order.
    scored.sort_by_key(|(s, _)| std::cmp::Reverse(*s));
    scored.truncate(ROW_CAP - ROW_HEAD - ROW_TAIL);
    let middle_kept = scored.len();
    let anomaly_kept = head_anomalies + middle_kept + tail_anomalies;
    let anomaly_idx: Vec<usize> = scored.into_iter().map(|(_, i)| i).collect();
    keep_head_anomaly_tail(
        items,
        anomaly_idx,
        tail_start,
        total,
        anomaly_kept,
        anomaly_total,
    )
}

/// Crush `items` to budget, preserving order.
///
/// Exact-dedups on `key`, then keeps head + tail + top-severity middle
/// anomalies up to [`ROW_CAP`].
/// Borrowed twin for already-owned data (openQA overview): same keep, no bulk clone.
pub(crate) fn crush_slice<'a, T, K: Eq + Hash>(
    items: &'a [T],
    mut key_of: impl FnMut(&'a T) -> K,
    mut severity_of: impl FnMut(&'a T) -> u8,
) -> CrushOutcome<&'a T> {
    let mut seen = HashSet::new();
    let mut uniq: Vec<&'a T> = Vec::new();
    for it in items {
        if seen.insert(key_of(it)) {
            uniq.push(it);
        }
    }
    let total = uniq.len();
    if total <= ROW_CAP {
        let n = uniq.iter().filter(|it| severity_of(it) > 0).count();
        return CrushOutcome {
            kept: uniq,
            total,
            truncated: 0,
            anomaly_kept: n,
            anomaly_total: n,
        };
    }
    let tail_start = total - ROW_TAIL;
    let head_anomalies = uniq[..ROW_HEAD]
        .iter()
        .filter(|it| severity_of(it) > 0)
        .count();
    let tail_anomalies = uniq[tail_start..]
        .iter()
        .filter(|it| severity_of(it) > 0)
        .count();
    let mut scored: Vec<(u8, usize)> = (ROW_HEAD..tail_start)
        .map(|i| (severity_of(uniq[i]), i))
        .filter(|(s, _)| *s > 0)
        .collect();
    let middle_total = scored.len();
    let anomaly_total = head_anomalies + middle_total + tail_anomalies;
    scored.sort_by_key(|(s, _)| std::cmp::Reverse(*s));
    scored.truncate(ROW_CAP - ROW_HEAD - ROW_TAIL);
    let middle_kept = scored.len();
    let anomaly_kept = head_anomalies + middle_kept + tail_anomalies;
    let anomaly_idx: Vec<usize> = scored.into_iter().map(|(_, i)| i).collect();
    keep_head_anomaly_tail(
        uniq,
        anomaly_idx,
        tail_start,
        total,
        anomaly_kept,
        anomaly_total,
    )
}

/// Shared head + capped-anomaly + tail keep, order-preserving.
fn keep_head_anomaly_tail<T>(
    mut items: Vec<T>,
    anomaly_idx: Vec<usize>,
    tail_start: usize,
    total: usize,
    anomaly_kept: usize,
    anomaly_total: usize,
) -> CrushOutcome<T> {
    let keep: HashSet<usize> = (0..ROW_HEAD)
        .chain(anomaly_idx)
        .chain(tail_start..total)
        .collect();
    let mut kept = Vec::with_capacity(keep.len());
    for (i, it) in items.drain(..).enumerate() {
        if keep.contains(&i) {
            kept.push(it);
        }
    }
    let truncated = total - kept.len();
    CrushOutcome {
        kept,
        total,
        truncated,
        anomaly_kept,
        anomaly_total,
    }
}

/// Human/JSON trailing notice naming the narrowing flags.
#[must_use]
pub(crate) fn row_notice(
    truncated: usize,
    total: usize,
    anomaly_kept: usize,
    anomaly_total: usize,
    hint: &str,
) -> String {
    if anomaly_total == 0 {
        format!("…[truncated {truncated} of {total} rows; narrow with {hint}]")
    } else {
        format!(
            "…[truncated {truncated} of {total} rows ({anomaly_kept}/{anomaly_total} anomalies kept); narrow with {hint}]"
        )
    }
}

/// Human probe note for a windowed survey (e.g. `--free` with `--limit`).
/// Decoupled from [`row_notice`]: it carries no anomaly counts, since unprobed
/// rows were never surveyed.
#[must_use]
pub(crate) fn probe_notice(probed: usize, total: usize) -> String {
    format!("…[probed {probed} of {total} hosts; unprobed hosts not surveyed for locks]")
}

/// Structured truncation metadata for `--json` envelopes. Omits the anomaly
/// keys when `anomaly_total == 0`, mirroring [`row_notice`].
#[must_use]
pub(crate) fn truncation_meta(
    truncated: usize,
    total: usize,
    anomaly_kept: usize,
    anomaly_total: usize,
    hint: &str,
) -> Value {
    if anomaly_total == 0 {
        json!({"truncated": truncated, "total": total, "hint": hint})
    } else {
        json!({
            "truncated": truncated,
            "total": total,
            "anomaly_kept": anomaly_kept,
            "anomaly_total": anomaly_total,
            "hint": hint,
        })
    }
}

/// Structured probe metadata for `--json` envelopes.
#[must_use]
pub(crate) fn probe_meta(probed: usize, total: usize) -> Value {
    json!({"probed": probed, "total": total})
}

/// Render a `--json` envelope: kept `rows` plus truncation/probe metadata.
/// stdout stays one valid JSON document; callers emit a plain array when both
/// are `None`.
#[must_use]
pub(crate) fn json_envelope(
    rows: Vec<Value>,
    truncation: Option<Value>,
    probe: Option<Value>,
) -> String {
    let mut obj = serde_json::Map::new();
    obj.insert("rows".to_owned(), Value::Array(rows));
    if let Some(t) = truncation {
        obj.insert("truncation".to_owned(), t);
    }
    if let Some(p) = probe {
        obj.insert("probe".to_owned(), p);
    }
    serde_json::to_string_pretty(&Value::Object(obj)).expect("envelope serializes")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn under_cap_passes_through_with_no_truncation() {
        let out = crush(vec![1, 2, 3], |v| *v, |_| 0);
        assert_eq!(out.kept, vec![1, 2, 3]);
        assert_eq!(out.truncated, 0);
    }

    #[test]
    fn over_cap_keeps_head_tail_and_all_middle_anomalies() {
        // 0..150, anomalies at 50 and 140 (tail) plus 100.
        let items: Vec<usize> = (0..150).collect();
        let out = crush(items, |v| *v, |v| u8::from(*v == 50 || *v == 100));
        assert_eq!(out.total, 150);
        // Head 0..40, anomalies 50+100, tail 140..150.
        assert!(out.kept.contains(&0) && out.kept.contains(&39));
        assert!(out.kept.contains(&50) && out.kept.contains(&100));
        assert!(out.kept.contains(&140) && out.kept.contains(&149));
        assert!(!out.kept.contains(&60), "non-anomaly middle row dropped");
        assert_eq!(out.kept.len(), ROW_HEAD + ROW_TAIL + 2);
        assert_eq!(out.truncated, 150 - out.kept.len());
        assert_eq!((out.anomaly_kept, out.anomaly_total), (2, 2));
        // Order preserved.
        let mut sorted = out.kept.clone();
        sorted.sort_unstable();
        assert_eq!(sorted, out.kept);
    }

    #[test]
    fn severe_anomaly_outranks_mild_when_overflowing() {
        // 300 rows: every middle row anomalous, one severe at the far end.
        // Severity-desc selection must keep the severe tail-middle row and
        // drop a mild head-middle one, restoring display in index order.
        let items: Vec<usize> = (0..300).collect();
        let out = crush(
            items,
            |v| *v,
            |v| {
                if *v == 250 {
                    2
                } else if *v >= ROW_HEAD {
                    1
                } else {
                    0
                }
            },
        );
        assert_eq!(out.kept.len(), ROW_CAP);
        assert_eq!((out.anomaly_kept, out.anomaly_total), (60, 260));
        assert!(
            out.kept.contains(&250),
            "severe middle row survives: {:?}",
            &out.kept[35..55]
        );
        assert!(
            !out.kept.contains(&89),
            "mild middle row drops once severe outranks it"
        );
        let n = row_notice(
            out.truncated,
            out.total,
            out.anomaly_kept,
            out.anomaly_total,
            "--limit",
        );
        assert!(n.contains("60/260 anomalies kept"), "{n}");
        // Display stays in original order despite severity selection.
        let mut sorted = out.kept.clone();
        sorted.sort_unstable();
        assert_eq!(sorted, out.kept);
    }

    #[test]
    fn anomaly_overflow_truncates_middle_first_deterministically() {
        // Every middle row anomalous: only the first fitting anomalies survive, in index order.
        let items: Vec<usize> = (0..300).collect();
        let out = crush(items, |v| *v, |v| u8::from(*v >= ROW_HEAD));
        assert_eq!(out.kept.len(), ROW_CAP);
        let expected: Vec<usize> = (0..ROW_HEAD)
            .chain(ROW_HEAD..ROW_HEAD + (ROW_CAP - ROW_HEAD - ROW_TAIL))
            .chain(300 - ROW_TAIL..300)
            .collect();
        assert_eq!(out.kept, expected);
    }

    #[test]
    fn boundary_100_passes_101_crushes() {
        let out100 = crush((0..100).collect::<Vec<_>>(), |v| *v, |_| 0);
        assert_eq!(out100.truncated, 0);
        assert_eq!(out100.kept.len(), 100);
        let out101 = crush((0..101).collect::<Vec<_>>(), |v| *v, |_| 0);
        assert_eq!(out101.total, 101);
        assert_eq!(out101.kept.len(), ROW_HEAD + ROW_TAIL);
        assert_eq!(out101.truncated, 101 - (ROW_HEAD + ROW_TAIL));
    }

    #[test]
    fn exact_duplicates_consume_no_budget() {
        let items = vec![7, 7, 7, 8, 8, 9];
        let out = crush(items, |v| *v, |_| 0);
        assert_eq!(out.kept, vec![7, 8, 9]);
        assert_eq!(out.truncated, 0);
    }

    #[test]
    fn notice_names_narrowing_flags() {
        let n = row_notice(90, 150, 5, 60, "--limit/--field/-G");
        assert!(n.starts_with("…[truncated"), "{n}");
        assert!(n.contains("[truncated 90 of 150"), "{n}");
        assert!(n.contains("5/60 anomalies kept"), "{n}");
        assert!(n.contains("--limit/--field/-G"), "{n}");
    }

    #[test]
    fn notice_omits_anomaly_parenthetical_when_none() {
        let n = row_notice(51, 101, 0, 0, "--limit");
        assert_eq!(n, "…[truncated 51 of 101 rows; narrow with --limit]");
        assert!(!n.contains("anomalies kept"), "{n}");
    }

    #[test]
    fn anomaly_counts_cover_head_and_tail() {
        // Anomalies in head (5) and tail (145) must count even though they are
        // always kept; middle-only counting would report 0/0 while anomalies
        // are visible.
        let items: Vec<usize> = (0..150).collect();
        let out = crush(items, |v| *v, |v| u8::from(*v == 5 || *v == 145));
        assert_eq!((out.anomaly_kept, out.anomaly_total), (2, 2));
        assert!(out.kept.contains(&5) && out.kept.contains(&145));
        let n = row_notice(
            out.truncated,
            out.total,
            out.anomaly_kept,
            out.anomaly_total,
            "--limit",
        );
        assert!(n.contains("2/2 anomalies kept"), "{n}");
    }

    #[test]
    fn snapshot_notice_and_probe_shapes() {
        let with = row_notice(90, 150, 5, 60, "--limit/--field/-G");
        let without = row_notice(51, 101, 0, 0, "--limit");
        let probe = probe_notice(2, 5);
        insta::assert_snapshot!(format!("{with}\n{without}\n{probe}"));
    }

    #[test]
    fn snapshot_json_envelope_shape() {
        let rows = vec![json!({"id": "row-000"}), json!({"id": "row-149"})];
        let trunc = truncation_meta(98, 150, 1, 1, "--limit");
        let probe = probe_meta(2, 5);
        insta::assert_snapshot!(json_envelope(rows, Some(trunc), Some(probe)));
    }

    #[test]
    fn truncation_meta_omits_anomaly_keys_when_none() {
        let v = truncation_meta(51, 101, 0, 0, "--limit");
        assert_eq!(v["truncated"], 51);
        assert_eq!(v["total"], 101);
        assert!(v.get("anomaly_kept").is_none(), "{v}");
        assert!(v.get("anomaly_total").is_none(), "{v}");
        let v = truncation_meta(90, 150, 5, 60, "--limit");
        assert_eq!(v["anomaly_kept"], 5);
        assert_eq!(v["anomaly_total"], 60);
    }

    #[test]
    fn slice_matches_owned_keep_without_cloning() {
        // Borrowed keys prove the no-clone path compiles and keeps identically.
        let items: Vec<usize> = (0..150).collect();
        let owned = crush(items.clone(), |v| *v, |v| u8::from(*v == 100));
        let borrowed = crush_slice(&items, |v| *v, |v| u8::from(*v == 100));
        assert_eq!(borrowed.total, owned.total);
        assert_eq!(borrowed.truncated, owned.truncated);
        assert_eq!(
            (borrowed.anomaly_kept, borrowed.anomaly_total),
            (owned.anomaly_kept, owned.anomaly_total)
        );
        assert_eq!(borrowed.kept, owned.kept.iter().collect::<Vec<_>>());
        assert!(borrowed.kept.contains(&&100));
        assert!(!borrowed.kept.contains(&&60));
    }

    #[test]
    fn slice_severity_prefers_failed_over_running() {
        // 150 borrowed rows, every middle row mild except one severe at 130:
        // severity selection keeps 130 and drops a mild (e.g. 41).
        let rows: Vec<(String, u8)> = (0..150)
            .map(|i| {
                let s = if i == 130 {
                    2
                } else if (ROW_HEAD..150 - ROW_TAIL).contains(&i) {
                    1
                } else {
                    0
                };
                (format!("v-{i:03}"), s)
            })
            .collect();
        let out = crush_slice(&rows, |(v, _)| v.as_str(), |(_, s)| *s);
        assert_eq!((out.anomaly_kept, out.anomaly_total), (50, 100));
        assert!(
            out.kept.iter().any(|(v, _)| v == "v-130"),
            "severe middle row survives"
        );
        assert!(
            !out.kept.iter().any(|(v, _)| v == "v-100"),
            "mild middle row drops once severe outranks it"
        );
    }

    #[test]
    fn slice_borrows_string_keys_and_dedups() {
        let rows: Vec<(String, String)> = (0..150)
            .map(|i| (format!("v-{i:03}"), "passed".to_owned()))
            .collect();
        let out = crush_slice(&rows, |(v, s)| (v.as_str(), s.as_str()), |_| 0);
        assert_eq!(out.total, 150);
        assert_eq!(out.kept.len(), ROW_HEAD + ROW_TAIL);
        assert_eq!(out.kept[0].0, "v-000");
    }
}
