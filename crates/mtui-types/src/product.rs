//! Refhost product schema, ported from `mtui/hosts/refhost/models.py`.
//!
//! These are the value-types a `refhosts.yml` row deserializes into:
//! [`Product`] (base product), [`Addon`] (module/extension), and [`Host`]
//! (one refhost row). The location-flatten/dedup loader and the golden
//! fixture test live in the next task (P1.3: refhost); this module only
//! defines the serde-derived types and verifies they round-trip an inline
//! row.
//!
//! Note: upstream also has a separate flat `Product` NamedTuple in
//! `mtui/types/product.py` used by `System` (name/version:str/arch). That one
//! lands with `system.rs` in a later task; this `Product` is the refhost
//! variant whose `version` is a structured [`Version`].

use serde::{Deserialize, Serialize};

use crate::version::Version;

/// A base product (`sles`, `SLE_RT`, …) with an optional version.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Product {
    /// The product name.
    pub name: String,
    /// The optional product version.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<Version>,
}

/// An addon (module / extension) shipped on a refhost.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Addon {
    /// The addon name.
    pub name: String,
    /// The optional addon version.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<Version>,
}

/// One refhost row loaded from `refhosts.yml`.
///
/// `name`, `arch`, and `product` are required; `addons` defaults to empty
/// because hosts without addons omit the key entirely.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Host {
    /// The host name.
    pub name: String,
    /// The host architecture (e.g. `x86_64`, `aarch64`).
    pub arch: String,
    /// The base product installed on the host.
    pub product: Product,
    /// Addons installed on the host (empty when the key is omitted).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub addons: Vec<Addon>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::version::VersionField;

    #[test]
    fn host_row_with_addon_and_numeric_minor_round_trips() {
        let yaml = "\
name: host-default-x86
arch: x86_64
product:
  name: sles
  version:
    major: 15
    minor: 5
addons:
  - name: sdk
    version:
      major: 15
      minor: 5
";
        let host: Host = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(host.name, "host-default-x86");
        assert_eq!(host.arch, "x86_64");
        assert_eq!(host.product.name, "sles");
        assert_eq!(
            host.product.version,
            Some(Version::new(15u64, Some(VersionField::Num(5))))
        );
        assert_eq!(host.addons.len(), 1);
        assert_eq!(host.addons[0].name, "sdk");

        // Round-trip preserves the row.
        let round = serde_yaml::to_string(&host).unwrap();
        let back: Host = serde_yaml::from_str(&round).unwrap();
        assert_eq!(back, host);
    }

    #[test]
    fn host_without_addons_defaults_to_empty() {
        let yaml = "\
name: host-default-noaddon
arch: x86_64
product:
  name: sles
  version:
    major: 12
    minor: sp4
";
        let host: Host = serde_yaml::from_str(yaml).unwrap();
        assert!(host.addons.is_empty());
        assert_eq!(
            host.product.version,
            Some(Version::new(
                12u64,
                Some(VersionField::Text("sp4".to_owned()))
            ))
        );
    }

    #[test]
    fn product_without_version_deserializes() {
        let yaml = "name: rhel\n";
        let product: Product = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(product.name, "rhel");
        assert_eq!(product.version, None);
    }
}
