//! Golden-vector tests for [`UpdateID`]: the thin wrapper over
//! [`RequestReviewID`] delegates faithfully — same parses, same error
//! categories, and a `Display` equal to the inner RRID's canonical string (the
//! per-update template-directory name downstream).

use mtui_types::{RequestReviewID, RridParseError, UpdateID};

/// Every RRID [`RequestReviewID::parse`] accepts also parses as an [`UpdateID`]
/// wrapping byte-for-byte the same RRID.
#[test]
fn valid_updateids_wrap_matching_rrid() {
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

        let uid = UpdateID::parse(&input)
            .unwrap_or_else(|e| panic!("expected {input:?} to parse, got {e}"));
        let rrid = RequestReviewID::parse(&input).unwrap();

        assert_eq!(uid.id, rrid, "inner id for {input:?}");
        assert_eq!(uid.to_string(), rrid.to_string(), "display for {input:?}");
    }
}

/// A malformed RRID fails through `UpdateID::parse` with the same error category
/// the RRID parser would raise — the wrapper adds no parsing of its own.
#[test]
fn invalid_updateids_delegate_error_category() {
    // (input, matcher description) — one representative per RRID error category.
    let missing = "SUSE:Maintenance:1";
    let component_parse = "SUSE:Maintenance:1:aa";
    let too_many = "SUSE:Maintenance:1:2:3";

    assert!(
        matches!(
            UpdateID::parse(missing).unwrap_err(),
            RridParseError::MissingComponent { .. }
        ),
        "expected MissingComponent for {missing:?}"
    );
    assert!(
        matches!(
            UpdateID::parse(component_parse).unwrap_err(),
            RridParseError::ComponentParse { .. }
        ),
        "expected ComponentParse for {component_parse:?}"
    );
    assert!(
        matches!(
            UpdateID::parse(too_many).unwrap_err(),
            RridParseError::TooManyComponents { limit: 4 }
        ),
        "expected TooManyComponents for {too_many:?}"
    );
}

/// The canonical string round-trips through the value type.
#[test]
fn display_round_trips() {
    let canonical = "SUSE:Maintenance:1:2";
    assert_eq!(canonical, UpdateID::parse(canonical).unwrap().to_string());
}
