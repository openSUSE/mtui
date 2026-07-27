//! Parsers that turn raw host output into typed domain values.
//!
//! * [`product`] — pure `(name, version, arch)` extraction from a product XML
//!   file or an `/etc/os-release` file.
//! * [`system`] — the SFTP-driven [`parse_system`] that probes
//!   `/etc/products.d`, resolves the base product, collects addons, applies the
//!   SLES_SAP repo workarounds, and detects transactional hosts.

// `pub` (not `pub(crate)`) solely so the detached cargo-fuzz harness in
// `fuzz/` can drive the pure parsers with arbitrary bytes.
pub mod product;
pub mod system;

pub use system::parse_system;
