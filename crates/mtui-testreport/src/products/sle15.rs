//! Normalizer for the SLE 15 product family.
//!
//! Ported from `mtui/template/products/sle15.py`. The if-chain order is
//! significant — the compound `LTSS-TERADATA` is matched before the bare
//! `LTSS`/`ERICSSON`/`TERADATA` tokens — and every unmatched name falls through
//! to a lowercase rewrite.
//!
//! Deviation from upstream: the `LTSS-ERICSSON` compound branch is intentionally
//! omitted because no SLE 15 product combines both tokens; such a version would
//! match the bare `LTSS` branch.

use mtui_types::SystemProduct;

/// Normalizes SLE 15 product information.
#[must_use]
pub fn normalize_sle15(mut x: SystemProduct) -> SystemProduct {
    if x.name == "SLE-Product-SLES" && x.version.contains("LTSS-TERADATA") {
        x.name = "SLES-LTSS-TERADATA".to_string();
        x.version = x.version.replace("-LTSS-TERADATA", "");
        return x;
    }
    if x.name == "SLE-Product-SLES" && x.version.contains("LTSS") {
        x.name = "SLES-LTSS".to_string();
        x.version = x.version.replace("-LTSS", "");
        return x;
    }
    if x.name == "SLE-Product-SLES" && x.version.contains("ERICSSON") {
        x.name = "ERICSSON".to_string();
        x.version = x.version.replace("-ERICSSON", "");
        return x;
    }
    if x.name == "SLE-Product-SLES" && x.version.contains("TERADATA") {
        x.name = "SLES_TERADATA".to_string();
        x.version = x.version.replace("-TERADATA", "");
        return x;
    }
    if x.name == "SLE-Product-SLES" {
        x.name = "SLES".to_string();
        return x;
    }
    if x.name == "SLE-Product-SLED" {
        x.name = "SLED".to_string();
        return x;
    }
    if x.name == "SLE-Product-WE" {
        x.name = "sle-we".to_string();
        return x;
    }
    if x.name == "SLE-Product-HA" {
        x.name = "sle-ha".to_string();
        return x;
    }
    if x.name == "SLE-Product-HPC" {
        x.name = "SLE_HPC".to_string();
        return x;
    }
    if x.name == "SLE-Product-SLES_SAP" {
        x.name = "SLES_SAP".to_string();
        return x;
    }
    if x.name == "SLE-Product-RT" {
        x.name = "SLE_RT".to_string();
        return x;
    }
    // All other SLE15 modules/extensions in lowercase.
    x.name = x.name.to_lowercase();
    x
}
