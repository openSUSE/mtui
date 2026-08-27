//! Typed container for openQA results attached to a `TestReport`: a statically
//! typed record of the "auto" and "kernel" workflow results instead of an
//! untyped map.
//!
//! [`OpenQAResults`] is generic over its auto (`A`), kernel (`K`) and overview
//! (`O`) types, each bounded by the small [`OpenQAResult`] / [`OverviewResult`]
//! traits: the concrete connectors — including `oqa_search`'s
//! `OpenQAOverviewResult` — live in the higher `mtui-datasources` crate, and
//! `mtui-types` must not depend upward. Call sites supply them.

/// The structural surface shared by all openQA result connectors.
pub trait OpenQAResult {
    /// The workflow discriminator (e.g. `"auto"`, `"kernel"`, `"base"`).
    fn kind(&self) -> &str;

    /// Whether this connector holds any results.
    fn has_results(&self) -> bool;
}

/// The truthiness surface of the `openqa_overview` payload, defined here so
/// [`OpenQAResults`] can stay in `mtui-types` while the concrete overview type
/// lives in `mtui-datasources`.
pub trait OverviewResult {
    /// Whether the overview carries any renderable section.
    fn has_overview(&self) -> bool;
}

/// A typed record of the openQA results attached to a `TestReport`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenQAResults<A, K, O>
where
    A: OpenQAResult,
    K: OpenQAResult,
    O: OverviewResult,
{
    /// The "auto" workflow result, `None` until populated.
    pub auto: Option<A>,
    /// The "kernel" workflow results — typically a regular and a baremetal
    /// openQA instance result for a kernel update.
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
    /// Creates an empty [`OpenQAResults`]. Each call allocates its own `kernel`
    /// vector, so instances never share mutable state.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A connector-like stub with a controllable truthiness.
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

    // --- Defaults. ---

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

    // --- Mutation. ---

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
