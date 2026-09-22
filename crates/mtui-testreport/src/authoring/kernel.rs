//! Authoring for kernel jobs: `testing.regression` from the kernel
//! connectors.
//!
//! `Regression.verdict` has no exporter-computed source: unlike `auto.rs`'s
//! `install_status`, `export::kernel` never derives a pass/fail roll-up —
//! `REGRESSION TEST SUMMARY`'s verdict is tester-typed text no exporter
//! reads (step 1 gap analysis, `plans/phase4-authoring.md`). Only `comment`
//! is authored here, from the same per-test/per-module matrix the text
//! exporter already renders.

use mtui_datasources::openqa::kernel::KernelOpenQA;
use mtui_types::OpenQAResult;
use mtui_types::report_document::{Regression, Req};

/// Builds `testing.regression` from the kernel connectors' rendered result
/// matrices.
///
/// `None` when no connector produced results — mirroring
/// `openqa_install_from_auto`'s "nothing to report yet" omission.
#[must_use]
pub fn regression_from_kernel(kernel: &[KernelOpenQA]) -> Option<Regression> {
    let lines: Vec<&str> = kernel
        .iter()
        .filter(|k| k.has_results())
        .flat_map(|k| k.pp())
        .map(String::as_str)
        .collect();
    if lines.is_empty() {
        return None;
    }
    Some(Regression {
        verdict: Req(None),
        comment: Req(Some(lines.join("\n"))),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // `KernelOpenQA::pp`/`has_results` are only populated by `run()` against
    // a live client (built from `ruoqa::Client` + an `IncidentName`, neither
    // mockable from outside `mtui-datasources`); `export::kernel`'s own test
    // suite has the same constraint and, likewise, never constructs a
    // populated connector. The no-connectors case needs no instance at all
    // and is the property this pure function actually adds.
    #[test]
    fn no_connectors_omits_the_object() {
        assert!(regression_from_kernel(&[]).is_none());
    }
}
