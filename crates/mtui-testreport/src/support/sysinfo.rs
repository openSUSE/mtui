//! Host-system detection for the `people.testers` entry an export authors.
//!
//! [`detect_system`] reports the OS and kernel of the machine running mtui.
//! Both are schema-optional, so a value that cannot be detected is `None` and
//! the export leaves the field out rather than sending a placeholder.

/// OS and kernel of the machine running mtui; `None` where undetectable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SystemInfo {
    /// `NAME-VERSION_ID` from os-release, or `macOS-27.2 (26B123)` on macOS.
    pub os: Option<String>,
    /// The `uname(2)` release (`uname -r`).
    pub kernel: Option<String>,
}

/// Detects the OS and kernel of the current machine.
///
/// macOS reads `SystemVersion.plist`; every other target reads
/// `/etc/os-release`, falling back to `/usr/lib/os-release`. The kernel is
/// `uname(2).release` on Unix targets.
#[must_use]
pub fn detect_system() -> SystemInfo {
    SystemInfo {
        os: detect_os(),
        kernel: kernel_release(),
    }
}

#[cfg(target_os = "macos")]
fn detect_os() -> Option<String> {
    let plist = std::fs::read_to_string("/System/Library/CoreServices/SystemVersion.plist").ok()?;
    os_from_system_version_plist(&plist)
}

#[cfg(not(target_os = "macos"))]
fn detect_os() -> Option<String> {
    ["/etc/os-release", "/usr/lib/os-release"]
        .iter()
        .find_map(|path| std::fs::read_to_string(path).ok())
        .and_then(|content| os_from_os_release(&content))
}

#[cfg(unix)]
fn kernel_release() -> Option<String> {
    let release = nix::sys::utsname::uname()
        .ok()?
        .release()
        .to_string_lossy()
        .into_owned();
    (!release.is_empty()).then_some(release)
}

#[cfg(not(unix))]
fn kernel_release() -> Option<String> {
    None
}

/// `NAME-VERSION_ID`, or just `NAME` when `VERSION_ID` is absent or empty.
/// `None` without a non-empty `NAME`.
#[cfg(any(not(target_os = "macos"), test))]
fn os_from_os_release(content: &str) -> Option<String> {
    let name = extract_quoted(content, "NAME=").filter(|n| !n.is_empty())?;
    match extract_quoted(content, "VERSION_ID=").filter(|v| !v.is_empty()) {
        Some(verid) => Some(format!("{name}-{verid}")),
        None => Some(name),
    }
}

/// `ProductName-ProductVersion (ProductBuildVersion)` from an XML
/// `SystemVersion.plist`; the build suffix is dropped when absent. `None`
/// without a non-empty name and version.
#[cfg(any(target_os = "macos", test))]
fn os_from_system_version_plist(content: &str) -> Option<String> {
    let name = plist_string(content, "ProductName")?;
    let version = plist_string(content, "ProductVersion")?;
    match plist_string(content, "ProductBuildVersion") {
        Some(build) => Some(format!("{name}-{version} ({build})")),
        None => Some(format!("{name}-{version}")),
    }
}

/// The non-empty `<string>` that directly follows `<key>{key}</key>`.
#[cfg(any(target_os = "macos", test))]
fn plist_string(content: &str, key: &str) -> Option<String> {
    let marker = format!("<key>{key}</key>");
    let after = content[content.find(&marker)? + marker.len()..].trim_start();
    let value = after.strip_prefix("<string>")?;
    let value = value[..value.find("</string>")?].trim();
    (!value.is_empty()).then(|| value.to_string())
}

/// Finds the first line beginning with `key` and returns its value with a
/// single surrounding pair of `"` or `'` stripped.
///
/// An os-release value may be double-quoted, single-quoted or bare
/// (`NAME=Fedora`, `VERSION_ID=15.6` are spec-legal); a value with mismatched
/// delimiters is returned verbatim.
#[cfg(any(not(target_os = "macos"), test))]
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

    const OS_RELEASE: &str = "NAME=\"SLES\"\nVERSION_ID=\"15.5\"\n";

    const MACOS_PLIST: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>BuildID</key>
	<string>A1B2C3D4-0000-0000-0000-000000000000</string>
	<key>ProductBuildVersion</key>
	<string>26B123</string>
	<key>ProductName</key>
	<string>macOS</string>
	<key>ProductVersion</key>
	<string>27.2</string>
</dict>
</plist>
"#;

    #[test]
    fn os_release_joins_name_and_version_id() {
        assert_eq!(os_from_os_release(OS_RELEASE).as_deref(), Some("SLES-15.5"));
    }

    #[test]
    fn os_release_without_version_id_is_the_bare_name() {
        assert_eq!(
            os_from_os_release("NAME=\"Tumbleweed\"\n").as_deref(),
            Some("Tumbleweed")
        );
        assert_eq!(
            os_from_os_release("NAME=Arch\nVERSION_ID=\"\"\n").as_deref(),
            Some("Arch")
        );
    }

    #[test]
    fn os_release_without_a_name_is_none() {
        assert_eq!(os_from_os_release("VERSION_ID=\"15.5\"\n"), None);
        assert_eq!(os_from_os_release("NAME=\"\"\nVERSION_ID=\"15.5\"\n"), None);
        assert_eq!(os_from_os_release(""), None);
    }

    #[test]
    fn plist_renders_name_version_and_build() {
        assert_eq!(
            os_from_system_version_plist(MACOS_PLIST).as_deref(),
            Some("macOS-27.2 (26B123)")
        );
    }

    #[test]
    fn plist_without_a_build_drops_the_suffix() {
        let plist = "<key>ProductName</key>\n<string>macOS</string>\n\
                     <key>ProductVersion</key>\n<string>27.2</string>";
        assert_eq!(
            os_from_system_version_plist(plist).as_deref(),
            Some("macOS-27.2")
        );
    }

    #[test]
    fn plist_missing_name_or_version_is_none() {
        let no_version = "<key>ProductName</key><string>macOS</string>";
        let no_name = "<key>ProductVersion</key><string>27.2</string>";
        let empty = "<key>ProductName</key><string></string>\
                     <key>ProductVersion</key><string>27.2</string>";
        assert_eq!(os_from_system_version_plist(no_version), None);
        assert_eq!(os_from_system_version_plist(no_name), None);
        assert_eq!(os_from_system_version_plist(empty), None);
    }

    #[test]
    fn plist_garbage_is_none() {
        assert_eq!(os_from_system_version_plist("bplist00\u{1}\u{2}"), None);
        assert_eq!(os_from_system_version_plist(""), None);
        let unterminated = "<key>ProductName</key><string>macOS";
        assert_eq!(os_from_system_version_plist(unterminated), None);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn detects_the_macos_host() {
        let info = detect_system();
        assert!(
            info.os
                .as_deref()
                .is_some_and(|os| os.starts_with("macOS-")),
            "os: {:?}",
            info.os
        );
        assert!(info.kernel.is_some());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn detects_the_linux_kernel() {
        assert!(detect_system().kernel.is_some());
    }
}
