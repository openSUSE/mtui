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

// Wave 4 — backend APIs, openQA/QEM queue & workflow.
mod apicall;
mod approve;
mod checkers;
mod openqa_jobs;
mod openqa_overview;
mod regenerate;
mod reload_openqa;
mod request_review;
mod simpleset;
mod updates;

// Wave 3 — testreport lifecycle, metadata & host-info commands.
mod checkout;
mod commit;
mod listbugs;
mod listhistory;
mod listhosts;
mod listlocks;
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

// Phase 5 follow-ups — deferred commands whose machinery has since landed.
mod export;
mod list_refhosts;
mod load_template;

// Phase 6 — REPL-only command-surface additions.
mod edit;
mod help;
mod terms;

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

pub use apicall::{Assign, Comment, Reject, Unassign};
pub use approve::Approve;
pub use checkers::Checkers;
pub use openqa_jobs::OpenQAJobs;
pub use openqa_overview::OpenQAOverview;
pub use regenerate::Regenerate;
pub use reload_openqa::ReloadOpenQA;
pub use request_review::RequestReview;
pub use simpleset::{SetLogLevel, SetWorkflow};
pub use updates::Updates;

pub use checkout::Checkout;
pub use commit::Commit;
pub use edit::Edit;
pub use export::Export;
pub use help::Help;
pub use list_refhosts::ListRefhosts;
pub use listbugs::ListBugs;
pub use listhistory::ListHistory;
pub use listhosts::ListHosts;
pub use listlocks::ListLocks;
pub use listmetadata::ListMetadata;
pub use listpackages::ListPackages;
pub use listsessions::ListSessions;
pub use listtimeout::ListTimeout;
pub use listupdatecommands::ListUpdateCommands;
pub use listversions::ListVersions;
pub use load_template::LoadTemplate;
pub use settimeout::SetTimeout;
pub use sftpget::SftpGet;
pub use sftpput::SftpPut;
pub use showdiff::{AnalyzeDiff, ShowDiff};
pub use showlog::ShowLog;
pub use terms::Terms;

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
    use mtui_hosts::{HostsGroup, MockConnection, RepoOp, SetRepo, Target};
    use mtui_testreport::{HashCheck, TestReport, TestReportBase};
    use mtui_types::SystemProduct;
    use mtui_types::enums::{ExecutionMode, TargetState};
    use mtui_types::hostlog::CommandLog;

    use crate::display::{ColorMode, CommandPromptDisplay};
    use crate::session::Session;

    /// A minimal loaded report with a settable RRID and host group.
    pub struct FakeReport {
        base: TestReportBase,
        rrid: String,
        /// When `true`, `perform_update` returns an `Err` so the command's
        /// failure path (error toast, non-`Ok` result) can be exercised.
        fail_update: bool,
        /// When `true`, the `perform_install`/`uninstall`/`prepare`/`downgrade`
        /// flows return an `Err` naming host `h1`, so the shared `perform::drive`
        /// failure path (per-host summary + non-`Ok` result) can be exercised.
        fail_perform: bool,
        /// When `true`, [`as_set_repo`](TestReport::as_set_repo) returns `Some`
        /// so `set_repo` drives the (empty-repo) `run_zypper` refresh — the only
        /// public path that records [`Target::last_repo`] — letting the command's
        /// per-host repo summary + failure aggregation be exercised via the
        /// mock's scripted `zypper -n ref` exit code.
        set_repo_enabled: bool,
    }

    impl FakeReport {
        /// The shared verdict for the `perform_install`-family overrides: an
        /// `Err` naming host `h1` when `fail_perform`, else `Ok(())`.
        fn perform_verdict(&self) -> Result<(), mtui_testreport::UpdateError> {
            if self.fail_perform {
                Err(mtui_testreport::UpdateError::new("RPM Error", "h1"))
            } else {
                Ok(())
            }
        }
    }

    #[async_trait]
    impl SetRepo for FakeReport {
        async fn set_repo(&self, target: &mut Target, _operation: RepoOp) {
            // Drive the empty-repo `run_zypper` refresh — the only public path
            // that records `Target::last_repo`. With no matched repos only the
            // trailing `zypper -n ref` runs, so the recorded verdict follows the
            // mock's scripted exit code for that command.
            let rrid = self.base.rrid.clone().expect("test rrid set");
            target
                .repo_manager()
                .run_zypper("ar", &std::collections::BTreeMap::new(), &rrid)
                .await;
        }
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
        async fn perform_update(
            &self,
            _targets: &mut HostsGroup,
            _noprepare: bool,
            _newpackage: bool,
            diagnostics: &mut Vec<mtui_testreport::Diagnostic>,
        ) -> Result<(), mtui_testreport::UpdateError> {
            // Emit both diagnostic shapes so the perform-layer render path is
            // exercised end-to-end (a highlighted section + a plain section).
            diagnostics.push(mtui_testreport::Diagnostic::highlighted(
                "\nwarning: extra rpm output\n",
            ));
            diagnostics.push(mtui_testreport::Diagnostic::plain(
                "The following package is not supported by its vendor:\nfoo",
            ));
            if self.fail_update {
                Err(mtui_testreport::UpdateError::new(
                    "update stack locked",
                    "h1",
                ))
            } else {
                Ok(())
            }
        }
        async fn perform_install(
            &self,
            _targets: &mut HostsGroup,
            _packages: &[String],
        ) -> Result<(), mtui_testreport::UpdateError> {
            self.perform_verdict()
        }
        async fn perform_uninstall(
            &self,
            _targets: &mut HostsGroup,
            _packages: &[String],
        ) -> Result<(), mtui_testreport::UpdateError> {
            self.perform_verdict()
        }
        async fn perform_prepare(
            &self,
            _targets: &mut HostsGroup,
            _packages: &[String],
            _force: bool,
            _testing: bool,
            _installed_only: bool,
        ) -> Result<(), mtui_testreport::UpdateError> {
            self.perform_verdict()
        }
        async fn perform_downgrade(
            &self,
            _targets: &mut HostsGroup,
            _packages: &[String],
        ) -> Result<(), mtui_testreport::UpdateError> {
            self.perform_verdict()
        }
        fn as_set_repo(&self) -> Option<&dyn SetRepo> {
            if self.set_repo_enabled {
                Some(self)
            } else {
                None
            }
        }
        fn parser(&self) -> HashMap<String, String> {
            HashMap::new()
        }
        fn update_repos_parser(&self) -> HashMap<SystemProduct, String> {
            HashMap::new()
        }
        fn list_update_commands(&self, _targets: &HostsGroup) {}
        async fn check_hash(&self) -> HashCheck {
            HashCheck::Ok
        }
    }

    /// A shared byte buffer used as the display sink.
    #[derive(Clone)]
    pub struct Buffer(Arc<Mutex<Vec<u8>>>);

    impl Buffer {
        /// A fresh, empty capture buffer (for tests building a custom session).
        #[must_use]
        pub fn new() -> Self {
            Self(Arc::new(Mutex::new(Vec::new())))
        }

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
        let targets: Vec<Target> = hosts.iter().map(|h| scripted_target(h, stdout)).collect();
        session_with_targets(rrid, targets)
    }

    /// A session (interactive `false`) whose active report holds one host per
    /// `(name, ok)` pair: `ok == false` wires a [`MockConnection`] whose
    /// `sftp_put` fails, so the `put` command's per-host upload aggregation can
    /// be exercised. The failure reason echoes the host name for assertion.
    #[must_use]
    pub fn session_with_upload_outcomes(rrid: &str, hosts: &[(&str, bool)]) -> (Session, Buffer) {
        let targets: Vec<Target> = hosts
            .iter()
            .map(|&(name, ok)| {
                let mut conn =
                    MockConnection::new(name).with_default(CommandLog::new("", "", "", 0, 0));
                if !ok {
                    conn = conn.with_sftp_put_failure(format!("{name} disk full"));
                }
                Target::with_connection(
                    name,
                    TargetState::Enabled,
                    ExecutionMode::Serial,
                    Box::new(conn),
                )
            })
            .collect();
        session_with_targets(rrid, targets)
    }

    /// A session (interactive `false`) whose active report holds one host per
    /// `(name, ok)` pair for the `reboot` command: `ok == true` wires a
    /// [`MockConnection::with_changing_boot_id`] (the boot id differs across the
    /// pre/post-reboot reads, so the group records a successful reboot); `ok ==
    /// false` scripts a *fixed* boot id, modelling a host that never rebooted (an
    /// unchanged id ⇒ recorded failure).
    #[must_use]
    pub fn session_with_reboot_outcomes(rrid: &str, hosts: &[(&str, bool)]) -> (Session, Buffer) {
        let targets: Vec<Target> = hosts
            .iter()
            .map(|&(name, ok)| {
                let mut conn =
                    MockConnection::new(name).with_default(CommandLog::new("", "", "", 0, 0));
                if ok {
                    conn = conn.with_changing_boot_id();
                } else {
                    // A *fixed non-empty* boot id models a host that did not
                    // reboot: the pre/post reads match, so the group records a
                    // failure (an empty id would read as "could not confirm" ⇒
                    // Ok, which is not what we want here).
                    conn = conn.with_response(
                        "cat /proc/sys/kernel/random/boot_id",
                        CommandLog::new(
                            "cat /proc/sys/kernel/random/boot_id",
                            "boot-fixed\n",
                            "",
                            0,
                            0,
                        ),
                    );
                }
                Target::with_connection(
                    name,
                    TargetState::Enabled,
                    ExecutionMode::Serial,
                    Box::new(conn),
                )
            })
            .collect();
        session_with_targets(rrid, targets)
    }

    /// A session (interactive `false`) whose active report holds one host per
    /// `(name, ok)` pair for the `get` command: `ok == false` wires a
    /// [`MockConnection::failing_sftp_get`] so the download aggregation's
    /// failure path can be exercised. The report carries a real checkout path so
    /// `report_wd` resolves.
    #[must_use]
    pub fn session_with_download_outcomes(
        rrid: &str,
        hosts: &[(&str, bool)],
        report_wd: &std::path::Path,
    ) -> (Session, Buffer) {
        let targets: Vec<Target> = hosts
            .iter()
            .map(|&(name, ok)| {
                let mut conn =
                    MockConnection::new(name).with_default(CommandLog::new("", "", "", 0, 0));
                if !ok {
                    conn = conn.failing_sftp_get();
                }
                Target::with_connection(
                    name,
                    TargetState::Enabled,
                    ExecutionMode::Serial,
                    Box::new(conn),
                )
            })
            .collect();
        let buf = Buffer(Arc::new(Mutex::new(Vec::new())));
        let display = CommandPromptDisplay::with_sink(Box::new(buf.clone()), ColorMode::Never);
        let mut session = Session::with_display(Config::default(), false, display);

        let mut base = TestReportBase::new(Config::default());
        base.targets = HostsGroup::new(targets, false);
        base.rrid = rrid.parse().ok();
        // `report_wd` is the parent dir of the loaded report path.
        base.path = Some(report_wd.join("report.txt"));
        session.templates.add(Box::new(FakeReport {
            base,
            rrid: rrid.to_owned(),
            fail_update: false,
            fail_perform: false,
            set_repo_enabled: false,
        }));
        session.activate(rrid);
        (session, buf)
    }

    /// A session (interactive `false`) whose active report has the given
    /// already-built `targets`, plus a captured display. The building block
    /// behind [`session_with_hosts`] for callers that need to pre-wire a
    /// target beyond the uniform stdout-echo mock — e.g. calling `.connect()`
    /// on a [`Target::with_connection`] host scripted with a system-parse
    /// fixture before adding it to the group.
    #[must_use]
    pub fn session_with_targets(rrid: &str, targets: Vec<Target>) -> (Session, Buffer) {
        let buf = Buffer(Arc::new(Mutex::new(Vec::new())));
        let display = CommandPromptDisplay::with_sink(Box::new(buf.clone()), ColorMode::Never);
        let mut session = Session::with_display(Config::default(), false, display);

        let mut base = TestReportBase::new(Config::default());
        base.targets = HostsGroup::new(targets, false);
        base.rrid = rrid.parse().ok();
        session.templates.add(Box::new(FakeReport {
            base,
            rrid: rrid.to_owned(),
            fail_update: false,
            fail_perform: false,
            set_repo_enabled: false,
        }));
        // Install the active handle so direct `command.call()` tests (which
        // bypass the fan-out driver) read the report through `metadata()`.
        session.activate(rrid);
        (session, buf)
    }

    /// A session whose active report's `perform_update` returns `Err`, so the
    /// `update` command's failure path (error toast, non-`Ok` result) can be
    /// exercised end-to-end.
    #[must_use]
    pub fn session_with_failing_update(rrid: &str, hosts: &[&str]) -> (Session, Buffer) {
        let buf = Buffer(Arc::new(Mutex::new(Vec::new())));
        let display = CommandPromptDisplay::with_sink(Box::new(buf.clone()), ColorMode::Never);
        let mut session = Session::with_display(Config::default(), false, display);

        let targets: Vec<Target> = hosts.iter().map(|h| scripted_target(h, "")).collect();
        let mut base = TestReportBase::new(Config::default());
        base.targets = HostsGroup::new(targets, false);
        base.rrid = rrid.parse().ok();
        session.templates.add(Box::new(FakeReport {
            base,
            rrid: rrid.to_owned(),
            fail_update: true,
            fail_perform: false,
            set_repo_enabled: false,
        }));
        session.activate(rrid);
        (session, buf)
    }

    /// A session (interactive `false`) whose active report holds one host per
    /// `(name, ok)` pair for the `lock`/`unlock` commands: `ok == false` scripts
    /// the exclusive write of the operation-lock file (`/var/lock/mtui.lock`) to
    /// a hard [`HostError::Sftp`] (not a benign collision), so the group's
    /// [`LockOutcome::Failed`] path can be exercised. `ok == true` locks/unlocks
    /// cleanly.
    #[must_use]
    pub fn session_with_lock_outcomes(rrid: &str, hosts: &[(&str, bool)]) -> (Session, Buffer) {
        let targets: Vec<Target> = hosts
            .iter()
            .map(|&(name, ok)| {
                let mut conn =
                    MockConnection::new(name).with_default(CommandLog::new("", "", "", 0, 0));
                if !ok {
                    conn = conn.with_exclusive_write_error("/var/lock/mtui.lock");
                }
                Target::with_connection(
                    name,
                    TargetState::Enabled,
                    ExecutionMode::Serial,
                    Box::new(conn),
                )
            })
            .collect();
        session_with_targets(rrid, targets)
    }

    /// A session (interactive `false`) whose active report is `SetRepo`-capable
    /// and holds one host per `(name, ok)` pair for the `set_repo` command:
    /// `ok == false` scripts the trailing `zypper -n ref` refresh to a non-zero
    /// exit so `last_repo` records a failure; `ok == true` leaves the default
    /// exit-0 mock so the run records success.
    #[must_use]
    pub fn session_with_setrepo_outcomes(rrid: &str, hosts: &[(&str, bool)]) -> (Session, Buffer) {
        let targets: Vec<Target> = hosts
            .iter()
            .map(|&(name, ok)| {
                let conn = MockConnection::new(name).with_default(if ok {
                    CommandLog::new("", "", "", 0, 0)
                } else {
                    CommandLog::new("zypper -n ref", "", "refresh failed", 1, 0)
                });
                Target::with_connection(
                    name,
                    TargetState::Enabled,
                    ExecutionMode::Serial,
                    Box::new(conn),
                )
            })
            .collect();
        let buf = Buffer(Arc::new(Mutex::new(Vec::new())));
        let display = CommandPromptDisplay::with_sink(Box::new(buf.clone()), ColorMode::Never);
        let mut session = Session::with_display(Config::default(), false, display);

        let mut base = TestReportBase::new(Config::default());
        base.targets = HostsGroup::new(targets, false);
        base.rrid = rrid.parse().ok();
        session.templates.add(Box::new(FakeReport {
            base,
            rrid: rrid.to_owned(),
            fail_update: false,
            fail_perform: false,
            set_repo_enabled: true,
        }));
        session.activate(rrid);
        (session, buf)
    }

    /// A session whose active report's `perform_install`/`uninstall`/`prepare`/
    /// `downgrade` flows return `Err` (naming host `h1`), so `perform::drive`'s
    /// failure path (per-host summary + non-`Ok` result) can be exercised.
    #[must_use]
    pub fn session_with_failing_perform(rrid: &str, hosts: &[&str]) -> (Session, Buffer) {
        let buf = Buffer(Arc::new(Mutex::new(Vec::new())));
        let display = CommandPromptDisplay::with_sink(Box::new(buf.clone()), ColorMode::Never);
        let mut session = Session::with_display(Config::default(), false, display);

        let targets: Vec<Target> = hosts.iter().map(|h| scripted_target(h, "")).collect();
        let mut base = TestReportBase::new(Config::default());
        base.targets = HostsGroup::new(targets, false);
        base.rrid = rrid.parse().ok();
        session.templates.add(Box::new(FakeReport {
            base,
            rrid: rrid.to_owned(),
            fail_update: false,
            fail_perform: true,
            set_repo_enabled: false,
        }));
        session.activate(rrid);
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
        base.rrid = rrid.parse().ok();
        Box::new(FakeReport {
            base,
            rrid: rrid.to_owned(),
            fail_update: false,
            fail_perform: false,
            set_repo_enabled: false,
        })
    }

    /// Boxes a caller-built [`TestReportBase`] under `rrid` into the shared
    /// [`FakeReport`] double.
    ///
    /// The escape hatch behind [`fake_report`] for tests that must pre-wire the
    /// host group beyond the uniform stdout-echo mock — e.g. a group holding a
    /// target whose mock `close` fails or wedges, to exercise the removal
    /// teardown paths.
    #[must_use]
    pub fn fake_report_from_base(base: TestReportBase) -> Box<dyn TestReport + Send + Sync> {
        let rrid = base
            .rrid
            .as_ref()
            .map(ToString::to_string)
            .unwrap_or_default();
        Box::new(FakeReport {
            base,
            rrid,
            fail_update: false,
            fail_perform: false,
            set_repo_enabled: false,
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
        base.rrid = rrid.parse().ok();
        session.templates.add(Box::new(FakeReport {
            base,
            rrid: rrid.to_owned(),
            fail_update: false,
            fail_perform: false,
            set_repo_enabled: false,
        }));
        session.activate(rrid);
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

    /// A session with hosts wired onto the active report but **no template
    /// loaded** (the active report stays the `NullReport`, so
    /// `session.metadata().is_loaded()` is `false`). Mirrors
    /// [`session_with_hosts`] but skips `templates.add`, so commands can be
    /// exercised in the no-metadata state a real REPL reaches before any
    /// template is checked out.
    #[must_use]
    pub fn session_host_no_template(hosts: &[&str], stdout: &str) -> (Session, Buffer) {
        let (mut session, buf) = empty_session();
        let targets: Vec<Target> = hosts.iter().map(|h| scripted_target(h, stdout)).collect();
        session.metadata_mut().base_mut().targets = HostsGroup::new(targets, false);
        (session, buf)
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

/// Standing MCP-contract guard: every non-denied command that reaches a
/// successful (`Ok(())`) dispatch must write **something** to the session
/// display.
///
/// ## Why this lives here (approach "b")
///
/// The MCP tool result text is exactly what a command writes to `session.display`
/// on `Ok(())`; an empty success block makes the MCP tool return an empty
/// confirmation and the client hallucinate a result. A whole epic fixed commands
/// that silently returned `Ok(())` with no output; this test stops the
/// regression coming back.
///
/// It would ideally live in `mtui-mcp` (where the deny-filter + tool synthesis
/// run), but the only machinery that drives the state-mutating fan-out commands
/// to a *real* success with per-host output is the [`testkit`] above — its
/// `FakeReport` double plus mock-host session builders — which is `#[cfg(test)]`
/// and `pub(crate)`, unreachable from another crate. Re-implementing that fixture
/// surface in `mtui-mcp` would be a large duplication for no extra signal, so the
/// guard runs here against the **same display sink** the MCP capture buffer wraps
/// and iterates the **same** [`register_all`](crate::register_all) registry minus
/// the **same** [`MCP_DENYLIST`](crate::MCP_DENYLIST) the MCP tool layer filters
/// on. Each command is driven through the real engine
/// ([`dispatch_command`](crate::dispatch_command)), not `Command::call`, so the
/// fan-out path an MCP call takes is exercised.
#[cfg(test)]
mod mcp_nonempty_success_guard {
    use super::testkit::{
        Buffer, empty_session, matches, session_with_download_outcomes, session_with_hosts,
        session_with_setrepo_outcomes, session_with_upload_outcomes,
    };
    use crate::session::Session;
    use crate::{MCP_DENYLIST, dispatch_command, register_all};

    const RRID: &str = "SUSE:Maintenance:1:1";

    /// Commands legitimately excluded from the non-empty-success guard, each with
    /// a `// why:` justification. Keep this list as small as honestly possible:
    /// every entry here is a command whose *success* path cannot be reached in an
    /// offline, headless test without standing up an external system (SVN, the
    /// OBS/IBS or Gitea API, openQA/QEM, a live SSH host) or without fixture
    /// plumbing far beyond what the shared testkit offers. Their own per-command
    /// unit tests assert they print a success line before returning `Ok(())`; this
    /// guard covers everything that can be driven offline.
    const ALLOW_EMPTY_SUCCESS: &[&str] = &[
        // why: checkout/commit shell out to `svn` against a working copy; success
        // needs a real SVN repo+checkout (their tests skip when svn is absent).
        "checkout",
        "commit",
        // why: the QAM review workflow talks to the OBS/IBS or Gitea API; the
        // success line ("assigned {rrid}" etc.) is only printed after a live
        // network call succeeds, unreachable offline without a mock server.
        "approve",
        "assign",
        "unassign",
        "reject",
        "comment",
        // why: query openQA/QEM (and the QEM Dashboard); success requires a mock
        // HTTP backend (wiremock) their own tests stand up per case.
        "checkers",
        "updates",
        "openqa_overview",
        "openqa_jobs",
        "reload_openqa",
        "regenerate",
        // why: builds the QEM incident + openQA "auto" result on every mode
        // change (build_incident/refresh_auto), so success needs the QEM
        // Dashboard + openQA backends its own tests mock per case.
        "set_workflow",
        // why: posts to the Slack Web API; the success line is printed only
        // after a live `auth.test` + `chat.postMessage` succeed, so driving it
        // needs the wiremock backend its own tests stand up per case.
        "request_review",
        // why: connects a brand-new host over SSH; success needs a reachable
        // reference host (its tests only exercise the connect-failure path).
        "add_host",
        // why: folds per-host update logs sourced from openQA into a template
        // file; a real success needs a source template + openQA/QEM backend.
        "export",
        // why: resolves the refhosts store (network or a configured local file);
        // driving success needs a refhosts YAML fixture beyond the shared testkit.
        "list_refhosts",
        // why: renders a source.diff / spec diff; success needs a checkout with a
        // diff fixture (its tests build one via a local-only helper).
        "show_diff",
        "analyze_diff",
    ];

    /// Build the loaded session + argv that drives `name` to a successful
    /// dispatch with output, for every command the guard *enforces*. Returns
    /// `None` for a name not enforced here (it must then be on the allow-list).
    fn fixture(name: &str) -> Option<(Session, Buffer, Vec<String>)> {
        // Most commands only need a single mock host echoing "ok".
        let hosts = || session_with_hosts(RRID, &["h1"], "ok");
        let argv = |a: &[&str]| a.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();

        let (session, buf, args): (Session, Buffer, Vec<String>) = match name {
            // Wave 1 — core workflow fan-outs.
            "run" => {
                let (s, b) = hosts();
                (s, b, argv(&["true"]))
            }
            "update" => {
                let (s, b) = hosts();
                (s, b, argv(&["--noprepare"]))
            }
            "install" => {
                let (s, b) = hosts();
                (s, b, argv(&["pkg"]))
            }
            "uninstall" => {
                let (s, b) = hosts();
                (s, b, argv(&["pkg"]))
            }
            "prepare" => {
                let (s, b) = hosts();
                (s, b, argv(&["-u"]))
            }
            "downgrade" => {
                let (s, b) = hosts();
                (s, b, argv(&[]))
            }
            "reboot" => {
                // Needs a boot-id that changes across the pre/post read.
                let (s, b) = super::testkit::session_with_reboot_outcomes(RRID, &[("h1", true)]);
                (s, b, argv(&[]))
            }
            "set_repo" => {
                let (s, b) = session_with_setrepo_outcomes(RRID, &[("h1", true)]);
                (s, b, argv(&["-A"]))
            }
            "show_update_repos" => {
                let (s, b) = hosts();
                (s, b, argv(&[]))
            }
            "list_update_commands" => {
                let (s, b) = hosts();
                (s, b, argv(&[]))
            }
            // Wave 2 — host & session management.
            "remove_host" => {
                let (s, b) = session_with_hosts(RRID, &["h1", "h2"], "ok");
                (s, b, argv(&["-t", "h1"]))
            }
            "set_host_state" => {
                let (s, b) = hosts();
                (s, b, argv(&["disabled", "-t", "h1"]))
            }
            "lock" => {
                let (s, b) = hosts();
                (s, b, argv(&[]))
            }
            "unlock" => {
                let (s, b) = hosts();
                (s, b, argv(&[]))
            }
            "list_hosts" => {
                let (s, b) = hosts();
                (s, b, argv(&[]))
            }
            "list_timeout" => {
                let (s, b) = hosts();
                (s, b, argv(&[]))
            }
            "list_products" => {
                let (s, b) = hosts();
                (s, b, argv(&["-t", "h1"]))
            }
            "reload_products" => {
                let (s, b) = hosts();
                (s, b, argv(&["-t", "h1"]))
            }
            "whoami" => {
                let (s, b) = empty_session();
                (s, b, argv(&[]))
            }
            "config" => {
                let (s, b) = empty_session();
                (s, b, argv(&["show", "session_user"]))
            }
            "unload" => {
                // Unloading the loaded template prints "unloaded <rrid>".
                let (s, b) = hosts();
                (s, b, argv(&[RRID]))
            }
            "list_templates" => {
                let (s, b) = hosts();
                (s, b, argv(&[]))
            }
            // Wave 3 — testreport metadata & host-info.
            "list_metadata" => {
                let (s, b) = hosts();
                (s, b, argv(&[]))
            }
            "list_bugs" => {
                let (s, b) = hosts();
                (s, b, argv(&[]))
            }
            "list_packages" => {
                let (s, b) = session_with_hosts(RRID, &["h1"], "bash 5.1-1\n");
                (s, b, argv(&["-p", "bash", "-t", "h1"]))
            }
            "list_versions" => {
                let (s, b) = session_with_hosts(RRID, &["h1"], "bash 5.1-1\n");
                (s, b, argv(&["-p", "bash", "-t", "h1"]))
            }
            "list_history" => {
                let (s, b) = hosts();
                (s, b, argv(&["-t", "h1"]))
            }
            "list_locks" => {
                let (s, b) = hosts();
                (s, b, argv(&["-t", "h1"]))
            }
            "list_sessions" => {
                let (s, b) = session_with_hosts(RRID, &["h1"], "10.0.0.1\n");
                (s, b, argv(&["-t", "h1"]))
            }
            "show_log" => {
                let (s, b) = hosts();
                (s, b, argv(&["-t", "h1"]))
            }
            "set_timeout" => {
                let (s, b) = hosts();
                (s, b, argv(&["300", "-t", "h1"]))
            }
            "set_log_level" => {
                let (s, b) = empty_session();
                (s, b, argv(&["warning"]))
            }
            "put" => {
                // `put` uploads a real local file; write one into a temp dir.
                let dir = tempfile::tempdir().expect("tempdir");
                let file = dir.path().join("payload.txt");
                std::fs::write(&file, b"payload").expect("write payload");
                // Leak the dir guard: the command reads the file synchronously
                // during dispatch; the OS reclaims it at process exit.
                std::mem::forget(dir);
                let (s, b) = session_with_upload_outcomes(RRID, &[("h1", true)]);
                (s, b, argv(&[file.to_str().expect("utf8 path")]))
            }
            "get" => {
                let dir = tempfile::tempdir().expect("tempdir");
                let (s, b) = session_with_download_outcomes(RRID, &[("h1", true)], dir.path());
                std::mem::forget(dir);
                (s, b, argv(&["/remote/file.log"]))
            }
            "load_template" => {
                // A kernel (`-k`) load of an on-disk template registers +
                // activates it without connecting, printing "loaded <rrid>".
                let (mut s, b) = empty_session();
                let dir = tempfile::tempdir().expect("tempdir");
                let krrid = "SUSE:Maintenance:24993:275518";
                let tdir = dir.path().join(krrid);
                std::fs::create_dir_all(&tdir).expect("mk template dir");
                std::fs::write(tdir.join("log"), "log\n").expect("write log");
                std::fs::write(
                    tdir.join("metadata.json"),
                    format!("{{\"rrid\": \"{krrid}\", \"repository\": \"http://x/\"}}"),
                )
                .expect("write metadata");
                s.config.template_dir = dir.path().to_path_buf();
                // Leak the dir guard: the load reads it synchronously during
                // dispatch; the OS reclaims it at process exit.
                std::mem::forget(dir);
                (s, b, argv(&["-k", krrid]))
            }
            _ => return None,
        };
        Some((session, buf, args))
    }

    /// Every non-denied command either has an enforced success fixture or is on
    /// the allow-list — never both, and never neither. This keeps the split
    /// exhaustive so a newly registered command cannot slip through unguarded.
    #[test]
    fn every_non_denied_command_is_enforced_or_allow_listed() {
        let registry = register_all();
        let mut names: Vec<&str> = registry.names().collect();
        names.sort_unstable();
        for name in names {
            if MCP_DENYLIST.contains(&name) {
                continue;
            }
            let enforced = fixture(name).is_some();
            let allowed = ALLOW_EMPTY_SUCCESS.contains(&name);
            assert!(
                enforced ^ allowed,
                "command {name:?} must be either enforced (has a success fixture) \
                 or on ALLOW_EMPTY_SUCCESS, exactly one — enforced={enforced}, \
                 allow_listed={allowed}",
            );
        }
        // The allow-list must not name a denied or unregistered command (drift).
        for allowed in ALLOW_EMPTY_SUCCESS {
            assert!(
                registry.contains(allowed) && !MCP_DENYLIST.contains(allowed),
                "ALLOW_EMPTY_SUCCESS entry {allowed:?} is denied or not registered",
            );
        }
    }

    /// The core invariant: every enforced command reaches `Ok(())` **and** leaves
    /// a non-empty display buffer, so its MCP tool result is never an empty
    /// success block.
    #[tokio::test]
    async fn enforced_commands_write_something_on_success() {
        let registry = register_all();
        let mut names: Vec<&str> = registry.names().collect();
        names.sort_unstable();
        for name in names {
            if MCP_DENYLIST.contains(&name) || ALLOW_EMPTY_SUCCESS.contains(&name) {
                continue;
            }
            let Some((mut session, buf, argv)) = fixture(name) else {
                panic!("enforced command {name:?} has no success fixture");
            };
            let command = registry.get(name).expect("registered");
            dispatch_command(command.as_ref(), &mut session, &argv)
                .await
                .unwrap_or_else(|e| panic!("{name} should dispatch successfully: {e}"));
            let out = buf.contents();
            assert!(
                !out.trim().is_empty(),
                "successful command {name:?} wrote nothing to the display; an MCP \
                 tool would return an empty success block. Print a confirmation, \
                 or (if it legitimately produces no output) add it to \
                 ALLOW_EMPTY_SUCCESS with a `// why:` note.",
            );
        }
    }

    /// Sanity check that the assertion is real, not a tautology: a fixture whose
    /// command prints nothing on success **would** fail the guard. We use a tiny
    /// stub command that returns `Ok(())` silently and confirm its buffer is
    /// empty (i.e. the enforced assertion above would fire on it).
    #[tokio::test]
    async fn guard_would_catch_a_silent_success() {
        use crate::command::Scope;
        use crate::{Command, CommandResult};
        use async_trait::async_trait;
        use clap::ArgMatches;

        struct Silent;
        #[async_trait]
        impl Command for Silent {
            fn name(&self) -> &'static str {
                "silent_probe"
            }
            fn scope(&self) -> Scope {
                Scope::Single
            }
            async fn call(&self, _s: &mut Session, _a: &ArgMatches) -> CommandResult {
                Ok(())
            }
        }

        let (mut session, buf) = empty_session();
        let args = matches(&Silent, &[]);
        Silent.call(&mut session, &args).await.unwrap();
        assert!(
            buf.contents().trim().is_empty(),
            "the silent stub must leave the buffer empty, so the guard's \
             non-empty assertion is a real check",
        );
    }
}
