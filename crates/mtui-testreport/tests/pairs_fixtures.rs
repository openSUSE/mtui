//! Smoke test for the committed paired fixtures
//! (`tests/fixtures/pairs/<RRID>/{document.json,metadata.json,log}`): every
//! `document.json` must parse into a [`ReportDocument`] and validate against
//! the committed schema copy.
//!
//! The schema-conformance check is a local copy of
//! `mtui-types/tests/schema_conformance.rs`'s helper, not a reuse of it: that
//! helper lives in another crate's integration-test binary, which cannot be
//! imported across a crate boundary.

use std::path::Path;
use std::sync::LazyLock;

use boon::{Compiler, Draft, SchemaIndex, Schemas};
use mtui_types::report_document::ReportDocument;
use serde_json::Value;

/// The four RRIDs harvested into `tests/fixtures/pairs/` (see
/// `PROVENANCE.md` alongside them).
const PAIRS: &[&str] = &[
    "SUSE:Maintenance:46456:424163",
    "SUSE:Maintenance:46572:423943",
    "SUSE:SLFO:1.2:7787",
    "SUSE:SLFO:1.2:7810",
];

fn pair_dir(rrid: &str) -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/pairs")
        .join(rrid)
}

const SCHEMA_JSON: &str =
    include_str!("../../mtui-types/tests/fixtures/document/report-template-v1.json");
const SCHEMA_LOC: &str = "https://qam.suse.de/schema/report-template-v1.json";

static COMPILED: LazyLock<(Schemas, SchemaIndex)> = LazyLock::new(|| {
    let schema_value: Value =
        serde_json::from_str(SCHEMA_JSON).expect("committed schema fixture is valid JSON");
    let mut compiler = Compiler::new();
    compiler.set_default_draft(Draft::V2019_09);
    compiler.enable_format_assertions();
    compiler
        .add_resource(SCHEMA_LOC, schema_value)
        .expect("adding the schema resource");
    let mut schemas = Schemas::new();
    let index = compiler
        .compile(SCHEMA_LOC, &mut schemas)
        .expect("compiling the committed schema");
    (schemas, index)
});

#[test]
fn every_pair_directory_has_all_three_files() {
    for rrid in PAIRS {
        let dir = pair_dir(rrid);
        for file in ["document.json", "metadata.json", "log"] {
            assert!(
                dir.join(file).is_file(),
                "{rrid}: missing {file} at {}",
                dir.display()
            );
        }
    }
}

#[test]
fn every_document_json_parses_as_a_report_document() {
    for rrid in PAIRS {
        let raw = std::fs::read_to_string(pair_dir(rrid).join("document.json"))
            .unwrap_or_else(|e| panic!("{rrid}: reading document.json: {e}"));
        let doc: ReportDocument = raw
            .parse()
            .unwrap_or_else(|e| panic!("{rrid}: document.json did not parse: {e}"));
        assert_eq!(doc.id, *rrid, "{rrid}: document id mismatch");
    }
}

#[test]
fn every_document_json_validates_against_the_committed_schema() {
    let (schemas, index) = &*COMPILED;
    for rrid in PAIRS {
        let raw = std::fs::read_to_string(pair_dir(rrid).join("document.json")).unwrap();
        let value: Value = serde_json::from_str(&raw).unwrap();
        schemas
            .validate(&value, *index)
            .unwrap_or_else(|e| panic!("{rrid}: schema validation failed: {e}"));
    }
}

/// The redaction discipline (`PROVENANCE.md`): no real name/email survives in
/// any committed fixture file.
#[test]
fn fixtures_carry_no_unredacted_pii() {
    let leaked = [
        "sirringhaus",
        "carvalho",
        "william.brown",
        "glaubitz",
        "Martin Pluskal",
        "mpluskal",
    ];
    for rrid in PAIRS {
        for file in ["document.json", "metadata.json", "log"] {
            let content = std::fs::read_to_string(pair_dir(rrid).join(file)).unwrap();
            for needle in leaked {
                assert!(
                    !content.contains(needle),
                    "{rrid}/{file}: unredacted PII {needle:?} found"
                );
            }
        }
    }
}
