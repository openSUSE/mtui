//! Exporter for the manual workflow.
//!
//! Ports `mtui.update_workflow.export.manual.ManualExport`. It rewrites the
//! per-host result sections of the template from the connected hosts' package
//! before/after versions and command logs, then runs the base sequence.
//!
//! ## Host input
//!
//! Upstream reads `self.results`, a list of connected `Target`s, for
//! `hostname`, `system`, `packages`, and `hostlog`. Coupling this crate to the
//! concrete `mtui-hosts::Target` (which carries live connection state) would be
//! the wrong dependency direction, so the exporter takes a decoupled
//! [`ManualHost`] view capturing exactly those four fields. The composition root
//! (Phase 5) builds these from the live targets, mirroring how the downloader
//! takes `(host, tests)` pairs.

use std::sync::LazyLock;

use mtui_datasources::OpenQAOverviewResult;
use mtui_datasources::qem_dashboard::dashboard_openqa::DashboardAutoOpenQA;
use mtui_types::hostlog::HostLog;
use mtui_types::package::Package;
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

/// Matches a `reference host: <name>` line and captures the hostname (upstream
/// `reference host:\s+([^)\s]+)`). The pre-fix pattern (`reference host:\s (.*)$`
/// with `group(0)`) required two spaces after the colon (the template emits one)
/// and read the whole match, so it never matched a bare hostname and the
/// stale-result cleanup was dead (upstream `c870fe58`).
static REFERENCE_HOST_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"reference host:\s+([^)\s]+)").expect("valid reference-host regex")
});

/// Matches a per-host verdict result line (upstream
/// `\s:\s(SUCCEEDED|(?<!PASSED/)FAILED|INTERNAL ERROR)`). Rust `regex` has no
/// look-behind, so the `PASSED/FAILED` placeholder is excluded explicitly in
/// [`is_result_line`].
static RESULT_LINE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\s:\s(SUCCEEDED|FAILED|INTERNAL ERROR)").expect("valid result regex")
});

/// Whether `line` is a per-host verdict result line to be stripped.
///
/// Excludes the `PASSED/FAILED` placeholder (upstream's negative look-behind
/// `(?<!PASSED/)FAILED`).
fn is_result_line(line: &str) -> bool {
    RESULT_LINE_RE.is_match(line) && !line.contains("PASSED/FAILED")
}

/// The manual-workflow exporter.
pub struct ManualExport {
    /// Shared export state and helpers.
    pub ctx: ExportContext,
    /// The connected-host views (upstream `self.results`).
    results: Vec<ManualHost>,
    /// The "auto" openQA connector (for `inject_openqa`), if present.
    ///
    /// Upstream's manual export reads `metadata.openqa.auto`, a
    /// [`DashboardAutoOpenQA`]; only its rendered [`pp`](DashboardAutoOpenQA::pp)
    /// block is consumed here.
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

    /// Converts a host's install log to template lines (upstream
    /// `_host_installog_to_template`).
    ///
    /// Emits a `log from <host>:` header followed by the stdout of each
    /// `zypper `/`transactional-update` command; returns empty for an unknown
    /// host.
    fn host_installog_to_template(&self, target: &str) -> Vec<String> {
        let Some(host) = self.results.iter().find(|h| h.hostname == target) else {
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

    /// Writes each host's install log and returns the filenames
    /// (upstream `get_logs`).
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

    /// Strips previously-exported verdict lines, then rebuilds host sections
    /// (upstream `install_results`).
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
    /// versions, flipping the verdict placeholder (upstream
    /// `_fillup_hosts_to_template`).
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

            // For before/after: track whether any package went un-updated.
            let mut failed = false;
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
                    let version = match state {
                        "before" => package.before(),
                        _ => package.after(),
                    };
                    let new_line = match version {
                        Some(v) => format!("\t{name}-{v}\n"),
                        None => format!("\tpackage {name} is not installed\n"),
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
            for j in index..self.ctx.template.len() {
                let line = &self.ctx.template[j];
                if line.contains("PASSED/FAILED") {
                    self.ctx.template[j] =
                        if failed { "=> FAILED\n" } else { "=> PASSED\n" }.to_string();
                    break;
                }
                if line.starts_with("comment:") || line.contains("reference host:") {
                    break;
                }
            }
        }
    }

    /// Runs the exporter (upstream `run`).
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
        // ...but the "=> PASSED/FAILED" placeholder is not (negative
        // look-behind in upstream's regex).
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
