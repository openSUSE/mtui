//! Sub-bead A (mtui-rs-0pe.1): `TestReport::read` + `make_testreport`.
//!
//! Ports the parse-and-populate slice of upstream `TestReport.read`
//! (`_open_and_parse` → `_parse_json` → `_enrich_issue_titles` →
//! `_update_repos_parse`) and the `UpdateID.make_testreport` factory
//! (`tr_factory` + `_checkout` + workflow/autoconnect selection).
//!
//! All tests run offline: `make_testreport` reads a template pre-placed on disk
//! (so the read-succeeds-first path is taken and no `svn` is spawned), and the
//! load-failure path points at an empty `template_dir` so the internal `svn`
//! checkout fails fast and the factory falls back to a [`NullReport`].

use std::path::{Path, PathBuf};

use mtui_config::options::Config;
use mtui_testreport::{ObsReport, TestReport, UpdateKind, make_testreport};
use mtui_types::{UpdateID, Workflow};

const RRID: &str = "SUSE:Maintenance:24993:275518";

/// The metadata envelope the checkout fixtures embed (a trimmed real report).
const METADATA_JSON: &str = r#"{
    "bugs": ["12345"],
    "jira": ["SLE-22357"],
    "category": "recommended",
    "packager": "slemke@suse.com",
    "rating": "low",
    "repository": "http://download.suse.de/ibs/SUSE:/Maintenance:/24993/",
    "rrid": "SUSE:Maintenance:24993:275518",
    "packages": {
        "15-SP3": ["sle-module-python2-release = 15.3-150300.59.4.1"]
    },
    "testplatform": [
        "base=sles(major=15,minor=sp3);arch=[x86_64];addon=python2(major=15,minor=sp3)"
    ]
}"#;

/// A `log` template body: two reference-host lines plus a bug title line the
/// `hosts` parser (`ReducedMetadataParser`) should pick up.
const LOG_TEMPLATE: &str = "\
Testreport for SUSE:Maintenance:24993:275518

  x86_64 (reference host: refhost-a.example.com)
  s390x (reference host: refhost-b.example.com)

Bug 12345 (\"broken thing\"):
";

/// `patchinfo.xml` carrying the real title for jira id SLE-22357 (the envelope
/// left it as the placeholder).
const PATCHINFO_XML: &str = r#"<patchinfo>
  <issue tracker="jsc" id="SLE-22357">Real Jira Title</issue>
  <issue tracker="bnc" id="99999">Unrelated Bug</issue>
</patchinfo>"#;

/// Builds a checkout directory `<root>/<rrid>/` with `log`, `metadata.json`, and
/// `project.xml` (so the OBS `update_repos_parser` resolves), plus an optional
/// `patchinfo.xml`. Returns the path to the `log` file.
fn make_checkout(root: &Path, with_patchinfo: bool) -> PathBuf {
    let dir = root.join(RRID);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("log"), LOG_TEMPLATE).unwrap();
    std::fs::write(dir.join("metadata.json"), METADATA_JSON).unwrap();
    // OBS `update_repos_parser` reads `project.xml` from `report_wd()`.
    std::fs::write(
        dir.join("project.xml"),
        concat!(
            "<project>\n",
            "  <repository name=\"SLE-15-x86_64\">\n",
            "    <path repository=\"update\" project=\"SUSE:SLE-15:Update\"/>\n",
            "    <releasetarget project=\"SLE-Product-SLES:15:x86_64\"/>\n",
            "  </repository>\n",
            "</project>\n"
        ),
    )
    .unwrap();
    if with_patchinfo {
        std::fs::write(dir.join("patchinfo.xml"), PATCHINFO_XML).unwrap();
    }
    dir.join("log")
}

fn cfg(template_dir: PathBuf) -> Config {
    let mut c = Config::default();
    c.template_dir = template_dir;
    c
}

/// `read` populates the base from the `log` (hosts parser) and `metadata.json`
/// (JSON parser), and sets `path`.
#[test]
fn read_populates_metadata_and_hosts() {
    let tmp = tempfile::tempdir().unwrap();
    let log = make_checkout(tmp.path(), false);

    let mut report = ObsReport::new(cfg(tmp.path().to_path_buf()));
    report.read(&log).expect("read should succeed");

    let base = report.base();
    // JSON envelope fields.
    assert_eq!(base.rrid.as_ref().unwrap().to_string(), RRID);
    assert_eq!(base.packager, "slemke@suse.com");
    assert_eq!(base.category, "recommended");
    assert_eq!(base.testplatforms.len(), 1);
    // Hosts parser picked up both reference hosts from the log.
    assert!(base.hostnames.contains("refhost-a.example.com"));
    assert!(base.hostnames.contains("refhost-b.example.com"));
    // Bug 12345 is listed in the JSON envelope, so after `_parse_json` the JSON
    // placeholder wins over the log's `Bug N ("title"):` line (upstream runs the
    // hosts parser first, then the JSON parser, which re-seeds the ids). Without
    // a patchinfo entry for 12345, the placeholder survives.
    assert_eq!(
        base.bugs.get("12345").map(String::as_str),
        Some("Description not available")
    );
    // `path` is recorded so `report_wd()` / update-repo parsing can resolve.
    assert_eq!(base.path.as_deref(), Some(log.as_path()));
    // OBS update-repo map was derived from project.xml during read.
    assert!(
        !base.update_repos.is_empty(),
        "update_repos should be parsed"
    );
}

/// `patchinfo.xml` overlays the real jira title onto the id the envelope carried
/// (leaving the id set authoritative — the unrelated bug 99999 is not added).
#[test]
fn read_enriches_issue_titles_from_patchinfo() {
    let tmp = tempfile::tempdir().unwrap();
    let log = make_checkout(tmp.path(), true);

    let mut report = ObsReport::new(cfg(tmp.path().to_path_buf()));
    report.read(&log).unwrap();

    let base = report.base();
    // The envelope's jira id got its real title from patchinfo.
    assert_eq!(
        base.jira.get("SLE-22357").map(String::as_str),
        Some("Real Jira Title")
    );
    // patchinfo's unrelated id 99999 was NOT introduced.
    assert!(!base.bugs.contains_key("99999"));
    assert!(!base.jira.contains_key("99999"));
}

/// A missing `log` file surfaces as an ENOENT template error (so the checkout
/// seam knows to attempt a fresh checkout).
#[test]
fn read_missing_log_is_not_found() {
    use mtui_testreport::ReadError;
    let tmp = tempfile::tempdir().unwrap();
    let log = tmp.path().join(RRID).join("log");

    let mut report = ObsReport::new(cfg(tmp.path().to_path_buf()));
    let err = report.read(&log).unwrap_err();
    match err {
        ReadError::Template(e) => assert!(e.is_not_found()),
        other => panic!("expected Template(ENOENT), got {other:?}"),
    }
}

/// A present `log` with a missing `metadata.json` is `MetadataMissing`.
#[test]
fn read_missing_metadata_errors() {
    use mtui_testreport::ReadError;
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join(RRID);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("log"), LOG_TEMPLATE).unwrap();

    let mut report = ObsReport::new(cfg(tmp.path().to_path_buf()));
    let err = report.read(&dir.join("log")).unwrap_err();
    assert!(matches!(err, ReadError::MetadataMissing));
}

/// A present-but-invalid `metadata.json` is `MetadataInvalid`.
#[test]
fn read_invalid_metadata_errors() {
    use mtui_testreport::ReadError;
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join(RRID);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("log"), LOG_TEMPLATE).unwrap();
    std::fs::write(dir.join("metadata.json"), "{ not json").unwrap();

    let mut report = ObsReport::new(cfg(tmp.path().to_path_buf()));
    let err = report.read(&dir.join("log")).unwrap_err();
    assert!(matches!(err, ReadError::MetadataInvalid));
}

/// `make_testreport` with an on-disk template loads it, selects the OBS report
/// (Maintenance kind), sets the AUTO workflow, and — since `-a` autoconnects by
/// default — marks the report autoconnect-pending.
#[tokio::test]
async fn make_testreport_auto_loads_and_marks_pending() {
    let tmp = tempfile::tempdir().unwrap();
    make_checkout(tmp.path(), false);
    let update = UpdateID::parse(RRID).unwrap();

    let report = make_testreport(
        &update,
        cfg(tmp.path().to_path_buf()),
        UpdateKind::Auto,
        true,
    )
    .await;

    assert_eq!(report.id(), RRID);
    assert_eq!(report.workflow(), Workflow::Auto);
    assert!(
        report.base().autoconnect_pending,
        "auto -a should defer a connect"
    );
}

/// The kernel kind (`-k`) starts the KERNEL workflow and does **not** autoconnect
/// even when `autoconnect=true` (matching upstream's `autoconnect=False` default
/// for `KernelOBSUpdateID.make_testreport`).
#[tokio::test]
async fn make_testreport_kernel_does_not_autoconnect() {
    let tmp = tempfile::tempdir().unwrap();
    make_checkout(tmp.path(), false);
    let update = UpdateID::parse(RRID).unwrap();

    let report = make_testreport(
        &update,
        cfg(tmp.path().to_path_buf()),
        UpdateKind::Kernel,
        true,
    )
    .await;

    assert_eq!(report.workflow(), Workflow::Kernel);
    assert!(
        !report.base().autoconnect_pending,
        "kernel -k must not autoconnect"
    );
}

/// Even for `-a`, an explicit `autoconnect=false` (e.g. `--sut` at startup)
/// suppresses the deferred connect.
#[tokio::test]
async fn make_testreport_auto_respects_explicit_no_autoconnect() {
    let tmp = tempfile::tempdir().unwrap();
    make_checkout(tmp.path(), false);
    let update = UpdateID::parse(RRID).unwrap();

    let report = make_testreport(
        &update,
        cfg(tmp.path().to_path_buf()),
        UpdateKind::Auto,
        false,
    )
    .await;

    assert_eq!(report.workflow(), Workflow::Auto);
    assert!(!report.base().autoconnect_pending);
}

/// When neither the template is on disk nor a checkout can succeed, the factory
/// falls back to a `NullReport` (upstream returns `NullTestReport`) — never an
/// error. `svn_path` is pointed at a bare local `file://` path so `svn co` fails
/// immediately and offline (no network / SSH).
#[tokio::test]
async fn make_testreport_falls_back_to_null_on_load_failure() {
    let tmp = tempfile::tempdir().unwrap();
    // No checkout placed; force the internal `svn co` to fail fast offline by
    // aiming it at a nonexistent local file:// repository.
    let mut config = cfg(tmp.path().to_path_buf());
    config.svn_path = format!("file://{}/no-such-svn-repo", tmp.path().display());
    let update = UpdateID::parse(RRID).unwrap();

    let report = make_testreport(&update, config, UpdateKind::Auto, true).await;

    assert!(
        !report.is_loaded(),
        "unloaded report should be the null object"
    );
    assert_eq!(report.id(), "");
}
