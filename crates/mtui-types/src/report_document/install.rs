//! `install`, `target`, `test_platform`, `platform_ref` (`$defs/install` and
//! siblings of the TeReGen report-document schema, v1.0).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// How to get the update onto a machine (`install`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Install {
    /// Base repository for the whole update.
    pub repository: String,
    /// One self-contained row per product and architecture (schema
    /// `minItems: 1`).
    pub targets: Vec<Target>,
    /// Reference-host platforms to validate on.
    pub test_platforms: Vec<TestPlatform>,
}

/// One `install.targets[]` entry.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Target {
    pub product: String,
    pub version: String,
    pub arch: String,
    /// Absolute URL serving this target, under which an RPM is at
    /// `<arch>/<name>-<version-release>.<arch>.rpm`.
    pub repository: String,
    /// Package name to `version-release.arch`, one entry per RPM this target
    /// carries (schema `minProperties: 1`).
    pub binaries: BTreeMap<String, String>,
}

/// One `install.test_platforms[]` entry.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TestPlatform {
    pub base: PlatformRef,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub addon: Option<PlatformRef>,
    /// Schema `minItems: 1`.
    pub archs: Vec<String>,
}

/// A base or addon platform reference (`$defs/platform_ref`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlatformRef {
    pub class: String,
    pub major: String,
    /// Stays a `String`: the numeric-or-`spN` refhosts convention
    /// (`crates/mtui-types` `Contracts` — `refhosts.yml`'s `version.minor`)
    /// depends on it.
    pub minor: String,
}
