//! The read-only host health check behind `check_hosts`.
//!
//! Every probe is a query: the check takes neither the operation lock nor a
//! pool claim, writes no history row, records nothing in the host log, and
//! changes no package or repository. `zypper lr` and `zypper needs-rebooting`
//! hold zypper's own lock while they run, so a concurrent transaction may see
//! it busy for that moment; they carry `ZYPP_LOCK_TIMEOUT=0`, so a lock held
//! by someone else makes the probe fail at once, and read unknown, instead of
//! waiting.
//!
//! A check that does not apply to a host (no systemd on SLE 11, a pending
//! snapshot on a read-only root) is not sent and is listed as not checked; it
//! does not make the host unknown.
//!
//! A probe is read only where the repository holds evidence for its result: a
//! parser or named exit code already relied on elsewhere, or a format pinned
//! by a standard. Anything else, and any probe that failed, timed out or gave
//! output that is not one of the shapes below, is [`Status::Unknown`], never
//! [`Status::Ok`]. `zypper verify` and `zypper list-patches` are not probed:
//! no capture shows how to read them (see
//! `tests/fixtures/check_hosts/PROVENANCE.md`).

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::time::Duration;

use mtui_types::enums::TargetState;
use mtui_types::hostlog::CommandLog;
use mtui_types::shellquote::quote_args;
use tokio_util::sync::CancellationToken;

use super::actions::boxed;
use super::hostgroup::HostsGroup;
use super::package_querier::{RPM_QUERY, not_installed_name};
use super::repo_manager::is_issue_alias;
use super::{Target, removal};
use crate::connection::NO_EXIT_CODE;

/// Wall-clock bound on one probe on one host. A host that hits it is sent no
/// further probe, so the worst case per host is five probes each just under
/// the bound (about five minutes). A probe cut off at the bound may still be
/// running on the host.
pub const PROBE_LIMIT: Duration = Duration::from_secs(60);

/// The packages every zypper host needs, all of them protected by the
/// removal guard.
const CORE_PACKAGES: &[&str] = &["rpm", "zypper", "glibc", "systemd", "bash"];

/// `zypper(8)` `ZYPPER_EXIT_INF_REBOOT_NEEDED`; `mtui-testreport` names the
/// same value, but this crate cannot depend on it.
const ZYPPER_EXIT_INF_REBOOT_NEEDED: i16 = 102;

/// `zypper -n lr` with no repository defined, as the update script's
/// test-repository cleanup reads it.
const ZYPPER_EXIT_NO_REPOS: i16 = 6;

/// The unit types systemd names a unit with.
const UNIT_TYPES: &[&str] = &[
    "service",
    "socket",
    "device",
    "mount",
    "automount",
    "swap",
    "target",
    "path",
    "timer",
    "slice",
    "scope",
];

/// How many names a detail lists before it says how many more there are.
const NAMES_SHOWN: usize = 5;

/// Characters of host output a detail quotes.
const QUOTE_LIMIT: usize = 120;

/// One check's result, ordered from best to worst so a host's verdict is the
/// maximum of its checks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Status {
    /// The check passed.
    Ok,
    /// Worth a look, but the host can be tested.
    Warn,
    /// The probe gave no readable answer.
    Unknown,
    /// The host is not fit for testing.
    Fail,
}

impl Status {
    /// The lower-case name printed and serialised.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Warn => "warn",
            Self::Unknown => "unknown",
            Self::Fail => "fail",
        }
    }
}

impl fmt::Display for Status {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The checks, in the order they run and print.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum CheckName {
    /// The core, configured and kernel packages are installed.
    Packages,
    /// `/` has room for a transaction.
    RootSpace,
    /// No systemd unit failed.
    FailedUnits,
    /// No reboot is pending.
    Reboot,
    /// No test repository is left configured.
    TestRepositories,
}

impl CheckName {
    /// Every check, in run order.
    pub const ALL: [Self; 5] = [
        Self::Packages,
        Self::RootSpace,
        Self::FailedUnits,
        Self::Reboot,
        Self::TestRepositories,
    ];

    /// The snake-case name printed and serialised.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Packages => "packages",
            Self::RootSpace => "root_space",
            Self::FailedUnits => "failed_units",
            Self::Reboot => "reboot",
            Self::TestRepositories => "test_repositories",
        }
    }
}

impl fmt::Display for CheckName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One check's outcome on one host. `detail` quotes host output and must be
/// sanitised before it is printed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckResult {
    /// Which check.
    pub name: CheckName,
    /// Its result.
    pub status: Status,
    /// One line saying why.
    pub detail: String,
}

/// Why a host ran no probe at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NotChecked {
    /// The host is disabled.
    Disabled,
    /// The host has no connection.
    NotConnected,
    /// The host's base product maps to no known release.
    UnknownSystem(String),
    /// A YUM-family host; the checks are zypper's.
    Yum,
}

impl fmt::Display for NotChecked {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Disabled => f.write_str("not checked: the host is disabled"),
            Self::NotConnected => f.write_str("not checked: the host is not connected"),
            Self::UnknownSystem(name) => write!(
                f,
                "not checked: base product {name:?} maps to no known release"
            ),
            Self::Yum => f.write_str("not checked: a YUM-family host, and the checks are zypper's"),
        }
    }
}

/// A check that does not apply to a host, and was not sent to it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NotApplicable {
    /// Which check.
    pub name: CheckName,
    /// Why it does not apply.
    pub reason: &'static str,
}

/// One host's result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostHealth {
    /// The host.
    pub host: String,
    /// Set when the host ran no probe at all; `checks` is then empty.
    pub skipped: Option<NotChecked>,
    /// The checks that apply to the host, in run order.
    pub checks: Vec<CheckResult>,
    /// The checks that do not apply to the host.
    pub not_checked: Vec<NotApplicable>,
}

impl HostHealth {
    /// The worst status of the host's checks; [`Status::Unknown`] for a host
    /// that ran none. A check that does not apply does not count.
    #[must_use]
    pub fn verdict(&self) -> Status {
        if self.skipped.is_some() {
            return Status::Unknown;
        }
        self.checks
            .iter()
            .map(|c| c.status)
            .max()
            .unwrap_or(Status::Unknown)
    }
}

/// Every selected host's result, in the order the hosts were given.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HealthReport {
    /// One entry per selected host.
    pub hosts: Vec<HostHealth>,
    /// The check stopped on a cancel; the checks it never ran are
    /// [`Status::Unknown`].
    pub cancelled: bool,
}

/// The site's part of the check, from `[target]`.
#[derive(Debug, Clone, Copy)]
pub struct HealthOptions<'a> {
    /// Packages required on top of the core set.
    pub required_packages: &'a [String],
    /// Free MiB on `/` below which `root_space` fails.
    pub min_root_free_mib: u64,
}

/// What decides a host's probes and how their results read.
#[derive(Debug, Clone, Copy)]
struct ProbePlan {
    /// SLE 11, which has no systemd.
    sle11: bool,
    /// A read-only or undecided root.
    transactional: bool,
}

impl ProbePlan {
    /// Why `check` does not apply to the host, if it does not.
    fn not_applicable(self, check: CheckName) -> Option<&'static str> {
        match check {
            CheckName::FailedUnits if self.sle11 => Some("SLE 11 has no systemd"),
            CheckName::Reboot if self.sle11 => {
                Some("SLE 11's zypper is not known to offer needs-rebooting")
            }
            CheckName::Reboot if self.transactional => Some(
                "on a read-only root a pending snapshot is not checked, and zypper's reboot \
                 flag is not shown to cover one",
            ),
            _ => None,
        }
    }
}

/// A probe's raw result on one host.
enum Probe {
    /// The command finished (possibly with mtui's `-1` sentinel).
    Ran(CommandLog),
    /// No answer within [`PROBE_LIMIT`].
    NoAnswer,
}

impl HostsGroup {
    /// Runs every check on `hosts`, each probe in parallel across them.
    ///
    /// `cancel` is consulted only between probes, never inside a fan-out;
    /// every check a cancel left unrun reads as not run.
    pub async fn check_health(
        &mut self,
        hosts: &[String],
        options: &HealthOptions<'_>,
        cancel: &CancellationToken,
    ) -> HealthReport {
        let mut results: Vec<HostHealth> = Vec::with_capacity(hosts.len());
        let mut plans: BTreeMap<String, ProbePlan> = BTreeMap::new();
        let mut seen = BTreeSet::new();
        for name in hosts.iter().filter(|n| seen.insert(n.as_str())) {
            let classified = match self.get(name) {
                Some(target) => classify(target),
                None => Err(NotChecked::NotConnected),
            };
            if let Ok(plan) = classified {
                plans.insert(name.clone(), plan);
            }
            let not_checked = classified
                .as_ref()
                .map(|plan| {
                    CheckName::ALL
                        .into_iter()
                        .filter_map(|name| {
                            plan.not_applicable(name)
                                .map(|reason| NotApplicable { name, reason })
                        })
                        .collect()
                })
                .unwrap_or_default();
            results.push(HostHealth {
                host: name.clone(),
                skipped: classified.err(),
                checks: Vec::new(),
                not_checked,
            });
        }

        let mut stopped: BTreeSet<String> = BTreeSet::new();
        let mut cancelled = false;
        for check in CheckName::ALL {
            cancelled = cancelled || cancel.is_cancelled();
            let commands: BTreeMap<String, String> = if cancelled {
                BTreeMap::new()
            } else {
                plans
                    .iter()
                    .filter(|(name, plan)| {
                        !stopped.contains(*name) && plan.not_applicable(check).is_none()
                    })
                    .map(|(name, plan)| (name.clone(), command(check, *plan, options)))
                    .collect()
            };
            let mut probes = self.probe(&commands).await;
            for health in &mut results {
                let Some(plan) = plans.get(&health.host) else {
                    continue;
                };
                if plan.not_applicable(check).is_some() {
                    continue;
                }
                let (status, detail) = match probes.remove(&health.host) {
                    Some(Probe::Ran(log)) if log.exitcode == NO_EXIT_CODE => {
                        stopped.insert(health.host.clone());
                        (Status::Unknown, not_run_detail(&log))
                    }
                    Some(Probe::Ran(log)) if log.truncated => (
                        Status::Unknown,
                        "the output was cut off, so it is not read".to_owned(),
                    ),
                    Some(Probe::Ran(log)) => judge(check, &log, *plan, options),
                    Some(Probe::NoAnswer) => {
                        stopped.insert(health.host.clone());
                        (
                            Status::Unknown,
                            format!("no answer within {} s", PROBE_LIMIT.as_secs()),
                        )
                    }
                    None if cancelled => (Status::Unknown, "not run: cancelled".to_owned()),
                    None => (
                        Status::Unknown,
                        "not run: an earlier probe on this host gave no answer".to_owned(),
                    ),
                };
                health.checks.push(CheckResult {
                    name: check,
                    status,
                    detail,
                });
            }
        }
        HealthReport {
            hosts: results,
            cancelled,
        }
    }

    /// Runs each host's command, bounded by [`PROBE_LIMIT`]; a host absent
    /// from `commands` is not touched.
    async fn probe(&mut self, commands: &BTreeMap<String, String>) -> BTreeMap<String, Probe> {
        if commands.is_empty() {
            return BTreeMap::new();
        }
        self.collect_fanout(
            "check_hosts",
            |t| commands.contains_key(t.hostname()),
            |t| {
                let cmd = commands.get(t.hostname()).cloned().unwrap_or_default();
                boxed(async move {
                    Some(
                        t.run_unlogged(&cmd, PROBE_LIMIT)
                            .await
                            .map_or(Probe::NoAnswer, Probe::Ran),
                    )
                })
            },
        )
        .await
    }
}

/// Whether `target` can be probed, and how.
fn classify(target: &Target) -> Result<ProbePlan, NotChecked> {
    if target.state() == TargetState::Disabled {
        return Err(NotChecked::Disabled);
    }
    if target.connection.is_none() {
        return Err(NotChecked::NotConnected);
    }
    let release = target
        .system()
        .get_release()
        .map_err(|_| NotChecked::UnknownSystem(target.system().get_base().name.clone()))?;
    if release == "YUM" {
        return Err(NotChecked::Yum);
    }
    Ok(ProbePlan {
        sle11: release == "11",
        transactional: target.transactional_state().is_transactional(),
    })
}

/// The core packages for a host; SLE 11 boots without systemd.
fn core_packages(plan: ProbePlan) -> impl Iterator<Item = &'static str> {
    CORE_PACKAGES
        .iter()
        .copied()
        .filter(move |name| !(plan.sle11 && *name == "systemd"))
}

/// Every name the `packages` probe queries, each once, in query order.
fn queried_packages(plan: ProbePlan, options: &HealthOptions<'_>) -> Vec<String> {
    let mut seen = BTreeSet::new();
    core_packages(plan)
        .map(str::to_owned)
        .chain(options.required_packages.iter().cloned())
        .chain(removal::kernel_flavours().map(str::to_owned))
        .filter(|name| seen.insert(name.clone()))
        .collect()
}

/// The command line a check sends to a host. Each one only reads.
fn command(check: CheckName, plan: ProbePlan, options: &HealthOptions<'_>) -> String {
    match check {
        CheckName::Packages => format!(
            "LC_ALL=C {RPM_QUERY} -- {}",
            quote_args(&queried_packages(plan, options))
        ),
        CheckName::RootSpace => "LC_ALL=C df -Pk /".to_owned(),
        CheckName::FailedUnits => {
            "LC_ALL=C systemctl --failed --no-legend --plain --no-pager".to_owned()
        }
        CheckName::Reboot => "LC_ALL=C ZYPP_LOCK_TIMEOUT=0 zypper -n needs-rebooting".to_owned(),
        CheckName::TestRepositories => "LC_ALL=C ZYPP_LOCK_TIMEOUT=0 zypper -n lr".to_owned(),
    }
}

/// Reads one probe's finished log.
fn judge(
    check: CheckName,
    log: &CommandLog,
    plan: ProbePlan,
    options: &HealthOptions<'_>,
) -> (Status, String) {
    match check {
        CheckName::Packages => judge_packages(log, plan, options),
        CheckName::RootSpace => judge_root_space(log, options.min_root_free_mib),
        CheckName::FailedUnits => judge_failed_units(log),
        CheckName::Reboot => judge_reboot(log),
        CheckName::TestRepositories => judge_test_repositories(log),
    }
}

/// `packages`: every queried name must be answered, installed or not.
fn judge_packages(
    log: &CommandLog,
    plan: ProbePlan,
    options: &HealthOptions<'_>,
) -> (Status, String) {
    let queried = queried_packages(plan, options);
    let mut installed: BTreeSet<&str> = BTreeSet::new();
    let mut missing: BTreeSet<&str> = BTreeSet::new();
    for line in log.stdout.lines().filter(|l| !l.trim().is_empty()) {
        let answered = match not_installed_name(line) {
            Some(name) => queried.iter().find(|q| *q == name).map(|q| (q, false)),
            None => {
                let mut parts = line.split_whitespace();
                match (parts.next(), parts.next(), parts.next()) {
                    (Some(name), Some(_version), None) => {
                        queried.iter().find(|q| *q == name).map(|q| (q, true))
                    }
                    _ => None,
                }
            }
        };
        match answered {
            Some((name, true)) => {
                installed.insert(name);
            }
            Some((name, false)) => {
                missing.insert(name);
            }
            None => {
                return (
                    Status::Unknown,
                    format!("unrecognised rpm output: {}", quote(line)),
                );
            }
        }
    }
    let unanswered: Vec<&str> = queried
        .iter()
        .map(String::as_str)
        .filter(|n| !installed.contains(n) && !missing.contains(n))
        .collect();
    if !unanswered.is_empty() {
        return (
            Status::Unknown,
            format!(
                "rpm exited {} without an answer for {}{}",
                log.exitcode,
                names(&unanswered),
                first_line_suffix(log)
            ),
        );
    }
    let mut needed: BTreeSet<&str> = core_packages(plan)
        .filter(|n| missing.contains(n))
        .collect();
    needed.extend(
        options
            .required_packages
            .iter()
            .map(String::as_str)
            .filter(|n| missing.contains(n)),
    );
    let needed: Vec<&str> = needed.into_iter().collect();
    let kernels: Vec<&str> = removal::kernel_flavours()
        .filter(|k| installed.contains(k))
        .collect();
    let mut problems = Vec::new();
    if !needed.is_empty() {
        problems.push(format!("missing {}", names(&needed)));
    }
    if kernels.is_empty() {
        problems.push("no kernel flavour installed".to_owned());
    }
    if problems.is_empty() {
        (
            Status::Ok,
            format!(
                "{} packages installed, kernel {}",
                installed.len(),
                kernels.join(", ")
            ),
        )
    } else {
        (Status::Fail, problems.join("; "))
    }
}

/// `root_space`: the POSIX `df -P` layout (XCU `df`, "STDOUT"): a header and
/// one line per file system, `name total used available capacity% mount`,
/// in 1024-byte units with `-k`.
fn judge_root_space(log: &CommandLog, min_free_mib: u64) -> (Status, String) {
    if log.exitcode != 0 {
        return unknown_exit("df", log);
    }
    let lines: Vec<&str> = log
        .stdout
        .lines()
        .filter(|l| !l.trim().is_empty())
        .collect();
    let unrecognised = || {
        (
            Status::Unknown,
            format!(
                "unrecognised df output: {}",
                quote(log.stdout.trim().lines().next().unwrap_or("(none)"))
            ),
        )
    };
    let [header, row] = lines.as_slice() else {
        return unrecognised();
    };
    if !(header.starts_with("Filesystem") && header.contains("1024-blocks")) {
        return unrecognised();
    }
    let fields: Vec<&str> = row.split_whitespace().collect();
    let [.., total, used, available, capacity, mount] = fields.as_slice() else {
        return unrecognised();
    };
    let numeric = |s: &str| s.parse::<u64>().ok();
    let (Some(_), Some(_), Some(available_kib), Some(_)) = (
        numeric(total),
        numeric(used),
        numeric(available),
        capacity.strip_suffix('%').and_then(numeric),
    ) else {
        return unrecognised();
    };
    if fields.len() < 6 || *mount != "/" {
        return unrecognised();
    }
    let free_mib = available_kib / 1024;
    if free_mib < min_free_mib {
        (
            Status::Fail,
            format!("{free_mib} MiB free on /, below min_root_free {min_free_mib} MiB"),
        )
    } else {
        (Status::Ok, format!("{free_mib} MiB free on /"))
    }
}

/// `failed_units`: one unit per line, its name first. No capture pins this
/// layout, so a line whose first word is not a unit name is unknown, and the
/// "no failed unit" reading of an empty answer is unverified on a host.
fn judge_failed_units(log: &CommandLog) -> (Status, String) {
    if log.exitcode != 0 || !log.stderr.trim().is_empty() {
        return unknown_exit("systemctl", log);
    }
    let mut units = Vec::new();
    for line in log.stdout.lines().filter(|l| !l.trim().is_empty()) {
        let mut words = line.split_whitespace();
        let first = words.next().unwrap_or_default();
        let unit = if first == "●" || first == "*" {
            words.next().unwrap_or_default()
        } else {
            first
        };
        let is_unit = unit
            .rsplit_once('.')
            .is_some_and(|(stem, kind)| !stem.is_empty() && UNIT_TYPES.contains(&kind));
        if !is_unit {
            return (
                Status::Unknown,
                format!("unrecognised systemctl output: {}", quote(line)),
            );
        }
        units.push(unit);
    }
    if units.is_empty() {
        (Status::Ok, "no failed units".to_owned())
    } else {
        (Status::Warn, format!("failed: {}", names(&units)))
    }
}

/// `reboot`: `zypper needs-rebooting` answers by exit code alone. Unverified
/// on a host: that `0` means no reboot is pending.
fn judge_reboot(log: &CommandLog) -> (Status, String) {
    match log.exitcode {
        ZYPPER_EXIT_INF_REBOOT_NEEDED => (
            Status::Warn,
            "reboot pending (zypper needs-rebooting exited 102)".to_owned(),
        ),
        0 => (Status::Ok, "no reboot pending".to_owned()),
        _ => unknown_exit("zypper needs-rebooting", log),
    }
}

/// `test_repositories`: the `|`-separated `zypper -n lr` table, alias in the
/// second column, as the update script's cleanup loop reads it; a row starts
/// with its number.
fn judge_test_repositories(log: &CommandLog) -> (Status, String) {
    match log.exitcode {
        ZYPPER_EXIT_NO_REPOS => return (Status::Ok, "no repositories defined".to_owned()),
        0 => {}
        _ => return unknown_exit("zypper -n lr", log),
    }
    let mut rows = 0usize;
    let mut left: Vec<&str> = Vec::new();
    for line in log.stdout.lines() {
        let mut fields = line.split('|').map(str::trim);
        let (Some(number), Some(alias)) = (fields.next(), fields.next()) else {
            continue;
        };
        if number.is_empty() || !number.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        rows += 1;
        if is_issue_alias(alias) {
            left.push(alias);
        }
    }
    if rows == 0 {
        return (
            Status::Unknown,
            "zypper -n lr exited 0 but listed no repository".to_owned(),
        );
    }
    if left.is_empty() {
        (Status::Ok, format!("no test repository ({rows} listed)"))
    } else {
        (
            Status::Warn,
            format!("test repositories still configured: {}", names(&left)),
        )
    }
}

/// The detail for a probe that ended on mtui's [`NO_EXIT_CODE`] sentinel.
fn not_run_detail(log: &CommandLog) -> String {
    if log.timed_out {
        "no output before the command timeout".to_owned()
    } else {
        "the command could not run on the host".to_owned()
    }
}

/// An unknown for a probe that exited with a status it gives no verdict on.
fn unknown_exit(what: &str, log: &CommandLog) -> (Status, String) {
    (
        Status::Unknown,
        format!("{what} exited {}{}", log.exitcode, first_line_suffix(log)),
    )
}

/// `": <first line>"` of stderr, else stdout, or nothing when both are empty.
fn first_line_suffix(log: &CommandLog) -> String {
    [log.stderr.as_str(), log.stdout.as_str()]
        .into_iter()
        .find_map(|text| text.lines().map(str::trim).find(|l| !l.is_empty()))
        .map(|line| format!(": {}", quote(line)))
        .unwrap_or_default()
}

/// Host text cut to [`QUOTE_LIMIT`] characters.
fn quote(text: &str) -> String {
    let text = text.trim();
    match text.char_indices().nth(QUOTE_LIMIT) {
        Some((at, _)) => format!("{}…", &text[..at]),
        None => text.to_owned(),
    }
}

/// Up to [`NAMES_SHOWN`] names, then how many more.
fn names(list: &[&str]) -> String {
    let shown: Vec<String> = list.iter().take(NAMES_SHOWN).map(|n| quote(n)).collect();
    match list.len().saturating_sub(NAMES_SHOWN) {
        0 => shown.join(", "),
        more => format!("{} and {more} more", shown.join(", ")),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::num::NonZeroU64;

    use mtui_types::system::{System, SystemProduct};

    use super::*;
    use crate::connection::{MockConnection, MockSftpOp};
    use crate::target::{POOL_LOCK_PATH, TARGET_LOCK_PATH, Transactional};

    // Every output below is hand-written to the shape PROVENANCE.md names for
    // its probe; none is a capture.

    const SLES: ProbePlan = ProbePlan {
        sle11: false,
        transactional: false,
    };

    fn opts(required: &[String]) -> HealthOptions<'_> {
        HealthOptions {
            required_packages: required,
            min_root_free_mib: 1024,
        }
    }

    fn log(stdout: &str, stderr: &str, exit: i16) -> CommandLog {
        CommandLog::new("probe", stdout, stderr, exit, 0)
    }

    /// Exit codes other than the ones a probe reads, to show any of them is
    /// unknown rather than one chosen value.
    const OTHER_EXITS: &[i16] = &[1, 2, 4, 5, 7, 8, 100, 101, 103, 104, 106, 107, 127, 255];

    /// `rpm -q` stdout for `plan`'s query with `absent` not installed.
    fn rpm_out(plan: ProbePlan, required: &[String], absent: &[&str]) -> String {
        queried_packages(plan, &opts(required))
            .iter()
            .map(|name| {
                if absent.contains(&name.as_str()) {
                    format!("package {name} is not installed\n")
                } else {
                    format!("{name} 1.0-1\n")
                }
            })
            .collect()
    }

    /// Every kernel flavour but `kernel-default`.
    fn other_kernels() -> Vec<&'static str> {
        removal::kernel_flavours()
            .filter(|k| *k != "kernel-default")
            .collect()
    }

    // --- packages ---------------------------------------------------------

    #[test]
    fn the_core_set_is_protected_by_the_removal_guard() {
        for name in CORE_PACKAGES {
            assert!(removal::is_protected(name), "{name}");
        }
        assert!(removal::kernel_flavours().any(|k| k == "kernel-default"));
        assert!(removal::kernel_flavours().all(|k| k.starts_with("kernel-")));
    }

    #[test]
    fn packages_ok_when_core_configured_and_a_kernel_are_installed() {
        let required = vec!["ca-certificates".to_owned()];
        let out = rpm_out(SLES, &required, &other_kernels());
        let (status, detail) = judge_packages(&log(&out, "", 8), SLES, &opts(&required));
        assert_eq!(status, Status::Ok, "{detail}");
        assert_eq!(detail, "7 packages installed, kernel kernel-default");
    }

    #[test]
    fn a_missing_core_package_fails() {
        let out = rpm_out(SLES, &[], &[&["systemd"][..], &other_kernels()].concat());
        assert_eq!(
            judge_packages(&log(&out, "", 9), SLES, &opts(&[])),
            (Status::Fail, "missing systemd".to_owned())
        );
    }

    #[test]
    fn a_missing_configured_package_fails() {
        let required = vec!["ca-certificates".to_owned()];
        let out = rpm_out(
            SLES,
            &required,
            &[&["ca-certificates"][..], &other_kernels()].concat(),
        );
        assert_eq!(
            judge_packages(&log(&out, "", 9), SLES, &opts(&required)),
            (Status::Fail, "missing ca-certificates".to_owned())
        );
    }

    #[test]
    fn no_kernel_flavour_at_all_fails() {
        let kernels: Vec<&str> = removal::kernel_flavours().collect();
        let out = rpm_out(SLES, &[], &kernels);
        assert_eq!(
            judge_packages(&log(&out, "", 8), SLES, &opts(&[])),
            (Status::Fail, "no kernel flavour installed".to_owned())
        );
    }

    #[test]
    fn rpm_failing_outright_is_unknown() {
        let (status, detail) = judge_packages(
            &log(
                "",
                "error: cannot open Packages database in /var/lib/rpm\n",
                1,
            ),
            SLES,
            &opts(&[]),
        );
        assert_eq!(status, Status::Unknown);
        assert!(
            detail.starts_with(
                "rpm exited 1 without an answer for rpm, zypper, glibc, systemd, bash and "
            ) && detail.ends_with(": error: cannot open Packages database in /var/lib/rpm"),
            "{detail}"
        );
    }

    /// The trap: exit 0 and no output reads as "nothing missing" to a parser
    /// that only looks for not-installed lines.
    #[test]
    fn rpm_answering_nothing_with_exit_zero_is_unknown() {
        assert_eq!(
            judge_packages(&log("", "", 0), SLES, &opts(&[])).0,
            Status::Unknown
        );
    }

    #[test]
    fn an_unrecognised_rpm_line_is_unknown() {
        let out = format!(
            "{}warning: something odd\n",
            rpm_out(SLES, &[], &other_kernels())
        );
        assert_eq!(
            judge_packages(&log(&out, "", 7), SLES, &opts(&[])),
            (
                Status::Unknown,
                "unrecognised rpm output: warning: something odd".to_owned()
            )
        );
    }

    // --- root_space ---------------------------------------------------------

    fn df(row: &str) -> CommandLog {
        log(
            &format!("Filesystem     1024-blocks    Used Available Capacity Mounted on\n{row}\n"),
            "",
            0,
        )
    }

    #[test]
    fn root_space_ok_above_the_threshold_and_fail_below() {
        let row = "/dev/vda3         41922560 9452292  31929116      23% /";
        assert_eq!(
            judge_root_space(&df(row), 1024),
            (Status::Ok, "31180 MiB free on /".to_owned())
        );
        assert_eq!(
            judge_root_space(&df(row), 31181),
            (
                Status::Fail,
                "31180 MiB free on /, below min_root_free 31181 MiB".to_owned()
            )
        );
        assert_eq!(judge_root_space(&df(row), 31180).0, Status::Ok);
    }

    #[test]
    fn root_space_unreadable_output_is_unknown() {
        for bad in [
            log("", "", 0),
            log("", "df: /: No such file or directory\n", 1),
            log(
                "Filesystem 512-blocks Used Available Capacity Mounted on\n/dev/vda3 2 1 1 50% /\n",
                "",
                0,
            ),
            df("/dev/vda3 41922560 9452292 31929116 23% /home"),
            df("/dev/vda3 41922560 9452292 lots 23% /"),
            df("/dev/vda3 41922560 9452292 31929116 23 /"),
            df("41922560 9452292 31929116 23% /"),
        ] {
            assert_eq!(judge_root_space(&bad, 0).0, Status::Unknown, "{bad:?}");
        }
    }

    // --- failed_units -------------------------------------------------------

    #[test]
    fn failed_units_ok_warn_and_unknown() {
        assert_eq!(
            judge_failed_units(&log("", "", 0)),
            (Status::Ok, "no failed units".to_owned())
        );
        assert_eq!(
            judge_failed_units(&log(
                "kdump.service loaded failed failed Load kdump kernel\n\
                 ● snapper-boot.timer loaded failed failed Take snapper snapshot\n",
                "",
                0
            )),
            (
                Status::Warn,
                "failed: kdump.service, snapper-boot.timer".to_owned()
            )
        );
        assert_eq!(
            judge_failed_units(&log("0 loaded units listed.\n", "", 0)).0,
            Status::Unknown
        );
    }

    /// Empty stdout is "no failed unit" only when nothing went to stderr.
    #[test]
    fn failed_units_with_stderr_or_another_exit_is_unknown() {
        assert_eq!(
            judge_failed_units(&log("", "Failed to list units: Access denied\n", 0)),
            (
                Status::Unknown,
                "systemctl exited 0: Failed to list units: Access denied".to_owned()
            )
        );
        for exit in OTHER_EXITS {
            assert_eq!(
                judge_failed_units(&log("", "", *exit)),
                (Status::Unknown, format!("systemctl exited {exit}")),
            );
        }
    }

    // --- reboot ---------------------------------------------------------------

    #[test]
    fn reboot_reads_only_0_and_102_and_any_other_exit_is_unknown() {
        assert_eq!(
            judge_reboot(&log("", "", 0)),
            (Status::Ok, "no reboot pending".to_owned())
        );
        assert_eq!(judge_reboot(&log("", "", 102)).0, Status::Warn);
        for exit in OTHER_EXITS {
            assert_eq!(
                judge_reboot(&log("", "some reason\n", *exit)),
                (
                    Status::Unknown,
                    format!("zypper needs-rebooting exited {exit}: some reason")
                ),
            );
        }
    }

    #[test]
    fn checks_that_do_not_apply_are_named_per_host() {
        let micro = ProbePlan {
            sle11: false,
            transactional: true,
        };
        let sle11 = ProbePlan {
            sle11: true,
            transactional: false,
        };
        let applicable = |plan: ProbePlan| -> Vec<CheckName> {
            CheckName::ALL
                .into_iter()
                .filter(|c| plan.not_applicable(*c).is_some())
                .collect()
        };
        assert_eq!(applicable(SLES), vec![]);
        assert_eq!(applicable(micro), vec![CheckName::Reboot]);
        assert_eq!(
            applicable(sle11),
            vec![CheckName::FailedUnits, CheckName::Reboot]
        );
    }

    // --- test_repositories ----------------------------------------------------

    const LR_TABLE: &str = "# | Alias | Name | Enabled | GPG Check | Refresh\n\
                            --+-------+------+---------+-----------+--------\n\
                            1 | repo-oss | Main | Yes | (r ) Yes | Yes\n";

    #[test]
    fn test_repositories_ok_warn_and_unknown() {
        assert_eq!(
            judge_test_repositories(&log(LR_TABLE, "", 0)),
            (Status::Ok, "no test repository (1 listed)".to_owned())
        );
        let left = format!("{LR_TABLE}2 | issue-SLES:15-SP5:p=1:2 | x | Yes | Yes | No\n");
        assert_eq!(
            judge_test_repositories(&log(&left, "", 0)),
            (
                Status::Warn,
                "test repositories still configured: issue-SLES:15-SP5:p=1:2".to_owned()
            )
        );
        assert_eq!(judge_test_repositories(&log("", "", 6)).0, Status::Ok);
        for exit in OTHER_EXITS {
            assert_eq!(
                judge_test_repositories(&log(LR_TABLE, "", *exit)).0,
                Status::Unknown,
                "{exit}"
            );
        }
        // The trap: no row read is not "no test repository".
        assert_eq!(judge_test_repositories(&log("", "", 0)).0, Status::Unknown);
    }

    #[test]
    fn host_text_is_cut_and_lists_are_capped() {
        assert_eq!(quote(&"x".repeat(200)).chars().count(), QUOTE_LIMIT + 1);
        assert_eq!(
            names(&["a", "b", "c", "d", "e", "f", "g"]),
            "a, b, c, d, e and 2 more"
        );
    }

    // --- the group driver ------------------------------------------------------

    const DF: &str = "LC_ALL=C df -Pk /";
    const SYSTEMCTL: &str = "LC_ALL=C systemctl --failed --no-legend --plain --no-pager";
    const NEEDS_REBOOTING: &str = "LC_ALL=C ZYPP_LOCK_TIMEOUT=0 zypper -n needs-rebooting";
    const LR: &str = "LC_ALL=C ZYPP_LOCK_TIMEOUT=0 zypper -n lr";
    const KERNELS: &str = "kernel-64kb kernel-azure kernel-default kernel-default-base \
                           kernel-kvmsmall kernel-preempt kernel-rt kernel-zfcpdump";

    fn rpm_command(core: &str) -> String {
        format!(
            "LC_ALL=C rpm -q --queryformat \"%{{Name}} %{{Version}}-%{{Release}}\n\" -- {core} \
             {KERNELS}"
        )
    }

    fn sles_rpm() -> String {
        rpm_command("rpm zypper glibc systemd bash")
    }

    /// A mock whose every unscripted command fails, so no test passes by
    /// leaving a probe unscripted.
    fn mock(name: &str) -> MockConnection {
        MockConnection::new(name).with_default(log("", "unscripted command", 127))
    }

    fn healthy_df() -> CommandLog {
        df("/dev/vda3 41922560 9452292 31929116 23% /")
    }

    /// A SLES 15 host answering every probe healthily.
    fn healthy(name: &str) -> MockConnection {
        mock(name)
            .with_response(
                sles_rpm(),
                log(&rpm_out(SLES, &[], &other_kernels()), "", 7),
            )
            .with_response(DF, healthy_df())
            .with_response(SYSTEMCTL, log("", "", 0))
            .with_response(NEEDS_REBOOTING, log("", "", 0))
            .with_response(LR, log(LR_TABLE, "", 0))
    }

    fn target(name: &str, conn: MockConnection, product: &str, version: &str) -> Target {
        let mut t = Target::with_connection(name, TargetState::Enabled, Box::new(conn));
        t.set_system(
            System::new(
                SystemProduct::new(product, version, "x86_64"),
                BTreeSet::new(),
                false,
            ),
            Transactional::No,
        );
        t
    }

    fn sles(name: &str, conn: MockConnection) -> Target {
        target(name, conn, "SLES", "15-SP5")
    }

    fn names_of(hosts: &[&str]) -> Vec<String> {
        hosts.iter().map(|h| (*h).to_owned()).collect()
    }

    async fn check(group: &mut HostsGroup, hosts: &[&str]) -> HealthReport {
        group
            .check_health(&names_of(hosts), &opts(&[]), &CancellationToken::new())
            .await
    }

    fn statuses(health: &HostHealth) -> Vec<(CheckName, Status)> {
        health.checks.iter().map(|c| (c.name, c.status)).collect()
    }

    fn not_checked(health: &HostHealth) -> Vec<CheckName> {
        health.not_checked.iter().map(|n| n.name).collect()
    }

    #[tokio::test]
    async fn a_healthy_host_sends_exactly_the_read_only_probes_and_leaves_no_trace() {
        let lock = b"1:alice:42:theirs\n".to_vec();
        let pool = b"1:alice:42:mtui pool SUSE:Maintenance:9:9\n".to_vec();
        let conn = healthy("h1")
            .with_file(TARGET_LOCK_PATH, lock.clone())
            .with_file(POOL_LOCK_PATH, pool.clone());
        let handle = conn.clone();
        let mut group = HostsGroup::new(vec![sles("h1", conn)], false);

        let report = check(&mut group, &["h1"]).await;

        assert!(!report.cancelled);
        let health = &report.hosts[0];
        assert_eq!(health.verdict(), Status::Ok, "{health:?}");
        assert_eq!(
            statuses(health),
            CheckName::ALL.map(|c| (c, Status::Ok)).to_vec()
        );
        assert!(health.not_checked.is_empty());
        assert_eq!(
            handle.commands(),
            vec![
                sles_rpm(),
                DF.to_owned(),
                SYSTEMCTL.to_owned(),
                NEEDS_REBOOTING.to_owned(),
                LR.to_owned()
            ]
        );
        assert_eq!(handle.sftp_ops(), Vec::<MockSftpOp>::new());
        assert_eq!(handle.file_contents(TARGET_LOCK_PATH), Some(lock));
        assert_eq!(handle.file_contents(POOL_LOCK_PATH), Some(pool));
        assert!(
            group.get("h1").unwrap().out().is_empty(),
            "probes reached the host log"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn the_no_output_timeout_is_raised_for_a_probe_and_restored_after() {
        let conn = healthy("h1").with_run_delay(Duration::from_secs(1));
        let handle = conn.clone();
        let mut t = sles("h1", conn);
        t.set_timeout(NonZeroU64::new(10).unwrap());
        let mut group = HostsGroup::new(vec![t], false);

        let (report, during) = tokio::join!(check(&mut group, &["h1"]), async {
            tokio::time::sleep(Duration::from_millis(500)).await;
            handle.command_timeout()
        });

        assert_eq!(report.hosts[0].verdict(), Status::Ok);
        assert!(during > PROBE_LIMIT, "{during:?}");
        assert_eq!(handle.command_timeout(), Duration::from_secs(10));
    }

    #[tokio::test]
    async fn a_healthy_read_only_root_host_is_ok_with_reboot_not_checked() {
        let conn = mock("m1")
            .with_response(
                sles_rpm(),
                log(&rpm_out(SLES, &[], &other_kernels()), "", 7),
            )
            .with_response(DF, healthy_df())
            .with_response(SYSTEMCTL, log("", "", 0))
            .with_response(LR, log(LR_TABLE, "", 0));
        let handle = conn.clone();
        let mut micro = target("m1", conn, "SL-Micro", "6.1");
        micro.set_system(micro.system().clone(), Transactional::Yes);
        let mut group = HostsGroup::new(vec![micro], false);

        let report = check(&mut group, &["m1"]).await;

        let h = &report.hosts[0];
        assert_eq!(h.verdict(), Status::Ok, "{h:?}");
        assert_eq!(not_checked(h), vec![CheckName::Reboot]);
        assert!(!handle.commands().contains(&NEEDS_REBOOTING.to_owned()));
        assert_eq!(handle.commands().len(), 4);
    }

    #[tokio::test]
    async fn a_healthy_sle11_host_is_ok_without_systemd_and_reboot() {
        let plan = ProbePlan {
            sle11: true,
            transactional: false,
        };
        let rpm = rpm_command("rpm zypper glibc bash");
        let conn = mock("old")
            .with_response(
                rpm.clone(),
                log(&rpm_out(plan, &[], &other_kernels()), "", 7),
            )
            .with_response(DF, healthy_df())
            .with_response(LR, log(LR_TABLE, "", 0));
        let handle = conn.clone();
        let mut group = HostsGroup::new(vec![target("old", conn, "SLES", "11-SP3")], false);

        let report = check(&mut group, &["old"]).await;

        let h = &report.hosts[0];
        assert_eq!(h.verdict(), Status::Ok, "{h:?}");
        assert_eq!(
            not_checked(h),
            vec![CheckName::FailedUnits, CheckName::Reboot]
        );
        assert_eq!(handle.commands(), vec![rpm, DF.to_owned(), LR.to_owned()]);
    }

    #[tokio::test]
    async fn truncated_output_is_unknown() {
        let conn = healthy("h1").with_response(DF, healthy_df().with_flags(true, false));
        let mut group = HostsGroup::new(vec![sles("h1", conn)], false);
        let report = check(&mut group, &["h1"]).await;
        let root = &report.hosts[0].checks[1];
        assert_eq!(
            (root.name, root.status, root.detail.as_str()),
            (
                CheckName::RootSpace,
                Status::Unknown,
                "the output was cut off, so it is not read"
            )
        );
    }

    /// The fixture guard itself: a host left unscripted reads unknown on
    /// every check, so a test cannot pass by omitting a probe.
    #[tokio::test]
    async fn an_unscripted_host_is_unknown_on_every_check() {
        let mut group = HostsGroup::new(vec![sles("h1", mock("h1"))], false);
        let report = check(&mut group, &["h1"]).await;
        assert_eq!(
            statuses(&report.hosts[0]),
            CheckName::ALL.map(|c| (c, Status::Unknown)).to_vec()
        );
    }

    #[tokio::test]
    async fn hosts_that_cannot_be_checked_are_named_unknown_and_sent_nothing() {
        let disabled = healthy("off");
        let disabled_handle = disabled.clone();
        let mut off = sles("off", disabled);
        off.set_state(TargetState::Disabled);
        let yum = mock("rh");
        let yum_handle = yum.clone();
        let mut unconnected = Target::new(
            &mtui_config::Config::default(),
            "gone",
            TargetState::Enabled,
        );
        unconnected.set_system(
            System::new(
                SystemProduct::new("SLES", "15-SP5", "x86_64"),
                BTreeSet::new(),
                false,
            ),
            Transactional::No,
        );
        let mut group = HostsGroup::new(
            vec![
                off,
                target("rh", yum, "rhel", "9"),
                unconnected,
                target("odd", mock("odd"), "unknown", ""),
                sles("h1", healthy("h1")),
            ],
            false,
        );

        let report = check(
            &mut group,
            &["off", "rh", "gone", "odd", "h1", "ghost", "h1"],
        )
        .await;

        let got: Vec<(&str, Status, Option<NotChecked>, usize)> = report
            .hosts
            .iter()
            .map(|h| {
                (
                    h.host.as_str(),
                    h.verdict(),
                    h.skipped.clone(),
                    h.checks.len(),
                )
            })
            .collect();
        assert_eq!(
            got,
            vec![
                ("off", Status::Unknown, Some(NotChecked::Disabled), 0),
                ("rh", Status::Unknown, Some(NotChecked::Yum), 0),
                ("gone", Status::Unknown, Some(NotChecked::NotConnected), 0),
                (
                    "odd",
                    Status::Unknown,
                    Some(NotChecked::UnknownSystem("unknown".to_owned())),
                    0
                ),
                ("h1", Status::Ok, None, 5),
                ("ghost", Status::Unknown, Some(NotChecked::NotConnected), 0),
            ]
        );
        assert!(disabled_handle.commands().is_empty());
        assert!(yum_handle.commands().is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn a_host_that_hangs_is_bounded_and_does_not_hold_the_others() {
        let stuck = healthy("slow").with_run_delay(PROBE_LIMIT * 10);
        let stuck_handle = stuck.clone();
        let mut group =
            HostsGroup::new(vec![sles("slow", stuck), sles("h1", healthy("h1"))], false);

        let start = tokio::time::Instant::now();
        let report = check(&mut group, &["slow", "h1"]).await;

        assert!(start.elapsed() < PROBE_LIMIT * 2, "{:?}", start.elapsed());
        let slow = &report.hosts[0];
        assert_eq!(slow.checks[0].detail, "no answer within 60 s");
        assert!(
            slow.checks[1..].iter().all(|c| c.status == Status::Unknown
                && c.detail == "not run: an earlier probe on this host gave no answer"),
            "{slow:?}"
        );
        assert_eq!(stuck_handle.commands(), vec![sles_rpm()]);
        assert_eq!(report.hosts[1].verdict(), Status::Ok);
    }

    #[tokio::test]
    async fn a_probe_that_could_not_run_stops_that_host() {
        let conn = healthy("h1").with_timeout(DF);
        let handle = conn.clone();
        let mut group = HostsGroup::new(vec![sles("h1", conn)], false);
        let report = check(&mut group, &["h1"]).await;
        let h = &report.hosts[0];
        assert_eq!(h.checks[0].status, Status::Ok);
        assert_eq!(
            (h.checks[1].status, h.checks[1].detail.as_str()),
            (Status::Unknown, "no output before the command timeout")
        );
        assert_eq!(handle.commands(), vec![sles_rpm(), DF.to_owned()]);
    }

    #[tokio::test]
    async fn a_cancel_between_probes_keeps_what_ran_and_marks_the_rest() {
        let token = CancellationToken::new();
        let conn = healthy("h1").with_cancel_on_command(DF, token.clone());
        let handle = conn.clone();
        let mut group = HostsGroup::new(vec![sles("h1", conn)], false);

        let report = group
            .check_health(&names_of(&["h1"]), &opts(&[]), &token)
            .await;

        assert!(report.cancelled);
        let h = &report.hosts[0];
        assert_eq!(
            statuses(h)[..2],
            [
                (CheckName::Packages, Status::Ok),
                (CheckName::RootSpace, Status::Ok)
            ]
        );
        assert!(
            h.checks[2..]
                .iter()
                .all(|c| c.status == Status::Unknown && c.detail == "not run: cancelled"),
            "{h:?}"
        );
        assert_eq!(handle.commands(), vec![sles_rpm(), DF.to_owned()]);
        assert_eq!(h.verdict(), Status::Unknown);
    }

    #[test]
    fn verdict_is_the_worst_check_ignores_not_applicable_and_is_unknown_without_checks() {
        let mut h = HostHealth {
            host: "h".to_owned(),
            skipped: None,
            checks: Vec::new(),
            not_checked: vec![NotApplicable {
                name: CheckName::Reboot,
                reason: "n/a",
            }],
        };
        assert_eq!(h.verdict(), Status::Unknown);
        for (status, want) in [
            (Status::Ok, Status::Ok),
            (Status::Warn, Status::Warn),
            (Status::Unknown, Status::Unknown),
            (Status::Fail, Status::Fail),
            (Status::Ok, Status::Fail),
        ] {
            h.checks.push(CheckResult {
                name: CheckName::Packages,
                status,
                detail: String::new(),
            });
            assert_eq!(h.verdict(), want);
        }
        h.skipped = Some(NotChecked::Disabled);
        h.checks.truncate(1);
        assert_eq!(h.verdict(), Status::Unknown);
    }
}
