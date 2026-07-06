//! System-information footer for exports and commits.
//!
//! Ports upstream `mtui.support.systemcheck`:
//!
//! * [`detect_system`] reads `/etc/os-release` and `/proc/version` to discover
//!   the distro, version id, and kernel of the machine running mtui.
//! * [`system_info`] formats a one-line footer appended to a testreport on
//!   export (`## export`) and reused by `commit` (`committed from`).
//!
//! **Deviation from upstream:** the upstream footer embeds
//! `paramiko {version}`. This port intentionally drops that SSH-library token —
//! it carries no useful information about the run — and instead reports real
//! mtui + system facts: the mtui version, the detected distro/version, kernel,
//! and the session user.

/// The mtui version string, taken from the crate version at build time.
const MTUI_VERSION: &str = env!("CARGO_PKG_VERSION");

/// The default export footer prefix (upstream default).
pub const EXPORT_PREFIX: &str = "## export";

/// Detects `(distro, version_id, kernel)` of the current machine.
///
/// Mirrors upstream `detect_system`: parses `NAME=` / `VERSION_ID=` from
/// `/etc/os-release` and the third whitespace-separated token of the first line
/// of `/proc/version`. On failure the fields fall back to upstream's sentinels
/// (`Unknown` / `None` for the os-release pair, `Unknown` for the kernel), so
/// the footer is always well-formed even off a Linux host.
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

/// Finds the first line beginning with `key` and returns the value with a
/// single surrounding pair of `"` or `|` stripped, mirroring upstream's
/// `NAME=["|](.*)["|]` regex (the trailing quote is part of the capture upstream
/// too, so we only strip a matching leading/trailing delimiter).
fn extract_quoted(content: &str, key: &str) -> Option<String> {
    let line = content.lines().find(|l| l.starts_with(key))?;
    let raw = &line[key.len()..];
    let bytes = raw.as_bytes();
    if bytes.len() >= 2 {
        let first = bytes[0];
        let last = bytes[bytes.len() - 1];
        if (first == b'"' || first == b'|') && (last == b'"' || last == b'|') {
            return Some(raw[1..raw.len() - 1].to_string());
        }
    }
    Some(raw.to_string())
}

/// Formats the system-information footer line (trailing `\n` included).
///
/// Shape: `"{prefix} MTUI:{mtui_version} on {distro}-{verid} (kernel: {kernel})
/// by {user}\n"`. The `prefix` defaults to [`EXPORT_PREFIX`] for the export
/// footer; the `commit` command passes `"committed from"`.
#[must_use]
pub fn system_info(distro: &str, verid: &str, kernel: &str, user: &str, prefix: &str) -> String {
    format!("{prefix} MTUI:{MTUI_VERSION} on {distro}-{verid} (kernel: {kernel}) by {user}\n")
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
    fn extract_leaves_unquoted_value() {
        let osr = "VERSION_ID=15.5\n";
        assert_eq!(extract_quoted(osr, "VERSION_ID=").as_deref(), Some("15.5"));
    }

    #[test]
    fn system_info_footer_shape() {
        let line = system_info("SLES", "15.5", "6.4.0-1", "alice", EXPORT_PREFIX);
        assert!(line.starts_with("## export MTUI:"));
        assert!(line.contains(" on SLES-15.5 (kernel: 6.4.0-1) by alice\n"));
        assert!(line.ends_with('\n'));
        // No SSH-library token.
        assert!(!line.contains("paramiko"));
        assert!(!line.contains("russh"));
    }

    #[test]
    fn system_info_commit_prefix() {
        let line = system_info("d", "v", "k", "u", "committed from");
        assert!(line.starts_with("committed from MTUI:"));
    }
}
