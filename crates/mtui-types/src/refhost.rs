//! Pure, I/O-free loader for the `refhosts.yml` document format.
//!
//! Ported from upstream `mtui/hosts/refhost/store.py::Refhosts._parse_refhosts`.
//! The legacy `refhosts.yml` groups host rows under top-level *location* keys
//! (`default:`, `nuremberg:`, …). Location support has been retired upstream, so
//! every group is merged into a single flat list of [`Host`]s.
//!
//! This crate is deliberately I/O-free (see `AGENTS.md`), so the loader takes
//! the already-read YAML text as a `&str` rather than opening a file. The
//! `Path`-based loader, the resolver chain, and the search / query / slot engine
//! (`store.py`'s `Refhosts` class and `_RefhostsFactory`) belong to Phase 3
//! (`mtui-datasources`) and are intentionally *not* ported here.
//!
//! # Divergence from upstream: load-time dedup
//! Upstream flattens groups at load and defers de-duplication to `query()`.
//! This port de-duplicates by [`Host::name`] **at load time** (first occurrence
//! wins), so downstream consumers receive a canonical, duplicate-free list.
//! Phase 3 must therefore *not* dedup again. The golden fixture has only unique
//! names, so the dedup path is covered by a dedicated unit test below.
//!
//! # Best-effort row handling
//! Like upstream `_host_from_dict`, a single malformed row (missing required
//! field, wrong nesting) is dropped and logged at `tracing::warn!` so one bad
//! row never aborts the whole load. Only a document-level YAML parse failure is
//! fatal and surfaces as [`RefhostsParseError`].

use std::collections::HashSet;

use serde::Deserialize;

use crate::error::RefhostsParseError;
use crate::product::Host;

/// The top-level `refhosts.yml` shape: location key → list of raw host rows.
///
/// Rows are kept as `serde_yaml::Value` first so that a single malformed row can
/// be dropped without failing the whole document (mirroring upstream's
/// per-row `_host_from_dict` degradation).
type RawDocument = std::collections::BTreeMap<String, Option<Vec<serde_yaml::Value>>>;

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
    // An empty document is a valid, empty host list (upstream `raw or {}`).
    let doc: RawDocument = match serde_yaml::from_str::<Option<RawDocument>>(yaml)? {
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
        // Same host name in two groups; first-wins, and the arch of the first
        // occurrence is the one retained.
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
        // The middle row is missing the required `arch` field; it must be
        // dropped while the surrounding valid rows survive.
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
        // A location key with no rows (`empty:` → null) must not crash.
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
}
