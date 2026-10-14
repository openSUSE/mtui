//! Golden checks of the exact output strings for all `*repoparse` variants
//! (`parse_product`, `slrepoparse`, `gitrepoparse`, `reporepoparse`,
//! `obsrepoparse`).

use std::fs;

use mtui_testreport::{gitrepoparse, obsrepoparse, parse_product, reporepoparse, slrepoparse};
use mtui_types::SystemProduct;

#[test]
fn parse_product_splits_archs() {
    let products = parse_product("SLES 15 (x86_64, aarch64)").expect("well-formed product");
    assert!(products.contains(&SystemProduct::new("SLES", "15", "x86_64")));
    assert!(products.contains(&SystemProduct::new("SLES", "15", "aarch64")));
}

#[test]
fn parse_product_rejects_malformed_strings() {
    // Externally-sourced (metadata.json) product strings are untrusted: a string
    // not shaped "<name> <version> (<archs>)" is a typed error, never a panic
    // (which under release panic=abort would terminate the process).
    assert!(
        parse_product("no-paren-here").is_err(),
        "missing ' (' should error"
    );
    assert!(
        parse_product(" (x86_64)").is_err(),
        "missing name/version token should error"
    );
}

#[test]
fn repoparse_variants_skip_malformed_and_keep_valid() {
    // A single malformed entry must be dropped-and-logged, never poisoning the
    // rest of the batch. Each variant keeps the well-formed product's repo.
    let products = vec!["garbage".to_string(), "SLES 15 (x86_64)".to_string()];
    let product = SystemProduct::new("SLES", "15", "x86_64");

    let repos = slrepoparse("https://example.com", &products);
    assert_eq!(repos.len(), 1);
    assert_eq!(
        repos[&product],
        "https://example.com/images/repo/SLES-15-x86_64/"
    );

    let repos = gitrepoparse("https://example.com", &products);
    assert_eq!(repos.len(), 1);
    assert_eq!(repos[&product], "https://example.com/standard");

    let repos = reporepoparse(
        &["https://example.com/SLES-15-x86_64/".to_string()],
        &products,
    );
    assert_eq!(repos.len(), 1);
    assert_eq!(repos[&product], "https://example.com/SLES-15-x86_64/");
}

#[test]
fn slrepoparse_builds_images_repo_url() {
    let repos = slrepoparse("https://example.com", &["SLES 15 (x86_64)".to_string()]);
    let product = SystemProduct::new("SLES", "15", "x86_64");
    assert_eq!(
        repos[&product],
        "https://example.com/images/repo/SLES-15-x86_64/"
    );
}

#[test]
fn gitrepoparse_builds_standard_url() {
    let repos = gitrepoparse("https://example.com", &["SLES 15 (x86_64)".to_string()]);
    let product = SystemProduct::new("SLES", "15", "x86_64");
    assert_eq!(repos[&product], "https://example.com/standard");
}

#[test]
fn reporepoparse_matches_repo_by_name_version_arch() {
    let repos = reporepoparse(
        &["https://example.com/SLES-15-x86_64/".to_string()],
        &["SLES 15 (x86_64)".to_string()],
    );
    let product = SystemProduct::new("SLES", "15", "x86_64");
    assert_eq!(repos[&product], "https://example.com/SLES-15-x86_64/");
}

#[test]
fn reporepoparse_drops_injection_and_bad_scheme_urls() {
    // Repo URLs are interpolated into root `zypper ar`/`rr` commands. A URL that
    // matches the product needle but carries shell metacharacters or an
    // unsupported scheme must be dropped at ingestion, never trusted.
    let product = SystemProduct::new("SLES", "15", "x86_64");

    // Shell metacharacter in an otherwise-matching URL → dropped.
    let repos = reporepoparse(
        &["https://example.com/SLES-15-x86_64/;reboot".to_string()],
        &["SLES 15 (x86_64)".to_string()],
    );
    assert!(
        !repos.contains_key(&product),
        "injection-shaped URL retained: {repos:?}"
    );

    // Unsupported scheme (still contains the needle) → dropped.
    let repos = reporepoparse(
        &["ssh://example.com/SLES-15-x86_64/".to_string()],
        &["SLES 15 (x86_64)".to_string()],
    );
    assert!(
        !repos.contains_key(&product),
        "bad-scheme URL retained: {repos:?}"
    );
}

#[test]
fn slrepoparse_drops_injection_shaped_base() {
    // An injection-shaped base repository URL yields a shell-unsafe derived URL,
    // which must be dropped rather than reaching a root zypper command.
    let repos = slrepoparse(
        "https://example.com/$(reboot)",
        &["SLES 15 (x86_64)".to_string()],
    );
    assert!(repos.is_empty(), "unsafe derived URL retained: {repos:?}");
}

#[test]
fn reporepoparse_drops_products_with_no_matching_repo() {
    // A product whose (name-version-arch) appears in no repo URL is omitted.
    let repos = reporepoparse(
        &["https://example.com/OTHER-1-aarch64/".to_string()],
        &["SLES 15 (x86_64)".to_string()],
    );
    assert!(repos.is_empty());
}

#[test]
fn obsrepoparse_parses_project_xml() {
    // The fixture's `SLE-Product-SLES:15:x86_64` releasetarget normalizes to
    // `Product("SLES", "15", "x86_64")`, keyed to `<repo>/<repository name>`.
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/obs");
    let repos = obsrepoparse("https://example.com", dir.as_ref());
    let product = SystemProduct::new("SLES", "15", "x86_64");
    assert_eq!(repos[&product], "https://example.com/SLE-15-x86_64");
}

#[test]
fn obsrepoparse_excludes_debug_repositories() {
    // A `<repository>` whose name contains `DEBUG` is dropped, leaving only
    // the update repo.
    let tmp = tempfile::tempdir().unwrap();
    fs::write(
        tmp.path().join("project.xml"),
        r#"<project>
  <repository name="SLE-15-x86_64">
    <path repository="update" project="SUSE:SLE-15:Update"/>
    <releasetarget project="SLE-Product-SLES:15:x86_64"/>
  </repository>
  <repository name="SLE-15-x86_64-DEBUG">
    <path repository="update" project="SUSE:SLE-15:Update"/>
    <releasetarget project="SLE-Product-SLES:15:x86_64"/>
  </repository>
</project>
"#,
    )
    .unwrap();

    let repos = obsrepoparse("https://example.com", tmp.path());
    let product = SystemProduct::new("SLES", "15", "x86_64");
    assert_eq!(repos.len(), 1);
    assert_eq!(repos[&product], "https://example.com/SLE-15-x86_64");
}

#[test]
fn obsrepoparse_skips_repositories_without_update_path() {
    // A `<repository>` with no `path[@repository='update']` child is not an
    // update target and is omitted.
    let tmp = tempfile::tempdir().unwrap();
    fs::write(
        tmp.path().join("project.xml"),
        r#"<project>
  <repository name="SLE-15-x86_64-images">
    <path repository="images" project="SUSE:SLE-15:Update"/>
    <releasetarget project="SLE-Product-SLES:15:x86_64"/>
  </repository>
</project>
"#,
    )
    .unwrap();

    let repos = obsrepoparse("https://example.com", tmp.path());
    assert!(repos.is_empty());
}

#[test]
fn obsrepoparse_missing_project_xml_yields_empty_map() {
    let tmp = tempfile::tempdir().unwrap();
    let repos = obsrepoparse("https://example.com", tmp.path());
    assert!(repos.is_empty());
}

mod credential_redaction {
    use mtui_testreport::{reporepoparse, slrepoparse};
    use mtui_types::SystemProduct;

    /// Every event `f` emits on this thread, all fields included.
    fn capture_events(f: impl FnOnce()) -> String {
        crate::log_capture::capture_logs(f).1
    }

    #[test]
    fn dropping_an_invalid_credential_url_logs_it_masked() {
        let products = vec!["SLES 15 (x86_64)".to_string()];
        let mut repos = None;

        let events = capture_events(|| {
            repos = Some(slrepoparse(
                "https://audituser:MTUI_SECRET_CANARY@repo.example/x;reboot",
                &products,
            ));
        });

        assert!(repos.expect("ran").is_empty(), "the unsafe URL is dropped");
        assert!(
            events.contains("skipping invalid repository URL"),
            "the drop event was not captured; the secrecy assertions would be vacuous"
        );
        assert!(
            !events.contains("MTUI_SECRET_CANARY") && !events.contains("audituser"),
            "the drop event leaked repository credentials"
        );
        assert!(events.contains("https://***@repo.example/"));
    }

    #[test]
    fn valid_credential_url_stays_exact_in_the_operational_map() {
        let raw = "https://audituser:MTUI_SECRET_CANARY@repo.example/SLES-15-x86_64/";
        let mut repos = None;

        let events = capture_events(|| {
            repos = Some(reporepoparse(
                &[raw.to_string()],
                &["SLES 15 (x86_64)".to_string()],
            ));
        });

        let repos = repos.expect("ran");
        assert_eq!(repos[&SystemProduct::new("SLES", "15", "x86_64")], raw);
        assert!(
            !events.contains("MTUI_SECRET_CANARY"),
            "a valid URL's parsing leaked repository credentials"
        );
    }
}
