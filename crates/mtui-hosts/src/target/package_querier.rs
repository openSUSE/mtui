//! Per-target package-version querier.
//!
//! Ported from `mtui/hosts/target/package_querier.py`. Turns a list of package
//! names into a `name -> Option<RPMVersion>` map by running `rpm -q` (or
//! `dpkg-query` on Ubuntu) on the target and parsing the output. `None` means
//! the package is not installed. Duplicate lines for one package collapse to the
//! highest version.

use std::collections::HashMap;

use mtui_types::rpmver::RPMVersion;
use tracing::warn;

use super::Target;

/// Extracts the package name from the rpm-style `package X is not installed`
/// line, or `None` when the line is not that shape. The corresponding dpkg
/// output (`no packages found matching X`) is reported on stderr and does not
/// appear in the stdout line loop, so it needs no matcher here.
///
/// Mirrors the upstream regex `package (.*) is not installed` exactly: the name
/// is everything between the fixed prefix and suffix.
fn not_installed_name(line: &str) -> Option<&str> {
    line.strip_prefix("package ")
        .and_then(|rest| rest.strip_suffix(" is not installed"))
}

/// Adapter that runs `rpm -q` / `dpkg-query` on a [`Target`] and parses the
/// output into a version map.
pub struct PackageQuerier<'a> {
    target: &'a mut Target,
}

impl<'a> PackageQuerier<'a> {
    /// Binds the querier to `target`, used both as the call sink (`run` /
    /// `lastout`) and as the source of the rpm-vs-dpkg system distinction.
    #[must_use]
    pub fn new(target: &'a mut Target) -> Self {
        Self { target }
    }

    /// Queries the installed versions of `packages`.
    ///
    /// Returns a map from package name to [`RPMVersion`], or to `None` when the
    /// package is not installed. Duplicate lines for the same package collapse
    /// to the highest version. A version string that fails to parse is logged
    /// and skipped (best-effort, mirroring upstream's tolerance of odd output).
    pub async fn versions(&mut self, packages: &[String]) -> HashMap<String, Option<RPMVersion>> {
        let joined = packages.join(" ");
        let is_ubuntu = self.target.system().get_base().name == "ubuntu";
        let cmd = if is_ubuntu {
            format!("dpkg-query -W -f='${{package}} ${{version}}\n' {joined}")
        } else {
            format!("rpm -q --queryformat \"%{{Name}} %{{Version}}-%{{Release}}\n\" {joined}")
        };
        self.target.run(&cmd).await;

        let output = self.target.lastout().to_owned();
        let mut pkgs: HashMap<String, Option<RPMVersion>> = HashMap::new();
        for line in output.lines() {
            if let Some(name) = not_installed_name(line) {
                pkgs.insert(name.to_owned(), None);
                continue;
            }
            let mut parts = line.split_whitespace();
            let (Some(name), Some(ver)) = (parts.next(), parts.next()) else {
                continue;
            };
            let new_ver = match RPMVersion::parse(ver) {
                Ok(v) => v,
                Err(e) => {
                    warn!(package = name, version = ver, error = %e, "unparseable rpm version");
                    continue;
                }
            };
            pkgs.entry(name.to_owned())
                .and_modify(|existing| {
                    *existing = Some(match existing.take() {
                        Some(cur) => cur.max(new_ver.clone()),
                        None => new_ver.clone(),
                    });
                })
                .or_insert(Some(new_ver));
        }
        pkgs
    }
}

#[cfg(test)]
mod tests {
    use mtui_types::enums::{ExecutionMode, TargetState};
    use mtui_types::hostlog::CommandLog;

    use super::*;
    use crate::connection::MockConnection;

    fn target_with_output(stdout: &str) -> Target {
        let (t, _mock) = target_and_mock(stdout);
        t
    }

    /// Builds a target and returns a shared `MockConnection` handle whose
    /// `commands()` observes what the target issued (the mock's `issued` log is
    /// an `Arc`, shared across clones even after the box is moved into the
    /// target).
    fn target_and_mock(stdout: &str) -> (Target, MockConnection) {
        let mock = MockConnection::new("h1").with_default(CommandLog::new("", stdout, "", 0, 0));
        let target = Target::with_connection(
            "h1",
            TargetState::Enabled,
            ExecutionMode::Parallel,
            Box::new(mock.clone()),
        );
        (target, mock)
    }

    #[tokio::test]
    async fn parses_installed_versions() {
        let mut t = target_with_output("bash 5.1-1\ncoreutils 8.32-2\n");
        let mut q = PackageQuerier::new(&mut t);
        let map = q
            .versions(&["bash".to_owned(), "coreutils".to_owned()])
            .await;
        assert_eq!(map["bash"], Some(RPMVersion::parse("5.1-1").unwrap()));
        assert_eq!(map["coreutils"], Some(RPMVersion::parse("8.32-2").unwrap()));
    }

    #[tokio::test]
    async fn not_installed_maps_to_none() {
        let mut t = target_with_output("package foo is not installed\nbash 5.1-1\n");
        let mut q = PackageQuerier::new(&mut t);
        let map = q.versions(&["foo".to_owned(), "bash".to_owned()]).await;
        assert_eq!(map["foo"], None);
        assert_eq!(map["bash"], Some(RPMVersion::parse("5.1-1").unwrap()));
    }

    #[tokio::test]
    async fn duplicate_lines_collapse_to_highest() {
        let mut t = target_with_output("kernel 5.3.18-1\nkernel 5.3.18-3\nkernel 5.3.18-2\n");
        let mut q = PackageQuerier::new(&mut t);
        let map = q.versions(&["kernel".to_owned()]).await;
        assert_eq!(map["kernel"], Some(RPMVersion::parse("5.3.18-3").unwrap()));
    }

    #[tokio::test]
    async fn rpm_command_used_for_non_ubuntu() {
        let (mut t, mock) = target_and_mock("");
        let mut q = PackageQuerier::new(&mut t);
        let _ = q.versions(&["bash".to_owned()]).await;
        assert!(mock.commands().iter().any(|c| c.starts_with("rpm -q")));
    }

    #[tokio::test]
    async fn dpkg_command_used_for_ubuntu() {
        let (mut t, mock) = target_and_mock("");
        t.set_system(
            mtui_types::system::System::new(
                mtui_types::system::SystemProduct::new("ubuntu", "22.04", "x86_64"),
                Default::default(),
                false,
            ),
            false,
        );
        let mut q = PackageQuerier::new(&mut t);
        let _ = q.versions(&["bash".to_owned()]).await;
        assert!(mock.commands().iter().any(|c| c.starts_with("dpkg-query")));
    }
}
