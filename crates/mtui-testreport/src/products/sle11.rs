//! Normalizer for the SLE 11 product family.
//!
//! Ported from `mtui/test_reports/products/sle11.py`. The upstream if-chain is
//! order-sensitive and contains a deliberate non-returning fallthrough on the
//! `CORE` branch; both are preserved exactly.

use mtui_types::SystemProduct;

/// Normalizes SLE 11 product information.
#[must_use]
pub fn normalize_sle11(mut x: SystemProduct) -> SystemProduct {
    if x.name == "SLE-SDK" {
        x.name = "sle-sdk".to_string();
        return x;
    }
    if x.name == "SLE-SAP-AIO" {
        x.name = "SUSE_SLES_SAP".to_string();
        return x;
    }
    let last_seg = x.version.rsplit('-').next().unwrap_or(&x.version);
    if x.name == "SLE-SERVER" && !matches!(last_seg, "TERADATA" | "SECURITY" | "PUBCLOUD" | "CORE")
    {
        x.name = "SUSE_SLES".to_string();
        x.version = x.version.replace("-LTSS", "").replace("-CLIENT-TOOLS", "");
        return x;
    }
    // Upstream intentionally does NOT return here — the `CORE` rewrite falls
    // through to the subsequent suffix checks. Preserved verbatim.
    if x.version.ends_with("CORE") {
        x.name = "SUSE_SLES_LTSS-EXTREME-CORE".to_string();
        x.version = x.version.replace("-LTSS-EXTREME-CORE", "");
    }
    if x.version.ends_with("TERADATA") {
        x.name = "teradata".to_string();
        x.version = x.version.replace("-TERADATA", "");
        return x;
    }
    if x.version.ends_with("SECURITY") {
        x.name = "security".to_string();
        x.version = "11".to_string();
        return x;
    }
    if x.version.ends_with("PUBCLOUD") {
        x.name = "sle-module-pubcloud".to_string();
        x.version = "11".to_string();
        return x;
    }
    if x.name == "SLE-SMT" {
        x.name = "sle-smt".to_string();
        return x;
    }
    if x.name == "SLE-HAE" {
        x.name = "sle-hae".to_string();
        return x;
    }
    x
}
