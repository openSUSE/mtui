//! Exporter for the manual workflow.
//!
//! Rewrites the per-host result sections of the template from the connected
//! hosts' package before/after versions and command logs, then runs the base
//! sequence.
//!
//! ## Host input
//!
//! Coupling this crate to the concrete `mtui-hosts::Target` (which carries
//! live connection state) would be the wrong dependency direction, so the
//! exporter takes a decoupled [`ManualHost`] view capturing exactly the
//! `hostname`, `system`, `packages`, and `hostlog` fields it needs. The
//! composition root (`mtui-core`) builds these from the live targets,
//! mirroring how the downloader takes `(host, tests)` pairs.

use std::sync::LazyLock;

use mtui_datasources::OpenQAOverviewResult;
use mtui_datasources::qem_dashboard::dashboard_openqa::DashboardAutoOpenQA;
use mtui_types::hostlog::HostLog;
use mtui_types::package::{Package, VersionCheck};
use regex::Regex;

use super::base::{ExportContext, OverwritePrompt};

/// A decoupled view of a connected host, holding exactly what the manual
/// exporter reads from a `Target`.
#[derive(Debug, Clone)]
pub struct ManualHost {
    /// The reference-host hostname.
    pub hostname: String,
    /// The system/product type string (e.g. `sles12sp5-x86_64`).
    pub system: String,
    /// The host's packages with before/after versions.
    pub packages: Vec<Package>,
    /// The host's command log.
    pub hostlog: HostLog,
}

/// Matches a `reference host: <name>` line and captures the hostname. The
/// template emits a single space after the colon, so the pattern requires at
/// least one (not two).
static REFERENCE_HOST_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"reference host:\s+([^)\s]+)").expect("valid reference-host regex")
});

/// Matches a per-host verdict result line: `\s:\s(SUCCEEDED|FAILED|INTERNAL
/// ERROR)`. Rust `regex` has no look-behind, so the `PASSED/FAILED`
/// placeholder is excluded explicitly in [`is_result_line`].
static RESULT_LINE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\s:\s(SUCCEEDED|FAILED|INTERNAL ERROR)").expect("valid result regex")
});

/// Whether `line` is a per-host verdict result line to be stripped.
///
/// Excludes the `PASSED/FAILED` placeholder, since Rust `regex` has no
/// negative look-behind to do it inline.
fn is_result_line(line: &str) -> bool {
    RESULT_LINE_RE.is_match(line) && !line.contains("PASSED/FAILED")
}

/// The manual-workflow exporter.
pub struct ManualExport {
    /// Shared export state and helpers.
    pub ctx: ExportContext,
    /// The connected-host views.
    results: Vec<ManualHost>,
    /// The "auto" openQA connector (for `inject_openqa`), if present.
    ///
    /// Only its rendered [`pp`](DashboardAutoOpenQA::pp) block is consumed
    /// here.
    auto: Option<DashboardAutoOpenQA>,
    /// The openqa_overview payload, if the overview command ran.
    overview: Option<OpenQAOverviewResult>,
}

impl ManualExport {
    /// Builds a manual exporter over `ctx`.
    #[must_use]
    pub fn new(
        ctx: ExportContext,
        results: Vec<ManualHost>,
        auto: Option<DashboardAutoOpenQA>,
        overview: Option<OpenQAOverviewResult>,
    ) -> Self {
        Self {
            ctx,
            results,
            auto,
            overview,
        }
    }

    /// Converts a host's install log to template lines.
    ///
    /// Emits a `log from <host>:` header followed by the stdout of each
    /// `zypper `/`transactional-update` command; returns empty for an unknown
    /// host.
    fn host_installog_to_template(&self, target: &str) -> Vec<String> {
        let Some(host) = self.results.iter().find(|h| h.hostname == target) else {
            // #396: an empty install log for a host nobody collected data for
            // must at least say so, not just silently write nothing.
            tracing::warn!("no install log recorded for {target}; exporting an empty log");
            return Vec::new();
        };

        let mut t = vec![format!("log from {}:\n", host.hostname)];
        for cmd_log in &host.hostlog {
            let cmd = &cmd_log.command;
            if cmd.contains("zypper ") || cmd.contains("transactional-update") {
                t.push(format!("# {cmd}\n{}\n", cmd_log.stdout));
            }
        }
        t
    }

    /// Writes each host's install log and returns the filenames.
    fn get_logs(&self, hosts: &[String], prompt: &dyn OverwritePrompt) -> Vec<String> {
        let dir = self.ctx.install_logs_dir();
        let mut filenames = Vec::new();
        for host in hosts {
            let lines = self.host_installog_to_template(host);
            let fn_name = format!("{host}.log");
            self.ctx.writer(&dir.join(&fn_name), &lines, prompt);
            filenames.push(fn_name);
        }
        filenames
    }

    /// Strips previously-exported verdict lines, then rebuilds host sections.
    pub fn install_results(&mut self) {
        let hostnames: Vec<String> = self.results.iter().map(|h| h.hostname.clone()).collect();

        let mut c_host: Option<String> = None;
        let mut tmp: Vec<String> = Vec::with_capacity(self.ctx.template.len());
        for line in &self.ctx.template {
            // Track which host section we are in so only the *current session's*
            // hosts get their stale result lines refreshed. The host header line
            // itself is kept — it is the section header.
            if let Some(cap) = REFERENCE_HOST_RE.captures(line) {
                c_host = Some(cap[1].to_string());
                tmp.push(line.clone());
                continue;
            }

            if c_host.is_some() && line.starts_with("comment:") {
                // End of this host's block (same boundary convention as the
                // verdict loop below). Without the reset the deletion window
                // bled past the last host section and ate tester-authored lines
                // like 'reproducer : FAILED before update' from the
                // regression-tests notes.
                tmp.push(line.clone());
                c_host = None;
                continue;
            }

            // Keep the line unless it is a result line for a known host.
            let is_known_host = c_host
                .as_deref()
                .is_some_and(|h| hostnames.iter().any(|hn| hn.as_str() == h));
            if !is_result_line(line) || !is_known_host {
                tmp.push(line.clone());
            }
        }
        self.ctx.template = tmp;

        self.fillup_hosts_to_template();
    }

    /// Ensures each host has a section and fills its package before/after
    /// versions, flipping the verdict placeholder.
    fn fillup_hosts_to_template(&mut self) {
        // Pass 1: ensure a section exists for every host.
        for host in &self.results {
            let hostname = &host.hostname;
            let systemtype = &host.system;
            let set_line = format!("{systemtype} (reference host: {hostname})\n");
            if self.ctx.template.contains(&set_line) {
                continue;
            }
            tracing::debug!("host section {hostname} not found, searching system");
            let unset_line = format!("{systemtype} (reference host: ?)\n");
            if let Some(idx) = self.ctx.template.iter().position(|l| *l == unset_line) {
                self.ctx.template[idx] = set_line;
                continue;
            }
            tracing::debug!("system section {systemtype} not found, creating new one");

            let anchor = self
                .ctx
                .template
                .iter()
                .position(|l| l == "Test results by product-arch:\n")
                .or_else(|| {
                    self.ctx
                        .template
                        .iter()
                        .position(|l| l == "Test results by test platform:\n")
                });
            let Some(anchor) = anchor else {
                tracing::error!("update results section not found");
                break;
            };
            let index = anchor + 2;
            let block = [
                "\n".to_string(),
                format!("{systemtype} (reference host: {hostname})\n"),
                "--------------\n".to_string(),
                "before:\n".to_string(),
                "after:\n".to_string(),
                "\n".to_string(),
                "=> PASSED/FAILED\n".to_string(),
                "\n".to_string(),
                "comment: (none)\n".to_string(),
                "\n".to_string(),
            ];
            self.ctx.template.splice(index..index, block);
        }

        // Pass 2: fill package versions and flip the verdict.
        for host in &self.results {
            let hostname = &host.hostname;
            let systemtype = &host.system;
            let set_line = format!("{systemtype} (reference host: {hostname})\n");
            let Some(mut index) = self.ctx.template.iter().position(|l| *l == set_line) else {
                tracing::warn!("host section {hostname} not found");
                continue;
            };

            // #396: with no recorded package data there is nothing honest to
            // write — leave the scaffold's own lines and its undecided
            // `=> PASSED/FAILED` marker in place rather than flipping a
            // verdict over unverified content.
            if host.packages.is_empty() {
                tracing::warn!(
                    "no package version data recorded for {hostname}; leaving its \
                     install result unverified (scaffold lines and => PASSED/FAILED kept)"
                );
                continue;
            }

            // For before/after: track whether any package went un-updated,
            // and whether any slot was never checked at all (#396).
            let mut failed = false;
            let mut unverified = false;
            for state in ["before", "after"] {
                let state_line = format!("{state}:\n");
                let Some(pos) = self
                    .ctx
                    .template
                    .iter()
                    .skip(index)
                    .position(|l| *l == state_line)
                    .map(|i| i + index)
                else {
                    tracing::error!("{state} packages section not found");
                    continue;
                };
                index = pos + 1;

                for package in &host.packages {
                    let name = &package.name;
                    let check = match state {
                        "before" => package.before_check(),
                        _ => package.after_check(),
                    };
                    // #396: an unchecked slot must not become the positive
                    // claim "is not installed" — only a check that ran and
                    // found nothing may say that.
                    let new_line = match check {
                        VersionCheck::Installed(v) => format!("\t{name}-{v}\n"),
                        VersionCheck::NotInstalled => {
                            format!("\tpackage {name} is not installed\n")
                        }
                        VersionCheck::NotChecked => {
                            unverified = true;
                            format!("\tpackage {name}: not checked (no version data recorded)\n")
                        }
                    };
                    if index < self.ctx.template.len()
                        && self.ctx.template[index].contains(name.as_str())
                    {
                        self.ctx.template[index] = new_line;
                    } else {
                        self.ctx.template.insert(index, new_line);
                    }
                    index += 1;
                }
            }

            // A package that did not strictly increase before -> after fails.
            for package in &host.packages {
                if let (Some(before), Some(after)) = (package.before(), package.after())
                    && before >= after
                {
                    failed = true;
                }
            }
            if failed {
                tracing::warn!(
                    "installation test result on {hostname} set to FAILED as some packages were not updated. please override manually."
                );
            }

            // Flip the verdict placeholder, bounded by this host's comment line
            // or the next host block, so an already-set verdict is preserved.
            // Precedence (#396): a genuine regression flips FAILED even when
            // other packages are unverified; an unverified block with no
            // failure keeps the undecided placeholder rather than claiming
            // PASSED over content nobody checked.
            if !failed && unverified {
                tracing::warn!(
                    "installation test result on {hostname} left undecided: some package \
                     versions were never checked (run `update`, or `prepare` plus a \
                     version check, before exporting)"
                );
            }
            for j in index..self.ctx.template.len() {
                let line = &self.ctx.template[j];
                if line.contains("PASSED/FAILED") {
                    if failed {
                        self.ctx.template[j] = "=> FAILED\n".to_string();
                    } else if !unverified {
                        self.ctx.template[j] = "=> PASSED\n".to_string();
                    }
                    break;
                }
                if line.starts_with("comment:") || line.contains("reference host:") {
                    break;
                }
            }
        }
    }

    /// Runs the exporter.
    pub fn run(&mut self, hosts: &[String], prompt: &dyn OverwritePrompt) -> Vec<String> {
        self.install_results();
        let pp: Vec<String> = self.auto.as_ref().map(|a| a.pp.clone()).unwrap_or_default();
        self.ctx.inject_openqa(&pp);
        if let Some(overview) = self.overview.clone() {
            self.ctx.inject_overview(&overview);
        }
        let filenames = self.get_logs(hosts, prompt);
        self.ctx.installlogs_lines(&filenames);
        self.ctx.add_sysinfo();
        self.ctx.dedup_lines();
        self.ctx.template.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mtui_config::options::Config;
    use mtui_types::hostlog::CommandLog;

    fn ctx(template: &[&str]) -> ExportContext {
        let cfg = Config::default();
        let rrid = "SUSE:Maintenance:1:2".parse().unwrap();
        let lines: Vec<String> = template.iter().map(|s| (*s).to_string()).collect();
        ExportContext::new(cfg, &lines, false, rrid)
    }

    fn pkg(name: &str, before: Option<&str>, after: Option<&str>) -> Package {
        let mut p = Package::new(name);
        p.set_before(before).unwrap();
        p.set_after(after).unwrap();
        p
    }

    fn host(packages: Vec<Package>) -> ManualHost {
        ManualHost {
            hostname: "h1".into(),
            system: "system1".into(),
            packages,
            hostlog: HostLog::new(),
        }
    }

    fn host_block() -> Vec<&'static str> {
        vec![
            "system1 (reference host: h1)\n",
            "before:\n",
            "after:\n",
            "\n",
            "=> PASSED/FAILED\n",
            "\n",
            "comment: (none)\n",
        ]
    }

    /// #396: a never-observed slot must render "not checked", never the
    /// positive claim "is not installed", and the block must not flip PASSED.
    #[test]
    fn install_results_says_not_checked_for_unobserved() {
        // Package::new with no set_* calls: nothing was ever observed.
        let mut ex = ManualExport::new(
            ctx(&host_block()),
            vec![host(vec![Package::new("bash")])],
            None,
            None,
        );
        ex.fillup_hosts_to_template();
        let body = ex.ctx.template.concat();
        assert!(
            body.contains("package bash: not checked (no version data recorded)"),
            "{body}"
        );
        assert!(!body.contains("bash is not installed"), "{body}");
        assert!(
            body.contains("=> PASSED/FAILED\n"),
            "undecided placeholder kept: {body}"
        );
        assert!(
            !body.contains("=> PASSED\n"),
            "must not claim PASSED: {body}"
        );
    }

    /// Observed-absent (a real `None` observation) keeps the classic wording.
    #[test]
    fn install_results_keeps_is_not_installed_for_observed_absent() {
        let mut ex = ManualExport::new(
            ctx(&host_block()),
            vec![host(vec![pkg("bash", None, None)])],
            None,
            None,
        );
        ex.fillup_hosts_to_template();
        let body = ex.ctx.template.concat();
        assert!(body.contains("package bash is not installed"), "{body}");
        assert!(!body.contains("not checked"), "{body}");
    }

    /// #396: a genuine regression still flips FAILED even when another package
    /// is unverified — unverified must never hide a failure.
    #[test]
    fn install_results_failed_wins_over_unverified() {
        let mut ex = ManualExport::new(
            ctx(&host_block()),
            vec![host(vec![
                pkg("bash", Some("2"), Some("2")),
                Package::new("zsh"),
            ])],
            None,
            None,
        );
        ex.fillup_hosts_to_template();
        let body = ex.ctx.template.concat();
        assert!(body.contains("=> FAILED\n"), "{body}");
        // The mixed block renders BOTH line kinds side by side.
        assert!(body.contains("\tbash-2\n"), "{body}");
        assert!(
            body.contains("package zsh: not checked (no version data recorded)"),
            "{body}"
        );
    }

    /// #396: a host with NO recorded package data keeps the scaffold verbatim —
    /// including its undecided verdict — instead of flipping PASSED over
    /// content nobody verified.
    #[test]
    fn install_results_skips_empty_host_block() {
        let scaffold = vec![
            "system1 (reference host: h1)\n",
            "before:\n",
            "\tpackage bash is not installed\n",
            "after:\n",
            "\tpackage bash is not installed\n",
            "\n",
            "=> PASSED/FAILED\n",
            "\n",
            "comment: (none)\n",
        ];
        let mut ex = ManualExport::new(ctx(&scaffold), vec![host(vec![])], None, None);
        ex.fillup_hosts_to_template();
        let body = ex.ctx.template.concat();
        assert!(
            body.contains("=> PASSED/FAILED\n"),
            "scaffold verdict kept: {body}"
        );
        assert!(!body.contains("=> PASSED\n"), "no invented PASSED: {body}");
        // The scaffold's own (unverified) lines are left untouched.
        assert_eq!(body.matches("is not installed").count(), 2, "{body}");
    }

    #[test]
    fn fillup_flips_passed_when_version_increases() {
        let mut ex = ManualExport::new(
            ctx(&host_block()),
            vec![host(vec![pkg("bash", Some("1"), Some("2"))])],
            None,
            None,
        );
        ex.fillup_hosts_to_template();
        let body = ex.ctx.template.concat();
        assert!(body.contains("=> PASSED\n"));
        assert!(!body.contains("=> PASSED/FAILED\n"));
    }

    #[test]
    fn fillup_flips_failed_when_version_unchanged() {
        let mut ex = ManualExport::new(
            ctx(&host_block()),
            vec![host(vec![pkg("bash", Some("2"), Some("2"))])],
            None,
            None,
        );
        ex.fillup_hosts_to_template();
        assert!(ex.ctx.template.concat().contains("=> FAILED\n"));
    }

    #[test]
    fn host_installog_filters_zypper_lines() {
        let mut h = host(vec![]);
        h.hostlog
            .push(CommandLog::new("zypper in bash", "ok", "", 0, 1));
        h.hostlog.push(CommandLog::new("ls", "x", "", 0, 1));
        let ex = ManualExport::new(ctx(&[]), vec![h], None, None);
        let out = ex.host_installog_to_template("h1");
        assert!(out.iter().any(|l| l.contains("zypper in bash")));
        assert!(
            !out[1..]
                .iter()
                .any(|l| l.contains("ls") && !l.contains("zypper"))
        );
    }

    #[test]
    fn host_installog_unknown_host_is_empty() {
        let ex = ManualExport::new(ctx(&[]), vec![], None, None);
        assert!(ex.host_installog_to_template("missing").is_empty());
    }

    #[test]
    fn is_result_line_excludes_placeholder() {
        // A real per-host verdict line is a result line...
        assert!(is_result_line("something : FAILED\n"));
        assert!(is_result_line("x : SUCCEEDED\n"));
        // ...but the "=> PASSED/FAILED" placeholder is not.
        assert!(!is_result_line("=> PASSED/FAILED\n"));
    }

    #[test]
    fn install_results_runs_fillup_and_flips_verdict() {
        // install_results delegates to fillup; an existing host block gets its
        // verdict decided from the package versions.
        let mut ex = ManualExport::new(
            ctx(&host_block()),
            vec![host(vec![pkg("bash", Some("1"), Some("2"))])],
            None,
            None,
        );
        ex.install_results();
        assert!(ex.ctx.template.concat().contains("=> PASSED\n"));
    }
}
