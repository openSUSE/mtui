//! Parsers that turn raw host output into typed domain values.
//!
//! Ported from `mtui/hosts/target/parsers/`:
//!
//! * [`product`] — pure `(name, version, arch)` extraction from a product XML
//!   file or an `/etc/os-release` file.
//! * [`system`] — the SFTP-driven [`parse_system`] that probes
//!   `/etc/products.d`, resolves the base product, collects addons, applies the
//!   SLES_SAP repo workarounds, and detects transactional hosts.

pub mod product;
pub mod system;

pub use product::{parse_os_release, parse_product};
pub use system::parse_system;
