//! Re-export idempotency tests for the export pipeline.
//!
//! Ports upstream `tests/test_export_idempotency.py` — regression tests for
//! four related bugs (upstream `c870fe58`), each of which made a second
//! `export` of the same testreport degrade the template instead of being a
//! no-op refresh (duplicate headers/notices, over-deletion to end of file,
//! dead stale-line cleanup).

use mtui_config::options::Config;
use mtui_testreport::{AutoExport, ExportContext, KernelExport, ManualExport, ManualHost};
use mtui_types::hostlog::HostLog;

const FOOTER: &str = "## export MTUI:12.0, paramiko 3.5 on SLES-15 (kernel: 6.4) by tester\n";
const RRID: &str = "SUSE:Maintenance:12358:199773";

/// Mirrors upstream `_config`: a config with the fixed reports URL / install
/// logs dir the fixtures assume.
fn config(tmp: &std::path::Path) -> Config {
    let mut cfg = Config::default();
    cfg.template_dir = tmp.to_path_buf();
    cfg.install_logs = std::path::PathBuf::from("install_logs");
    cfg.reports_url = "https://reports".to_string();
    cfg.session_user = "tester".to_string();
    cfg
}

/// Builds an `ExportContext` over `template` (the shared base the exporters
/// embed; upstream drives these methods through a concrete exporter, but they
/// all live on `BaseExport`).
fn ctx(tmp: &std::path::Path, template: &[&str]) -> ExportContext {
    let lines: Vec<String> = template.iter().map(|s| (*s).to_string()).collect();
    ExportContext::new(config(tmp), &lines, false, RRID.parse().unwrap())
}

fn count(template: &[String], line: &str) -> usize {
    template.iter().filter(|l| l.as_str() == line).count()
}

fn index_of(template: &[String], line: &str) -> usize {
    template
        .iter()
        .position(|l| l.as_str() == line)
        .unwrap_or_else(|| panic!("line not found: {line:?}"))
}

// ---------------------------------------------------------------------------
// 31: auto install_results must bound its replacement at the export footer
// ---------------------------------------------------------------------------

#[test]
fn auto_install_results_bounds_at_footer() {
    // No 'Links for update logs:' section: the fallback bound is the footer.
    // The footer line is '## export MTUI:<version> ...', never exactly
    // '## export MTUI:', so a whole-line match fallback would run to
    // len(template) and delete the footer.
    let tmp = tempfile::tempdir().unwrap();
    let mut ex = AutoExport::new(
        ctx(
            tmp.path(),
            &[
                "some intro\n",
                "##############\n",
                "Install tests:\n",
                "##############\n",
                "\n",
                "old status line\n",
                "\n",
                FOOTER,
            ],
        ),
        None,
        None,
    );

    ex.install_results();

    let t = &ex.ctx.template;
    assert_eq!(count(t, FOOTER), 1, "footer survived");
    assert_eq!(t.last().unwrap(), FOOTER);
    assert!(t.iter().any(|l| l == "Install tests:\n"));
    assert!(
        !t.iter().any(|l| l == "old status line\n"),
        "section body replaced"
    );
}

// ---------------------------------------------------------------------------
// 32: installlogs_lines must reuse an existing 'Links for update logs:' header
// ---------------------------------------------------------------------------

#[test]
fn installlogs_lines_reuses_existing_header() {
    let tmp = tempfile::tempdir().unwrap();
    let old_link = format!("https://reports/{RRID}/install_logs/old.log\n");
    let mut c = ctx(
        tmp.path(),
        &[
            "body\n",
            "\n",
            "Links for update logs:\n",
            "\n",
            &old_link,
            "\n",
            FOOTER,
        ],
    );

    c.installlogs_lines(&["old.log".to_string(), "new.log".to_string()]);

    let t = &c.template;
    assert_eq!(count(t, "Links for update logs:\n"), 1);
    assert_eq!(count(t, &old_link), 1, "still de-duplicated");
    let new_link = format!("https://reports/{RRID}/install_logs/new.log\n");
    assert_eq!(count(t, &new_link), 1);
    // both links live under the one header, before the footer
    let header = index_of(t, "Links for update logs:\n");
    assert!(header < index_of(t, &new_link));
    assert!(index_of(t, &new_link) < index_of(t, FOOTER));
}

#[test]
fn installlogs_lines_twice_is_idempotent() {
    let tmp = tempfile::tempdir().unwrap();
    let mut c = ctx(tmp.path(), &["body\n", FOOTER]);

    c.installlogs_lines(&["a.log".to_string()]);
    c.installlogs_lines(&["a.log".to_string()]);

    let t = &c.template;
    assert_eq!(count(t, "Links for update logs:\n"), 1);
    assert_eq!(
        count(t, &format!("https://reports/{RRID}/install_logs/a.log\n")),
        1
    );
}

// ---------------------------------------------------------------------------
// 33: base install_results notice must not multiply on kernel re-export
// ---------------------------------------------------------------------------

#[test]
fn kernel_install_notice_not_duplicated_on_reexport() {
    let tmp = tempfile::tempdir().unwrap();
    let notice = "All installation tests done in openQA please see installlogs section\n";
    let mut ex = KernelExport::new(
        ctx(
            tmp.path(),
            &[
                "Test results by product-arch:\n",
                "(x)\n",
                "(y)\n",
                "\n",
                "tail\n",
            ],
        ),
        Vec::new(),
        None,
    );

    ex.ctx.install_results();
    ex.ctx.install_results(); // second export

    assert_eq!(count(&ex.ctx.template, notice), 1);
}

// ---------------------------------------------------------------------------
// 34: manual install_results stale-result cleanup must actually work
// ---------------------------------------------------------------------------

/// Mirrors upstream `_session_host`: a decoupled host view with no packages.
fn session_host(hostname: &str, system: &str) -> ManualHost {
    ManualHost {
        hostname: hostname.to_string(),
        system: system.to_string(),
        packages: Vec::new(),
        hostlog: HostLog::new(),
    }
}

#[test]
fn manual_install_results_strips_stale_lines_for_session_hosts() {
    // Stale per-command result lines are refreshed for session hosts only.
    let tmp = tempfile::tempdir().unwrap();
    let mut ex = ManualExport::new(
        ctx(
            tmp.path(),
            &[
                "system1 (reference host: h1)\n",
                "old-cmd : FAILED\n",
                "good line\n",
                "othersys (reference host: elsewhere)\n",
                "their-cmd : SUCCEEDED\n",
            ],
        ),
        vec![session_host("h1", "system1")],
        None,
        None,
    );

    ex.install_results();

    let t = &ex.ctx.template;
    // both host section headers survive
    assert!(t.iter().any(|l| l == "system1 (reference host: h1)\n"));
    assert!(
        t.iter()
            .any(|l| l == "othersys (reference host: elsewhere)\n")
    );
    // the session host's stale result line is gone, its other content kept
    assert!(!t.iter().any(|l| l == "old-cmd : FAILED\n"));
    assert!(t.iter().any(|l| l == "good line\n"));
    // a host NOT in this session keeps its result lines untouched
    assert!(t.iter().any(|l| l == "their-cmd : SUCCEEDED\n"));
}

#[test]
fn manual_cleanup_stops_at_host_section_end() {
    // The deletion window must not bleed past the host block: tester-authored
    // lines like 'reproducer : FAILED before update' in the regression-tests
    // notes must survive.
    let tmp = tempfile::tempdir().unwrap();
    let mut ex = ManualExport::new(
        ctx(
            tmp.path(),
            &[
                "system1 (reference host: h1)\n",
                "old-cmd : FAILED\n",
                "comment: (none)\n",
                "\n",
                "regression tests:\n",
                "=================\n",
                "bsc#1234 reproducer : FAILED before update\n",
                "bsc#1234 reproducer : SUCCEEDED after update\n",
                "\n",
            ],
        ),
        vec![session_host("h1", "system1")],
        None,
        None,
    );

    ex.install_results();

    let t = &ex.ctx.template;
    assert!(
        !t.iter().any(|l| l == "old-cmd : FAILED\n"),
        "in-section stale line removed"
    );
    // tester content after the host block survives
    assert!(
        t.iter()
            .any(|l| l == "bsc#1234 reproducer : FAILED before update\n")
    );
    assert!(
        t.iter()
            .any(|l| l == "bsc#1234 reproducer : SUCCEEDED after update\n")
    );
    assert!(t.iter().any(|l| l == "comment: (none)\n"));
}

#[test]
fn installlogs_trailing_blank_does_not_accumulate() {
    // A re-export that adds one new link must not add another blank line: the
    // section already ends with a blank, so the trailing-blank insert is guarded.
    let tmp = tempfile::tempdir().unwrap();
    let old_link = format!("https://reports/{RRID}/install_logs/old.log\n");
    let mut c = ctx(
        tmp.path(),
        &["Links for update logs:\n", "\n", &old_link, "\n", FOOTER],
    );

    c.installlogs_lines(&["new.log".to_string()]);

    let t = &c.template;
    let new_link = format!("https://reports/{RRID}/install_logs/new.log\n");
    assert_eq!(count(t, &new_link), 1);
    // exactly one blank between the links and the footer
    let footer_at = index_of(t, FOOTER);
    assert_eq!(t[footer_at - 1], "\n");
    assert_ne!(t[footer_at - 2], "\n");
}

#[test]
fn installlogs_converges_damaged_stacked_headers() {
    // Templates damaged by the old bug converge back to one section.
    let tmp = tempfile::tempdir().unwrap();
    let old_link = format!("https://reports/{RRID}/install_logs/old.log\n");
    let mut c = ctx(
        tmp.path(),
        &[
            "body\n",
            "\n",
            "Links for update logs:\n",
            "\n",
            &old_link,
            "\n",
            "\n",
            "Links for update logs:\n",
            "\n",
            "\n",
            "Links for update logs:\n",
            "\n",
            FOOTER,
        ],
    );

    c.installlogs_lines(&["old.log".to_string()]);

    let t = &c.template;
    assert_eq!(count(t, "Links for update logs:\n"), 1);
    assert_eq!(count(t, &old_link), 1);
    assert_eq!(t.last().unwrap(), FOOTER);
}

#[test]
fn installlogs_hand_trimmed_section_keeps_links_before_footer() {
    // Header directly followed by the footer: links must not land after it.
    let tmp = tempfile::tempdir().unwrap();
    let mut c = ctx(tmp.path(), &["body\n", "Links for update logs:\n", FOOTER]);

    c.installlogs_lines(&["a.log".to_string()]);

    let t = &c.template;
    let link = format!("https://reports/{RRID}/install_logs/a.log\n");
    assert!(index_of(t, "Links for update logs:\n") < index_of(t, &link));
    assert!(index_of(t, &link) < index_of(t, FOOTER));
    assert_eq!(t.last().unwrap(), FOOTER);
}

#[test]
fn kernel_install_notice_converges_from_damaged_template() {
    // Notices stacked by pre-fix exports are reduced back to one.
    let tmp = tempfile::tempdir().unwrap();
    let notice = "All installation tests done in openQA please see installlogs section\n";
    let mut ex = KernelExport::new(
        ctx(
            tmp.path(),
            &[
                "Test results by product-arch:\n",
                "(x)\n",
                "(y)\n",
                notice,
                "\n",
                notice,
                "\n",
                notice,
                "\n",
                "tail\n",
            ],
        ),
        Vec::new(),
        None,
    );

    ex.ctx.install_results();

    let t = &ex.ctx.template;
    assert_eq!(count(t, notice), 1);
    assert!(t.iter().any(|l| l == "tail\n"));
}
