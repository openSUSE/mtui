//! Authoring for the manual workflow: `testing.install` from connected hosts.
//!
//! Mirrors `export::manual`'s verdict rules (P4-D3), extracted from
//! `ManualExport::fillup_hosts_to_template` rather than re-derived, so a
//! divergence between the text and typed paths fails a test instead of
//! shipping.

use std::collections::BTreeMap;

use mtui_types::package::VersionCheck;
use mtui_types::report_document::{Install, InstallCheck, Req, TargetRef, TestingInstall, Verdict};

use crate::export::ManualHost;

/// Builds `testing.install` from the connected hosts' observed package
/// versions, joining each host to `targets.targets[]` by the exact
/// `(product, version, arch)` triple the schema mandates.
///
/// `verdict`/`comment` are left unset (`Req(None)`): per the step 1 gap
/// analysis (`plans/phase4-authoring.md`, P4-D3-amendment), both are
/// documented as sourced from the tester-typed `INSTALL TESTS SUMMARY` text,
/// which no exporter input carries — folding them from `checks[]` would
/// invent a judgment call the exporter never made.
#[must_use]
pub fn install_from_hosts(hosts: &[ManualHost], targets: &Install) -> TestingInstall {
    TestingInstall {
        verdict: Req(None),
        checks: hosts
            .iter()
            .map(|host| install_check(host, targets))
            .collect(),
        comment: Req(None),
    }
}

/// One host's [`InstallCheck`].
fn install_check(host: &ManualHost, targets: &Install) -> InstallCheck {
    let matched = targets.targets.iter().find(|t| {
        t.product == host.product.name
            && t.version == host.product.version
            && t.arch == host.product.arch
    });
    if matched.is_none() {
        tracing::warn!(
            hostname = %host.hostname,
            product = %host.product.name,
            version = %host.product.version,
            arch = %host.product.arch,
            "no install.targets[] row matches this host's product/version/arch; \
             authoring its check from the observed triple anyway"
        );
    }
    let target = TargetRef {
        product: host.product.name.clone(),
        version: host.product.version.clone(),
        arch: host.product.arch.clone(),
    };

    let mut before = BTreeMap::new();
    let mut after = BTreeMap::new();
    // #396's precedence, extracted from `fillup_hosts_to_template`: a
    // regression always flips FAILED even when another package is
    // unverified; an unverified block with no failure stays undecided.
    let mut failed = false;
    let mut unverified = false;

    for package in &host.packages {
        if let VersionCheck::Installed(v) = package.before_check() {
            before.insert(package.name.clone(), v.to_string());
        }
        if let VersionCheck::Installed(v) = package.after_check() {
            after.insert(package.name.clone(), v.to_string());
        }
        if !package.before_check().is_checked() || !package.after_check().is_checked() {
            unverified = true;
        }
        if let (Some(b), Some(a)) = (package.before(), package.after())
            && b >= a
        {
            failed = true;
        }
    }

    let verdict = if host.packages.is_empty() {
        Req(None)
    } else if failed {
        Req(Some(Verdict::Failed))
    } else if unverified {
        Req(None)
    } else {
        Req(Some(Verdict::Passed))
    };

    InstallCheck {
        target,
        refhost: host.hostname.clone(),
        verdict,
        before,
        after,
    }
}

#[cfg(test)]
mod tests {
    use mtui_types::hostlog::HostLog;
    use mtui_types::package::Package;
    use mtui_types::report_document::{Target, TestPlatform};
    use mtui_types::system::SystemProduct;

    use super::*;

    fn pkg(name: &str, before: Option<&str>, after: Option<&str>) -> Package {
        let mut p = Package::new(name);
        p.set_before(before).unwrap();
        p.set_after(after).unwrap();
        p
    }

    fn host(product: SystemProduct, packages: Vec<Package>) -> ManualHost {
        ManualHost {
            hostname: "h1".into(),
            system: product.to_string(),
            product,
            packages,
            hostlog: HostLog::new(),
        }
    }

    fn install() -> Install {
        Install {
            repository: "https://example/repo".into(),
            targets: vec![Target {
                product: "SLES".into(),
                version: "15.5".into(),
                arch: "x86_64".into(),
                repository: "https://example/target".into(),
                binaries: BTreeMap::from([("bash".into(), "1-1.x86_64".into())]),
            }],
            test_platforms: Vec::<TestPlatform>::new(),
        }
    }

    fn sles() -> SystemProduct {
        SystemProduct::new("SLES", "15.5", "x86_64")
    }

    /// Mirrors `manual.rs::install_results_says_not_checked_for_unobserved`:
    /// an unobserved package leaves the check undecided, never "not
    /// installed".
    #[test]
    fn unobserved_package_is_unverified_not_failed() {
        let doc = install_from_hosts(&[host(sles(), vec![Package::new("bash")])], &install());
        assert_eq!(doc.checks.len(), 1);
        let check = &doc.checks[0];
        assert_eq!(*check.verdict, None);
        assert!(!check.before.contains_key("bash"));
        assert!(!check.after.contains_key("bash"));
    }

    /// Mirrors `manual.rs::install_results_keeps_is_not_installed_for_observed_absent`.
    #[test]
    fn observed_absent_is_not_installed() {
        let doc = install_from_hosts(&[host(sles(), vec![pkg("bash", None, None)])], &install());
        let check = &doc.checks[0];
        assert!(!check.before.contains_key("bash"));
        assert!(!check.after.contains_key("bash"));
        // Observed-absent with no version increase still passes (#396 does
        // not fail a package that was never installed on either side).
        assert_eq!(*check.verdict, Some(Verdict::Passed));
    }

    /// Mirrors `manual.rs::install_results_failed_wins_over_unverified`.
    #[test]
    fn failed_wins_over_unverified() {
        let doc = install_from_hosts(
            &[host(
                sles(),
                vec![pkg("bash", Some("2"), Some("2")), Package::new("zsh")],
            )],
            &install(),
        );
        let check = &doc.checks[0];
        assert_eq!(*check.verdict, Some(Verdict::Failed));
        assert_eq!(check.before.get("bash").map(String::as_str), Some("2"));
    }

    /// Mirrors `manual.rs::fillup_flips_passed_when_version_increases`.
    #[test]
    fn version_increase_passes() {
        let doc = install_from_hosts(
            &[host(sles(), vec![pkg("bash", Some("1"), Some("2"))])],
            &install(),
        );
        assert_eq!(*doc.checks[0].verdict, Some(Verdict::Passed));
    }

    /// Mirrors `manual.rs::fillup_flips_failed_when_version_unchanged`.
    #[test]
    fn version_unchanged_fails() {
        let doc = install_from_hosts(
            &[host(sles(), vec![pkg("bash", Some("2"), Some("2"))])],
            &install(),
        );
        assert_eq!(*doc.checks[0].verdict, Some(Verdict::Failed));
    }

    /// Mirrors `manual.rs::install_results_skips_empty_host_block`: no
    /// recorded package data keeps the verdict undecided.
    #[test]
    fn no_packages_is_undecided() {
        let doc = install_from_hosts(&[host(sles(), vec![])], &install());
        assert_eq!(*doc.checks[0].verdict, None);
    }

    /// The join is by the exact triple, not a formatted string: a host whose
    /// product/version/arch matches no `targets[]` row still gets a check
    /// (from its own observed triple), not a silently dropped one.
    #[test]
    fn unmatched_target_still_authors_a_check() {
        let unknown = SystemProduct::new("SLED", "16", "aarch64");
        let doc = install_from_hosts(&[host(unknown.clone(), vec![])], &install());
        assert_eq!(doc.checks.len(), 1);
        assert_eq!(doc.checks[0].target.product, "SLED");
        assert_eq!(doc.checks[0].target.version, "16");
        assert_eq!(doc.checks[0].target.arch, "aarch64");
    }

    #[test]
    fn testing_install_verdict_and_comment_stay_unset() {
        let doc = install_from_hosts(
            &[host(sles(), vec![pkg("bash", Some("1"), Some("2"))])],
            &install(),
        );
        assert_eq!(*doc.verdict, None);
        assert_eq!(*doc.comment, None);
    }
}
