//! Ports `tests/test_overview_inject.py` — the idempotent overview injector.
//!
//! The renderer half (`render_overview`) is unit-tested in
//! `mtui-datasources::oqa_search::render`; here we exercise the injector's
//! placement, idempotency, and blank-line invariants against a testreport-shaped
//! template.

use mtui_datasources::oqa_search::render::{OVERVIEW_BEGIN_MARKER, OVERVIEW_END_MARKER};
use mtui_datasources::{BuildCheckResult, GroupResult, VersionResult};
use mtui_testreport::export::overview_inject::inject_overview;

fn template_with_regression_section() -> Vec<String> {
    [
        "Maintenance Test Update Installer\n",
        "\n",
        "Some preamble line.\n",
        "\n",
        "regression tests:\n",
        "-----------------\n",
        "\n",
        "(put your details here)\n",
        "\n",
        "build log review:\n",
        "-----------------\n",
        "\n",
        "TEST_SUITE_PRESENT: NO\n",
    ]
    .into_iter()
    .map(String::from)
    .collect()
}

fn sample_overview() -> (Vec<VersionResult>, Vec<GroupResult>, Vec<BuildCheckResult>) {
    let single = vec![
        VersionResult {
            version: "15-SP5".into(),
            url: "https://oqa/u1".into(),
            status: "passed".into(),
            ..Default::default()
        },
        VersionResult {
            version: "15-SP4".into(),
            url: "https://oqa/u2".into(),
            status: "failed".into(),
            failed_count: 3,
            ..Default::default()
        },
    ];
    let aggregated = vec![GroupResult {
        group: "core".into(),
        versions: vec![VersionResult {
            version: "15-SP5".into(),
            url: "https://oqa/agg".into(),
            status: "passed".into(),
            ..Default::default()
        }],
    }];
    let build_checks = vec![BuildCheckResult {
        url: "https://qam/xz.x86_64.log".into(),
        matches: vec!["[   28s] All 9 tests passed".into()],
        ..Default::default()
    }];
    (single, aggregated, build_checks)
}

fn body(template: &[String]) -> String {
    template.concat()
}

fn count(hay: &str, needle: &str) -> usize {
    hay.matches(needle).count()
}

#[test]
fn inserts_block_under_regression_section() {
    let mut template = template_with_regression_section();
    let (s, a, b) = sample_overview();

    let modified = inject_overview(&mut template, &s, &a, &b, false);

    assert!(modified);
    let body = body(&template);
    assert!(body.contains(OVERVIEW_BEGIN_MARKER));
    assert!(body.contains(OVERVIEW_END_MARKER));
    assert!(body.contains("## OpenQA Overview"));
    assert!(body.contains("FAILED (3 jobs)"));

    let begin = body.find(OVERVIEW_BEGIN_MARKER).unwrap();
    let end = body.find(OVERVIEW_END_MARKER).unwrap();
    let next_section = body.find("build log review:").unwrap();
    let regression = body.find("regression tests:").unwrap();
    assert!(begin < end && end < next_section);
    assert!(regression < begin);
}

#[test]
fn preserves_existing_user_text_in_section() {
    let mut template = template_with_regression_section();
    let idx = template
        .iter()
        .position(|l| l == "(put your details here)\n")
        .unwrap();
    template[idx] = "My manual notes go here.\n".into();

    let (s, a, b) = sample_overview();
    inject_overview(&mut template, &s, &a, &b, false);

    let body = body(&template);
    assert!(body.contains("My manual notes go here."));
    let user = body.find("My manual notes go here.").unwrap();
    let begin = body.find(OVERVIEW_BEGIN_MARKER).unwrap();
    assert!(user < begin);
}

#[test]
fn idempotent_on_reexport() {
    let mut template = template_with_regression_section();
    let (s, a, b) = sample_overview();

    inject_overview(&mut template, &s, &a, &b, false);
    let first = body(&template);
    assert_eq!(count(&first, OVERVIEW_BEGIN_MARKER), 1);

    let single2 = vec![VersionResult {
        version: "12-SP5".into(),
        url: "u_new".into(),
        status: "passed".into(),
        ..Default::default()
    }];
    inject_overview(&mut template, &single2, &[], &[], false);
    let second = body(&template);

    assert_eq!(count(&second, OVERVIEW_BEGIN_MARKER), 1);
    assert_eq!(count(&second, OVERVIEW_END_MARKER), 1);
    assert!(second.contains("12-SP5"));
    assert!(!second.contains("15-SP4"));
}

#[test]
fn returns_false_when_no_regression_section() {
    let mut template: Vec<String> = vec!["A line\n".into(), "Another line\n".into()];
    let (s, a, b) = sample_overview();

    let modified = inject_overview(&mut template, &s, &a, &b, false);

    assert!(!modified);
    assert_eq!(
        template,
        vec!["A line\n".to_string(), "Another line\n".to_string()]
    );
}

#[test]
fn works_when_no_next_section_header() {
    let mut template: Vec<String> = [
        "regression tests:\n",
        "-----------------\n",
        "\n",
        "(put your details here)\n",
    ]
    .into_iter()
    .map(String::from)
    .collect();
    let (s, a, b) = sample_overview();

    let modified = inject_overview(&mut template, &s, &a, &b, false);

    assert!(modified);
    let body = body(&template);
    assert!(body.contains(OVERVIEW_BEGIN_MARKER));
    assert!(body.find("regression tests:").unwrap() < body.find(OVERVIEW_BEGIN_MARKER).unwrap());
}

#[test]
fn leaves_one_blank_above_block() {
    let mut template = template_with_regression_section();
    let (s, a, b) = sample_overview();
    inject_overview(&mut template, &s, &a, &b, false);

    let begin_line = format!("{OVERVIEW_BEGIN_MARKER}\n");
    let begin = template.iter().position(|l| *l == begin_line).unwrap();
    assert_eq!(template[begin - 1], "\n");
    assert_ne!(template[begin - 2], "\n");
}

#[test]
fn leaves_one_blank_below_block() {
    let mut template = template_with_regression_section();
    let (s, a, b) = sample_overview();
    inject_overview(&mut template, &s, &a, &b, false);

    let end_line = format!("{OVERVIEW_END_MARKER}\n");
    let end = template.iter().position(|l| *l == end_line).unwrap();
    assert_eq!(template[end + 1], "\n");
    assert_ne!(template[end + 2], "\n");
}

#[test]
fn blank_counts_stable_across_reexports() {
    let mut template = template_with_regression_section();
    let (s, a, b) = sample_overview();

    inject_overview(&mut template, &s, &a, &b, false);
    let blanks_first = template.iter().filter(|l| **l == "\n").count();

    inject_overview(&mut template, &s, &a, &b, false);
    let blanks_second = template.iter().filter(|l| **l == "\n").count();

    inject_overview(&mut template, &s, &a, &b, false);
    let blanks_third = template.iter().filter(|l| **l == "\n").count();

    assert_eq!(blanks_first, blanks_second);
    assert_eq!(blanks_second, blanks_third);
}

/// Golden snapshot of the fully-injected template.
///
/// The `overview_inject` BEGIN/END block is a **cross-implementation text
/// contract** (see `AGENTS.md`: "the `overview_inject` BEGIN/END idempotent
/// block under `regression tests:`"). The other tests in this file assert
/// placement, idempotency, and blank-line invariants structurally; this one
/// freezes the exact rendered bytes so the block layout cannot silently drift
/// away from what a Python `mtui` reading the same template would expect.
#[test]
fn injected_block_text_is_stable() {
    let mut template = template_with_regression_section();
    let (s, a, b) = sample_overview();

    let modified = inject_overview(&mut template, &s, &a, &b, false);
    assert!(modified);

    insta::assert_snapshot!("overview_inject_block", body(&template));
}
