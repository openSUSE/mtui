//! Typed document authoring — builds `testing.*` subtrees from the same data
//! the legacy text exporters already collect, instead of mutating a
//! `Vec<String>` template (P4-D1).
//!
//! Every function here is pure: no I/O, no template anchors, no `Vec<String>`.
//! The legacy exporters (`crate::export`) are untouched and stay the shipped
//! path until Phase 7 deletes them.
//!
//! Gated behind the `api-ingest` Cargo feature, the same switch Phase 3's
//! ingest path uses (P4-D2) — off in every default build, compiled and tested
//! by CI's `--all-features`/`--features api-ingest` jobs.

pub mod auto;
pub mod kernel;
pub mod manual;
pub mod overview;

use std::collections::BTreeMap;

use mtui_types::report_document::{
    Openqa, OpenqaInstall, Regression, ReportDocument, TesterEntry, TestingInstall,
};
use serde_json::Value;

/// Composes the typed subtrees `manual`/`auto`/`kernel`/`overview` build onto
/// an already-loaded [`ReportDocument`], plus the append-only
/// `people.testers` entry. Mutates in place.
///
/// Never touches `update`/`install`/`issues`, nor the document-level
/// `verdict`/`comment` — those are tester-typed text (`SUMMARY:`/`comment:`
/// in the legacy log) with no exporter-computed source, the same reasoning
/// that already leaves `TestingInstall`/`Regression`'s own `verdict`/
/// `comment` unset.
///
/// Returns the top-level pointers actually touched, so a caller can report
/// what changed — `people.testers` is only included when a tester was
/// actually pushed, not on a duplicate.
pub fn author_document(
    document: &mut ReportDocument,
    install: Option<TestingInstall>,
    openqa_install: Option<OpenqaInstall>,
    regression: Option<Regression>,
    openqa_extra: BTreeMap<String, Value>,
    tester: Option<TesterEntry>,
) -> Vec<&'static str> {
    let mut touched = Vec::new();
    if let Some(install) = install {
        document.testing.install = Some(install);
        touched.push("testing.install");
    }
    if openqa_install.is_some() || !openqa_extra.is_empty() {
        let openqa = document.testing.openqa.get_or_insert_with(Openqa::default);
        if let Some(openqa_install) = openqa_install {
            openqa.install = Some(openqa_install);
        }
        openqa.extra.extend(openqa_extra);
        touched.push("testing.openqa");
    }
    if let Some(regression) = regression {
        document.testing.regression = Some(regression);
        touched.push("testing.regression");
    }
    if let Some(tester) = tester
        && !document.people.testers.contains(&tester)
    {
        document.people.testers.push(tester);
        touched.push("people.testers");
    }
    touched
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use mtui_types::report_document::{
        Install, Issues, OpenqaVerdict, People, Req, Reviewer, Testing, Update, Verdict,
    };

    use super::*;

    fn testers(names: &[&str]) -> Vec<TesterEntry> {
        names
            .iter()
            .map(|n| TesterEntry {
                name: (*n).to_owned(),
                mtui: None,
                os: None,
                kernel: None,
                at: None,
            })
            .collect()
    }

    fn empty_document() -> ReportDocument {
        ReportDocument {
            schema_version: mtui_types::report_document::SchemaVersion,
            id: "SUSE:Maintenance:1:2".to_owned(),
            kind: mtui_types::report_document::DocumentKind::Maintenance,
            workflow: mtui_types::report_document::DocWorkflow::Obs,
            generated_at: "2026-01-01T00:00:00Z".to_owned(),
            verdict: Req(None),
            comment: Req(None),
            people: People {
                testers: Vec::new(),
                reviewer: Reviewer {
                    name: Req(None),
                    slack: None,
                },
            },
            update: Update {
                packager: "p".to_owned(),
                source_packages: vec!["a".to_owned()],
                origin: mtui_types::report_document::Origin::default(),
                products: Vec::new(),
                category: None,
                rating: None,
                patches: None,
            },
            install: Install {
                repository: "https://example/repo".into(),
                targets: Vec::new(),
                test_platforms: Vec::new(),
            },
            issues: Issues::default(),
            testing: Testing::default(),
            review: None,
        }
    }

    fn install_checks() -> TestingInstall {
        TestingInstall {
            verdict: Req(None),
            checks: Vec::new(),
            comment: Req(None),
        }
    }

    fn openqa_install() -> OpenqaInstall {
        OpenqaInstall {
            verdict: Req(Some(OpenqaVerdict::Passed)),
            jobs: Vec::new(),
        }
    }

    fn regression() -> Regression {
        Regression {
            verdict: Req(None),
            comment: Req(Some("regression notes".to_owned())),
        }
    }

    #[test]
    fn none_of_everything_leaves_testing_and_people_untouched() {
        let mut doc = empty_document();
        let before = doc.clone();
        let touched = author_document(&mut doc, None, None, None, BTreeMap::new(), None);
        assert_eq!(doc, before);
        assert!(touched.is_empty());
    }

    #[test]
    fn install_is_stored_when_present() {
        let mut doc = empty_document();
        let touched = author_document(
            &mut doc,
            Some(install_checks()),
            None,
            None,
            BTreeMap::new(),
            None,
        );
        assert!(doc.testing.install.is_some());
        assert_eq!(touched, ["testing.install"]);
    }

    #[test]
    fn openqa_install_and_extra_share_one_openqa_object() {
        let mut doc = empty_document();
        let mut extra = BTreeMap::new();
        extra.insert("single_incidents".to_owned(), serde_json::json!([1]));
        let touched = author_document(&mut doc, None, Some(openqa_install()), None, extra, None);
        let openqa = doc.testing.openqa.expect("openqa object created");
        assert!(openqa.install.is_some());
        assert!(openqa.extra.contains_key("single_incidents"));
        assert_eq!(touched, ["testing.openqa"]);
    }

    #[test]
    fn openqa_extra_merges_without_clobbering_an_existing_install() {
        let mut doc = empty_document();
        doc.testing.openqa = Some(Openqa {
            install: Some(openqa_install()),
            incident: None,
            extra: BTreeMap::new(),
        });
        let mut extra = BTreeMap::new();
        extra.insert("build_checks".to_owned(), serde_json::json!([]));
        author_document(&mut doc, None, None, None, extra, None);
        let openqa = doc.testing.openqa.expect("openqa object retained");
        assert!(
            openqa.install.is_some(),
            "pre-existing install must survive"
        );
        assert!(openqa.extra.contains_key("build_checks"));
    }

    #[test]
    fn regression_is_stored_when_present() {
        let mut doc = empty_document();
        let touched = author_document(
            &mut doc,
            None,
            None,
            Some(regression()),
            BTreeMap::new(),
            None,
        );
        assert_eq!(
            doc.testing.regression.unwrap().comment.into_inner(),
            Some("regression notes".to_owned())
        );
        assert_eq!(touched, ["testing.regression"]);
    }

    #[test]
    fn tester_is_appended() {
        let mut doc = empty_document();
        let tester = testers(&["alice"]).remove(0);
        let touched = author_document(&mut doc, None, None, None, BTreeMap::new(), Some(tester));
        assert_eq!(doc.people.testers.len(), 1);
        assert_eq!(doc.people.testers[0].name, "alice");
        assert_eq!(touched, ["people.testers"]);
    }

    /// Append-only, but never a duplicate of an entry already present.
    #[test]
    fn tester_is_not_duplicated_on_a_second_identical_author() {
        let mut doc = empty_document();
        let tester = || testers(&["alice"]).remove(0);
        author_document(&mut doc, None, None, None, BTreeMap::new(), Some(tester()));
        let touched = author_document(&mut doc, None, None, None, BTreeMap::new(), Some(tester()));
        assert_eq!(doc.people.testers.len(), 1);
        assert!(touched.is_empty(), "a duplicate tester touches nothing");
    }

    /// A different tester still gets its own entry (append-only is not
    /// "keep only the latest").
    #[test]
    fn a_different_tester_gets_its_own_entry() {
        let mut doc = empty_document();
        let [alice, bob]: [TesterEntry; 2] = testers(&["alice", "bob"]).try_into().unwrap();
        author_document(&mut doc, None, None, None, BTreeMap::new(), Some(alice));
        author_document(&mut doc, None, None, None, BTreeMap::new(), Some(bob));
        assert_eq!(doc.people.testers.len(), 2);
    }

    /// Authoring twice from the same inputs yields an equal document — the
    /// typed replacement for `tests/export_idempotency.rs` (step 3's
    /// idempotency property, realised at assemble time).
    #[test]
    fn authoring_twice_from_the_same_inputs_is_idempotent() {
        let mut once = empty_document();
        let mut twice = empty_document();
        let mut extra = BTreeMap::new();
        extra.insert("aggregated_updates".to_owned(), serde_json::json!([1]));
        for doc in [&mut once, &mut twice] {
            author_document(
                doc,
                Some(install_checks()),
                Some(openqa_install()),
                Some(regression()),
                extra.clone(),
                Some(testers(&["alice"]).remove(0)),
            );
        }
        author_document(
            &mut twice,
            Some(install_checks()),
            Some(openqa_install()),
            Some(regression()),
            extra,
            Some(testers(&["alice"]).remove(0)),
        );
        assert_eq!(once, twice);
        assert_eq!(
            twice.people.testers.len(),
            1,
            "must not duplicate the tester"
        );
    }

    /// `verdict`/`comment` are never touched here: no exporter-computed
    /// source exists for these two root fields (they are tester-typed text).
    #[test]
    fn root_verdict_and_comment_are_never_touched() {
        let mut doc = empty_document();
        doc.verdict = Req(Some(Verdict::Passed));
        doc.comment = Req(Some("hand-typed".to_owned()));
        author_document(
            &mut doc,
            Some(install_checks()),
            Some(openqa_install()),
            Some(regression()),
            BTreeMap::new(),
            Some(testers(&["alice"]).remove(0)),
        );
        assert_eq!(*doc.verdict, Some(Verdict::Passed));
        assert_eq!(doc.comment.into_inner(), Some("hand-typed".to_owned()));
    }
}
