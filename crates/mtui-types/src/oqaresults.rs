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
//! * **`overview` generic.** The upstream `overview` field
//!   (`OpenQAOverviewResult`) references the `oqa_search` result types, which
//!   live in the higher `mtui-datasources` crate. Since `mtui-types` must not
//!   depend upward, [`OpenQAResults`] is generic over the overview type `O`
//!   bounded by the small [`OverviewResult`] trait (its truthiness predicate),
//!   mirroring how `auto`/`kernel` are generic over [`OpenQAResult`]. Call sites
//!   in `mtui-testreport` supply the concrete `OpenQAOverviewResult`.

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

/// The truthiness surface of the `openqa_overview` payload.
///
/// Mirrors the `__bool__` of upstream `OpenQAOverviewResult`: `true` when any of
/// its sections has content. Defined here so [`OpenQAResults`] can stay in the
/// dependency-free `mtui-types` crate while the concrete overview type lives in
/// `mtui-datasources`.
pub trait OverviewResult {
    /// Whether the overview carries any renderable section.
    fn has_overview(&self) -> bool;
}

/// A typed record of the openQA results attached to a `TestReport`.
///
/// Mirrors the upstream `OpenQAResults` dataclass:
///
/// * `auto` — the "auto" workflow result, `None` until populated.
/// * `kernel` — the list of "kernel" workflow results (typically a regular and
///   a baremetal openQA instance result for kernel updates).
/// * `overview` — the `openqa_overview` payload, `None` until the command runs.
///   Generic over `O` so the concrete `oqa_search` type stays in the higher
///   `mtui-datasources` crate (see module docs).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenQAResults<A, K, O>
where
    A: OpenQAResult,
    K: OpenQAResult,
    O: OverviewResult,
{
    /// The "auto" workflow result, `None` until populated.
    pub auto: Option<A>,
    /// The "kernel" workflow results.
    pub kernel: Vec<K>,
    /// The `openqa_overview` payload, `None` until populated.
    pub overview: Option<O>,
}

impl<A, K, O> Default for OpenQAResults<A, K, O>
where
    A: OpenQAResult,
    K: OpenQAResult,
    O: OverviewResult,
{
    fn default() -> Self {
        Self {
            auto: None,
            kernel: Vec::new(),
            overview: None,
        }
    }
}

impl<A, K, O> OpenQAResults<A, K, O>
where
    A: OpenQAResult,
    K: OpenQAResult,
    O: OverviewResult,
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
    }

    impl OpenQAResult for StubResult {
        fn kind(&self) -> &str {
            &self.kind
        }

        fn has_results(&self) -> bool {
            self.truthy
        }
    }

    /// An overview stub with controllable truthiness.
    #[derive(Debug, Clone, PartialEq, Eq)]
    struct StubOverview {
        truthy: bool,
    }

    impl OverviewResult for StubOverview {
        fn has_overview(&self) -> bool {
            self.truthy
        }
    }

    type Results = OpenQAResults<StubResult, StubResult, StubOverview>;

    // TestOpenQAResultsDefaults

    #[test]
    fn defaults_are_none_and_empty_list() {
        let r = Results::new();
        assert!(r.auto.is_none());
        assert!(r.kernel.is_empty());
        assert!(r.overview.is_none());
    }

    #[test]
    fn kernel_default_is_distinct_per_instance() {
        // Guard against the mutable-default footgun.
        let mut a = Results::new();
        let b = Results::new();
        a.kernel.push(StubResult::truthy());
        assert!(b.kernel.is_empty());
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
