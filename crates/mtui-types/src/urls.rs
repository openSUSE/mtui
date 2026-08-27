//! A set of URLs describing an openQA install-log artefact.
//!
//! Produced by the "auto" openQA connectors (e.g. `Dashboardn` in
//! `mtui-datasources`) to point at the install-test log a passing job published,
//! tagged with the distri / arch / version it ran on and the job result.

/// A distribution/arch/version-tagged URL for an openQA log artefact.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct URLs {
    /// The distribution (e.g. `SLES`).
    pub distri: String,
    /// The architecture (e.g. `x86_64`).
    pub arch: String,
    /// The version (e.g. `15-SP5`).
    pub version: String,
    /// The URL of the log artefact.
    pub url: String,
    /// The job result (e.g. `passed`); empty when not set.
    pub result: String,
}

impl URLs {
    /// Creates a new [`URLs`] with an explicit `result`.
    #[must_use]
    pub fn new(
        distri: impl Into<String>,
        arch: impl Into<String>,
        version: impl Into<String>,
        url: impl Into<String>,
        result: impl Into<String>,
    ) -> Self {
        Self {
            distri: distri.into(),
            arch: arch.into(),
            version: version.into(),
            url: url.into(),
            result: result.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_sets_all_fields() {
        let u = URLs::new("SLES", "x86_64", "15-SP5", "https://oqa/log", "passed");
        assert_eq!(u.distri, "SLES");
        assert_eq!(u.arch, "x86_64");
        assert_eq!(u.version, "15-SP5");
        assert_eq!(u.url, "https://oqa/log");
        assert_eq!(u.result, "passed");
    }

    #[test]
    fn empty_result_is_representable() {
        let u = URLs::new("SLES", "x86_64", "15-SP5", "https://oqa/log", "");
        assert_eq!(u.result, "");
    }
}
