//! Golden-vector tests for [`RequestReviewID`], ported from upstream
//! `mtui/tests/test_types.py`. These lock the RRID parse Contract: the exact
//! set of strings that parse, and the exact error category each malformed
//! string produces.

use mtui_types::{RequestKind, RequestReviewID, RridParseError};

/// Upstream `test_RRID_ok`: every listed template parses. The `{m}` slot is the
/// maintenance ID and `{r}` the review ID; for SLFO the maintenance ID is the
/// literal `1.1` string, otherwise a plain integer.
#[test]
fn valid_rrids_parse() {
    // (template, maintenance_id token, review_id token)
    let cases = [
        ("SUSE:Maintenance:{m}:{r}", "7", "42"),
        ("S:M:{m}:{r}", "7", "42"),
        ("SUSE:M:{m}:{r}", "7", "42"),
        ("S:Maintenance:{m}:{r}", "7", "42"),
        ("S:S:{m}:{r}", "1.1", "42"),
        ("SUSE:S:{m}:{r}", "1.1", "42"),
        ("SUSE:SLFO:{m}:{r}", "1.1", "42"),
        ("S:SLFO:{m}:{r}", "1.1", "42"),
    ];

    for (template, mid, rid) in cases {
        let input = template.replace("{m}", mid).replace("{r}", rid);
        let rrid = RequestReviewID::parse(&input)
            .unwrap_or_else(|e| panic!("expected {input:?} to parse, got {e}"));

        // project always normalises to the canonical long form.
        assert_eq!(rrid.project, "SUSE", "project for {input:?}");
        assert_eq!(rrid.review_id, 42, "review_id for {input:?}");

        if rrid.kind == RequestKind::Slfo {
            // Upstream: SLFO keeps the maintenance_id as the "1.1" string.
            assert_eq!(
                rrid.maintenance_id, "1.1",
                "SLFO maintenance_id for {input:?}"
            );
        } else {
            assert_eq!(rrid.maintenance_id, "7", "maintenance_id for {input:?}");
        }
    }
}

/// Upstream `test_parse_rrid_mc`: an absent component is a MissingComponent.
/// Four components are *required*, not merely a maximum, so any input with
/// one, two, or three components fails on the first absent one. The 3-token
/// `SUSE:Maintenance:1` case (only the review_id missing) locks the exact-count
/// lower bound alongside the upstream 1- and 2-token fixtures.
#[test]
fn missing_components_error() {
    for input in ["SUSE:Maintenance:1", "SUSE:Maintenance", "SUSE:M", "SUSE"] {
        let err = RequestReviewID::parse(input)
            .expect_err(&format!("expected {input:?} to fail with MissingComponent"));
        assert!(
            matches!(err, RridParseError::MissingComponent { .. }),
            "expected MissingComponent for {input:?}, got {err:?}"
        );
    }
}

/// Upstream `test_parse_rrid_cpe`: a component that fails its parser is a
/// ComponentParse error — unknown project (`DOOH`, `openSUSE`), unknown kind
/// (`boo`), a non-integer review ID (`aa`), or a single bogus token whose first
/// component isn't a valid project (`d131dd02c5e6eec4`).
#[test]
fn component_parse_errors() {
    for input in [
        "SUSE:Maintenance:1:aa",
        "d131dd02c5e6eec4",
        "DOOH:Maintenance:1:2",
        "openSUSE:boo:1:2",
    ] {
        let err = RequestReviewID::parse(input)
            .expect_err(&format!("expected {input:?} to fail with ComponentParse"));
        assert!(
            matches!(err, RridParseError::ComponentParse { .. }),
            "expected ComponentParse for {input:?}, got {err:?}"
        );
    }
}

/// Upstream `test_parse_rrid_long`: more than four components is TooMany.
#[test]
fn too_many_components_error() {
    let err = RequestReviewID::parse("SUSE:Maintenance:1:2:3")
        .expect_err("expected too-many-components failure");
    assert!(
        matches!(err, RridParseError::TooManyComponents { limit: 4 }),
        "got {err:?}"
    );
}

/// Upstream `test_str`: the canonical string round-trips.
#[test]
fn display_round_trips() {
    let rrid = "SUSE:Maintenance:1:2";
    assert_eq!(rrid, RequestReviewID::parse(rrid).unwrap().to_string());
}

/// Upstream `test_cmp`: equality is by canonical string identity, so the short
/// and long forms compare equal, and differing review IDs compare unequal.
#[test]
fn equality_is_by_canonical_identity() {
    let long = RequestReviewID::parse("SUSE:Maintenance:1:1").unwrap();
    let short = RequestReviewID::parse("S:M:1:1").unwrap();
    let other = RequestReviewID::parse("SUSE:Maintenance:1:2").unwrap();

    assert_eq!(long, short);
    assert_ne!(long, other);
}

/// Hashing agrees with equality (`S:M:1:1` and `SUSE:Maintenance:1:1` collide),
/// matching upstream's `__hash__` = `hash(str(self))`.
#[test]
fn equal_rrids_hash_equally() {
    use std::collections::HashSet;

    let mut set = HashSet::new();
    set.insert(RequestReviewID::parse("SUSE:Maintenance:1:1").unwrap());
    // Inserting the short form of the same RRID must not grow the set.
    assert!(!set.insert(RequestReviewID::parse("S:M:1:1").unwrap()));
    assert_eq!(set.len(), 1);
}
