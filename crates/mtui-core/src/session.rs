//! Shared, explicitly-passed command state (`Session`).
//!
//! The Rust replacement for upstream's `CommandPrompt` god-object. Commands
//! receive `&mut Session` and read/mutate its state through methods — there are
//! no hidden globals. It owns the [`Config`], the [`TemplateRegistry`] (loaded
//! templates + active pointer), the [`CommandPromptDisplay`] output sink, and
//! the `interactive` flag that distinguishes the REPL (`true`) from headless
//! callers such as `mtui-mcp` (`false`).
//!
//! The scalar `metadata` / `targets` accessors upstream exposes as read-only
//! properties are provided here as [`metadata`](Session::metadata) /
//! [`targets`](Session::targets), delegating to the active report so command
//! bodies and tests keep working as the registry grows past one entry.

use mtui_config::Config;
use mtui_datasources::http::VerifyPolicy;
use mtui_datasources::refhost::{Attributes, RefhostsFactory, ResolveConfig};
use mtui_hosts::{HostsGroup, Target};
use mtui_testreport::{TestReport, UpdateKind, make_testreport};
use mtui_types::UpdateID;
use mtui_types::enums::{ExecutionMode, TargetState};
use tracing::{info, warn};

use crate::display::CommandPromptDisplay;
use crate::template_registry::TemplateRegistry;

/// The explicitly-passed state every command operates on.
pub struct Session {
    /// The application configuration.
    pub config: Config,
    /// Loaded templates and the active pointer.
    pub templates: TemplateRegistry,
    /// Formatted-output sink.
    pub display: CommandPromptDisplay,
    /// `true` for the interactive REPL, `false` for headless callers (MCP).
    ///
    /// Drives the fan-out default: with several templates loaded and no
    /// interactive `switch` to pick an active one, an otherwise-unscoped command
    /// fans out across every template instead of silently picking one.
    pub interactive: bool,
    /// Set by the `quit` command to ask the interactive REPL loop to exit after
    /// the current dispatch returns.
    ///
    /// The Rust replacement for upstream `Quit` raising `SystemExit`/returning a
    /// truthy value from `onecmd`: rather than routing process-exit through the
    /// command error channel, `quit` flips this flag and returns `Ok(())`; the
    /// Phase-6 REPL checks [`should_exit`](Self::should_exit) after each line and
    /// breaks its loop. Headless callers (MCP) ignore it.
    should_exit: bool,
    /// Optional sink for runtime log-level changes (upstream
    /// `prompt.log.setLevel`).
    ///
    /// `set_log_level` calls this with the requested [`LogLevel`] when present.
    /// The Phase-6 REPL installs a callback backed by a
    /// `tracing_subscriber::reload` handle; headless callers and tests leave it
    /// `None`, so the command still logs the change but mutates nothing.
    log_level_sink: Option<LogLevelSink>,
}

/// The log levels `set_log_level` accepts (upstream `info`/`warning`/`error`/
/// `debug`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogLevel {
    /// Only errors.
    Error,
    /// Warnings and above.
    Warning,
    /// Informational and above (the default).
    Info,
    /// Everything, incl. debug tracing.
    Debug,
}

impl LogLevel {
    /// Parses the upstream level name, or `None` if unrecognised.
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "error" => Some(Self::Error),
            "warning" => Some(Self::Warning),
            "info" => Some(Self::Info),
            "debug" => Some(Self::Debug),
            _ => None,
        }
    }

    /// The corresponding [`tracing::Level`].
    #[must_use]
    pub fn as_tracing(self) -> tracing::Level {
        match self {
            Self::Error => tracing::Level::ERROR,
            Self::Warning => tracing::Level::WARN,
            Self::Info => tracing::Level::INFO,
            Self::Debug => tracing::Level::DEBUG,
        }
    }
}

/// A callback the REPL installs to apply a runtime log-level change.
pub type LogLevelSink = Box<dyn FnMut(LogLevel) + Send>;

impl Session {
    /// Builds a session for `config`, defaulting the display to stdout.
    ///
    /// `interactive` mirrors upstream: `true` for the REPL, `false` for MCP.
    #[must_use]
    pub fn new(config: Config, interactive: bool) -> Self {
        let templates = TemplateRegistry::new(config.clone());
        Self {
            config,
            templates,
            display: CommandPromptDisplay::stdout(),
            interactive,
            should_exit: false,
            log_level_sink: None,
        }
    }

    /// Builds a session with an explicit display sink (test/embedding seam).
    #[must_use]
    pub fn with_display(config: Config, interactive: bool, display: CommandPromptDisplay) -> Self {
        let templates = TemplateRegistry::new(config.clone());
        Self {
            config,
            templates,
            display,
            interactive,
            should_exit: false,
            log_level_sink: None,
        }
    }

    /// The active report (upstream `prompt.metadata`). Never `None` — the
    /// [`TemplateRegistry`] returns a null object when nothing is loaded.
    #[must_use]
    pub fn metadata(&self) -> &(dyn TestReport + Send + Sync) {
        self.templates.active()
    }

    /// The active report's connected targets (upstream `prompt.targets`).
    #[must_use]
    pub fn targets(&self) -> &HostsGroup {
        &self.templates.active().base().targets
    }

    /// Mutably borrows the active report's connected targets.
    ///
    /// The mutable counterpart of [`targets`](Self::targets); command bodies
    /// that fan a command out across hosts (`run`, `reboot`, `set_repo`) need
    /// `&mut HostsGroup`.
    pub fn targets_mut(&mut self) -> &mut HostsGroup {
        &mut self.templates.active_mut().base_mut().targets
    }

    /// Moves the active report's targets out, leaving an empty group in place.
    ///
    /// The counterpart to [`restore_targets`](Self::restore_targets). The
    /// report's `perform_*` methods take `&self` **and** `&mut HostsGroup`;
    /// because the targets live inside the active report, a single
    /// `&mut Box<dyn TestReport>` cannot hand out both borrows at once. Taking
    /// the group out by value breaks that tie: the caller then holds an owned
    /// `HostsGroup` (no borrow of `self`) and can freely re-borrow the report via
    /// [`metadata`](Self::metadata) to drive `perform_*`, restoring the group
    /// afterwards.
    ///
    /// Mirrors upstream, where a command reads `self.metadata` and `self.targets`
    /// as two views of the same active report.
    #[must_use]
    pub fn take_targets(&mut self) -> HostsGroup {
        let interactive = self.interactive;
        std::mem::replace(
            &mut self.templates.active_mut().base_mut().targets,
            HostsGroup::new(Vec::new(), interactive),
        )
    }

    /// Restores the active report's targets, undoing [`take_targets`](Self::take_targets).
    pub fn restore_targets(&mut self, targets: HostsGroup) {
        self.templates.active_mut().base_mut().targets = targets;
    }

    /// Takes the active report's targets and splits them into the `-t` selection
    /// and the unselected remainder.
    ///
    /// The lossless replacement for `take_targets()` + `HostsGroup::select` in
    /// the `perform_*` / `set_repo` drivers. A `-t` subset operation must run over
    /// only the selected hosts, yet the unselected hosts must survive in the live
    /// report — upstream gets this for free because its child group shares
    /// `Target` references with the parent dict, but a Rust `Target` owns its
    /// connection and cannot be shared. This hands back both halves so the driver
    /// can drive the op over `selected`, then hand both back to
    /// [`restore_split_targets`](Self::restore_split_targets), which merges the
    /// remainder back in.
    ///
    /// `hosts` is the parsed `-t` value: `None` (or `-t all`, which callers pass
    /// as `None`) selects every enabled host with an empty remainder; `Some` names
    /// exactly those hosts and keeps the rest in the remainder. Selection is
    /// `enabled`-filtered (disabled hosts land in the remainder, never dropped).
    ///
    /// # Errors
    ///
    /// [`mtui_hosts::HostError::NotConnected`] when a named `-t` host is not a
    /// member of the active report's group.
    ///
    /// On error the group is left empty in the report (the taken group is
    /// consumed by the failed split); callers surface the error immediately, so
    /// no host is observable in that window.
    pub fn split_targets(
        &mut self,
        hosts: Option<&[String]>,
    ) -> mtui_hosts::Result<(HostsGroup, HostsGroup)> {
        self.take_targets().select_split(hosts, true)
    }

    /// Merges the untouched `remainder` back into the operated `selected` group
    /// and restores it as the active report's targets.
    ///
    /// The counterpart to [`split_targets`](Self::split_targets): recombining the
    /// two halves preserves the hosts a `-t` subset operation did not touch.
    pub fn restore_split_targets(&mut self, mut selected: HostsGroup, remainder: HostsGroup) {
        selected.merge(remainder);
        self.restore_targets(selected);
    }

    /// Loads a template into the registry and, when requested, connects its
    /// reference hosts (upstream `prompt.load_update`).
    ///
    /// Mirrors upstream `CommandPrompt.load_update`:
    ///
    /// 1. [`make_testreport`] checks out and reads the report (or returns a null
    ///    report on failure, which [`TemplateRegistry::add`] silently ignores).
    /// 2. The report is added to the registry and — when it carries a real RRID —
    ///    made active. Re-loading an already-loaded RRID replaces its stored
    ///    report and makes it active; sibling templates are untouched.
    /// 3. If the report asked for autoconnect ([`TestReportBase::autoconnect_pending`],
    ///    set by `make_testreport` for `-a` with `autoconnect`), its reference
    ///    hosts are connected. The connect is driven **here** (the composition
    ///    root) rather than inside `mtui-testreport`, so that crate never depends
    ///    on `mtui-hosts`/`mtui-datasources` — no crate cycle.
    ///
    /// The connect resolves hosts from two sources, matching upstream
    /// `TestReport.autoconnect`: the template's own `reference host:` lines
    /// (already parsed into `hostnames`) plus one host per matching slot resolved
    /// from each testplatform through the refhosts inventory. Every connect is
    /// best-effort: an unreachable host is logged and skipped so one dead host
    /// never aborts the load.
    ///
    /// Returns the loaded report's RRID (empty when the load failed and the null
    /// report was substituted).
    pub async fn load_update(
        &mut self,
        update: &UpdateID,
        autoconnect: bool,
        kind: UpdateKind,
    ) -> String {
        let report = make_testreport(update, self.config.clone(), kind, autoconnect).await;
        let rrid = report.id();
        let pending = report.base().autoconnect_pending;

        // `templates.add` ignores the empty-RRID null sentinel; a real report
        // becomes active (re-load replaces + re-activates).
        self.templates.add(report);
        if !rrid.is_empty() {
            self.templates.set_active(&rrid);
        }

        if pending && !rrid.is_empty() {
            self.autoconnect_active(&rrid).await;
        }
        rrid
    }

    /// Connects the active report's reference hosts (the deferred half of
    /// [`load_update`](Self::load_update)).
    ///
    /// Computes the wanted host list via [`autoconnect_hosts`](Self::autoconnect_hosts)
    /// — the template's parsed `reference host:` names plus one host per matching
    /// slot resolved from each testplatform — then builds and connects a
    /// [`Target`] for each, stamping the report's RRID as the pool-claim
    /// ownership identity. Connect failures are logged and the host dropped
    /// (best-effort, upstream `connect_targets`).
    ///
    /// The offline host-selection is factored into the pure, unit-tested
    /// [`autoconnect_hosts`](Self::autoconnect_hosts); this thin connect loop
    /// builds real [`Target`]s and is exercised by the gated sshd integration
    /// path (the same seam `list_refhosts --free` uses for its live probe).
    async fn autoconnect_active(&mut self, rrid: &str) {
        // Snapshot everything needed from the active report *synchronously* (no
        // `&Session` may cross the resolver await — `Session` is not `Sync`, so a
        // borrow held across the await would make this future non-`Send`, which
        // the `Command::call` trait requires).
        let config = self.config.clone();
        let (ref_hosts, already, testplatforms) = {
            let base = self.templates.active().base();
            (
                base.hostnames.iter().cloned().collect::<Vec<_>>(),
                base.targets.names(),
                base.testplatforms.clone(),
            )
        };
        let wanted = Self::autoconnect_hosts(&config, ref_hosts, already, testplatforms).await;

        let targets = self.targets_mut();
        for host in wanted {
            let mut target = Target::new(
                &config,
                host.clone(),
                TargetState::Enabled,
                ExecutionMode::Parallel,
            );
            target.set_rrid(rrid.to_owned());
            match target.connect().await {
                Ok(()) => targets.add(target),
                Err(e) => warn!(host = %host, "autoconnect: connect failed, skipping: {e}"),
            }
        }
    }

    /// Computes the deduplicated host list to autoconnect from plain inputs (the
    /// offline, unit-tested half of [`autoconnect_active`](Self::autoconnect_active)).
    ///
    /// Combines `ref_hosts` (the template's parsed `reference host:` names) with
    /// one candidate per matching slot resolved from each testplatform
    /// ([`resolve_testplatform_hosts`](Self::resolve_testplatform_hosts)), drops
    /// hosts in `already` (connected in the active group), and dedups — the exact
    /// set [`autoconnect_active`](Self::autoconnect_active) then connects.
    ///
    /// A **static** async fn (owned/borrowed plain data, no `&Session`) so the
    /// caller's connect future stays `Send` — `Session` is not `Sync`, so a
    /// borrow across the resolver await would violate the `Command::call` bound.
    async fn autoconnect_hosts(
        config: &Config,
        ref_hosts: Vec<String>,
        already: Vec<String>,
        testplatforms: Vec<String>,
    ) -> Vec<String> {
        let mut wanted = ref_hosts;
        // Deterministic order so the connect sequence (and tests) are stable;
        // `hostnames` is a HashSet with no inherent order.
        wanted.sort();
        wanted.dedup();

        for host in Self::resolve_testplatform_hosts(config, &testplatforms).await {
            if !wanted.contains(&host) {
                wanted.push(host);
            }
        }

        // Skip hosts already in the active group (upstream connect is a no-op for
        // already-connected targets).
        wanted.retain(|h| !already.contains(h));
        wanted
    }

    /// Resolves one candidate host per matching slot from the given
    /// testplatforms (the refhosts-from-testplatform half of autoconnect).
    ///
    /// Builds the refhosts factory on demand from `config` (the same pattern
    /// `list_refhosts`/`add_host` use — no cached Session state), resolves the
    /// inventory, and for each testplatform searches for matching host names. A
    /// resolver failure degrades to an empty result (upstream `except
    /// RefhostsResolveFailedError: return`), so autoconnect still connects the
    /// template's own reference hosts.
    ///
    /// Takes owned/borrowed plain data (not `&Session`) so the caller's connect
    /// future stays `Send` across this await.
    async fn resolve_testplatform_hosts(config: &Config, testplatforms: &[String]) -> Vec<String> {
        if testplatforms.is_empty() {
            return Vec::new();
        }

        let factory = match RefhostsFactory::production(
            config.refhosts_path.clone(),
            VerifyPolicy::from_config(&config.ssl_verify),
        ) {
            Ok(f) => f,
            Err(e) => {
                warn!("autoconnect: refhosts resolver init failed: {e}");
                return Vec::new();
            }
        };
        let store = match factory
            .resolve(ResolveConfig {
                refhosts_resolvers: &config.refhosts_resolvers,
                refhosts_path: &config.refhosts_path,
                refhosts_https_uri: &config.refhosts_https_uri,
                refhosts_https_expiration: config.refhosts_https_expiration,
                ssl_verify: &config.ssl_verify,
            })
            .await
        {
            Ok(s) => s,
            Err(e) => {
                warn!("autoconnect: refhosts resolve failed: {e}");
                return Vec::new();
            }
        };

        let mut hosts: Vec<String> = Vec::new();
        for tp in testplatforms {
            let attrs = Attributes::from_testplatform(tp);
            let found = store.search(&attrs);
            if found.is_empty() {
                info!("autoconnect: nothing found for testplatform {tp:?}");
            }
            for host in found {
                if !hosts.contains(&host) {
                    hosts.push(host);
                }
            }
        }
        hosts
    }

    /// Requests that the interactive REPL loop exit after the current dispatch.
    ///
    /// Set by the `quit` command; read by the Phase-6 REPL via
    /// [`should_exit`](Self::should_exit).
    pub fn request_exit(&mut self) {
        self.should_exit = true;
    }

    /// Whether the `quit` command has asked the REPL loop to exit.
    #[must_use]
    pub fn should_exit(&self) -> bool {
        self.should_exit
    }

    /// Installs the callback `set_log_level` uses to apply a runtime level change.
    ///
    /// The Phase-6 REPL wires this to a `tracing_subscriber::reload` handle so
    /// `set_log_level debug` takes effect immediately; headless callers leave it
    /// unset.
    pub fn set_log_level_sink(&mut self, sink: LogLevelSink) {
        self.log_level_sink = Some(sink);
    }

    /// Applies `level` through the installed sink, if any (upstream
    /// `prompt.log.setLevel`).
    ///
    /// Returns `true` when a sink was present and invoked; `false` when none is
    /// installed (headless/tests), so the caller can still log the change.
    pub fn apply_log_level(&mut self, level: LogLevel) -> bool {
        if let Some(sink) = self.log_level_sink.as_mut() {
            sink(level);
            true
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> Config {
        Config::default()
    }

    #[test]
    fn fresh_session_active_is_null_and_unloaded() {
        let s = Session::new(config(), true);
        assert!(!s.metadata().is_loaded());
        assert!(s.templates.is_empty());
        assert_eq!(s.metadata().id(), "");
    }

    #[test]
    fn interactive_flag_is_honored() {
        assert!(Session::new(config(), true).interactive);
        assert!(!Session::new(config(), false).interactive);
    }

    #[test]
    fn targets_of_unloaded_session_is_empty() {
        let s = Session::new(config(), true);
        assert!(s.targets().is_empty());
    }

    #[test]
    fn log_level_parse_and_tracing_mapping() {
        assert_eq!(LogLevel::parse("error"), Some(LogLevel::Error));
        assert_eq!(LogLevel::parse("warning"), Some(LogLevel::Warning));
        assert_eq!(LogLevel::parse("info"), Some(LogLevel::Info));
        assert_eq!(LogLevel::parse("debug"), Some(LogLevel::Debug));
        assert_eq!(LogLevel::parse("bogus"), None);
        assert_eq!(LogLevel::Debug.as_tracing(), tracing::Level::DEBUG);
        assert_eq!(LogLevel::Error.as_tracing(), tracing::Level::ERROR);
    }

    #[test]
    fn apply_log_level_invokes_sink_when_installed() {
        use std::sync::{Arc, Mutex};
        let mut s = Session::new(config(), true);
        // No sink installed → returns false, no panic.
        assert!(!s.apply_log_level(LogLevel::Debug));

        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink_seen = Arc::clone(&seen);
        s.set_log_level_sink(Box::new(move |lvl| sink_seen.lock().unwrap().push(lvl)));
        assert!(s.apply_log_level(LogLevel::Warning));
        assert_eq!(*seen.lock().unwrap(), vec![LogLevel::Warning]);
    }

    #[test]
    fn with_display_uses_supplied_sink() {
        use crate::display::{ColorMode, CommandPromptDisplay};
        let display = CommandPromptDisplay::with_sink(Box::new(Vec::new()), ColorMode::Always);
        let s = Session::with_display(config(), false, display);
        assert_eq!(s.display.color(), ColorMode::Always);
        assert!(!s.interactive);
    }

    // --- Sub-bead B: load_update + autoconnect host resolution -------------

    use mtui_testreport::{ObsReport, TestReport};
    use mtui_types::RequestReviewID;

    const REFHOSTS_FIXTURE: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../mtui-datasources/tests/fixtures/refhosts.yml"
    );

    /// A config whose refhosts resolver is the offline file-backed `path`
    /// resolver pointed at the ported fixture (no network).
    fn config_with_path_refhosts() -> Config {
        let mut c = Config::default();
        c.refhosts_resolvers = "path".to_owned();
        c.refhosts_path = REFHOSTS_FIXTURE.into();
        c
    }

    /// Adds an active `ObsReport` with the given reference hostnames and
    /// testplatforms to `session`.
    fn seed_active_report(
        session: &mut Session,
        rrid: &str,
        hostnames: &[&str],
        testplatforms: &[&str],
    ) {
        let mut report = ObsReport::new(session.config.clone());
        report.base_mut().rrid = Some(RequestReviewID::parse(rrid).unwrap());
        for h in hostnames {
            report.base_mut().hostnames.insert((*h).to_owned());
        }
        report.base_mut().testplatforms = testplatforms.iter().map(|s| (*s).to_owned()).collect();
        session.templates.add(Box::new(report));
        session.templates.set_active(rrid);
    }

    /// Snapshots the active report's autoconnect inputs and runs the static
    /// [`Session::autoconnect_hosts`] resolver — mirrors what
    /// [`Session::autoconnect_active`] does synchronously before connecting.
    async fn autoconnect_hosts_of(s: &Session) -> Vec<String> {
        let config = s.config.clone();
        let (ref_hosts, already, testplatforms) = {
            let base = s.templates.active().base();
            (
                base.hostnames.iter().cloned().collect::<Vec<_>>(),
                base.targets.names(),
                base.testplatforms.clone(),
            )
        };
        Session::autoconnect_hosts(&config, ref_hosts, already, testplatforms).await
    }

    /// `autoconnect_hosts` combines the template's reference hosts with the
    /// hosts resolved from its testplatforms (offline `path` resolver), sorted
    /// and deduplicated.
    #[tokio::test]
    async fn autoconnect_hosts_merges_reference_and_testplatform_hosts() {
        let mut s = Session::new(config_with_path_refhosts(), false);
        // A testplatform matching the sles 15.5 x86_64 hosts in the fixture
        // (fixture minor is the numeric `5`, so the query must use `minor=5`).
        seed_active_report(
            &mut s,
            "SUSE:Maintenance:1:1",
            &["ref-a.example.com"],
            &["base=sles(major=15,minor=5);arch=[x86_64]"],
        );

        let hosts = autoconnect_hosts_of(&s).await;

        // The explicit reference host is always present.
        assert!(hosts.contains(&"ref-a.example.com".to_owned()));
        // The testplatform resolved at least one fixture host (sles 15-SP5 x86_64).
        assert!(
            hosts.iter().any(|h| h.contains("x86")),
            "expected a resolved x86 refhost, got: {hosts:?}"
        );
        // Deduplicated: no host appears twice.
        let mut sorted = hosts.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), hosts.len(), "hosts must be deduplicated");
    }

    /// With no testplatforms, `autoconnect_hosts` is exactly the reference-host
    /// set (no resolver call needed).
    #[tokio::test]
    async fn autoconnect_hosts_reference_only_when_no_testplatforms() {
        let mut s = Session::new(config_with_path_refhosts(), false);
        seed_active_report(&mut s, "SUSE:Maintenance:1:1", &["only.example.com"], &[]);

        let hosts = autoconnect_hosts_of(&s).await;
        assert_eq!(hosts, vec!["only.example.com".to_owned()]);
    }

    /// A testplatform matching nothing in the inventory contributes no hosts;
    /// the reference hosts still stand.
    #[tokio::test]
    async fn autoconnect_hosts_unmatched_testplatform_yields_reference_only() {
        let mut s = Session::new(config_with_path_refhosts(), false);
        seed_active_report(
            &mut s,
            "SUSE:Maintenance:1:1",
            &["ref-only.example.com"],
            &["base=sles(major=99,minor=sp9);arch=[nonesuch]"],
        );

        let hosts = autoconnect_hosts_of(&s).await;
        assert_eq!(hosts, vec!["ref-only.example.com".to_owned()]);
    }

    /// `load_update` for a kernel update loads the on-disk template, activates
    /// it, and does **not** autoconnect (so no live-host access on load).
    #[tokio::test]
    async fn load_update_kernel_loads_and_activates_without_connect() {
        let tmp = tempfile::tempdir().unwrap();
        let rrid = "SUSE:Maintenance:24993:275518";
        let dir = tmp.path().join(rrid);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("log"), "log\n").unwrap();
        std::fs::write(
            dir.join("metadata.json"),
            format!("{{\"rrid\": \"{rrid}\", \"repository\": \"http://x/\"}}"),
        )
        .unwrap();

        let mut config = config_with_path_refhosts();
        config.template_dir = tmp.path().to_path_buf();
        let mut s = Session::new(config, false);

        let update = UpdateID::parse(rrid).unwrap();
        let loaded = s.load_update(&update, true, UpdateKind::Kernel).await;

        assert_eq!(loaded, rrid);
        assert!(s.templates.contains(rrid));
        assert_eq!(s.templates.active_rrid(), Some(rrid));
        // Kernel does not autoconnect: no targets were connected.
        assert!(s.targets().is_empty());
    }

    /// `load_update` for an unloadable RRID (no template, offline `svn`) falls
    /// back to the null report: nothing is registered, empty RRID returned.
    #[tokio::test]
    async fn load_update_missing_report_returns_empty_and_registers_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let mut config = config_with_path_refhosts();
        config.template_dir = tmp.path().to_path_buf();
        // Force the internal `svn co` to fail fast offline.
        config.svn_path = format!("file://{}/no-such-repo", tmp.path().display());
        let mut s = Session::new(config, false);

        let update = UpdateID::parse("SUSE:Maintenance:1:1").unwrap();
        let loaded = s.load_update(&update, true, UpdateKind::Auto).await;

        assert_eq!(loaded, "");
        assert!(s.templates.is_empty());
    }
}
