//! Pure, I/O-free loader for the `refhosts.yml` document format.
//!
//! The file groups host rows under top-level *location* keys (`default:`,
//! `nuremberg:`, …), but location is a legacy grouping rather than a live query
//! dimension, so every group is merged into one flat list of [`Host`]s.
//!
//! This crate is I/O-free, so the loader takes the already-read YAML as a
//! `&str`; the `Path`-based loader and the resolver / search / query / slot
//! engine belong to `mtui-datasources`.
//!
//! # Load-time dedup
//! Rows are de-duplicated by [`Host::name`] **at load time** (first occurrence
//! wins), so `mtui-datasources` must *not* dedup again. The golden fixture has
//! only unique names, so a dedicated unit test below covers the dedup path.
//!
//! # Best-effort row handling
//! A malformed row (missing required field, wrong nesting) is dropped and logged
//! at `tracing::warn!` so one bad row never aborts the whole load. Only a
//! document-level YAML failure is fatal ([`RefhostsParseError`]).
//!
//! # Merge keys (`<<: *anchor`)
//! Merge keys are valid YAML, so a row using one is resolved rather than read as
//! an ordinary `<<` field.

use std::collections::HashSet;

use serde::Deserialize;

use crate::error::RefhostsParseError;
use crate::product::Host;

/// The top-level `refhosts.yml` shape: location key → list of raw host rows.
///
/// `serde-saphyr` builds no DOM, so rows are held as `serde_json::Value` (an
/// existing workspace dep that implements `Deserializer`). Keeping them untyped
/// first is what lets one malformed row be dropped without failing the document.
type RawDocument = std::collections::BTreeMap<String, Option<Vec<serde_json::Value>>>;

/// Parser options for `refhosts.yml`:
/// - `strict_booleans`: without it YAML 1.1 booleans (`no`, `on`, `y`, …) would
///   swallow hostnames and values that must stay strings.
/// - `reject_non_finite_typeless_float: false`: a single `.inf`/`.nan` anywhere
///   is otherwise document-fatal under `deserialize_any` — exactly the failure
///   mode this loader exists to prevent. `false` degrades it to a string, so
///   `Host::deserialize` drops just that row.
/// - `budget`: the default `max_nodes` (250,000) would cap the inventory at
///   roughly 21k hosts; the raised ceiling is still deliberate DoS hardening for
///   a document fetched over the network, not a disabled check.
fn options() -> serde_saphyr::Options {
    serde_saphyr::options! {
        strict_booleans: true,
        reject_non_finite_typeless_float: false,
        budget: serde_saphyr::budget! { max_nodes: 5_000_000, max_events: 10_000_000 },
    }
}

/// Parse a `refhosts.yml` document into a flat, de-duplicated list of hosts.
///
/// All top-level location groups are merged into one list (group order, then
/// row order, preserved). Rows are de-duplicated by [`Host::name`], keeping the
/// first occurrence. Malformed rows are dropped (logged at `warn`); a
/// document-level YAML failure returns [`RefhostsParseError`].
///
/// # Errors
/// Returns [`RefhostsParseError::Yaml`] if `yaml` is not a valid `refhosts.yml`
/// document (top-level mapping of location → row list).
pub fn load_refhosts(yaml: &str) -> Result<Vec<Host>, RefhostsParseError> {
    // An empty document is a valid, empty host list.
    let doc: RawDocument =
        match serde_saphyr::from_str_with_options::<Option<RawDocument>>(yaml, options())? {
            Some(doc) => doc,
            None => return Ok(Vec::new()),
        };

    let mut seen: HashSet<String> = HashSet::new();
    let mut hosts: Vec<Host> = Vec::new();

    for (location, rows) in doc {
        for row in rows.into_iter().flatten() {
            match Host::deserialize(row.clone()) {
                Ok(host) => {
                    if seen.insert(host.name.clone()) {
                        hosts.push(host);
                    } else {
                        tracing::debug!(
                            host = %host.name,
                            %location,
                            "refhosts: dropping duplicate host row (first occurrence wins)",
                        );
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        %location,
                        error = %e,
                        row = ?row,
                        "refhosts: dropping malformed host row",
                    );
                }
            }
        }
    }

    Ok(hosts)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::version::{Version, VersionField};

    #[test]
    fn merges_all_location_groups_into_one_flat_list() {
        let yaml = "\
default:
  - name: a
    arch: x86_64
    product:
      name: sles
nuremberg:
  - name: b
    arch: aarch64
    product:
      name: sles
";
        let hosts = load_refhosts(yaml).unwrap();
        let names: Vec<_> = hosts.iter().map(|h| h.name.as_str()).collect();
        assert_eq!(names, ["a", "b"]);
    }

    #[test]
    fn dedups_same_name_rows_keeping_first_occurrence() {
        let yaml = "\
default:
  - name: dup
    arch: x86_64
    product:
      name: sles
nuremberg:
  - name: dup
    arch: aarch64
    product:
      name: sled
";
        let hosts = load_refhosts(yaml).unwrap();
        assert_eq!(hosts.len(), 1);
        assert_eq!(hosts[0].name, "dup");
        assert_eq!(hosts[0].arch, "x86_64");
        assert_eq!(hosts[0].product.name, "sles");
    }

    #[test]
    fn drops_malformed_row_but_keeps_valid_ones() {
        // The middle row is missing the required `arch` field.
        let yaml = "\
default:
  - name: good1
    arch: x86_64
    product:
      name: sles
  - name: bad
    product:
      name: sles
  - name: good2
    arch: aarch64
    product:
      name: sles
";
        let hosts = load_refhosts(yaml).unwrap();
        let names: Vec<_> = hosts.iter().map(|h| h.name.as_str()).collect();
        assert_eq!(names, ["good1", "good2"]);
    }

    #[test]
    fn broken_yaml_returns_err() {
        let yaml = "default: [unclosed";
        assert!(load_refhosts(yaml).is_err());
    }

    #[test]
    fn empty_document_yields_no_hosts() {
        assert!(load_refhosts("").unwrap().is_empty());
        assert!(load_refhosts("---\n").unwrap().is_empty());
    }

    #[test]
    fn null_group_value_is_skipped() {
        let yaml = "\
empty:
present:
  - name: a
    arch: x86_64
    product:
      name: sles
";
        let hosts = load_refhosts(yaml).unwrap();
        assert_eq!(hosts.len(), 1);
        assert_eq!(hosts[0].name, "a");
    }

    #[test]
    fn preserves_structured_version_and_addons() {
        let yaml = "\
default:
  - name: h
    arch: x86_64
    product:
      name: sles
      version:
        major: 15
        minor: 5
    addons:
      - name: sdk
        version:
          major: 15
          minor: 5
";
        let hosts = load_refhosts(yaml).unwrap();
        let h = &hosts[0];
        assert_eq!(
            h.product.version,
            Some(Version::new(15u64, Some(VersionField::Num(5))))
        );
        assert_eq!(h.addons.len(), 1);
        assert_eq!(h.addons[0].name, "sdk");
    }

    // The four tests below pin the scalar/merge-key behaviour `options()`
    // selects; each names the mutation it must catch.

    #[test]
    fn leading_zero_minor_drops_the_row() {
        // `minor: 05` resolves to the float 5.0, matching neither
        // `VersionField::Num(u64)` nor `::Text(String)`, so the row drops like
        // any other malformed one — a silently smaller fleet, not an error.
        // Mutation: a parser default that read `05` as a string would keep the
        // row and break the `["good"]` expectation.
        let yaml = "\
default:
  - name: good
    arch: x86_64
    product:
      name: sles
  - name: leading-zero
    arch: x86_64
    product:
      name: sles
      version:
        major: 15
        minor: 05
";
        let hosts = load_refhosts(yaml).unwrap();
        let names: Vec<_> = hosts.iter().map(|h| h.name.as_str()).collect();
        assert_eq!(names, ["good"]);
    }

    #[test]
    fn underscore_grouped_minor_is_read_as_a_number_not_text() {
        // Underscore grouping is supported: `minor: 1_000` is the integer 1000
        // (`VersionField::Num`), not `Text("1_000")`.
        let yaml = "\
default:
  - name: h
    arch: x86_64
    product:
      name: sles
      version:
        major: 15
        minor: 1_000
";
        let hosts = load_refhosts(yaml).unwrap();
        assert_eq!(hosts.len(), 1);
        assert_eq!(
            hosts[0].product.version,
            Some(Version::new(15u64, Some(VersionField::Num(1_000))))
        );
    }

    #[test]
    fn non_finite_float_in_one_row_does_not_fail_the_whole_document() {
        // Mutation: `reject_non_finite_typeless_float: true` (the parser's own
        // default) makes one stray `.inf` document-fatal under `deserialize_any`
        // over the untyped rows, so `load_refhosts` would return `Err` instead
        // of both hosts with `.inf` degraded to a string.
        let yaml = "\
default:
  - name: good
    arch: x86_64
    product:
      name: sles
  - name: infinite-arch
    arch: .inf
    product:
      name: sles
";
        let hosts = load_refhosts(yaml).expect("a stray .inf must not fail the whole document");
        let names: Vec<_> = hosts.iter().map(|h| h.name.as_str()).collect();
        assert_eq!(names, ["good", "infinite-arch"]);
        assert_eq!(hosts[1].arch, ".inf");
    }

    #[test]
    fn merge_key_row_expands_into_a_second_host() {
        // Mutation: `merge_keys` set to `AsOrdinary` or `Error` instead of
        // `Merge` drops the merged row (a `<<` key does not deserialize into
        // `Host`), so `hosts.len()` would fall to 1.
        let yaml = "\
default:
  - &base
    name: base-host
    arch: x86_64
    product:
      name: sles
  - <<: *base
    name: overridden-host
";
        let hosts = load_refhosts(yaml).unwrap();
        let names: Vec<_> = hosts.iter().map(|h| h.name.as_str()).collect();
        assert_eq!(names, ["base-host", "overridden-host"]);
        assert_eq!(hosts[1].arch, "x86_64");
        assert_eq!(hosts[1].product.name, "sles");
    }
}
