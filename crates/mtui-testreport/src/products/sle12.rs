//! Normalizer for the SLE 12 product family.
//!
//! Ported from `mtui/test_reports/products/sle12.py`. The if-chain order is
//! significant (more specific `LTSS-*` suffixes must be matched before the bare
//! `LTSS`/`TERADATA` ones) and is preserved verbatim.

use mtui_types::SystemProduct;

/// Normalizes SLE 12 product information.
#[must_use]
pub fn normalize_sle12(mut x: SystemProduct) -> SystemProduct {
    if x.name == "SLE-SERVER" && x.version.contains("LTSS-Extended-Security") {
        x.name = "SLES-LTSS-Extended-Security".to_string();
        x.version = x.version.replace("-LTSS-Extended-Security", "");
        return x;
    }
    if x.name == "SLE-SERVER" && x.version.contains("LTSS-ERICSSON") {
        x.name = "SLES-LTSS-ERICSSON".to_string();
        x.version = x.version.replace("-LTSS-ERICSSON", "");
        return x;
    }
    if x.name == "SLE-SERVER" && x.version.contains("LTSS-SAP") {
        x.name = "SLES-LTSS-SAP".to_string();
        x.version = x.version.replace("-LTSS-SAP", "");
        return x;
    }
    if x.name == "SLE-SERVER" && x.version.contains("LTSS-TERADATA") {
        x.name = "SLES_LTSS_TERADATA".to_string();
        x.version = x.version.replace("-LTSS-TERADATA", "");
        return x;
    }
    if x.name == "SLE-SERVER" && x.version.contains("LTSS") {
        x.name = "SLES-LTSS".to_string();
        x.version = x.version.replace("-LTSS", "");
        return x;
    }
    if x.name == "SLE-SERVER" && x.version.contains("TERADATA") {
        x.name = "SLES_TERADATA".to_string();
        x.version = x.version.replace("-TERADATA", "");
        return x;
    }
    if x.name == "SLE-SERVER" {
        x.name = "SLES".to_string();
        return x;
    }
    if x.name == "SLE-DESKTOP" {
        x.name = "SLED".to_string();
        return x;
    }
    if x.name == "SLE-RPI" {
        x.name = "SLES_RPI".to_string();
        return x;
    }
    if x.name == "SLE-SAP" {
        x.name = "SLES_SAP".to_string();
        return x;
    }
    // All other SLE12 modules/extensions in lowercase.
    x.name = x.name.to_lowercase();
    x
}
