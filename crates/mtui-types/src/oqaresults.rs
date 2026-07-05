//! Typed container for openQA results attached to a `TestReport`, ported from
//! `mtui/types/oqaresults.py`.
//!
//! Upstream replaced an untyped `dict[str, Any]` (`{"auto": ..., "kernel":
//! [...]}`) with a small dataclass so accessors are statically typed. This port
//! keeps that intent: [`OpenQAResults`] is a typed record of the "auto" and
//! "kernel" workflow results.
//!
//! ## The [`OpenQAResult`] trait
//!
//! Upstream models the shared surface of the concrete connectors
//! (`AutoOpenQA`, `KernelOpenQA`, `DashboardAutoOpenQA`) as a `runtime_checkable`
//! `Protocol` with a `kind` attribute and `__bool__`. This port expresses the
//! same structural contract as the [`OpenQAResult`] trait: a `kind` and a
//! truthiness predicate ([`OpenQAResult::has_results`], mirroring `__bool__`).
//!
//! ## Deviations from upstream
//!
//! * **Generic over the result types.** Python's `Protocol` uses `Any` for the
//!   per-connector `pp`/`results` element types. Rust models this by making
//!   [`OpenQAResults`] generic over the concrete auto (`A`) and kernel (`K`)
//!   result types, each bounded by [`OpenQAResult`]. Call sites pick the
//!   concrete connectors.
//! * **`overview` deferred.** The upstream `overview` field
//!   (`OpenQAOverviewResult`) references the `oqa_search` result types, which
//!   land in a later task. It is intentionally omitted here and will be added
//!   when those types exist, to keep this module free of a forward dependency.

/// The structural surface shared by all openQA result connectors.
///
/// Mirrors the upstream `OpenQAResult` `Protocol`: every connector exposes a
/// [`kind`](OpenQAResult::kind) discriminator and a truthiness predicate
/// ([`has_results`](OpenQAResult::has_results), the port of `__bool__`).
pub trait OpenQAResult {
    /// The workflow discriminator (e.g. `"auto"`, `"kernel"`, `"base"`).
    fn kind(&self) -> &str;

    /// Whether this connector holds any results.
    ///
    /// The port of upstream `__bool__`: `true` when the connector carries
    /// results worth reporting.
    fn has_results(&self) -> bool;
}

/// A typed record of the openQA results attached to a `TestReport`.
///
/// Mirrors the upstream `OpenQAResults` dataclass (minus the deferred
/// `overview` field — see the module docs):
///
/// * `auto` — the "auto" workflow result, `None` until populated.
/// * `kernel` — the list of "kernel" workflow results (typically a regular and
///   a baremetal openQA instance result for kernel updates).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenQAResults<A, K>
where
    A: OpenQAResult,
    K: OpenQAResult,
{
    /// The "auto" workflow result, `None` until populated.
    pub auto: Option<A>,
    /// The "kernel" workflow results.
    pub kernel: Vec<K>,
}

impl<A, K> Default for OpenQAResults<A, K>
where
    A: OpenQAResult,
    K: OpenQAResult,
{
    fn default() -> Self {
        Self {
            auto: None,
            kernel: Vec::new(),
        }
    }
}

impl<A, K> OpenQAResults<A, K>
where
    A: OpenQAResult,
    K: OpenQAResult,
{
    /// Creates an empty [`OpenQAResults`].
    ///
    /// Equivalent to [`Default::default`]; every instance gets its own
    /// `kernel` vector (guarding against the mutable-default footgun the
    /// upstream `field(default_factory=list)` avoids).
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether any result is present and truthy.
    ///
    /// The port of upstream `__bool__`: `true` when `auto` has results or any
    /// `kernel` entry has results.
    #[must_use]
    pub fn has_results(&self) -> bool {
        self.auto.as_ref().is_some_and(OpenQAResult::has_results)
            || self.kernel.iter().any(OpenQAResult::has_results)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A connector-like stub with a controllable truthiness, mirroring the
    /// `_truthy_result` / `_falsy_result` mocks in `test_oqaresults.py`.
    #[derive(Debug, Clone, PartialEq, Eq)]
    struct StubResult {
        kind: String,
        truthy: bool,
    }

    impl StubResult {
        fn truthy() -> Self {
            Self {
                kind: "auto".to_string(),
                truthy: true,
            }
        }

        fn falsy() -> Self {
            Self {
                kind: "auto".to_string(),
                truthy: false,
            }
        }
    }

    impl OpenQAResult for StubResult {
        fn kind(&self) -> &str {
            &self.kind
        }

        fn has_results(&self) -> bool {
            self.truthy
        }
    }

    type Results = OpenQAResults<StubResult, StubResult>;

    // TestOpenQAResultsDefaults

    #[test]
    fn defaults_are_none_and_empty_list() {
        let r = Results::new();
        assert!(r.auto.is_none());
        assert!(r.kernel.is_empty());
    }

    #[test]
    fn kernel_default_is_distinct_per_instance() {
        // Guard against the mutable-default footgun.
        let mut a = Results::new();
        let b = Results::new();
        a.kernel.push(StubResult::truthy());
        assert!(b.kernel.is_empty());
    }

    // TestOpenQAResultsBool

    #[test]
    fn empty_is_falsy() {
        assert!(!Results::new().has_results());
    }

    #[test]
    fn truthy_auto_makes_truthy() {
        let r = Results {
            auto: Some(StubResult::truthy()),
            kernel: Vec::new(),
        };
        assert!(r.has_results());
    }

    #[test]
    fn falsy_auto_alone_is_falsy() {
        let r = Results {
            auto: Some(StubResult::falsy()),
            kernel: Vec::new(),
        };
        assert!(!r.has_results());
    }

    #[test]
    fn truthy_kernel_makes_truthy() {
        let r = Results {
            auto: None,
            kernel: vec![StubResult::truthy()],
        };
        assert!(r.has_results());
    }

    #[test]
    fn kernel_with_only_falsy_is_falsy() {
        let r = Results {
            auto: None,
            kernel: vec![StubResult::falsy(), StubResult::falsy()],
        };
        assert!(!r.has_results());
    }

    #[test]
    fn truthy_kernel_among_falsy_makes_truthy() {
        let r = Results {
            auto: None,
            kernel: vec![StubResult::falsy(), StubResult::truthy()],
        };
        assert!(r.has_results());
    }

    // TestOpenQAResultsMutation

    #[test]
    fn assign_auto() {
        let mut r = Results::new();
        let item = StubResult::truthy();
        r.auto = Some(item.clone());
        assert_eq!(r.auto, Some(item));
    }

    #[test]
    fn append_to_kernel() {
        let mut r = Results::new();
        let a = StubResult::truthy();
        let b = StubResult::truthy();
        r.kernel.push(a.clone());
        r.kernel.push(b.clone());
        assert_eq!(r.kernel, vec![a, b]);
    }
}
