//! The mtui command implementations.
//!
//! Every command implements [`Command`](crate::Command) and is wired into the
//! [`Registry`](crate::Registry) by [`register_all`](crate::register_all). Each
//! is a thin async adapter over the lower crates' services, reached through the
//! [`Session`](crate::Session).
//!
//! Ported in port-order "waves" (`PLAN-phase5.md §5.5`). Wave 1 — the core
//! workflow that gates the phase — lands here: `run`, `lrun`, `update`,
//! `install`/`uninstall`, `prepare`, `downgrade`, `reboot`, `set_repo`,
//! `show_update_repos`.

pub mod support;

mod perform;

mod downgrade;
mod localrun;
mod prepare;
mod reboot;
mod run;
mod setrepo;
mod showrepos;
mod update;
mod zypper;

// Wave 2 — host & session management.
mod addhost;
mod config;
mod hostslock;
mod hoststate;
mod hostsunlock;
mod products;
mod quit;
mod reload;
mod removehost;
mod shell;
mod switch;
mod templates;
mod unload;
mod whoami;

// Wave 3 — testreport lifecycle, metadata & host-info commands.
mod checkout;
mod commit;
mod listbugs;
mod listhistory;
mod listhosts;
mod listmetadata;
mod listpackages;
mod listsessions;
mod listtimeout;
mod listupdatecommands;
mod listversions;
mod settimeout;
mod sftpget;
mod sftpput;
mod showdiff;
mod showlog;

pub use downgrade::Downgrade;
pub use localrun::LocalRun;
pub use prepare::Prepare;
pub use reboot::Reboot;
pub use run::Run;
pub use setrepo::SetRepo;
pub use showrepos::ShowUpdateRepos;
pub use update::Update;
pub use zypper::{Install, Uninstall};

pub use addhost::AddHost;
pub use config::ConfigCmd;
pub use hostslock::HostLock;
pub use hoststate::HostState;
pub use hostsunlock::HostsUnlock;
pub use products::ListProducts;
pub use quit::Quit;
pub use reload::ReloadProducts;
pub use removehost::RemoveHost;
pub use shell::Shell;
pub use switch::Switch;
pub use templates::ListTemplates;
pub use unload::Unload;
pub use whoami::Whoami;

pub use checkout::Checkout;
pub use commit::Commit;
pub use listbugs::ListBugs;
pub use listhistory::ListHistory;
pub use listhosts::ListHosts;
pub use listmetadata::ListMetadata;
pub use listpackages::ListPackages;
pub use listsessions::ListSessions;
pub use listtimeout::ListTimeout;
pub use listupdatecommands::ListUpdateCommands;
pub use listversions::ListVersions;
pub use settimeout::SetTimeout;
pub use sftpget::SftpGet;
pub use sftpput::SftpPut;
pub use showdiff::{AnalyzeDiff, ShowDiff};
pub use showlog::ShowLog;

/// Shared test scaffolding for the Wave-1 command bodies.
///
/// Builds a [`Session`](crate::Session) whose active report carries scripted
/// [`MockConnection`](mtui_hosts::MockConnection) hosts and a captured display,
/// so a command's `run` can be driven end-to-end offline and its output asserted.
#[cfg(test)]
pub(crate) mod testkit {
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    use async_trait::async_trait;
    use mtui_config::Config;
    use mtui_hosts::{HostsGroup, MockConnection, Target};
    use mtui_testreport::{TestReport, TestReportBase};
    use mtui_types::SystemProduct;
    use mtui_types::enums::{ExecutionMode, TargetState};
    use mtui_types::hostlog::CommandLog;

    use crate::display::{ColorMode, CommandPromptDisplay};
    use crate::session::Session;

    /// A minimal loaded report with a settable RRID and host group.
    pub struct FakeReport {
        base: TestReportBase,
        rrid: String,
    }

    #[async_trait]
    impl TestReport for FakeReport {
        fn base(&self) -> &TestReportBase {
            &self.base
        }
        fn base_mut(&mut self) -> &mut TestReportBase {
            &mut self.base
        }
        fn id(&self) -> String {
            self.rrid.clone()
        }
        fn parser(&self) -> HashMap<String, String> {
            HashMap::new()
        }
        fn update_repos_parser(&self) -> HashMap<SystemProduct, String> {
            HashMap::new()
        }
        fn list_update_commands(&self, _targets: &HostsGroup) {}
        async fn check_hash(&self) -> (bool, String, String) {
            (true, String::new(), String::new())
        }
    }

    /// A shared byte buffer used as the display sink.
    #[derive(Clone)]
    pub struct Buffer(Arc<Mutex<Vec<u8>>>);

    impl Buffer {
        /// The captured output as a `String`.
        #[must_use]
        pub fn contents(&self) -> String {
            String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
        }
    }

    impl std::io::Write for Buffer {
        fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(data);
            Ok(data.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// Builds a `Target` backed by a mock whose every command returns `stdout`
    /// with exit code 0 (serial mode keeps output deterministic).
    fn scripted_target(host: &str, stdout: &str) -> Target {
        let conn = MockConnection::new(host).with_default(CommandLog::new("", stdout, "", 0, 0));
        Target::with_connection(
            host,
            TargetState::Enabled,
            ExecutionMode::Serial,
            Box::new(conn),
        )
    }

    /// A session (interactive `false`) whose active report has the named hosts,
    /// each mock echoing `stdout`, plus a captured display. Returns the session
    /// and the buffer handle.
    #[must_use]
    pub fn session_with_hosts(rrid: &str, hosts: &[&str], stdout: &str) -> (Session, Buffer) {
        let buf = Buffer(Arc::new(Mutex::new(Vec::new())));
        let display = CommandPromptDisplay::with_sink(Box::new(buf.clone()), ColorMode::Never);
        let mut session = Session::with_display(Config::default(), false, display);

        let mut base = TestReportBase::new(Config::default());
        let targets: Vec<Target> = hosts.iter().map(|h| scripted_target(h, stdout)).collect();
        base.targets = HostsGroup::new(targets, false);
        session.templates.add(Box::new(FakeReport {
            base,
            rrid: rrid.to_owned(),
        }));
        (session, buf)
    }

    /// Builds a standalone loaded report with the named hosts (each mock echoing
    /// `stdout`), boxed for [`TemplateRegistry::add`](crate::TemplateRegistry).
    ///
    /// The building block behind [`session_with_hosts`]; command tests that need
    /// more than one loaded template call this to `add` extra reports.
    #[must_use]
    pub fn fake_report(
        rrid: &str,
        hosts: &[&str],
        stdout: &str,
    ) -> Box<dyn TestReport + Send + Sync> {
        let mut base = TestReportBase::new(Config::default());
        let targets: Vec<Target> = hosts.iter().map(|h| scripted_target(h, stdout)).collect();
        base.targets = HostsGroup::new(targets, false);
        Box::new(FakeReport {
            base,
            rrid: rrid.to_owned(),
        })
    }

    /// A session whose single host scripts `command` to a log that echoes the
    /// command back (as a real host does), so `lastin()` reflects what ran.
    #[must_use]
    pub fn session_scripting(
        rrid: &str,
        host: &str,
        command: &str,
        stdout: &str,
    ) -> (Session, Buffer) {
        let buf = Buffer(Arc::new(Mutex::new(Vec::new())));
        let display = CommandPromptDisplay::with_sink(Box::new(buf.clone()), ColorMode::Never);
        let mut session = Session::with_display(Config::default(), false, display);

        let conn = MockConnection::new(host)
            .with_response(command, CommandLog::new(command, stdout, "", 0, 0));
        let target = Target::with_connection(
            host,
            TargetState::Enabled,
            ExecutionMode::Serial,
            Box::new(conn),
        );
        let mut base = TestReportBase::new(Config::default());
        base.targets = HostsGroup::new(vec![target], false);
        session.templates.add(Box::new(FakeReport {
            base,
            rrid: rrid.to_owned(),
        }));
        (session, buf)
    }

    /// An empty (no templates loaded) session with a captured display.
    #[must_use]
    pub fn empty_session() -> (Session, Buffer) {
        let buf = Buffer(Arc::new(Mutex::new(Vec::new())));
        let display = CommandPromptDisplay::with_sink(Box::new(buf.clone()), ColorMode::Never);
        (
            Session::with_display(Config::default(), false, display),
            buf,
        )
    }

    /// Parses `argv` into `ArgMatches` for `command`, mirroring the engine's
    /// per-command parser (base template flags + the command's own args).
    #[must_use]
    pub fn matches(command: &dyn crate::Command, argv: &[&str]) -> clap::ArgMatches {
        let base = clap::Command::new(command.name()).no_binary_name(true);
        command
            .configure(base)
            .try_get_matches_from(argv)
            .expect("argv should parse")
    }
}
