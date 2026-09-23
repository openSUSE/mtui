//! Feature-independent adapter between the `export` command and
//! [`crate::authoring`].
//!
//! [`author_export`] is **always** compiled and callable, so `mtui-core`
//! never needs to declare the `api-ingest` Cargo feature itself — its body is
//! a real no-op unless this crate was built with the feature, and even then
//! only when `document` is `Some`. That matters beyond tidiness:
//! `mtui-core`'s own test suite calls `make_testreport` (SVN checkout)
//! throughout, and that function's `#[cfg(feature = "api-ingest")]` branch
//! replaces SVN with a live document fetch crate-wide — turning the feature
//! on for the whole of `mtui-core` would send unrelated tests to
//! `qam.suse.de` for real. Keeping the gate confined to this crate (already
//! the case for `authoring`/`ingest`) avoids that entirely.

use mtui_datasources::OpenQAOverviewResult;
use mtui_datasources::openqa::kernel::KernelOpenQA;
use mtui_datasources::qem_dashboard::DashboardAutoOpenQA;
use mtui_types::report_document::{ReportDocument, TesterEntry};

use crate::export::ManualHost;

/// Composes authoring's typed `testing.*` subtrees onto `document`, in
/// addition to the text export the caller already performs, never instead of
/// it. `hosts` is only meaningful for the manual workflow; pass `None` for
/// `Auto`/`Kernel`.
///
/// Returns the top-level pointers touched — empty without `api-ingest` or
/// when `document` is `None`, since neither case authors anything.
#[cfg_attr(
    not(feature = "api-ingest"),
    allow(
        unused_variables,
        reason = "the whole body compiles out without api-ingest"
    )
)]
pub fn author_export(
    document: &mut Option<ReportDocument>,
    hosts: Option<&[ManualHost]>,
    auto: Option<&DashboardAutoOpenQA>,
    kernel: &[KernelOpenQA],
    overview: Option<&OpenQAOverviewResult>,
    tester: Option<TesterEntry>,
) -> Vec<&'static str> {
    #[cfg(feature = "api-ingest")]
    {
        let Some(document) = document.as_mut() else {
            return Vec::new();
        };
        let install = hosts
            .map(|hosts| crate::authoring::manual::install_from_hosts(hosts, &document.install));
        let openqa_install = crate::authoring::auto::openqa_install_from_auto(auto);
        let regression = crate::authoring::kernel::regression_from_kernel(kernel);
        let openqa_extra = overview
            .map(crate::authoring::overview::openqa_extra_from_overview)
            .unwrap_or_default();
        crate::authoring::author_document(
            document,
            install,
            openqa_install,
            regression,
            openqa_extra,
            tester,
        )
    }
    #[cfg(not(feature = "api-ingest"))]
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_a_no_op_when_no_document_is_loaded() {
        let mut document: Option<ReportDocument> = None;
        let touched = author_export(&mut document, None, None, &[], None, None);
        assert!(document.is_none());
        assert!(touched.is_empty());
    }

    /// The composition itself (join, verdicts, tester dedup) is unit-tested
    /// exhaustively in `authoring::tests`; this proves the adapter actually
    /// reaches it with a document present, under the same feature-scoped CI
    /// job that already runs `--features api-ingest` for this crate.
    #[cfg(feature = "api-ingest")]
    #[test]
    fn composes_testing_install_when_a_document_and_hosts_are_present() {
        use mtui_types::hostlog::HostLog;
        use mtui_types::package::Package;
        use mtui_types::report_document::{
            DocWorkflow, DocumentKind, Install, Issues, Origin, People, Req, Reviewer,
            SchemaVersion, Testing, Update,
        };
        use mtui_types::system::SystemProduct;

        let mut document = Some(ReportDocument {
            schema_version: SchemaVersion,
            id: "SUSE:Maintenance:1:2".to_owned(),
            kind: DocumentKind::Maintenance,
            workflow: DocWorkflow::Obs,
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
                origin: Origin::default(),
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
        });

        let mut pkg = Package::new("bash");
        pkg.set_before(Some("1-1")).unwrap();
        pkg.set_after(Some("1-2")).unwrap();
        let host = ManualHost {
            hostname: "h1".to_owned(),
            system: "sles-15.5-x86_64".to_owned(),
            product: SystemProduct::new("SLES", "15.5", "x86_64"),
            packages: vec![pkg],
            hostlog: HostLog::new(),
        };

        let touched = author_export(&mut document, Some(&[host]), None, &[], None, None);
        assert_eq!(touched, ["testing.install"]);

        let install = document
            .unwrap()
            .testing
            .install
            .expect("testing.install authored");
        assert_eq!(install.checks.len(), 1);
        assert_eq!(install.checks[0].refhost, "h1");
    }
}
