//! Assembles authoring's typed subtrees onto a real fetched document, and
//! validates the result against the committed schema.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::LazyLock;

use boon::{Compiler, Draft, SchemaIndex, Schemas};
use mtui_testreport::authoring::author_document;
use mtui_testreport::{ManualHost, install_from_hosts};
use mtui_types::hostlog::HostLog;
use mtui_types::package::Package;
use mtui_types::report_document::ReportDocument;
use mtui_types::system::SystemProduct;
use serde_json::Value;

/// The same four RRIDs `pairs_fixtures.rs` harvested (see its
/// `PROVENANCE.md`); duplicated rather than shared, matching that file's own
/// precedent (each test file is a self-contained unit here, not a shared
/// module).
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

/// A synthetic connected host matching the fixture's first `install.targets[]`
/// row, so `install_from_hosts` has something real to join against. None of
/// the paired fixtures carry live host-connection data (they are captured
/// `report.json` bodies, not mtui sessions), so this is invented test input,
/// not derived from the fixture.
fn host_for(document: &ReportDocument) -> ManualHost {
    let target = document
        .install
        .targets
        .first()
        .expect("fixture has at least one install target");
    let product = SystemProduct::new(&target.product, &target.version, &target.arch);
    let mut pkg = Package::new("bash");
    pkg.set_before(Some("1-1")).unwrap();
    pkg.set_after(Some("1-2")).unwrap();
    ManualHost {
        hostname: "authoring-test-host".to_owned(),
        system: product.to_string(),
        product,
        packages: vec![pkg],
        hostlog: HostLog::new(),
    }
}

/// Authors `testing.install` (the only subtree these fixtures give us real
/// join material for) onto `document`, in place.
fn author_install_onto(document: &mut ReportDocument) {
    let host = host_for(document);
    let install = install_from_hosts(&[host], &document.install);
    author_document(document, Some(install), None, None, BTreeMap::new(), None);
}

/// Authoring onto a real fetched document leaves `update`, `install`
/// and `issues` byte-equal to the input; only the authored pointers move.
///
/// *Mutation observed red*: making `author_document` also touch
/// `document.update` (e.g. clearing `packager`) turns the `update`
/// byte-equality assertion red.
#[test]
fn authoring_onto_a_real_document_leaves_update_install_issues_byte_equal() {
    for rrid in PAIRS {
        let raw = std::fs::read_to_string(pair_dir(rrid).join("document.json")).unwrap();
        let original: Value = serde_json::from_str(&raw).unwrap();
        let mut document: ReportDocument = raw.parse().unwrap();

        author_install_onto(&mut document);

        let round_tripped = serde_json::to_value(&document).unwrap();
        for key in ["update", "install", "issues"] {
            assert_eq!(
                original[key], round_tripped[key],
                "{rrid}: {key} must stay byte-equal to the input"
            );
        }
        assert_ne!(
            original["testing"], round_tripped["testing"],
            "{rrid}: testing must have moved"
        );
    }
}

const SCHEMA_JSON: &str =
    include_str!("../../mtui-types/tests/fixtures/document/report-template-v1.json");
const SCHEMA_LOC: &str = "https://qam.suse.de/schema/report-template-v1.json";

/// A local copy of `mtui-types/tests/schema_conformance.rs`'s compiled
/// schema, for the same cross-crate-test-binary reason `pairs_fixtures.rs`
/// keeps its own copy.
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

/// Every golden authoring produces validates against the committed
/// schema, format assertions on.
#[test]
fn every_authored_golden_validates_against_the_committed_schema() {
    let (schemas, index) = &*COMPILED;
    for rrid in PAIRS {
        let raw = std::fs::read_to_string(pair_dir(rrid).join("document.json")).unwrap();
        let mut document: ReportDocument = raw.parse().unwrap();
        author_install_onto(&mut document);
        let value = serde_json::to_value(&document).unwrap();
        schemas
            .validate(&value, *index)
            .unwrap_or_else(|e| panic!("{rrid}: authored golden failed conformance: {e}"));
    }
}

/// The `Req<T>` trap, on an authored golden rather than a hand-built one:
/// deleting a required-nullable key entirely (rather than serializing it as
/// `null`) must fail conformance.
#[test]
fn a_broken_golden_missing_a_required_nullable_key_fails_validation() {
    let (schemas, index) = &*COMPILED;
    let raw = std::fs::read_to_string(pair_dir(PAIRS[0]).join("document.json")).unwrap();
    let mut document: ReportDocument = raw.parse().unwrap();
    author_install_onto(&mut document);
    let mut value = serde_json::to_value(&document).unwrap();
    value.as_object_mut().unwrap().remove("verdict");
    let err = schemas
        .validate(&value, *index)
        .expect_err("a missing required-nullable key must fail conformance");
    assert!(format!("{err}").contains("verdict"), "{err}");
}
