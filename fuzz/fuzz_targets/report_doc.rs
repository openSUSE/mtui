//! Fuzzes the TeReGen report document: the parser every fetched body goes
//! through, the ingest that maps it onto a report, and the repo derivation that
//! follows. The body is network-controlled. Besides "no panic" it asserts that a
//! parsed document survives the serialize/parse round trip mtui relies on when
//! it uploads one.
#![no_main]

use libfuzzer_sys::fuzz_target;
use mtui_config::Config;
use mtui_testreport::fuzzing::check_document;
use mtui_testreport::{ObsReport, PiReport, SlReport, TestReport, apply_document};
use mtui_types::report_document::{ReportDocument, parse_and_warn_on_dropped_keys};

/// The load's order: `ObsReport` derives its repos from the stored document.
fn ingest(mut report: impl TestReport, doc: &ReportDocument) {
    apply_document(report.base_mut(), doc);
    report.base_mut().document = Some(doc.clone());
    report.update_repos_parser();
}

fuzz_target!(|data: &[u8]| {
    let Ok(raw) = std::str::from_utf8(data) else {
        return;
    };
    let Ok(doc) = parse_and_warn_on_dropped_keys(raw) else {
        return;
    };

    let json = serde_json::to_string(&doc).expect("a parsed document serializes");
    let again: ReportDocument = json.parse().expect("a serialized document parses");
    assert_eq!(again, doc, "the document changed across a round trip");

    if check_document(&doc).is_err() {
        return;
    }
    // The report class comes from the RRID the operator loads, not from
    // `doc.kind`, and nothing checks that the two agree.
    ingest(SlReport::new(Config::default()), &doc);
    ingest(PiReport::new(Config::default()), &doc);
    ingest(ObsReport::new(Config::default()), &doc);
});
