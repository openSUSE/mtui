//! The wall clock, as Unix seconds.
//!
//! The workspace pins `chrono` without the `clock` feature (single-static-
//! binary contract), so "now" comes from [`std::time::SystemTime`] rather than
//! `chrono::Utc::now()`.

use std::time::{SystemTime, UNIX_EPOCH};

/// The current Unix timestamp in seconds, `0` for a clock before the epoch.
pub(crate) fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

/// The current instant as an RFC3339 UTC timestamp.
pub(crate) fn now_rfc3339() -> String {
    chrono::DateTime::from_timestamp(unix_now(), 0)
        .unwrap_or_default()
        .to_rfc3339()
}

/// The current UTC date.
pub(crate) fn utc_today() -> chrono::NaiveDate {
    chrono::DateTime::from_timestamp(unix_now(), 0)
        .map(|dt| dt.date_naive())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_three_views_agree_on_the_same_instant() {
        let before = unix_now();
        let stamp = now_rfc3339();
        let parsed = chrono::DateTime::parse_from_rfc3339(&stamp).unwrap();
        assert!((parsed.timestamp() - before).abs() <= 5, "{stamp}");
        assert!((utc_today() - parsed.date_naive()).num_days().abs() <= 1);
    }
}
