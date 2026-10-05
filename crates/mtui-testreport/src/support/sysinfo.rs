//! Host-system detection for the `people.testers` entry an export authors.
//!
//! [`detect_system`] reads `/etc/os-release` and `/proc/version` to discover
//! the distro, version id, and kernel of the machine running mtui.

/// Detects `(distro, version_id, kernel)` of the current machine.
///
/// Parses `NAME=` / `VERSION_ID=` from `/etc/os-release` and the third
/// whitespace-separated token of the first line of `/proc/version`. On
/// failure the fields fall back to sentinels (`Unknown` / `None` for the
/// os-release pair, `Unknown` for the kernel), so the result is always
/// well-formed even off a Linux host.
#[must_use]
pub fn detect_system() -> (String, String, String) {
    let (distro, verid) = match std::fs::read_to_string("/etc/os-release") {
        Ok(content) => {
            let distro = extract_quoted(&content, "NAME=").unwrap_or_default();
            let verid = extract_quoted(&content, "VERSION_ID=").unwrap_or_default();
            (distro, verid)
        }
        Err(_) => ("Unknown".to_string(), "None".to_string()),
    };

    let kernel = std::fs::read_to_string("/proc/version")
        .ok()
        .and_then(|s| s.lines().next().map(str::to_string))
        .and_then(|line| line.split(' ').nth(2).map(str::to_string))
        .unwrap_or_else(|| "Unknown".to_string());

    (distro, verid, kernel)
}

/// Finds the first line beginning with `key` and returns its value with a
/// single surrounding pair of `"` or `'` stripped.
///
/// An os-release value may be double-quoted, single-quoted or bare
/// (`NAME=Fedora`, `VERSION_ID=15.6` are spec-legal); a value with mismatched
/// delimiters is returned verbatim.
fn extract_quoted(content: &str, key: &str) -> Option<String> {
    let line = content.lines().find(|l| l.starts_with(key))?;
    let raw = &line[key.len()..];
    let bytes = raw.as_bytes();
    if bytes.len() >= 2 {
        let first = bytes[0];
        let last = bytes[bytes.len() - 1];
        if (first == b'"' || first == b'\'') && first == last {
            return Some(raw[1..raw.len() - 1].to_string());
        }
    }
    Some(raw.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_strips_matching_delimiters() {
        let osr = "NAME=\"SLES\"\nVERSION_ID=\"15.5\"\n";
        assert_eq!(extract_quoted(osr, "NAME=").as_deref(), Some("SLES"));
        assert_eq!(extract_quoted(osr, "VERSION_ID=").as_deref(), Some("15.5"));
        assert_eq!(extract_quoted(osr, "MISSING=").as_deref(), None);
    }

    #[test]
    fn extract_strips_single_quotes() {
        // Regression: single-quoted values (spec-legal, e.g. NAME='openSUSE')
        // must not leak their quotes into the result.
        let osr = "NAME='openSUSE'\nVERSION_ID='15.6'\n";
        assert_eq!(extract_quoted(osr, "NAME=").as_deref(), Some("openSUSE"));
        assert_eq!(extract_quoted(osr, "VERSION_ID=").as_deref(), Some("15.6"));
    }

    #[test]
    fn extract_leaves_unquoted_value() {
        // Bare values (NAME=Fedora, VERSION_ID=15.6) pass through verbatim.
        let osr = "NAME=Fedora\nVERSION_ID=15.6\n";
        assert_eq!(extract_quoted(osr, "NAME=").as_deref(), Some("Fedora"));
        assert_eq!(extract_quoted(osr, "VERSION_ID=").as_deref(), Some("15.6"));
    }

    #[test]
    fn extract_leaves_mismatched_delimiters_verbatim() {
        // An unmatched leading quote is not stripped, and `|` is not a quote.
        assert_eq!(
            extract_quoted("NAME=\"oops\n", "NAME=").as_deref(),
            Some("\"oops")
        );
        assert_eq!(
            extract_quoted("NAME=|weird|\n", "NAME=").as_deref(),
            Some("|weird|")
        );
    }
}
