//! Typed document authoring — builds `testing.*` subtrees from the data the
//! exporters collect.
//!
//! Every function here is pure: no I/O, no template anchors, no `Vec<String>`.
//! The exporters (`crate::export`) only write install logs.

pub mod auto;
pub mod kernel;
pub mod manual;
pub mod overview;

use std::collections::BTreeMap;

use mtui_types::report_document::{
    Openqa, OpenqaInstall, Regression, ReportDocument, Req, TesterEntry, TestingInstall,
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
/// `testing.install` and `testing.regression` merge into what the document
/// already holds rather than replacing it: the tester's verdicts and
/// comments survive, the install checks are replaced wholesale, and the
/// kernel matrix is spliced into the regression comment as a marked block.
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
        document.testing.install = Some(merge_install(document.testing.install.take(), install));
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
        document.testing.regression = Some(merge_regression(
            document.testing.regression.take(),
            regression,
        ));
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

fn merge_install(existing: Option<TestingInstall>, new: TestingInstall) -> TestingInstall {
    match existing {
        Some(old) => TestingInstall {
            verdict: old.verdict,
            checks: new.checks,
            comment: old.comment,
        },
        None => new,
    }
}

fn merge_regression(existing: Option<Regression>, new: Regression) -> Regression {
    let Some(old) = existing else {
        return new;
    };
    let comment = match new.comment.into_inner() {
        Some(block) => Req(Some(kernel::splice_block(old.comment.as_deref(), &block))),
        None => old.comment,
    };
    Regression {
        verdict: old.verdict,
        comment,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use mtui_types::report_document::{
        Install, InstallCheck, Issues, OpenqaVerdict, People, Reviewer, TargetRef, Testing, Update,
        Verdict,
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
            comment: Req(Some(kernel::wrap_block("regression notes"))),
        }
    }

    fn check(refhost: &str, verdict: Option<Verdict>) -> InstallCheck {
        InstallCheck {
            target: TargetRef {
                product: "SLES".to_owned(),
                version: "15.5".to_owned(),
                arch: "x86_64".to_owned(),
            },
            refhost: refhost.to_owned(),
            verdict: Req(verdict),
            before: BTreeMap::new(),
            after: BTreeMap::new(),
        }
    }

    fn author_install_and_regression(
        doc: &mut ReportDocument,
        install: TestingInstall,
        regression: Regression,
    ) {
        author_document(
            doc,
            Some(install),
            None,
            Some(regression),
            BTreeMap::new(),
            None,
        );
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
            Some(kernel::wrap_block("regression notes"))
        );
        assert_eq!(touched, ["testing.regression"]);
    }

    #[test]
    fn the_testers_install_verdict_and_comment_survive_authoring() {
        let mut doc = empty_document();
        doc.testing.install = Some(TestingInstall {
            verdict: Req(Some(Verdict::Failed)),
            checks: Vec::new(),
            comment: Req(Some("h1: broke on reboot".to_owned())),
        });
        author_install_and_regression(&mut doc, install_checks(), regression());
        let install = doc.testing.install.unwrap();
        assert_eq!(*install.verdict, Some(Verdict::Failed));
        assert_eq!(
            install.comment.into_inner(),
            Some("h1: broke on reboot".to_owned())
        );
    }

    #[test]
    fn install_checks_are_replaced_wholesale() {
        let mut doc = empty_document();
        doc.testing.install = Some(TestingInstall {
            verdict: Req(None),
            checks: vec![
                check("gone", Some(Verdict::Passed)),
                check("kept", Some(Verdict::Passed)),
            ],
            comment: Req(None),
        });
        let new = TestingInstall {
            verdict: Req(None),
            checks: vec![check("kept", Some(Verdict::Failed))],
            comment: Req(None),
        };
        author_install_and_regression(&mut doc, new, regression());
        let checks = doc.testing.install.unwrap().checks;
        assert_eq!(checks.len(), 1, "the disconnected host's check is dropped");
        assert_eq!(checks[0].refhost, "kept");
        assert_eq!(
            *checks[0].verdict,
            Some(Verdict::Failed),
            "export wins over the tester's PASSED"
        );
    }

    #[test]
    fn the_testers_regression_verdict_and_text_survive_authoring() {
        let mut doc = empty_document();
        doc.testing.regression = Some(Regression {
            verdict: Req(Some(Verdict::Passed)),
            comment: Req(Some("looks fine".to_owned())),
        });
        author_install_and_regression(&mut doc, install_checks(), regression());
        let regression = doc.testing.regression.unwrap();
        assert_eq!(*regression.verdict, Some(Verdict::Passed));
        assert_eq!(
            regression.comment.into_inner(),
            Some(format!(
                "looks fine\n\n{}",
                kernel::wrap_block("regression notes")
            ))
        );
    }

    #[test]
    fn a_null_regression_comment_becomes_just_the_block() {
        let mut doc = empty_document();
        doc.testing.regression = Some(Regression {
            verdict: Req(Some(Verdict::Failed)),
            comment: Req(None),
        });
        author_install_and_regression(&mut doc, install_checks(), regression());
        let regression = doc.testing.regression.unwrap();
        assert_eq!(*regression.verdict, Some(Verdict::Failed));
        assert_eq!(
            regression.comment.into_inner(),
            Some(kernel::wrap_block("regression notes"))
        );
    }

    #[test]
    fn re_export_keeps_exactly_one_kernel_block() {
        let mut doc = empty_document();
        doc.testing.regression = Some(Regression {
            verdict: Req(None),
            comment: Req(Some("looks fine".to_owned())),
        });
        for _ in 0..2 {
            author_install_and_regression(&mut doc, install_checks(), regression());
        }
        let comment = doc
            .testing
            .regression
            .unwrap()
            .comment
            .into_inner()
            .unwrap();
        assert_eq!(comment.matches("rewritten by every export").count(), 1);
        assert!(comment.starts_with("looks fine"));
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
