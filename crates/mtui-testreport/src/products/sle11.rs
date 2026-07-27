//! Normalizer for the SLE 11 product family.
//!
//! The if-chain is
//! order-sensitive, and the `CORE` branch does not return — see the comment on
//! that branch for why it is left that way.

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
    // No `return` here, deliberately kept. The fall-through is inert for every
    // known product string: after the rewrite the name is
    // `SUSE_SLES_LTSS-EXTREME-CORE`, so the trailing SLE-SMT/SLE-HAE *name*
    // checks cannot match, and the post-strip version ends in none of
    // TERADATA/SECURITY/PUBCLOUD — so all three of those checks are no-ops too.
    // Only a doubly-suffixed version (`…-TERADATA-LTSS-EXTREME-CORE`) would
    // reach one, and no such string has been observed. Left as-is rather than
    // "fixed", because there is no evidence which answer it would want, and
    // this key selects the update repository a host gets.
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
