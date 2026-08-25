//! Parsers for product and OS-release metadata read from a target host.
//!
//! Both functions are pure:
//! they take the already-read file bytes (the
//! [`Connection::sftp_open`](crate::Connection::sftp_open) SFTP read returns
//! the bytes directly) and return a flat `(name, version, arch)` tuple.
//!
//! Both are `pub` (not `pub(crate)`) solely so the detached cargo-fuzz harness
//! in `fuzz/` can drive them with arbitrary bytes — the content is
//! host-controlled, so it is exactly the input a fuzzer should own.

use quick_xml::events::Event;
use quick_xml::reader::Reader;

use crate::error::{HostError, Result};

/// Parses a `/etc/products.d/*.prod` product XML file.
///
/// Reads the `name` and `arch` elements (defaulting to the empty string when
/// absent), then derives the version:
///
/// * if `<baseversion>` is present, the version is `baseversion`, with a
///   `-SP{patchlevel}` suffix appended when `<patchlevel>` is present and not
///   `"0"`;
/// * otherwise the `<version>` element is used verbatim.
///
/// Non-UTF-8 bytes are decoded lossily (replacement characters), matching
/// [`parse_os_release`]'s handling of the same host-supplied-text problem, so
/// a mojibake `.prod` file degrades rather than newly failing `sysinfo`.
///
/// # Errors
/// Returns [`HostError::Sftp`] with a parse reason when the (lossily-decoded)
/// text is not well-formed XML.
pub fn parse_product(bytes: &[u8]) -> Result<(String, String, String)> {
    let fields = collect_fields(bytes)?;

    let text = |k: &str| fields.get(k).cloned().unwrap_or_default();

    let name = text("name");
    let arch = text("arch");

    let version = if let Some(baseversion) = fields.get("baseversion") {
        // sp = patchlevel-text ("" default) unless patchlevel == "0".
        let sp = match fields.get("patchlevel").map(String::as_str) {
            Some("0") | None => String::new(),
            Some(other) => other.to_owned(),
        };
        if sp.is_empty() {
            baseversion.clone()
        } else {
            format!("{baseversion}-SP{sp}")
        }
    } else {
        text("version")
    };

    Ok((name, version, arch))
}

/// Parses an `/etc/os-release` file.
///
/// Splits each `KEY=VALUE` line, strips surrounding double quotes and trailing
/// newlines, skips comment (`#`) and blank lines, and returns
/// `(ID, VERSION_ID, "x86_64")`. The architecture is hard-coded to `x86_64`.
///
/// # Errors
/// Returns [`HostError::Sftp`] when the required `ID` or `VERSION_ID` keys are
/// absent.
pub fn parse_os_release(bytes: &[u8]) -> Result<(String, String, String)> {
    let text = String::from_utf8_lossy(bytes);
    let mut info: std::collections::HashMap<String, String> = std::collections::HashMap::new();

    for line in text.lines() {
        if line.starts_with('#') || line.is_empty() {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        // Strip a trailing '\r' (CRLF), then surrounding double quotes (all
        // '"' characters are dropped).
        let value = value.trim_end_matches(['\r']).replace('"', "");
        info.insert(key.to_owned(), value);
    }

    let missing = |k: &str| HostError::Sftp {
        host: String::new(),
        reason: format!("/etc/os-release missing required key {k}"),
    };
    let id = info.get("ID").ok_or_else(|| missing("ID"))?.clone();
    let version = info
        .get("VERSION_ID")
        .ok_or_else(|| missing("VERSION_ID"))?
        .clone();

    Ok((id, version, "x86_64".to_owned()))
}

/// Reads the direct child elements of the product XML root into a
/// `tag -> text` map. Only the leaf elements the parser needs (`name`, `arch`,
/// `baseversion`, `patchlevel`, `version`) matter; duplicates keep the last
/// occurrence, matching `ElementTree.findtext`'s "first match wins" only
/// loosely — product files carry each tag once, so the distinction is moot.
fn collect_fields(bytes: &[u8]) -> Result<std::collections::HashMap<String, String>> {
    // Decode lossily rather than let the reader's UTF-8 validation reject the
    // whole file: see `parse_product`'s doc comment.
    let text = String::from_utf8_lossy(bytes);
    let mut reader = Reader::from_str(&text);
    reader.config_mut().trim_text(true);

    let mut fields = std::collections::HashMap::new();
    let mut current: Option<String> = None;

    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => {
                current = Some(e.local_name().as_ref().to_owned());
            }
            Ok(Event::Text(e)) => {
                if let Some(tag) = &current {
                    fields.entry(tag.clone()).or_insert_with(|| e.to_string());
                }
            }
            Ok(Event::End(_)) => current = None,
            Ok(Event::Eof) => break,
            Err(e) => return Err(xml_err(&e)),
            _ => {}
        }
    }

    Ok(fields)
}

fn xml_err(e: &impl std::fmt::Display) -> HostError {
    HostError::Sftp {
        host: String::new(),
        reason: format!("product XML parse error: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SLES_SP5: &[u8] = br#"<?xml version="1.0" encoding="UTF-8"?>
<product>
    <name>SLES</name>
    <baseversion>15</baseversion>
    <patchlevel>5</patchlevel>
    <arch>x86_64</arch>
</product>"#;

    const SLES_SP0: &[u8] = br#"<?xml version="1.0" encoding="UTF-8"?>
<product>
    <name>SLES</name>
    <baseversion>15</baseversion>
    <patchlevel>0</patchlevel>
    <arch>x86_64</arch>
</product>"#;

    const MICRO_VERSION_ONLY: &[u8] = br#"<?xml version="1.0" encoding="UTF-8"?>
<product>
    <name>SL-Micro</name>
    <version>6.0</version>
    <arch>x86_64</arch>
</product>"#;

    #[test]
    fn basic_product_appends_sp_suffix() {
        let (name, version, arch) = parse_product(SLES_SP5).expect("parse");
        assert_eq!(name, "SLES");
        assert_eq!(version, "15-SP5");
        assert_eq!(arch, "x86_64");
    }

    #[test]
    fn patchlevel_zero_has_no_sp_suffix() {
        let (_, version, _) = parse_product(SLES_SP0).expect("parse");
        assert_eq!(version, "15");
    }

    #[test]
    fn version_element_used_when_no_baseversion() {
        let (name, version, arch) = parse_product(MICRO_VERSION_ONLY).expect("parse");
        assert_eq!(name, "SL-Micro");
        assert_eq!(version, "6.0");
        assert_eq!(arch, "x86_64");
    }

    #[test]
    fn missing_tags_default_to_empty() {
        let xml = br#"<product><name>foo</name></product>"#;
        let (name, version, arch) = parse_product(xml).expect("parse");
        assert_eq!(name, "foo");
        assert_eq!(version, "");
        assert_eq!(arch, "");
    }

    #[test]
    fn invalid_xml_is_an_error() {
        // Mismatched end tag — quick-xml rejects this even at EOF.
        let err = parse_product(b"<product><name>x</wrong></product>").expect_err("should fail");
        assert!(matches!(err, HostError::Sftp { .. }));
    }

    #[test]
    fn invalid_utf8_is_decoded_lossily_not_rejected() {
        // A mojibake `.prod` file (non-UTF-8 bytes in a text node) must degrade
        // like `parse_os_release` does, not newly fail `sysinfo`.
        let xml = b"<product><name>SLES\xFF</name></product>";
        let (name, _, _) = parse_product(xml).expect("should decode lossily, not error");
        assert!(name.starts_with("SLES"));
        assert!(name.contains('\u{FFFD}'));
    }

    #[test]
    fn basic_os_release() {
        let content = b"ID=\"ubuntu\"\nVERSION_ID=\"22.04\"\nNAME=\"Ubuntu\"\n";
        let (name, version, arch) = parse_os_release(content).expect("parse");
        assert_eq!(name, "ubuntu");
        assert_eq!(version, "22.04");
        assert_eq!(arch, "x86_64");
    }

    #[test]
    fn os_release_skips_comments_and_blanks() {
        let content = b"# This is a comment\nID=\"sles\"\n\nVERSION_ID=\"15.5\"\n";
        let (name, version, _) = parse_os_release(content).expect("parse");
        assert_eq!(name, "sles");
        assert_eq!(version, "15.5");
    }

    #[test]
    fn os_release_without_quotes() {
        let content = b"ID=fedora\nVERSION_ID=40\n";
        let (name, version, _) = parse_os_release(content).expect("parse");
        assert_eq!(name, "fedora");
        assert_eq!(version, "40");
    }

    #[test]
    fn os_release_missing_required_key_errors() {
        let err = parse_os_release(b"NAME=\"x\"\n").expect_err("should fail");
        assert!(matches!(err, HostError::Sftp { .. }));
    }
}
