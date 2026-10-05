//! Exporter for the manual workflow.
//!
//! Writes each connected host's update command log under the install-logs
//! directory.
//!
//! Coupling this crate to the concrete `mtui-hosts::Target` (which carries live
//! connection state) would be the wrong dependency direction, so the exporter
//! takes a decoupled [`ManualHost`] view of exactly the fields it needs; the
//! composition root (`mtui-core`) builds these from the live targets.

use mtui_types::hostlog::HostLog;
use mtui_types::package::Package;
use mtui_types::system::SystemProduct;

use super::base::{ExportContext, OverwritePrompt};

/// A decoupled view of a connected host, holding exactly what the manual
/// exporter reads from a `Target`.
#[derive(Debug, Clone)]
pub struct ManualHost {
    /// The reference-host hostname.
    pub hostname: String,
    /// The system/product type string (e.g. `sles12sp5-x86_64`).
    pub system: String,
    /// The host's base product, version and architecture — structured,
    /// unlike [`system`](Self::system): the `(product, version, arch)` triple
    /// `testing.install.checks[].target` joins against `install.targets[]`
    /// on (Phase 4 authoring, `authoring::manual`).
    pub product: SystemProduct,
    /// The host's packages with before/after versions.
    pub packages: Vec<Package>,
    /// The host's command log.
    pub hostlog: HostLog,
}

/// Appended when a host's install log carries only its header — no
/// `zypper `/`transactional-update` command ever ran for it.
const NO_UPDATE_RECORDED: &str = "no update command was recorded for this host\n";

/// The manual-workflow exporter.
pub struct ManualExport {
    /// Shared export state and helpers.
    pub ctx: ExportContext,
    /// The connected-host views.
    results: Vec<ManualHost>,
}

impl ManualExport {
    /// Builds a manual exporter over `ctx`.
    #[must_use]
    pub fn new(ctx: ExportContext, results: Vec<ManualHost>) -> Self {
        Self { ctx, results }
    }

    /// Converts a host's install log to template lines.
    ///
    /// Always emits a `log from <target>:` header. If a matching host ran any
    /// `zypper `/`transactional-update` command, each contributes a `#
    /// <command>` line plus its stdout; a command that ran always leaves that
    /// line, so a header followed only by [`NO_UPDATE_RECORDED`] is unambiguous
    /// — nothing that looked like an update ever executed on this host,
    /// whether because it is unknown to `results` or its `hostlog` had no
    /// matching command.
    fn host_installog_to_template(&self, target: &str) -> Vec<String> {
        let mut t = vec![format!("log from {target}:\n")];

        if let Some(host) = self.results.iter().find(|h| h.hostname == target) {
            for cmd_log in &host.hostlog {
                let cmd = &cmd_log.command;
                if cmd.contains("zypper ") || cmd.contains("transactional-update") {
                    t.push(format!("# {cmd}\n{}\n", cmd_log.stdout));
                }
            }
        } else {
            tracing::warn!("no install log recorded for {target}; exporting a log that says so");
        }

        if t.len() == 1 {
            t.push(NO_UPDATE_RECORDED.to_owned());
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

    /// Writes each host's install log, returning the filenames.
    pub fn write_logs(&self, hosts: &[String], prompt: &dyn OverwritePrompt) -> Vec<String> {
        self.get_logs(hosts, prompt)
    }
}

#[cfg(test)]
mod tests {
    use super::super::base::DenyOverwrite;
    use super::*;
    use mtui_config::options::Config;
    use mtui_types::hostlog::CommandLog;

    fn ctx() -> ExportContext {
        let cfg = Config::default();
        let rrid = "SUSE:Maintenance:1:2".parse().unwrap();
        ExportContext::new(cfg, false, rrid)
    }

    fn ctx_in(dir: &std::path::Path) -> ExportContext {
        let mut cfg = Config::default();
        cfg.template_dir = dir.to_path_buf();
        let rrid = "SUSE:Maintenance:1:2".parse().unwrap();
        ExportContext::new(cfg, false, rrid)
    }

    fn host(packages: Vec<Package>) -> ManualHost {
        ManualHost {
            hostname: "h1".into(),
            system: "system1".into(),
            product: SystemProduct::new("system1", "1", "x86_64"),
            packages,
            hostlog: HostLog::new(),
        }
    }

    #[test]
    fn host_installog_filters_zypper_lines() {
        let mut h = host(vec![]);
        h.hostlog
            .push(CommandLog::new("zypper in bash", "ok", "", 0, 1));
        h.hostlog.push(CommandLog::new("ls", "x", "", 0, 1));
        let ex = ManualExport::new(ctx(), vec![h]);
        let out = ex.host_installog_to_template("h1");
        assert!(out.iter().any(|l| l.contains("zypper in bash")));
        assert!(
            !out[1..]
                .iter()
                .any(|l| l.contains("ls") && !l.contains("zypper"))
        );
    }

    /// A `hostlog` present but carrying no `zypper `/`transactional-update`
    /// command (lock contention, a dropped link, or `export` run without
    /// `update`) must not produce a header-only file.
    #[test]
    fn install_log_marks_that_no_update_ran() {
        let mut h = host(vec![]);
        h.hostlog
            .push(CommandLog::new("rpm -q wicked", "wicked-0.6\n", "", 0, 1));
        h.hostlog
            .push(CommandLog::new("cat /var/lock/mtui.lock", "", "", 0, 1));
        let ex = ManualExport::new(ctx(), vec![h]);
        let out = ex.host_installog_to_template("h1");
        assert_eq!(out, ["log from h1:\n", NO_UPDATE_RECORDED]);
    }

    #[test]
    fn host_installog_unknown_host_marks_no_update() {
        let ex = ManualExport::new(ctx(), vec![]);
        let out = ex.host_installog_to_template("missing");
        assert_eq!(out, ["log from missing:\n", NO_UPDATE_RECORDED]);
    }

    /// End-to-end: the *written* `install_logs/h1.log` file, not just the
    /// in-memory helper output, carries the marker for a host with no
    /// matching command.
    #[test]
    fn get_logs_writes_marker_when_no_update_ran() {
        let dir = tempfile::tempdir().unwrap();
        let mut h = host(vec![]);
        h.hostlog
            .push(CommandLog::new("rpm -q wicked", "wicked-0.6\n", "", 0, 1));
        let ex = ManualExport::new(ctx_in(dir.path()), vec![h]);

        let filenames = ex.get_logs(&["h1".to_owned()], &DenyOverwrite);
        assert_eq!(filenames, ["h1.log"]);

        let written = std::fs::read_to_string(ex.ctx.install_logs_dir().join("h1.log")).unwrap();
        assert!(!written.is_empty(), "must not be zero-byte: {written:?}");
        assert!(written.contains(NO_UPDATE_RECORDED), "{written:?}");
    }
}
