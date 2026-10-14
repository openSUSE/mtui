//! Golden checks of the exact output strings for all `*repoparse` variants
//! (`parse_product`, `slrepoparse`, `gitrepoparse`, `reporepoparse`).

use mtui_testreport::{gitrepoparse, parse_product, reporepoparse, slrepoparse};
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

mod credential_redaction {
    use std::cell::RefCell;
    use std::fmt::Write as _;
    use std::sync::OnceLock;

    use mtui_testreport::{reporepoparse, slrepoparse};
    use mtui_types::SystemProduct;
    use tracing::field::{Field, Visit};
    use tracing_subscriber::layer::{Context, Layer, SubscriberExt};
    use tracing_subscriber::registry::Registry;

    thread_local! {
        static SINK: RefCell<Option<Vec<String>>> = const { RefCell::new(None) };
    }

    struct CaptureLayer;

    struct FieldVisitor(String);

    impl Visit for FieldVisitor {
        fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
            let _ = write!(self.0, " {}={value:?}", field.name());
        }
    }

    impl<S: tracing::Subscriber> Layer<S> for CaptureLayer {
        fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
            SINK.with(|s| {
                if let Some(buf) = s.borrow_mut().as_mut() {
                    let mut v = FieldVisitor(String::new());
                    event.record(&mut v);
                    buf.push(v.0);
                }
            });
        }
    }

    /// Every event `f` emits on this thread, all fields included. The
    /// subscriber is global and only the sink is thread-local, so a callsite
    /// first reached from another thread cannot be cached as disabled.
    fn capture_events(f: impl FnOnce()) -> String {
        static ONCE: OnceLock<()> = OnceLock::new();
        ONCE.get_or_init(|| {
            let _ = tracing::subscriber::set_global_default(Registry::default().with(CaptureLayer));
        });
        SINK.with(|s| *s.borrow_mut() = Some(Vec::new()));
        f();
        SINK.with(|s| s.borrow_mut().take())
            .unwrap_or_default()
            .join("\n")
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
