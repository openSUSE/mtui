//! `cargo xtask schema-check` — a drift detector for the live
//! `/api/v2/schema` endpoint.
//!
//! Fetches the endpoint through `mtui_datasources::HttpClient`, parses both
//! sides as `serde_json::Value` and compares by **value**, never by bytes: the
//! live form is Mojo::JSON's compact, key-sorted canonical encoding, the
//! committed copy is pretty-printed, so a byte diff would be permanently red.
//! Read-only: a single `GET`, no `Authorization` header.

use anyhow::{Context, Result, bail};
use mtui_datasources::{HttpClient, MAX_API_BODY, VerifyPolicy};
use mtui_types::report_document::schema_diffs;
use serde_json::Value;

/// The default live schema endpoint.
pub const DEFAULT_SCHEMA_URL: &str = "https://qam.suse.de/api/v2/schema";

/// The schema copy this repo commits, at
/// `crates/mtui-types/schema/report-template-v1.json`.
const COMMITTED_SCHEMA: &str = mtui_types::report_document::SCHEMA_JSON;

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

    let diffs = schema_diffs(&committed, &live);
    eprintln!(
        "schema-check: DRIFT DETECTED — {} differing pointer(s):",
        diffs.len()
    );
    for diff in &diffs {
        eprintln!("  {diff}");
    }
    bail!("live schema at {url} no longer matches the committed copy");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn committed_schema_fixture_is_valid_json() {
        let _: Value = serde_json::from_str(COMMITTED_SCHEMA).unwrap();
    }
}
