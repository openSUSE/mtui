//! Byte-identical handles on the report-document fixtures in
//! `tests/fixtures/document/`, the SSH test key in `tests/fixtures/ssh/`,
//! and the refhosts inventory in `tests/fixtures/refhosts.yml`, for other
//! crates' tests.
//!
//! A compile-time file read of `../../mtui-types/tests/...` from another
//! crate resolves outside that crate's tarball, and a run-time read of
//! another crate's `tests/` tree through `CARGO_MANIFEST_DIR` is gone with
//! the excluded tree, so `cargo test` from a crates.io checkout can neither
//! compile nor run it. These constants carry the same bytes through the
//! compiled `mtui-types` instead, which every test crate already depends on.
//! They are test data, not API: nothing outside `#[cfg(test)]` code (and doc
//! examples) should reach for them.

/// OBS maintenance-request document.
pub const MAINTENANCE_OBS: &str = include_str!("../tests/fixtures/document/maintenance_obs.json");
/// Maintenance-request document with addon test platforms.
pub const MAINTENANCE_ADDON: &str =
    include_str!("../tests/fixtures/document/maintenance_addon.json");
/// SLFO/Gitea update document.
pub const SLFO_GITEA: &str = include_str!("../tests/fixtures/document/slfo_gitea.json");
/// Maintenance-request document with an openQA L3 `testing` block.
pub const MAINTENANCE_OPENQA_L3: &str =
    include_str!("../tests/fixtures/document/maintenance_openqa_l3.json");
/// Maintenance document with a pipeline issue, a tester-filed (`new`) issue
/// and the matching `new_issues` view, captured from a teregen container.
pub const MAINTENANCE_NEW_ISSUES: &str =
    include_str!("../tests/fixtures/document/maintenance_new_issues.json");
/// Maximal document exercising every optional section.
pub const MAXIMAL: &str = include_str!("../tests/fixtures/document/maximal.json");
/// Product-increment document.
pub const PI: &str = include_str!("../tests/fixtures/document/pi.json");
/// OpenSSH Ed25519 private key for SSH-signature/auth unit tests that sign
/// with a real file-backed key (byte-identical to the `obs` fixture tree's).
pub const SSH_ID_ED25519: &str = include_str!("../tests/fixtures/ssh/id_ed25519");
/// The public half of [`SSH_ID_ED25519`], for tests
/// that resolve a key through its `.pub` sibling.
pub const SSH_ID_ED25519_PUB: &str = include_str!("../tests/fixtures/ssh/id_ed25519.pub");
/// Offline refhosts inventory for unit tests that resolve testplatforms
/// through the file-backed `path` resolver (byte-identical to the
/// datasources fixture tree's `refhosts.yml`).
pub const REFHOSTS_YML: &str = include_str!("../tests/fixtures/refhosts.yml");
