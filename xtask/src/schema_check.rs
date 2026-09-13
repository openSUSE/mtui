//! `cargo xtask schema-check` — a drift detector for the live
//! `/api/v2/schema` endpoint.
//!
//! Fetches the endpoint through `mtui_datasources::HttpClient`, parses both
//! sides as `serde_json::Value` and compares by **value**, never by bytes: the
//! live form is Mojo::JSON's compact, key-sorted canonical encoding, the
//! committed copy is pretty-printed, so a byte diff would be permanently red.
//! Read-only: a single `GET`, no `Authorization` header.

use std::collections::BTreeSet;

use anyhow::{Context, Result, bail};
use mtui_datasources::{HttpClient, MAX_API_BODY, VerifyPolicy};
use serde_json::Value;

/// The default live schema endpoint.
pub const DEFAULT_SCHEMA_URL: &str = "https://qam.suse.de/api/v2/schema";

/// The schema copy this repo commits, mirrored at
/// `crates/mtui-types/tests/fixtures/document/report-template-v1.json`.
const COMMITTED_SCHEMA: &str =
    include_str!("../../crates/mtui-types/tests/fixtures/document/report-template-v1.json");

/// Fetch `url` and compare it against the committed schema copy, printing the
/// differing pointers and exiting non-zero on drift.
///
/// # Errors
///
/// Returns an error if the endpoint cannot be fetched or parsed, or if the two
/// schemas differ (drift).
pub async fn run(url: &str, verify: VerifyPolicy) -> Result<()> {
    let http = HttpClient::new(verify).context("building the shared HTTP client")?;
    let bytes = http
        .get_bytes_capped(url, MAX_API_BODY)
        .await
        .with_context(|| format!("fetching {url}"))?;
    let live: Value = serde_json::from_slice(&bytes).context("parsing live schema as JSON")?;
    let committed: Value =
        serde_json::from_str(COMMITTED_SCHEMA).context("parsing committed schema as JSON")?;

    if live == committed {
        println!("schema-check: OK — live schema matches the committed copy (by value)");
        return Ok(());
    }

    let mut diffs = Vec::new();
    collect_diffs(&committed, &live, &mut String::new(), &mut diffs);
    eprintln!(
        "schema-check: DRIFT DETECTED — {} differing pointer(s):",
        diffs.len()
    );
    for diff in &diffs {
        eprintln!("  {diff}");
    }
    bail!("live schema at {url} no longer matches the committed copy");
}

/// Recursively collect human-readable `(pointer, description)` lines for every
/// value that differs between `committed` and `live`.
fn collect_diffs(committed: &Value, live: &Value, path: &mut String, out: &mut Vec<String>) {
    if committed == live {
        return;
    }
    match (committed, live) {
        (Value::Object(a), Value::Object(b)) => {
            let mut keys: BTreeSet<&String> = a.keys().collect();
            keys.extend(b.keys());
            for key in keys {
                let mark = path.len();
                path.push('/');
                path.push_str(key);
                match (a.get(key), b.get(key)) {
                    (Some(av), Some(bv)) => collect_diffs(av, bv, path, out),
                    (Some(_), None) => out.push(format!("{path}: removed from live")),
                    (None, Some(_)) => out.push(format!("{path}: added to live")),
                    (None, None) => unreachable!("key came from one of the two maps"),
                }
                path.truncate(mark);
            }
        }
        (Value::Array(a), Value::Array(b)) if a.len() == b.len() => {
            for (index, (av, bv)) in a.iter().zip(b.iter()).enumerate() {
                let mark = path.len();
                path.push('/');
                path.push_str(&index.to_string());
                collect_diffs(av, bv, path, out);
                path.truncate(mark);
            }
        }
        _ => {
            let pointer = if path.is_empty() { "/" } else { path.as_str() };
            out.push(format!("{pointer}: {committed} != {live}"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identical_values_produce_no_diffs() {
        let v = serde_json::json!({"a": 1, "b": [1, 2, {"c": "x"}]});
        let mut out = Vec::new();
        collect_diffs(&v, &v, &mut String::new(), &mut out);
        assert!(out.is_empty());
    }

    #[test]
    fn a_changed_leaf_reports_its_pointer() {
        let committed = serde_json::json!({"properties": {"kind": {"const": "1.0"}}});
        let live = serde_json::json!({"properties": {"kind": {"const": "1.1"}}});
        let mut out = Vec::new();
        collect_diffs(&committed, &live, &mut String::new(), &mut out);
        assert_eq!(out, vec!["/properties/kind/const: \"1.0\" != \"1.1\""]);
    }

    #[test]
    fn an_added_key_is_reported() {
        let committed = serde_json::json!({"a": 1});
        let live = serde_json::json!({"a": 1, "b": 2});
        let mut out = Vec::new();
        collect_diffs(&committed, &live, &mut String::new(), &mut out);
        assert_eq!(out, vec!["/b: added to live"]);
    }

    #[test]
    fn a_removed_key_is_reported() {
        let committed = serde_json::json!({"a": 1, "b": 2});
        let live = serde_json::json!({"a": 1});
        let mut out = Vec::new();
        collect_diffs(&committed, &live, &mut String::new(), &mut out);
        assert_eq!(out, vec!["/b: removed from live"]);
    }

    #[test]
    fn committed_schema_fixture_is_valid_json() {
        let _: Value = serde_json::from_str(COMMITTED_SCHEMA).unwrap();
    }
}
