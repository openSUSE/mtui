//! Authoring for `openqa_overview`'s three row sets.
//!
//! Confirmed lossy against `OpenqaIncident`'s typed `summary`/`failures`
//! shape by the step 1 gap analysis (`plans/phase4-authoring.md`): neither
//! `VersionResult` nor `GroupResult` carries a `flavor`/`archs` breakdown,
//! and `BuildCheckResult` has no version/flavor/arch dimension at all.
//! Inventing those fields would violate P4-D6, so the rows are parked
//! verbatim under three named `testing.openqa.extra` keys instead — never
//! coerced into `OpenqaIncident` (schema-growth request T13 tracks a proper
//! home upstream).

use std::collections::BTreeMap;

use mtui_datasources::OpenQAOverviewResult;
use serde_json::Value;

/// Named `testing.openqa.extra` keys the three row sets park under.
const SINGLE_INCIDENTS_KEY: &str = "single_incidents";
const AGGREGATED_UPDATES_KEY: &str = "aggregated_updates";
const BUILD_CHECKS_KEY: &str = "build_checks";

/// Builds the `testing.openqa.extra` entries for `overview`'s row sets.
///
/// A row set with no rows contributes no key at all (an absent key, not an
/// invented empty array) — `has_overview` already gates whether this is
/// called.
#[must_use]
pub fn openqa_extra_from_overview(overview: &OpenQAOverviewResult) -> BTreeMap<String, Value> {
    let mut extra = BTreeMap::new();
    insert_if_non_empty(&mut extra, SINGLE_INCIDENTS_KEY, &overview.single_incidents);
    insert_if_non_empty(
        &mut extra,
        AGGREGATED_UPDATES_KEY,
        &overview.aggregated_updates,
    );
    insert_if_non_empty(&mut extra, BUILD_CHECKS_KEY, &overview.build_checks);
    extra
}

/// Inserts `rows` under `key`, serialized as-is, unless empty.
fn insert_if_non_empty<T: serde::Serialize>(
    extra: &mut BTreeMap<String, Value>,
    key: &str,
    rows: &[T],
) {
    if rows.is_empty() {
        return;
    }
    match serde_json::to_value(rows) {
        Ok(value) => {
            extra.insert(key.to_owned(), value);
        }
        Err(e) => {
            // Every field of `VersionResult`/`GroupResult`/`BuildCheckResult`
            // is a plain String/usize/Vec — serialization cannot fail for
            // these types. Logged rather than `expect`ed so a future field
            // addition degrades instead of panicking a pure function.
            tracing::error!("failed to serialize {key} into testing.openqa.extra: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use mtui_datasources::{BuildCheckResult, GroupResult, VersionResult};

    use super::*;

    #[test]
    fn empty_overview_yields_no_keys() {
        let extra = openqa_extra_from_overview(&OpenQAOverviewResult::default());
        assert!(extra.is_empty());
    }

    #[test]
    fn each_non_empty_row_set_gets_its_own_key() {
        let overview = OpenQAOverviewResult {
            single_incidents: vec![VersionResult {
                version: "15-SP5".into(),
                status: "passed".into(),
                ..Default::default()
            }],
            aggregated_updates: vec![GroupResult {
                group: "core".into(),
                ..Default::default()
            }],
            build_checks: vec![BuildCheckResult {
                url: "https://qam/x.log".into(),
                ..Default::default()
            }],
            skip_aggregated: false,
        };
        let extra = openqa_extra_from_overview(&overview);
        assert_eq!(extra.len(), 3);
        assert!(extra.contains_key(SINGLE_INCIDENTS_KEY));
        assert!(extra.contains_key(AGGREGATED_UPDATES_KEY));
        assert!(extra.contains_key(BUILD_CHECKS_KEY));
        assert_eq!(
            extra[SINGLE_INCIDENTS_KEY][0]["version"],
            Value::String("15-SP5".into())
        );
    }

    #[test]
    fn empty_row_set_contributes_no_key() {
        let overview = OpenQAOverviewResult {
            single_incidents: vec![VersionResult::default()],
            ..Default::default()
        };
        let extra = openqa_extra_from_overview(&overview);
        assert_eq!(extra.len(), 1);
        assert!(!extra.contains_key(AGGREGATED_UPDATES_KEY));
        assert!(!extra.contains_key(BUILD_CHECKS_KEY));
    }
}
