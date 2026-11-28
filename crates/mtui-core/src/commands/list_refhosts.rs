//! The `list_refhosts` command: query/search the refhost inventory offline.
//!
//! Reads the refhost inventory (the source `add_host` resolves through
//! [`RefhostsFactory`]) and prints matching hosts **without connecting**: no
//! SSH, no lock, no loaded template. Results are de-duplicated by name.
//! `--free` additionally connects to report live operation-lock and pool-claim
//! state, without reaping: it is a survey, so a stale operation lock is left
//! alone.
//!
//! [`gather`], [`render_table`] and [`render_json`] are pure over an in-memory
//! [`Refhosts`] store; only [`call`](ListRefhosts::call) touches the network.

use async_trait::async_trait;
use clap::{Arg, ArgAction, ArgMatches};
use mtui_datasources::Refhosts;
use mtui_datasources::http::VerifyPolicy;
use mtui_datasources::refhost::{Attributes, RefhostsFactory, ResolveConfig, Slot};
use mtui_hosts::Target;
use mtui_hosts::target::LockRow;
use mtui_types::Host;
use mtui_types::enums::TargetState;
use serde_json::{Value, json};
use tokio::task::JoinSet;

use crate::command::{Command, Scope};
use crate::error::{CommandError, CommandResult};
use crate::session::Session;

use super::row_budget::{add_window_args, crush, json_envelope, probe_meta, probe_notice, window};

/// Narrowing flags named in the row-budget notice.
const REFHOSTS_HINT: &str = "--limit/--offset/--name/--arch/--product/--version/--addon";

/// Non-`free` or pool-claimed rows survive the crush: the actionable lock signal.
/// Severity: `locked` or pool-claimed (2) outrank other states (1).
fn anomaly_severity(r: &Record) -> u8 {
    r.state().severity()
}

/// One matched refhost, rendered as a table row or a JSON object.
#[derive(Debug, Clone)]
pub struct Record {
    /// Host name.
    pub name: String,
    /// Architecture.
    pub arch: String,
    /// Base product name.
    pub product: String,
    /// Product version string (`15-SP6` / `15` / empty).
    pub version: String,
    /// Installed addon names.
    pub addons: Vec<String>,
    /// The test-target slot key when `--pool` grouped the result, else `None`.
    pub slot: Option<String>,
    /// Live operation-lock state from `--free` (`locked` / `free` /
    /// `unreachable`), else `None`. Omitted from JSON when absent. Keeps its
    /// original vocabulary regardless of the pool claim — see [`Self::pool`].
    pub lock: Option<String>,
    /// The RRID claiming the pool lock from `--free`, or `None` when unclaimed
    /// (or unset — same "only under `--free`" gate as [`Self::lock`]).
    pub pool: Option<String>,
    /// The operation lock's holder from `--free`, when it is held.
    pub lock_holder: Option<LockRow>,
    /// The pool claim's holder from `--free`, when it is claimed.
    pub pool_holder: Option<LockRow>,
}

impl Record {
    /// The row's `--free` state.
    fn state(&self) -> HostState {
        HostState::of(self.lock.as_deref(), self.pool.as_deref())
    }

    /// Omits the `--free` fields when unset (only set under `--free`).
    fn to_json(&self) -> Value {
        let mut obj = json!({
            "name": self.name,
            "arch": self.arch,
            "product": self.product,
            "version": self.version,
            "addons": self.addons,
            "slot": self.slot,
        });
        if let Some(lock) = &self.lock {
            obj["lock"] = json!(lock);
            obj["pool"] = json!(self.pool);
            obj["lock_holder"] = holder_json(self.lock_holder.as_ref(), true);
            obj["pool_holder"] = holder_json(self.pool_holder.as_ref(), false);
        }
        obj
    }
}

/// A held lock's owner and start time, or `null` when free or unparseable.
fn holder_json(row: Option<&LockRow>, with_comment: bool) -> Value {
    match row {
        Some(row) if !row.invalid => {
            let mut obj = json!({"user": row.locked_by, "since": row.time});
            if with_comment {
                obj["comment"] = json!(row.comment);
            }
            obj
        }
        _ => Value::Null,
    }
}

/// Lists reference hosts from `refhosts.yml` (offline search, no connect).
pub struct ListRefhosts;

/// Renders a [`Host`]'s version string.
fn ver_str(host: &Host) -> String {
    match &host.product.version {
        None => String::new(),
        Some(v) => match &v.minor {
            None => v.major.to_string(),
            Some(m) if m.to_string().is_empty() => v.major.to_string(),
            Some(m) => format!("{}-{m}", v.major),
        },
    }
}

/// Build a [`Record`] from a matched host, tagging its `--pool` slot.
fn record(host: &Host, with_slot: bool) -> Record {
    let slot = with_slot.then(|| slot_label(&Refhosts::slot_of(host), false));
    Record {
        name: host.name.clone(),
        arch: host.arch.clone(),
        product: host.product.name.clone(),
        version: ver_str(host),
        addons: host.addons.iter().map(|a| a.name.clone()).collect(),
        slot,
        lock: None,
        pool: None,
        lock_holder: None,
        pool_holder: None,
    }
}

/// Resolve refhosts against the parsed filters, offline. `testplatform` (parsed
/// to [`Attributes`]) takes precedence over the ad-hoc field filters.
#[must_use]
pub fn gather(store: &Refhosts, args: &Filters<'_>) -> Vec<Record> {
    let hits = if let Some(tp) = args.testplatform {
        let attrs = Attributes::from_testplatform(tp);
        store.query(Some(&attrs), None, &[], None, None, &[])
    } else {
        store.query(
            None,
            args.name,
            args.arch,
            args.product,
            args.version,
            args.addon,
        )
    };
    hits.iter().map(|h| record(h, args.pool)).collect()
}

/// The offline filters `gather` consumes, decoupled from `clap` so it can be
/// unit-tested directly.
#[derive(Debug, Default)]
pub struct Filters<'a> {
    /// A full SMELT `testplatform` query (mutually exclusive with the fields).
    pub testplatform: Option<&'a str>,
    /// Hostname glob.
    pub name: Option<&'a str>,
    /// Arch filter (repeatable).
    pub arch: &'a [String],
    /// Base-product substring.
    pub product: Option<&'a str>,
    /// Loose product version.
    pub version: Option<&'a str>,
    /// Addon-name substrings (repeatable).
    pub addon: &'a [String],
    /// Group by test-target slot.
    pub pool: bool,
}

/// Combines a record's operation-lock and pool-claim states into one table
/// label: `free`/`locked` widen to `pool`/`locked+pool` when a pool claim
/// exists; `unreachable`/`unknown` (and an unset `lock`) pass straight
/// through, since the probe never resolved a trustworthy state to combine.
#[must_use]
fn lock_label(lock: Option<&str>, pool: Option<&str>) -> String {
    HostState::of(lock, pool).label().to_owned()
}

/// Who holds a row's operation lock and pool claim, in `list_locks`'s
/// `since <time> by <owner> : <comment>` shape; empty when neither is held.
fn holder_label(r: &Record) -> String {
    let by = |row: &LockRow| match &row.sibling {
        Some(label) => mtui_hosts::sibling_holder_name(label),
        None if row.is_mine => "me".to_owned(),
        None => row.locked_by.clone(),
    };
    let mut parts = Vec::new();
    if let Some(op) = &r.lock_holder {
        parts.push(if op.invalid {
            "invalid lock file".to_owned()
        } else if op.comment.is_empty() {
            format!("since {} by {}", op.time, by(op))
        } else {
            format!("since {} by {} : {}", op.time, by(op), op.comment)
        });
    }
    // A claim belongs to an RRID, which the probe does not have: never "me".
    if let Some(claim) = &r.pool_holder {
        parts.push(if claim.invalid {
            "invalid pool claim file".to_owned()
        } else if claim.comment.is_empty() {
            format!("pool since {} by {}", claim.time, claim.locked_by)
        } else {
            format!(
                "pool {} since {} by {}",
                claim.comment, claim.time, claim.locked_by
            )
        });
    }
    parts.join("; ")
}

/// Render `records` as one aligned multi-line table, grouped by slot when
/// `pool`. `total` is the pre-window matched count; the footer shows
/// `kept of total` when windowing/crush dropped rows.
#[must_use]
pub fn render_table(
    records: &[Record],
    pool: bool,
    free: bool,
    verbose: bool,
    total: usize,
) -> String {
    let fmt = |r: &Record| -> String {
        let prod = format!("{} {}", r.product, r.version);
        let prod = prod.trim();
        let mut cols = vec![
            format!("{:<34}", r.name),
            format!("{prod:<22}"),
            format!("{:<8}", r.arch),
        ];
        if free {
            cols.push(format!(
                "{:<22}",
                lock_label(r.lock.as_deref(), r.pool.as_deref())
            ));
        }
        if verbose {
            cols.push(r.addons.join(","));
        }
        if free {
            cols.push(holder_label(r));
        }
        cols.join(" ").trim_end().to_owned()
    };

    let mut out = String::new();
    if pool {
        let mut slots: Vec<&str> = records
            .iter()
            .map(|r| r.slot.as_deref().unwrap_or("?"))
            .collect();
        slots.sort_unstable();
        slots.dedup();
        for slot in slots {
            out.push_str(&format!("== {slot} ==\n"));
            for r in records
                .iter()
                .filter(|r| r.slot.as_deref().unwrap_or("?") == slot)
            {
                out.push_str(&format!("  {}\n", fmt(r)));
            }
        }
    } else {
        for r in records {
            out.push_str(&fmt(r));
            out.push('\n');
        }
    }
    if records.len() == total {
        out.push_str(&format!("\n{} refhost(s)", records.len()));
    } else {
        out.push_str(&format!("\n{} of {total} refhost(s)", records.len()));
    }
    out
}

/// Serialize `records` as pretty JSON.
///
/// # Panics
/// Never in practice: the value is a plain array of objects.
#[must_use]
pub fn render_json(records: &[Record]) -> String {
    let arr = Value::Array(records.iter().map(Record::to_json).collect());
    serde_json::to_string_pretty(&arr).expect("records serialize")
}

/// One host's `--free` state, folded from its probed `lock` and `pool`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HostState {
    /// Not probed: `--free` was not given, or the host was outside the window.
    Unprobed,
    /// Neither an operation lock nor a pool claim.
    Free,
    /// Pool-claimed, without an operation lock.
    Pool,
    /// Operation-locked, without a pool claim.
    Locked,
    /// Operation-locked and pool-claimed.
    LockedPool,
    /// The operation lock could not be read.
    Unreachable,
    /// The probe never finished.
    Unknown,
}

impl HostState {
    /// `unreachable`/`unknown` ignore the claim: the probe resolved no state to combine.
    fn of(lock: Option<&str>, pool: Option<&str>) -> Self {
        match (lock, pool.is_some()) {
            (None, _) => Self::Unprobed,
            (Some("free"), false) => Self::Free,
            (Some("free"), true) => Self::Pool,
            (Some("locked"), false) => Self::Locked,
            (Some("locked"), true) => Self::LockedPool,
            (Some("unreachable"), _) => Self::Unreachable,
            (Some(_), _) => Self::Unknown,
        }
    }

    /// The table's lock column.
    fn label(self) -> &'static str {
        match self {
            Self::Unprobed => "",
            Self::Free => "free",
            Self::Pool => "pool",
            Self::Locked => "locked",
            Self::LockedPool => "locked+pool",
            Self::Unreachable => "unreachable",
            Self::Unknown => "unknown",
        }
    }

    /// Row-budget severity: a lock or claim (2) outranks an unread state (1).
    fn severity(self) -> u8 {
        match self {
            Self::Locked | Self::LockedPool | Self::Pool => 2,
            Self::Unreachable | Self::Unknown => 1,
            Self::Free | Self::Unprobed => 0,
        }
    }
}

/// Whether a slot has a free host, per its `--free` survey.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// At least one host is free.
    Free,
    /// Every host was reached and every one is taken.
    Busy,
    /// No host is free, and at least one was unreachable or not probed.
    Unknown,
    /// The inventory has no host for the slot.
    NoHost,
}

impl Verdict {
    /// The wire spelling, shared by the table and `--json`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Free => "free",
            Self::Busy => "busy",
            Self::Unknown => "unknown",
            Self::NoHost => "none",
        }
    }

    /// Row-budget severity: a slot nobody can draw from now (2) outranks an
    /// undecided one (1).
    fn severity(self) -> u8 {
        match self {
            Self::NoHost | Self::Busy => 2,
            Self::Unknown => 1,
            Self::Free => 0,
        }
    }
}

impl std::fmt::Display for Verdict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One `--summary` row: a slot and the matched hosts that serve it.
#[derive(Debug, Clone)]
pub struct SlotRow {
    /// The slot key: as pool selection keys it under `--testplatform`, else
    /// [`Refhosts::slot_of`].
    pub slot: Slot,
    /// The hosts serving the slot, each probed under `--free`.
    pub hosts: Vec<Record>,
}

/// A slot's hosts folded by [`HostState`]; the counts are disjoint.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Tally {
    /// Names of the hosts with neither an operation lock nor a pool claim.
    pub free: Vec<String>,
    /// Hosts holding an operation lock, pool-claimed or not.
    pub locked: usize,
    /// Hosts pool-claimed without an operation lock.
    pub pool: usize,
    /// Hosts the probe could not read, or never finished.
    pub unreachable: usize,
    /// Hosts never probed.
    pub unprobed: usize,
}

impl SlotRow {
    /// Folds the hosts' probe states into a [`Tally`].
    #[must_use]
    pub fn tally(&self) -> Tally {
        let mut tally = Tally::default();
        for r in &self.hosts {
            match r.state() {
                HostState::Free => tally.free.push(r.name.clone()),
                HostState::Locked | HostState::LockedPool => tally.locked += 1,
                HostState::Pool => tally.pool += 1,
                HostState::Unreachable | HostState::Unknown => tally.unreachable += 1,
                HostState::Unprobed => tally.unprobed += 1,
            }
        }
        tally
    }

    /// [`Verdict::NoHost`] for a slot no host serves; with `free` probed, the
    /// survey's verdict; otherwise none.
    #[must_use]
    pub fn verdict(&self, free: bool) -> Option<Verdict> {
        if self.hosts.is_empty() {
            return Some(Verdict::NoHost);
        }
        if !free {
            return None;
        }
        let tally = self.tally();
        Some(if !tally.free.is_empty() {
            Verdict::Free
        } else if tally.unreachable + tally.unprobed > 0 {
            Verdict::Unknown
        } else {
            Verdict::Busy
        })
    }

    /// Every key is always present; the `--free` counts are `null` unless `free`.
    fn to_json(&self, free: bool) -> Value {
        let (product, version, arch, addons) = &self.slot;
        let tally = free.then(|| self.tally());
        json!({
            "slot": {"product": product, "version": version, "arch": arch, "addons": addons},
            "hosts": self.hosts.len(),
            "free": tally.as_ref().map(|t| &t.free),
            "locked": tally.as_ref().map(|t| t.locked),
            "pool": tally.as_ref().map(|t| t.pool),
            "unreachable": tally.as_ref().map(|t| t.unreachable),
            "verdict": self.verdict(free).map(Verdict::as_str),
        })
    }
}

/// A slot as `product-version arch`, then ` +addon,…` when `with_addons` and it
/// requires any.
fn slot_label((product, version, arch, addons): &Slot, with_addons: bool) -> String {
    let name = [product.as_str(), version.as_str()]
        .into_iter()
        .filter(|p| !p.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    let mut label = format!("{name} {arch}").trim().to_owned();
    if with_addons && !addons.is_empty() {
        label.push_str(&format!(" +{}", addons.join(",")));
    }
    label
}

/// Group the matched hosts into `--summary` slot rows, offline.
#[must_use]
pub fn gather_slots(store: &Refhosts, args: &Filters<'_>) -> Vec<SlotRow> {
    let rows = |slots: Vec<(Slot, Vec<Host>)>| {
        slots
            .into_iter()
            .map(|(slot, hosts)| SlotRow {
                slot,
                hosts: hosts.iter().map(|h| record(h, false)).collect(),
            })
            .collect()
    };
    if let Some(tp) = args.testplatform {
        // The slots `add_host`'s pool selection keys for this testplatform.
        return rows(store.pool_slots_by_query(&Attributes::from_testplatform(tp)));
    }
    let mut slots: indexmap::IndexMap<Slot, Vec<Host>> = indexmap::IndexMap::new();
    for host in store.query(
        None,
        args.name,
        args.arch,
        args.product,
        args.version,
        args.addon,
    ) {
        slots
            .entry(Refhosts::slot_of(&host))
            .or_default()
            .push(host);
    }
    rows(slots.into_iter().collect())
}

/// Render slot rows as an aligned table; `total` is the pre-window slot count.
#[must_use]
pub fn render_slot_table(rows: &[SlotRow], free: bool, total: usize) -> String {
    let line = |slot: &str, hosts: &str, counts: [&str; 4], verdict: &str, names: &str| {
        let mut cols = vec![format!("{slot:<34}"), format!("{hosts:>5}")];
        if free {
            cols.push(format!(
                "{:>4} {:>6} {:>4} {:>11}",
                counts[0], counts[1], counts[2], counts[3]
            ));
        }
        cols.push(format!("{verdict:<7}"));
        if free {
            cols.push(names.to_owned());
        }
        cols.join(" ").trim_end().to_owned()
    };
    let mut out = line(
        "SLOT",
        "HOSTS",
        ["FREE", "LOCKED", "POOL", "UNREACHABLE"],
        "VERDICT",
        "FREE HOSTS",
    );
    out.push('\n');
    for r in rows {
        let tally = r.tally();
        let counts = [
            tally.free.len().to_string(),
            tally.locked.to_string(),
            tally.pool.to_string(),
            tally.unreachable.to_string(),
        ];
        out.push_str(&line(
            &slot_label(&r.slot, true),
            &r.hosts.len().to_string(),
            [&counts[0], &counts[1], &counts[2], &counts[3]],
            r.verdict(free).map_or("", Verdict::as_str),
            &tally.free.join(","),
        ));
        out.push('\n');
    }
    if rows.len() == total {
        out.push_str(&format!("\n{} slot(s)", rows.len()));
    } else {
        out.push_str(&format!("\n{} of {total} slot(s)", rows.len()));
    }
    out
}

/// Serialize slot rows as pretty JSON.
///
/// # Panics
/// Never in practice: the value is a plain array of objects.
#[must_use]
pub fn render_slot_json(rows: &[SlotRow], free: bool) -> String {
    let arr = Value::Array(rows.iter().map(|r| r.to_json(free)).collect());
    serde_json::to_string_pretty(&arr).expect("slot rows serialize")
}

/// Probes every host of every row in one bounded fan-out, as [`probe_locks`].
async fn probe_slots(config: &ProbeConfig, rows: &mut [SlotRow], connect: Connector) {
    let lens: Vec<usize> = rows.iter().map(|r| r.hosts.len()).collect();
    let mut flat: Vec<Record> = rows
        .iter_mut()
        .flat_map(|r| std::mem::take(&mut r.hosts))
        .collect();
    probe_locks(config, &mut flat, connect).await;
    let mut flat = flat.into_iter();
    for (row, n) in rows.iter_mut().zip(lens) {
        row.hosts = flat.by_ref().take(n).collect();
    }
}

/// What a `--summary` run reads: the parsed flags, the inventory, and how a
/// `--free` probe reaches a host.
struct Survey<'a> {
    args: &'a ArgMatches,
    config: &'a mtui_config::Config,
    store: &'a Refhosts,
    filters: &'a Filters<'a>,
    connect: Connector,
}

/// The windowed rows of a `--summary` run, before the row budget.
struct SummaryRows {
    /// The slot rows `--offset`/`--limit` kept, each host probed under `--free`.
    rows: Vec<SlotRow>,
    /// Slots matched before the window.
    total: usize,
    /// Hosts matched before the window.
    total_hosts: usize,
}

/// The `--summary` slot rows windowed by `--offset`/`--limit`, with every host
/// of a kept slot probed under `--free`.
async fn summary_rows(survey: &Survey<'_>) -> SummaryRows {
    let rows = gather_slots(survey.store, survey.filters);
    let total = rows.len();
    let total_hosts = rows.iter().map(|r| r.hosts.len()).sum();
    let mut rows = window(survey.args, rows);
    if survey.args.get_flag("free") && !rows.is_empty() {
        probe_slots(&ProbeConfig::new(survey.config), &mut rows, survey.connect).await;
    }
    SummaryRows {
        rows,
        total,
        total_hosts,
    }
}

/// `--summary`: [`summary_rows`] under the row budget, as a table or JSON.
///
/// JSON is a plain array unless rows were cut, by the window or the row
/// budget: then `{"rows", "window"?, "truncation"?, "probe"?}`.
async fn summarize(session: &mut Session, survey: &Survey<'_>) {
    let free = survey.args.get_flag("free");
    let SummaryRows {
        rows,
        total,
        total_hosts,
    } = summary_rows(survey).await;
    let windowed = rows.len();
    let probed: usize = rows.iter().map(|r| r.hosts.len()).sum();
    let probe_cut = free && probed < total_hosts;
    let crushed = crush(
        rows,
        |r| r.slot.clone(),
        |r| r.verdict(free).map_or(0, Verdict::severity),
    );
    if survey.args.get_flag("json") {
        let truncation = crushed.meta(REFHOSTS_HINT);
        if windowed == total && truncation.is_none() {
            session
                .display
                .println(&render_slot_json(&crushed.kept, free));
            return;
        }
        let mut obj = serde_json::Map::new();
        let rows = crushed.kept.iter().map(|r| r.to_json(free)).collect();
        obj.insert("rows".to_owned(), Value::Array(rows));
        if windowed < total {
            obj.insert(
                "window".to_owned(),
                json!({"kept": windowed, "total": total}),
            );
        }
        if let Some(t) = truncation {
            obj.insert("truncation".to_owned(), t);
        }
        if probe_cut {
            obj.insert("probe".to_owned(), probe_meta(probed, total_hosts));
        }
        session.display.println(
            &serde_json::to_string_pretty(&Value::Object(obj)).expect("envelope serializes"),
        );
        return;
    }
    if total == 0 {
        session.display.println("no refhosts match");
        return;
    }
    session
        .display
        .println(&render_slot_table(&crushed.kept, free, total));
    if let Some(notice) = crushed.notice(REFHOSTS_HINT) {
        session.display.println(&notice);
    }
    if probe_cut {
        session.display.println(&probe_notice(probed, total_hosts));
    }
}

#[async_trait]
impl Command for ListRefhosts {
    fn name(&self) -> &'static str {
        "list_refhosts"
    }

    fn about(&self) -> Option<&'static str> {
        Some("Lists reference hosts from `refhosts.yml` (offline search, no connect).")
    }

    fn scope(&self) -> Scope {
        // Inventory query — independent of any loaded template; never fan out.
        Scope::Single
    }

    fn reads_resolved_report(&self) -> bool {
        // Inventory query, as above.
        false
    }

    fn configure(&self, cmd: clap::Command) -> clap::Command {
        let cmd = cmd
            .arg(
                Arg::new("testplatform")
                    .long("testplatform")
                    .value_name("QUERY")
                    .help("match a SMELT testplatform query"),
            )
            .arg(
                Arg::new("name")
                    .short('n')
                    .long("name")
                    .value_name("GLOB")
                    .help("hostname glob, e.g. 'whale-*' or '*.qam.suse.cz'"),
            )
            .arg(
                Arg::new("arch")
                    .short('a')
                    .long("arch")
                    .value_name("ARCH")
                    .action(ArgAction::Append)
                    .help("arch filter (repeatable): x86_64 / aarch64 / ppc64le / s390x"),
            )
            .arg(
                Arg::new("product")
                    .short('p')
                    .long("product")
                    .value_name("SUBSTR")
                    .help("base-product substring, e.g. sles / sled / SLE_HPC"),
            )
            .arg(
                Arg::new("version")
                    .long("version")
                    .value_name("VERSION")
                    .help("product version: 15-SP6 / 15.6 / 15 (SP optional)"),
            )
            .arg(
                Arg::new("addon")
                    .long("addon")
                    .value_name("SUBSTR")
                    .action(ArgAction::Append)
                    .help("addon-name substring (repeatable)"),
            )
            .arg(
                Arg::new("pool")
                    .long("pool")
                    .action(ArgAction::SetTrue)
                    .help("group by test-target slot (product+version+arch+addons)"),
            )
            .arg(
                Arg::new("summary")
                    .long("summary")
                    .action(ArgAction::SetTrue)
                    .conflicts_with_all(["pool", "verbose"])
                    .help(
                        "one row per slot instead of per host: its host count, and with \
                     --free the free/locked/pool/unreachable counts, the free hosts and a \
                     verdict (free/busy/unknown/none); with --testplatform every requested \
                     arch gets a row, `none` when no host serves it",
                    ),
            )
            .arg(
                Arg::new("json")
                    .long("json")
                    .action(ArgAction::SetTrue)
                    .help(
                        "emit a JSON array of kept rows; over-cap stdout is a JSON envelope \
                     {\"rows\": [...], \"truncation\": {...}} so stdout stays valid JSON \
                     (--free windowing adds a \"probe\" object); with --summary, a \
                     --limit/--offset cut also gives the envelope, with a \
                     \"window\": {\"kept\", \"total\"} object",
                    ),
            );
        add_window_args(cmd)
            .arg(
                Arg::new("free")
                    .long("free")
                    .action(ArgAction::SetTrue)
                    .help(
                        "also probe live operation-lock and pool-claim state \
                   (connects to each shown host; --offset/--limit window first)",
                    ),
            )
            .arg(
                Arg::new("verbose")
                    .short('v')
                    .long("verbose")
                    .action(ArgAction::SetTrue)
                    .help("include addons in the output"),
            )
    }

    fn complete(&self, _session: &Session, text: &str, line: &str) -> Vec<String> {
        super::support::complete_choices(
            &[
                &["--testplatform"],
                &["-n", "--name"],
                &["-a", "--arch"],
                &["-p", "--product"],
                &["--version"],
                &["--addon"],
                &["--pool"],
                &["--summary"],
                &["--json"],
                &["--limit"],
                &["--offset"],
                &["--free"],
                &["-v", "--verbose"],
            ],
            Vec::new(),
            line,
            text,
        )
    }

    async fn call(&self, session: &mut Session, args: &ArgMatches) -> CommandResult {
        if let Some(tp) = args.get_one::<String>("testplatform") {
            Attributes::try_from_testplatform(tp)
                .map_err(|e| CommandError::Other(e.to_string()))?;
        }
        let config = session.config.clone();

        let factory = RefhostsFactory::production(
            config.refhosts_path.clone(),
            VerifyPolicy::from_config(&config.ssl_verify),
        )
        .map_err(|e| CommandError::Other(format!("refhosts resolver init failed: {e}")))?;
        let store = factory
            .resolve(ResolveConfig {
                refhosts_resolvers: &config.refhosts_resolvers,
                refhosts_path: &config.refhosts_path,
                refhosts_https_uri: &config.refhosts_https_uri,
                refhosts_https_expiration: config.refhosts_https_expiration,
            })
            .await
            .map_err(|e| CommandError::Other(format!("refhosts resolve failed: {e}")))?;

        let arch: Vec<String> = args
            .get_many::<String>("arch")
            .map(|it| it.cloned().collect())
            .unwrap_or_default();
        let addon: Vec<String> = args
            .get_many::<String>("addon")
            .map(|it| it.cloned().collect())
            .unwrap_or_default();
        let pool = args.get_flag("pool");
        let free = args.get_flag("free");
        let verbose = args.get_flag("verbose");
        let as_json = args.get_flag("json");

        let filters = Filters {
            testplatform: args.get_one::<String>("testplatform").map(String::as_str),
            name: args.get_one::<String>("name").map(String::as_str),
            arch: &arch,
            product: args.get_one::<String>("product").map(String::as_str),
            version: args.get_one::<String>("version").map(String::as_str),
            addon: &addon,
            pool,
        };

        if args.get_flag("summary") {
            let survey = Survey {
                args,
                config: &config,
                store: &store,
                filters: &filters,
                connect: ssh_target,
            };
            summarize(session, &survey).await;
            return Ok(());
        }

        let records = gather(&store, &filters);
        let matched_total = records.len();

        // Windowing BEFORE the --free probes so paging reduces SSH cost: only
        // the windowed subset is probed. Order is the deterministic matched
        // inventory order (refhosts.yml file order after filters); the
        // notice/footer below still report pre-window totals.
        let mut windowed = window(args, records);
        let window_dropped = matched_total.saturating_sub(windowed.len());

        if free && !windowed.is_empty() {
            probe_locks(&ProbeConfig::new(&config), &mut windowed, ssh_target).await;
        }

        // Row budget backstops the whole-inventory dump: head+tail+anomalies, exact-deduped.
        // Row-cap is not byte-cap: MCP max_output_bytes can still cut mid-array on huge rows.
        let probed_len = windowed.len();
        let crushed = crush(
            windowed,
            |r| {
                (
                    r.name.clone(),
                    r.arch.clone(),
                    r.product.clone(),
                    r.version.clone(),
                    r.addons.clone(),
                    r.slot.clone(),
                    r.lock.clone(),
                    r.pool.clone(),
                    r.lock_holder.clone(),
                    r.pool_holder.clone(),
                )
            },
            anomaly_severity,
        );
        // Truncation notices only a real row-budget cut; --free windowing gets
        // its own probe note so anomaly counts never cover unprobed hosts.
        let trunc_notice = crushed.notice(REFHOSTS_HINT);
        let probe_needed = free && window_dropped > 0;
        let probe_note = probe_needed.then(|| probe_notice(probed_len, matched_total));
        if as_json {
            let trunc_meta = crushed.meta(REFHOSTS_HINT);
            let probe_meta_opt = probe_needed.then(|| probe_meta(probed_len, matched_total));
            if trunc_meta.is_some() || probe_meta_opt.is_some() {
                let rows: Vec<Value> = crushed.kept.iter().map(Record::to_json).collect();
                session
                    .display
                    .println(&json_envelope(rows, trunc_meta, probe_meta_opt));
            } else {
                session.display.println(&render_json(&crushed.kept));
            }
            return Ok(());
        }
        if crushed.kept.is_empty() {
            session.display.println("no refhosts match");
            return Ok(());
        }
        session.display.println(&render_table(
            &crushed.kept,
            pool,
            free,
            verbose,
            matched_total,
        ));
        if let Some(notice) = trunc_notice {
            session.display.println(&notice);
        }
        if let Some(note) = probe_note {
            session.display.println(&note);
        }
        Ok(())
    }
}

/// A `Config` that provably cannot reap: the only constructor clears both reap
/// flags, so a `--free` survey cannot force-remove another tester's operation
/// lock or pool claim just for listing the host.
mod probe_config {
    pub(super) struct ProbeConfig(mtui_config::Config);

    impl ProbeConfig {
        pub(super) fn new(config: &mtui_config::Config) -> Self {
            let mut config = config.clone();
            config.lock_reap_stale = false;
            config.pool_reap_stale = false;
            Self(config)
        }

        pub(super) fn get(&self) -> &mtui_config::Config {
            &self.0
        }
    }
}
use probe_config::ProbeConfig;

/// Builds the not-yet-connected [`Target`] a `--free` probe reads one host
/// through; a seam so tests probe mock connections.
type Connector = fn(&mtui_config::Config, &str) -> Target;

/// The production [`Connector`]: a real SSH target.
fn ssh_target(config: &mtui_config::Config, name: &str) -> Target {
    Target::new(config, name.to_owned(), TargetState::Enabled)
}

/// What a `--free` probe learned about one host: the lock label and the held
/// operation lock and pool claim, if any.
type Probe = (String, Option<LockRow>, Option<LockRow>);

/// Probes one connected target's operation-lock and pool-claim state.
///
/// The operation-lock read decides reachability; a pool-claim read that fails
/// (a permission-denied read) degrades to "no claim" with a warning rather than
/// mislabelling a host that answered as `unreachable`. An empty or unparseable
/// `/var/lock/mtui-pool.lock` reads as a claim naming no RRID, and an invalid
/// operation lock reads as `locked`; neither is ever reported as free.
async fn probe_state(target: &mut Target, host: &str) -> Probe {
    let Ok(op) = target.try_lock_status(false).await else {
        return ("unreachable".to_owned(), None, None);
    };
    let state = if op.is_locked { "locked" } else { "free" }.to_owned();
    let op = op.is_locked.then_some(op);
    match target.try_lock_status(true).await {
        Ok(claim) => (state, op, claim.is_locked.then_some(claim)),
        Err(e) => {
            tracing::warn!(host, error = %e, "pool claim unreadable; reporting no claim");
            (state, op, None)
        }
    }
}

/// Connect to each matched host in parallel and record its live operation-lock
/// and pool-claim state under [`Record::lock`] / [`Record::pool`].
/// Connects without reaping ([`ProbeConfig`]): a survey must not force-remove
/// another tester's lock just for being listed.
///
/// The only on-wire part of the command; failures are swallowed per host so one
/// dead host never aborts the listing.
async fn probe_locks(config: &ProbeConfig, records: &mut [Record], connect: Connector) {
    // Caps peak concurrent connections on large inventories: one task per
    // record, but at most `[connection] max_parallel` connecting at once.
    let bound = (config.get().max_parallel as usize).max(1);
    let sem = std::sync::Arc::new(tokio::sync::Semaphore::new(bound));
    let mut set: JoinSet<(String, Probe)> = JoinSet::new();
    for r in records.iter() {
        let config = config.get().clone();
        let name = r.name.clone();
        let sem = std::sync::Arc::clone(&sem);
        set.spawn(async move {
            // Held for the task's lifetime, so at most `bound` probes run.
            let _permit = sem.acquire_owned().await.expect("semaphore is not closed");
            let mut target = connect(&config, &name);
            let probe = match target.connect().await {
                Ok(()) => probe_state(&mut target, &name).await,
                Err(_) => ("unreachable".to_owned(), None, None),
            };
            (name, probe)
        });
    }
    let mut states = std::collections::HashMap::new();
    while let Some(res) = set.join_next().await {
        if let Ok((name, probe)) = res {
            states.insert(name, probe);
        }
    }
    for r in records.iter_mut() {
        match states.get(&r.name).cloned() {
            Some(probe) => apply_probe(r, probe),
            None => r.lock = Some("unknown".to_owned()),
        }
    }
}

/// Records a [`probe_state`] result on its row; the pool RRID is the claim's
/// parsed stamp, `""` for a claim that names none.
fn apply_probe(r: &mut Record, (state, op, claim): Probe) {
    r.lock = Some(state);
    r.pool = claim.as_ref().map(|c| c.comment.clone());
    r.lock_holder = op;
    r.pool_holder = claim;
}

#[cfg(test)]
mod tests {
    use super::*;
    use mtui_types::version::VersionField;
    use mtui_types::{Addon, Product, Version};

    fn ver(major: u64, minor: Option<VersionField>) -> Version {
        Version {
            major: VersionField::Num(major),
            minor,
        }
    }

    fn host(name: &str, arch: &str, minor: Option<VersionField>, addons: Vec<&str>) -> Host {
        Host {
            name: name.to_owned(),
            arch: arch.to_owned(),
            product: Product {
                name: "sles".to_owned(),
                version: Some(ver(15, minor)),
            },
            addons: addons
                .into_iter()
                .map(|a| Addon {
                    name: a.to_owned(),
                    version: None,
                })
                .collect(),
        }
    }

    fn store() -> Refhosts {
        Refhosts::from_hosts(vec![
            host(
                "whale-01",
                "x86_64",
                Some(VersionField::Num(6)),
                vec!["sdk"],
            ),
            host("whale-02", "aarch64", Some(VersionField::Num(6)), vec![]),
            host("shark-01", "x86_64", Some(VersionField::Num(5)), vec![]),
        ])
    }

    #[test]
    fn name_and_single_scope() {
        assert_eq!(ListRefhosts.name(), "list_refhosts");
        assert_eq!(ListRefhosts.scope(), Scope::Single);
    }

    #[test]
    fn gather_no_filters_returns_all() {
        let recs = gather(&store(), &Filters::default());
        assert_eq!(recs.len(), 3);
        assert_eq!(recs[0].name, "whale-01");
        assert_eq!(recs[0].version, "15-6");
        assert_eq!(recs[0].addons, vec!["sdk".to_owned()]);
        assert!(recs[0].slot.is_none());
    }

    #[test]
    fn gather_name_glob_filters() {
        let f = Filters {
            name: Some("whale-*"),
            ..Default::default()
        };
        let recs = gather(&store(), &f);
        let names: Vec<_> = recs.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, ["whale-01", "whale-02"]);
    }

    #[test]
    fn gather_arch_and_version_filters() {
        let arch = ["x86_64".to_owned()];
        let f = Filters {
            arch: &arch,
            version: Some("15-SP6"),
            ..Default::default()
        };
        let recs = gather(&store(), &f);
        let names: Vec<_> = recs.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, ["whale-01"]);
    }

    #[test]
    fn gather_testplatform_takes_precedence() {
        let arch = ["ppc64le".to_owned()]; // would exclude all, but ignored
        let f = Filters {
            testplatform: Some("base=sles(major=15,minor=6);arch=[x86_64]"),
            arch: &arch,
            ..Default::default()
        };
        let recs = gather(&store(), &f);
        let names: Vec<_> = recs.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, ["whale-01"]);
    }

    #[test]
    fn gather_pool_tags_slot() {
        let f = Filters {
            pool: true,
            ..Default::default()
        };
        let recs = gather(&store(), &f);
        assert_eq!(recs[0].slot.as_deref(), Some("sles-15-6 x86_64"));
        assert_eq!(recs[2].slot.as_deref(), Some("sles-15-5 x86_64"));
    }

    #[test]
    fn pool_label_of_a_host_without_a_version_has_no_trailing_hyphen() {
        let mut bare = host("whale-09", "x86_64", None, vec![]);
        bare.product.version = None;
        assert_eq!(record(&bare, true).slot.as_deref(), Some("sles x86_64"));
        let versioned = host(
            "whale-10",
            "x86_64",
            Some(VersionField::Num(6)),
            vec!["sdk"],
        );
        assert_eq!(
            record(&versioned, true).slot.as_deref(),
            Some("sles-15-6 x86_64")
        );
    }

    #[test]
    fn render_table_plain_lists_and_counts() {
        let recs = gather(&store(), &Filters::default());
        let total = recs.len();
        let out = render_table(&recs, false, false, false, total);
        assert!(out.contains("whale-01"));
        assert!(out.contains("sles 15-6"));
        assert!(out.ends_with("3 refhost(s)"));
        // addons hidden without -v
        assert!(!out.contains("sdk"));
    }

    #[test]
    fn render_table_verbose_shows_addons() {
        let recs = gather(&store(), &Filters::default());
        let total = recs.len();
        let out = render_table(&recs, false, false, true, total);
        assert!(out.contains("sdk"));
    }

    #[test]
    fn render_table_free_column_present() {
        let mut recs = gather(&store(), &Filters::default());
        recs[0].lock = Some("locked".to_owned());
        let total = recs.len();
        let out = render_table(&recs, false, true, false, total);
        assert!(out.contains("locked"));
    }

    #[test]
    fn lock_label_combines_op_and_pool_state() {
        let cases: &[(Option<&str>, Option<&str>, &str)] = &[
            (Some("free"), None, "free"),
            (Some("free"), Some("SUSE:Maintenance:9:9"), "pool"),
            (Some("free"), Some(""), "pool"),
            (Some("locked"), None, "locked"),
            (Some("locked"), Some("SUSE:Maintenance:9:9"), "locked+pool"),
            (
                Some("unreachable"),
                Some("SUSE:Maintenance:9:9"),
                "unreachable",
            ),
            (Some("unknown"), Some("SUSE:Maintenance:9:9"), "unknown"),
        ];
        for (lock, pool, expected) in cases {
            assert_eq!(
                lock_label(*lock, *pool),
                *expected,
                "lock={lock:?} pool={pool:?}"
            );
        }
    }

    #[test]
    fn render_table_pool_groups_by_slot() {
        let f = Filters {
            pool: true,
            ..Default::default()
        };
        let recs = gather(&store(), &f);
        let total = recs.len();
        let out = render_table(&recs, true, false, false, total);
        assert!(out.contains("== sles-15-5 x86_64 =="));
        assert!(out.contains("== sles-15-6 x86_64 =="));
        assert!(out.contains("  whale-01"));
    }

    #[test]
    fn probe_config_disables_both_reap_flags() {
        let config = mtui_config::Config::default();
        assert!(config.lock_reap_stale);
        assert!(config.pool_reap_stale);
        let probe = ProbeConfig::new(&config);
        assert!(!probe.get().lock_reap_stale);
        assert!(!probe.get().pool_reap_stale);
    }

    #[tokio::test]
    async fn probe_does_not_reap_a_stale_foreign_lock() {
        use mtui_hosts::{MockConnection, MockSftpOp, TARGET_LOCK_PATH, Target};
        use std::path::PathBuf;

        // Years-old, so `connect()`'s normal stale-reap would remove it if the
        // probe didn't disable reaping first.
        let conn = MockConnection::new("whale-01")
            .with_file(TARGET_LOCK_PATH, b"1700000000:alice:4242:busy".to_vec());
        let handle = conn.clone();
        let mut target = Target::with_connection_and_config(
            ProbeConfig::new(&mtui_config::Config::default()).get(),
            "whale-01",
            mtui_types::enums::TargetState::Enabled,
            Box::new(conn),
        );

        target.connect().await.expect("connect");

        assert!(
            !handle
                .sftp_ops()
                .contains(&MockSftpOp::Remove(PathBuf::from(TARGET_LOCK_PATH))),
            "a --free probe must not reap a foreign lock"
        );
        assert!(
            target.is_locked().await.expect("is_locked ok"),
            "the foreign lock must still be reported as held"
        );
    }

    #[test]
    fn render_json_shape() {
        let recs = gather(&store(), &Filters::default());
        let json = render_json(&recs);
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed[0]["name"], "whale-01");
        assert_eq!(parsed[0]["version"], "15-6");
        // lock/pool absent → keys omitted
        assert!(parsed[0].get("lock").is_none());
        assert!(parsed[0].get("pool").is_none());
    }

    #[test]
    fn render_json_includes_lock_when_probed() {
        let mut recs = gather(&store(), &Filters::default());
        recs[0].lock = Some("free".to_owned());
        let json = render_json(&recs);
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed[0]["lock"], "free");
        assert!(parsed[0]["pool"].is_null());
    }

    #[test]
    fn render_json_includes_pool_rrid_when_claimed() {
        let mut recs = gather(&store(), &Filters::default());
        recs[0].lock = Some("free".to_owned());
        recs[0].pool = Some("SUSE:Maintenance:9:9".to_owned());
        let json = render_json(&recs);
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed[0]["lock"], "free");
        assert_eq!(parsed[0]["pool"], "SUSE:Maintenance:9:9");
    }

    #[test]
    fn render_json_includes_empty_pool_string_for_unparseable_claim() {
        // `pool_claim_rrid` returns `Some("")` for a claim whose comment is not
        // a `mtui pool <RRID>` stamp — still claimed, just unparseable. `""` is
        // JS-falsy, so this pins the shape as a decision rather than an
        // accident.
        let mut recs = gather(&store(), &Filters::default());
        recs[0].lock = Some("free".to_owned());
        recs[0].pool = Some(String::new());
        let json = render_json(&recs);
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed[0]["pool"], "");
    }

    #[tokio::test]
    async fn probe_state_reports_pool_claim() {
        use mtui_hosts::{MockConnection, POOL_LOCK_PATH, Target};

        // Only the pool lock is seeded — the op lock is deliberately absent, so
        // the pool claim is the only thing that can make the label non-`free`.
        let conn = MockConnection::new("whale-01").with_file(
            POOL_LOCK_PATH,
            b"1700000000:bob:99:mtui pool SUSE:Maintenance:9:9 [bob]".to_vec(),
        );
        let mut target = Target::with_connection(
            "whale-01",
            mtui_types::enums::TargetState::Enabled,
            Box::new(conn),
        );

        let (state, _, claim) = probe_state(&mut target, "whale-01").await;
        let pool = claim.map(|c| c.comment);

        assert_eq!(state, "free");
        assert_eq!(pool.as_deref(), Some("SUSE:Maintenance:9:9"));
        assert_eq!(lock_label(Some(&state), pool.as_deref()), "pool");
    }

    #[tokio::test]
    async fn probe_state_reports_a_garbled_pool_lock_as_a_claim_not_as_free() {
        use mtui_hosts::{MockConnection, POOL_LOCK_PATH, TARGET_LOCK_PATH, Target};

        // "not-a-lock" has <3 colon fields, so `RemoteLock::from_lockfile`
        // rejects it — a valid line here would disarm this test.
        let conn = MockConnection::new("whale-01")
            .with_file(TARGET_LOCK_PATH, b"1700000000:alice:4242:busy".to_vec())
            .with_file(POOL_LOCK_PATH, b"not-a-lock".to_vec());
        let mut target = Target::with_connection(
            "whale-01",
            mtui_types::enums::TargetState::Enabled,
            Box::new(conn),
        );

        let (state, _, claim) = probe_state(&mut target, "whale-01").await;
        let pool = claim.map(|c| c.comment);

        assert_eq!(state, "locked");
        assert_eq!(pool.as_deref(), Some(""));
        assert_eq!(lock_label(Some(&state), pool.as_deref()), "locked+pool");
    }

    #[tokio::test]
    async fn probe_state_reports_an_invalid_op_lock_as_locked_not_as_free() {
        use mtui_hosts::{MockConnection, TARGET_LOCK_PATH, Target};

        let conn = MockConnection::new("whale-01").with_file(TARGET_LOCK_PATH, Vec::new());
        let mut target = Target::with_connection(
            "whale-01",
            mtui_types::enums::TargetState::Enabled,
            Box::new(conn),
        );

        let (state, _, claim) = probe_state(&mut target, "whale-01").await;
        let pool = claim.map(|c| c.comment);

        assert_eq!(state, "locked");
        assert_eq!(pool, None);
    }

    #[tokio::test]
    async fn probe_state_unreachable_when_op_lock_read_fails() {
        use mtui_hosts::{MockConnection, MockSftpOp, POOL_LOCK_PATH, TARGET_LOCK_PATH, Target};
        use std::path::PathBuf;

        let conn = MockConnection::new("whale-01").with_open_error(TARGET_LOCK_PATH);
        let handle = conn.clone();
        let mut target = Target::with_connection(
            "whale-01",
            mtui_types::enums::TargetState::Enabled,
            Box::new(conn),
        );

        let (state, _, claim) = probe_state(&mut target, "whale-01").await;
        let pool = claim.map(|c| c.comment);

        assert_eq!(state, "unreachable");
        assert_eq!(pool, None);
        assert!(
            !handle
                .sftp_ops()
                .contains(&MockSftpOp::Open(PathBuf::from(POOL_LOCK_PATH))),
            "an unreachable op-lock read must not pay a second round-trip for the pool lock"
        );
    }

    /// A pool claim is owned by its RRID, not by the user, and the probe has no
    /// RRID: a claim under this user's name is another template's all the same.
    #[tokio::test]
    async fn free_probe_never_calls_a_pool_claim_mine() {
        use mtui_hosts::{MockConnection, POOL_LOCK_PATH, Target};

        let base = mtui_config::Config::default();
        let user = base.session_user.clone();
        let probe_config = ProbeConfig::new(&base);
        let line = format!("1600000000:{user}:99:mtui pool SUSE:Maintenance:9:9 [{user}]");
        let conn = MockConnection::new("whale-01").with_file(POOL_LOCK_PATH, line.into_bytes());
        let mut target = Target::with_connection_and_config(
            probe_config.get(),
            "whale-01",
            mtui_types::enums::TargetState::Enabled,
            Box::new(conn),
        );
        target.connect().await.expect("connect");

        let probe = probe_state(&mut target, "whale-01").await;
        let mut recs = gather(&store(), &Filters::default());
        recs.truncate(1);
        apply_probe(&mut recs[0], probe);

        let table = render_table(&recs, false, true, false, 1);
        assert!(
            table.contains(&format!(
                "pool SUSE:Maintenance:9:9 since Sunday, 13.09.2020 12:26 UTC by {user}"
            )),
            "{table}"
        );
        assert!(!table.contains("by me"), "{table}");
    }

    #[tokio::test]
    async fn free_probe_reports_holder_time_comment_and_claim_without_writing() {
        use mtui_hosts::{MockConnection, MockSftpOp, POOL_LOCK_PATH, TARGET_LOCK_PATH, Target};

        let conn = MockConnection::new("whale-01")
            .with_file(
                TARGET_LOCK_PATH,
                b"1700000000:alice:4242:kernel bisect, < 2h".to_vec(),
            )
            .with_file(
                POOL_LOCK_PATH,
                b"1600000000:bob:99:mtui pool SUSE:Maintenance:9:9 [bob]".to_vec(),
            );
        let handle = conn.clone();
        let mut target = Target::with_connection_and_config(
            ProbeConfig::new(&mtui_config::Config::default()).get(),
            "whale-01",
            mtui_types::enums::TargetState::Enabled,
            Box::new(conn),
        );
        target.connect().await.expect("connect");

        let probe = probe_state(&mut target, "whale-01").await;
        let mut recs = gather(&store(), &Filters::default());
        recs.truncate(1);
        apply_probe(&mut recs[0], probe);

        let table = render_table(&recs, false, true, false, 1);
        assert!(
            table.contains(
                "locked+pool            since Tuesday, 14.11.2023 22:13 UTC by alice : \
                 kernel bisect, < 2h; pool SUSE:Maintenance:9:9 since Sunday, 13.09.2020 \
                 12:26 UTC by bob"
            ),
            "{table}"
        );
        let json: Value = serde_json::from_str(&render_json(&recs)).unwrap();
        assert_eq!(json[0]["lock"], "locked");
        assert_eq!(json[0]["pool"], "SUSE:Maintenance:9:9");
        assert_eq!(
            json[0]["lock_holder"],
            json!({"user": "alice", "since": "Tuesday, 14.11.2023 22:13 UTC",
                   "comment": "kernel bisect, < 2h"})
        );
        assert_eq!(
            json[0]["pool_holder"],
            json!({"user": "bob", "since": "Sunday, 13.09.2020 12:26 UTC"})
        );

        let writes: Vec<_> = handle
            .sftp_ops()
            .into_iter()
            .filter(|op| {
                matches!(
                    op,
                    MockSftpOp::Put { .. }
                        | MockSftpOp::PutBytes { .. }
                        | MockSftpOp::Write { .. }
                        | MockSftpOp::Append(_)
                        | MockSftpOp::Remove(_)
                )
            })
            .collect();
        assert!(
            writes.is_empty(),
            "a --free probe must only read: {writes:?}"
        );
    }

    #[test]
    fn free_json_holders_are_null_when_free_or_unparseable() {
        let mut recs = gather(&store(), &Filters::default());
        recs.truncate(1);
        apply_probe(
            &mut recs[0],
            (
                "locked".to_owned(),
                Some(LockRow {
                    is_locked: true,
                    invalid: true,
                    ..LockRow::default()
                }),
                None,
            ),
        );
        let json: Value = serde_json::from_str(&render_json(&recs)).unwrap();
        assert!(json[0]["lock_holder"].is_null(), "{json}");
        assert!(json[0]["pool_holder"].is_null(), "{json}");
        let table = render_table(&recs, false, true, false, 1);
        assert!(
            table.contains("locked                 invalid lock file"),
            "{table}"
        );
    }

    #[test]
    fn ver_str_handles_missing_version_and_minor() {
        // No version at all → empty.
        let mut h = host("h", "x86_64", None, vec![]);
        h.product.version = None;
        assert_eq!(ver_str(&h), "");
        // Version with no minor → bare major.
        let h = host("h", "x86_64", None, vec![]);
        assert_eq!(ver_str(&h), "15");
    }

    #[test]
    fn complete_offers_all_search_flags() {
        use crate::commands::testkit::empty_session;
        let (session, _buf) = empty_session();
        let all = ListRefhosts.complete(&session, "", "list_refhosts ");
        for f in [
            "--testplatform",
            "-n",
            "--name",
            "-a",
            "--arch",
            "-p",
            "--product",
            "--version",
            "--addon",
            "--pool",
            "--json",
            "--limit",
            "--offset",
            "--free",
            "-v",
            "--verbose",
        ] {
            assert!(all.contains(&f.to_owned()), "missing {f}: {all:?}");
        }
        assert_eq!(
            ListRefhosts.complete(&session, "--ver", "list_refhosts --ver"),
            vec!["--version", "--verbose"]
        );
    }

    #[test]
    fn configure_parses_all_flags() {
        use crate::commands::testkit::matches;
        let args = matches(
            &ListRefhosts,
            &[
                "--testplatform",
                "base=sles(major=15,minor=6);arch=[x86_64]",
                "-n",
                "whale-*",
                "-a",
                "x86_64",
                "-a",
                "aarch64",
                "-p",
                "sles",
                "--version",
                "15-SP6",
                "--addon",
                "sdk",
                "--pool",
                "--json",
                "--limit",
                "10",
                "--offset",
                "5",
                "--free",
                "-v",
            ],
        );
        assert_eq!(
            args.get_one::<String>("testplatform").map(String::as_str),
            Some("base=sles(major=15,minor=6);arch=[x86_64]")
        );
        assert_eq!(
            args.get_one::<String>("name").map(String::as_str),
            Some("whale-*")
        );
        let arch: Vec<_> = args
            .get_many::<String>("arch")
            .unwrap()
            .map(String::as_str)
            .collect();
        assert_eq!(arch, ["x86_64", "aarch64"]);
        assert!(args.get_flag("pool"));
        assert!(args.get_flag("json"));
        assert!(args.get_flag("free"));
        assert!(args.get_flag("verbose"));
        assert_eq!(args.get_one::<usize>("limit").copied(), Some(10));
        assert_eq!(args.get_one::<usize>("offset").copied(), Some(5));
    }

    /// A session resolving refhosts from a local `path` file, plus the temp dir
    /// keeping that file alive.
    fn session_with_refhosts_file(
        yaml: &str,
    ) -> (Session, crate::commands::testkit::Buffer, tempfile::TempDir) {
        use crate::commands::testkit::Buffer;
        use crate::display::{ColorMode, CommandPromptDisplay};

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("refhosts.yml");
        std::fs::write(&path, yaml).unwrap();

        let mut config = mtui_config::Config::default();
        config.refhosts_resolvers = "path".to_owned();
        config.refhosts_path = path;

        let buf = Buffer::new();
        let display = CommandPromptDisplay::with_sink(Box::new(buf.clone()), ColorMode::Never);
        let session = Session::with_display(config, false, display);
        (session, buf, dir)
    }

    const YAML: &str = "\
default:
  - name: whale-01
    arch: x86_64
    product:
      name: sles
      version:
        major: 15
        minor: 6
  - name: shark-01
    arch: aarch64
    product:
      name: sles
      version:
        major: 15
        minor: 5
";

    #[tokio::test]
    async fn call_offline_renders_table() {
        use crate::commands::testkit::matches;
        let (mut session, buf, _dir) = session_with_refhosts_file(YAML);
        let args = matches(&ListRefhosts, &[]);
        ListRefhosts.call(&mut session, &args).await.unwrap();
        let out = buf.contents();
        assert!(out.contains("whale-01"), "{out}");
        assert!(out.contains("shark-01"), "{out}");
        assert!(out.contains("2 refhost(s)"), "{out}");
    }

    #[tokio::test]
    async fn call_offline_json_and_filter() {
        use crate::commands::testkit::matches;
        let (mut session, buf, _dir) = session_with_refhosts_file(YAML);
        let args = matches(&ListRefhosts, &["-n", "whale-*", "--json"]);
        ListRefhosts.call(&mut session, &args).await.unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&buf.contents()).unwrap();
        assert_eq!(parsed.as_array().unwrap().len(), 1);
        assert_eq!(parsed[0]["name"], "whale-01");
    }

    #[tokio::test]
    async fn call_refuses_a_testplatform_that_requests_no_slot() {
        use crate::commands::testkit::matches;
        for (tp, why) in [
            (
                "base=sles(major=15,minor=6)",
                "it has no `arch=[…]` segment",
            ),
            (
                "base=sles(major=15,minor=6);arch=x86_64",
                "its arch segment does not parse",
            ),
            ("base=sles(major=15,minor=6);arch=[]", "names an empty arch"),
            (
                "base=sles(major=15,minor=6);arch=[x86_64,]",
                "names an empty arch",
            ),
        ] {
            for json in [false, true] {
                let (mut session, buf, _dir) = session_with_refhosts_file(YAML);
                let mut argv = vec!["--testplatform", tp];
                if json {
                    argv.push("--json");
                }
                let err = ListRefhosts
                    .call(&mut session, &matches(&ListRefhosts, &argv))
                    .await
                    .expect_err("a testplatform requesting no slot is refused");
                assert!(err.to_string().contains(why), "{tp}: {err}");
                assert_eq!(buf.contents(), "", "{tp}: nothing printed");
            }
        }
    }

    #[tokio::test]
    async fn call_offline_no_match_message() {
        use crate::commands::testkit::matches;
        let (mut session, buf, _dir) = session_with_refhosts_file(YAML);
        let args = matches(&ListRefhosts, &["-n", "nope-*"]);
        ListRefhosts.call(&mut session, &args).await.unwrap();
        assert!(buf.contents().contains("no refhosts match"));
    }

    // ------------------------------------------------------------ row budget

    /// 150-record inventory with one mid-list `locked` anomaly and exact duplicates.
    fn crush_records() -> Vec<Record> {
        let mut recs: Vec<Record> = (0..150)
            .map(|i| Record {
                name: format!("host-{i:03}"),
                arch: "x86_64".to_owned(),
                product: "sles".to_owned(),
                version: "15-6".to_owned(),
                addons: vec![],
                slot: None,
                lock: Some("free".to_owned()),
                pool: None,
                lock_holder: None,
                pool_holder: None,
            })
            .collect();
        recs[100].name = "host-anomaly".to_owned();
        recs[100].lock = Some("locked".to_owned());
        recs.push(recs[0].clone());
        recs.push(recs[1].clone());
        recs
    }

    #[test]
    fn row_budget_crushes_inventory_and_keeps_locked_anomaly() {
        use super::super::row_budget::{ROW_CAP, crush};
        let out = crush(
            crush_records(),
            |r| {
                (
                    r.name.clone(),
                    r.arch.clone(),
                    r.product.clone(),
                    r.version.clone(),
                    r.addons.clone(),
                    r.slot.clone(),
                    r.lock.clone(),
                    r.pool.clone(),
                )
            },
            anomaly_severity,
        );
        assert_eq!(out.total, 150, "deduped total");
        assert!(out.kept.len() <= ROW_CAP, "{}", out.kept.len());
        assert!(out.kept.iter().any(|r| r.name == "host-anomaly"));
        assert!(!out.kept.iter().any(|r| r.name == "host-060"));
        assert!(out.truncated > 0);
        assert_eq!((out.anomaly_kept, out.anomaly_total), (1, 1));
    }

    #[test]
    fn locked_outranks_unreachable_when_overflowing() {
        use super::super::row_budget::crush;
        // 300 probed rows: mild `unreachable` everywhere middle except one
        // severe `locked` at 250 — only 50 middle anomalies fit.
        let recs: Vec<Record> = (0..300)
            .map(|i| Record {
                name: format!("host-{i:03}"),
                arch: "x86_64".to_owned(),
                product: "sles".to_owned(),
                version: "15-6".to_owned(),
                addons: vec![],
                slot: None,
                lock: Some(if i == 250 {
                    "locked".to_owned()
                } else if (super::super::row_budget::ROW_HEAD
                    ..300 - super::super::row_budget::ROW_TAIL)
                    .contains(&i)
                {
                    "unreachable".to_owned()
                } else {
                    "free".to_owned()
                }),
                pool: None,
                lock_holder: None,
                pool_holder: None,
            })
            .collect();
        assert_eq!(anomaly_severity(&recs[250]), 2);
        assert_eq!(anomaly_severity(&recs[41]), 1);
        assert_eq!(anomaly_severity(&recs[0]), 0);
        let out = crush(
            recs,
            |r| {
                (
                    r.name.clone(),
                    r.arch.clone(),
                    r.product.clone(),
                    r.version.clone(),
                    r.addons.clone(),
                    r.slot.clone(),
                    r.lock.clone(),
                    r.pool.clone(),
                )
            },
            anomaly_severity,
        );
        let names: Vec<_> = out.kept.iter().map(|r| r.name.as_str()).collect();
        assert!(names.contains(&"host-250"), "severe locked survives");
        assert!(!names.contains(&"host-200"), "mild unreachable drops");
        assert_eq!((out.anomaly_kept, out.anomaly_total), (50, 250));
    }

    #[test]
    fn footer_shows_pre_window_total_when_paged() {
        let recs = crush_records();
        let total = recs.len();
        let windowed = recs[50..100].to_vec();
        let out = render_table(&windowed, false, false, false, total);
        assert!(out.ends_with("50 of 152 refhost(s)"), "{out}");
    }

    #[tokio::test]
    async fn row_budget_json_emits_envelope() {
        use crate::commands::testkit::matches;
        let mut yaml = String::from("default:\n");
        for i in 0..150 {
            yaml.push_str(&format!(
                "  - name: host-{i:03}\n    arch: x86_64\n    product:\n      name: sles\n      version:\n        major: 15\n        minor: 6\n"
            ));
        }
        let (mut session, buf, _dir) = session_with_refhosts_file(&yaml);
        let args = matches(&ListRefhosts, &["--json"]);
        ListRefhosts.call(&mut session, &args).await.unwrap();
        let out = buf.contents();
        let parsed: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
        let obj = parsed.as_object().unwrap();
        assert!(obj.contains_key("rows"), "{out}");
        assert!(obj.contains_key("truncation"), "{out}");
        let rows = obj["rows"].as_array().unwrap();
        assert!(
            rows.len() <= super::super::row_budget::ROW_CAP,
            "{}",
            rows.len()
        );
        assert!(rows.iter().any(|r| r["name"] == "host-000"), "{out}");
        assert!(rows.iter().any(|r| r["name"] == "host-149"), "{out}");
        assert!(!rows.iter().any(|r| r["name"] == "host-060"), "{out}");
        assert!(!out.contains("…[truncated"), "no trailing plaintext: {out}");
    }

    #[tokio::test]
    async fn call_crushes_large_inventory_with_notice() {
        use crate::commands::testkit::matches;
        let mut yaml = String::from("default:\n");
        for i in 0..150 {
            yaml.push_str(&format!(
                "  - name: host-{i:03}\n    arch: x86_64\n    product:\n      name: sles\n      version:\n        major: 15\n        minor: 6\n"
            ));
        }
        let (mut session, buf, _dir) = session_with_refhosts_file(&yaml);
        let args = matches(&ListRefhosts, &[]);
        ListRefhosts.call(&mut session, &args).await.unwrap();
        let out = buf.contents();
        assert!(
            out.lines()
                .last()
                .is_some_and(|l| l.starts_with("…[truncated")),
            "{out}"
        );
        assert!(
            out.contains("--limit/--offset/--name/--arch/--product/--version/--addon"),
            "{out}"
        );
        assert!(
            out.contains("host-000") && out.contains("host-149"),
            "{out}"
        );
        assert!(!out.contains("host-060"), "middle row dropped: {out}");
    }

    #[test]
    fn json_help_mentions_truncation() {
        let base = clap::Command::new("list_refhosts").no_binary_name(true);
        let mut cmd = ListRefhosts.configure(base);
        let help = cmd.render_help().to_string();
        assert!(help.contains("truncation"), "{help}");
        assert!(help.contains("envelope"), "{help}");
        assert!(help.contains("valid JSON"), "{help}");
    }

    #[tokio::test]
    async fn middle_row_recoverable_via_offset_limit() {
        use crate::commands::testkit::matches;
        let mut yaml = String::from("default:\n");
        for i in 0..150 {
            yaml.push_str(&format!(
                "  - name: host-{i:03}\n    arch: x86_64\n    product:\n      name: sles\n      version:\n        major: 15\n        minor: 6\n"
            ));
        }
        let (mut session, buf, _dir) = session_with_refhosts_file(&yaml);
        let args = matches(&ListRefhosts, &["--offset", "50", "--limit", "50"]);
        ListRefhosts.call(&mut session, &args).await.unwrap();
        let out = buf.contents();
        assert!(
            out.contains("host-060"),
            "paged middle row must appear: {out}"
        );
        assert!(
            !out.contains("…[truncated"),
            "50-row window fits budget: {out}"
        );
        assert!(out.contains("50 of 150 refhost(s)"), "{out}");
    }

    #[tokio::test]
    async fn free_window_reports_pre_window_totals() {
        use crate::commands::testkit::matches;
        // Tiny inventory so the --free probes fail fast (unresolvable names);
        // windowing must happen before probing and the footer must still name
        // the pre-window matched total. A pure window emits only the probe
        // note, never a truncation notice with fabricated anomaly counts.
        let mut yaml = String::from("default:\n");
        for i in 0..5 {
            yaml.push_str(&format!(
                "  - name: host-{i:03}\n    arch: x86_64\n    product:\n      name: sles\n      version:\n        major: 15\n        minor: 6\n"
            ));
        }
        let (mut session, buf, _dir) = session_with_refhosts_file(&yaml);
        let args = matches(&ListRefhosts, &["--free", "--offset", "1", "--limit", "2"]);
        ListRefhosts.call(&mut session, &args).await.unwrap();
        let out = buf.contents();
        assert!(
            out.contains("host-001") && out.contains("host-002"),
            "{out}"
        );
        assert!(
            !out.contains("host-000") && !out.contains("host-004"),
            "{out}"
        );
        assert!(out.contains("2 of 5 refhost(s)"), "{out}");
        assert!(
            out.lines()
                .last()
                .is_some_and(|l| l.starts_with("…[probed")),
            "probed window notices the unprobed remainder: {out}"
        );
        assert!(out.contains("probed 2 of 5 hosts"), "{out}");
        assert!(
            !out.contains("…[truncated"),
            "user windowing alone never truncates: {out}"
        );
        assert!(
            !out.contains("anomalies kept"),
            "probe note carries no fabricated anomaly counts: {out}"
        );
    }

    #[tokio::test]
    async fn free_limit_json_emits_probe_envelope_without_truncation() {
        use crate::commands::testkit::matches;
        // `--free --limit 10 --json` over 150 hosts: windowed 10 fits the row
        // budget, so no truncation envelope — only a probe object, and stdout
        // stays one valid JSON document.
        let mut yaml = String::from("default:\n");
        for i in 0..150 {
            yaml.push_str(&format!(
                "  - name: host-{i:03}\n    arch: x86_64\n    product:\n      name: sles\n      version:\n        major: 15\n        minor: 6\n"
            ));
        }
        let (mut session, buf, _dir) = session_with_refhosts_file(&yaml);
        let args = matches(&ListRefhosts, &["--free", "--limit", "10", "--json"]);
        ListRefhosts.call(&mut session, &args).await.unwrap();
        let out = buf.contents();
        let parsed: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
        let obj = parsed.as_object().unwrap();
        assert!(obj.contains_key("rows"), "{out}");
        assert!(!obj.contains_key("truncation"), "{out}");
        assert!(obj.contains_key("probe"), "{out}");
        assert_eq!(obj["probe"]["probed"], 10, "{out}");
        assert_eq!(obj["probe"]["total"], 150, "{out}");
        assert_eq!(obj["rows"].as_array().unwrap().len(), 10, "{out}");
        assert!(!out.contains("…[truncated"), "{out}");
        assert!(!out.contains("…[probed"), "{out}");
    }

    // ------------------------------------------------------------ --summary

    fn slot_names(rows: &[SlotRow]) -> Vec<(String, Vec<&str>)> {
        rows.iter()
            .map(|r| {
                (
                    slot_label(&r.slot, true),
                    r.hosts.iter().map(|h| h.name.as_str()).collect(),
                )
            })
            .collect()
    }

    fn rows_of(tp: &str) -> Vec<SlotRow> {
        gather_slots(
            &store(),
            &Filters {
                testplatform: Some(tp),
                ..Default::default()
            },
        )
    }

    #[test]
    fn summary_keys_every_requested_arch_including_an_unserved_one() {
        let rows = rows_of("base=sles(major=15,minor=6);arch=[x86_64,s390x,aarch64]");
        assert_eq!(
            slot_names(&rows),
            vec![
                ("sles-15-6 x86_64".to_owned(), vec!["whale-01"]),
                ("sles-15-6 aarch64".to_owned(), vec!["whale-02"]),
                ("sles-15-6 s390x".to_owned(), vec![]),
            ]
        );
        assert_eq!(rows[2].verdict(false), Some(Verdict::NoHost));
        assert_eq!(rows[2].verdict(true), Some(Verdict::NoHost));
        assert_eq!(rows[0].verdict(false), None);
    }

    /// A host matches at most one attribute of a testplatform (they differ only
    /// by arch), so the only way to count one twice is to query a repeated arch
    /// twice: that must still give one row holding the host once.
    #[test]
    fn summary_counts_a_host_once_when_its_arch_is_requested_twice() {
        let rows = rows_of("base=sles(major=15,minor=6);arch=[x86_64,aarch64,x86_64]");
        assert_eq!(
            slot_names(&rows),
            vec![
                ("sles-15-6 x86_64".to_owned(), vec!["whale-01"]),
                ("sles-15-6 aarch64".to_owned(), vec!["whale-02"]),
            ]
        );
    }

    #[test]
    fn summary_counts_a_host_serving_the_slot_through_an_addon() {
        let mut ltss = host("ltss-01", "x86_64", Some(VersionField::Num(6)), vec![]);
        ltss.product.name = "SLES".to_owned();
        ltss.addons.push(mtui_types::Addon {
            name: "SLES-LTSS".to_owned(),
            version: Some(ver(15, Some(VersionField::Num(6)))),
        });
        let store = Refhosts::from_hosts(vec![ltss]);
        let rows = gather_slots(
            &store,
            &Filters {
                testplatform: Some("base=SLES-LTSS(major=15,minor=6);arch=[x86_64]"),
                ..Default::default()
            },
        );
        assert_eq!(
            slot_names(&rows),
            vec![("SLES-LTSS-15-6 x86_64".to_owned(), vec!["ltss-01"])]
        );
    }

    #[test]
    fn summary_without_testplatform_groups_by_full_slot() {
        let rows = gather_slots(&store(), &Filters::default());
        assert_eq!(
            slot_names(&rows),
            vec![
                ("sles-15-6 x86_64 +sdk".to_owned(), vec!["whale-01"]),
                ("sles-15-6 aarch64".to_owned(), vec!["whale-02"]),
                ("sles-15-5 x86_64".to_owned(), vec!["shark-01"]),
            ]
        );
    }

    fn claim() -> Option<LockRow> {
        Some(LockRow {
            is_locked: true,
            locked_by: "bob".to_owned(),
            comment: "SUSE:Maintenance:9:9".to_owned(),
            ..LockRow::default()
        })
    }

    /// A one-slot row whose hosts carry the given `(lock, claimed)` probe
    /// results; a `""` lock leaves the host unprobed.
    fn probed(states: &[(&str, bool)]) -> SlotRow {
        let hosts = states
            .iter()
            .enumerate()
            .map(|(i, (lock, claimed))| {
                let mut r = record(
                    &host(
                        &format!("whale-{i:02}"),
                        "x86_64",
                        Some(VersionField::Num(6)),
                        vec![],
                    ),
                    false,
                );
                if !lock.is_empty() {
                    let op = (*lock == "locked").then(|| LockRow {
                        is_locked: true,
                        locked_by: "alice".to_owned(),
                        ..LockRow::default()
                    });
                    apply_probe(
                        &mut r,
                        ((*lock).to_owned(), op, claimed.then(claim).flatten()),
                    );
                }
                r
            })
            .collect();
        SlotRow {
            slot: (
                "sles".to_owned(),
                "15-6".to_owned(),
                "x86_64".to_owned(),
                vec![],
            ),
            hosts,
        }
    }

    fn tally(free: &[&str], locked: usize, pool: usize, unreachable: usize) -> Tally {
        Tally {
            free: free.iter().map(|s| (*s).to_owned()).collect(),
            locked,
            pool,
            unreachable,
            unprobed: 0,
        }
    }

    #[test]
    fn summary_free_host_next_to_an_unreachable_one_is_free() {
        let row = probed(&[("unreachable", false), ("free", false)]);
        assert_eq!(row.tally(), tally(&["whale-01"], 0, 0, 1));
        assert_eq!(row.verdict(true), Some(Verdict::Free));
    }

    #[test]
    fn summary_every_host_taken_is_busy() {
        let row = probed(&[("locked", false), ("free", true), ("locked", true)]);
        assert_eq!(row.tally(), tally(&[], 2, 1, 0));
        assert_eq!(row.verdict(true), Some(Verdict::Busy));
    }

    #[test]
    fn summary_no_free_host_and_one_unreachable_is_unknown() {
        let row = probed(&[("locked", false), ("unreachable", false)]);
        assert_eq!(row.verdict(true), Some(Verdict::Unknown));
        let row = probed(&[("unknown", false)]);
        assert_eq!(row.tally(), tally(&[], 0, 0, 1));
        assert_eq!(row.verdict(true), Some(Verdict::Unknown));
    }

    #[test]
    fn summary_pool_claim_without_operation_lock_is_taken_not_free() {
        let row = probed(&[("free", true)]);
        assert_eq!(row.tally(), tally(&[], 0, 1, 0));
        assert_eq!(row.verdict(true), Some(Verdict::Busy));
    }

    #[test]
    fn summary_unprobed_host_is_not_counted_as_unreachable() {
        let row = probed(&[("locked", false), ("", false)]);
        assert_eq!(
            row.tally(),
            Tally {
                unprobed: 1,
                ..tally(&[], 1, 0, 0)
            }
        );
        assert_eq!(row.verdict(true), Some(Verdict::Unknown));
    }

    #[test]
    fn host_state_classifies_every_probe_outcome_once() {
        let cases = [
            (None::<&str>, None::<&str>, HostState::Unprobed, "", 0),
            (Some("free"), None, HostState::Free, "free", 0),
            (Some("free"), Some(""), HostState::Pool, "pool", 2),
            (Some("locked"), None, HostState::Locked, "locked", 2),
            (
                Some("locked"),
                Some("R"),
                HostState::LockedPool,
                "locked+pool",
                2,
            ),
            (
                Some("unreachable"),
                None,
                HostState::Unreachable,
                "unreachable",
                1,
            ),
            (Some("unknown"), Some("R"), HostState::Unknown, "unknown", 1),
        ];
        for (lock, pool, state, label, severity) in cases {
            let got = HostState::of(lock, pool);
            assert_eq!(got, state, "{lock:?} {pool:?}");
            assert_eq!(got.label(), label);
            assert_eq!(got.severity(), severity);
        }
    }

    #[test]
    fn verdict_severity_ranks_undrawable_over_undecided_over_free() {
        let order = [
            Verdict::NoHost,
            Verdict::Busy,
            Verdict::Unknown,
            Verdict::Free,
        ]
        .map(Verdict::severity);
        assert_eq!(order, [2, 2, 1, 0]);
        assert_eq!(Verdict::NoHost.to_string(), "none");
    }

    fn summary_fixture() -> Vec<SlotRow> {
        let mut free = probed(&[("free", false), ("unreachable", false), ("free", true)]);
        free.hosts[2].name = "whale-03".to_owned();
        let mut busy = probed(&[("locked", false)]);
        busy.slot.2 = "aarch64".to_owned();
        busy.slot.3 = vec!["sdk".to_owned()];
        let none = SlotRow {
            slot: (
                "sles".to_owned(),
                "15-6".to_owned(),
                "s390x".to_owned(),
                vec![],
            ),
            hosts: vec![],
        };
        vec![free, busy, none]
    }

    #[test]
    fn render_slot_table_snapshot() {
        let rows = summary_fixture();
        let with = render_slot_table(&rows, true, 3);
        let without = render_slot_table(&rows, false, 5);
        insta::assert_snapshot!(format!("{with}\n---\n{without}"));
    }

    #[test]
    fn render_slot_json_shape() {
        let rows = summary_fixture();
        let probed: Value = serde_json::from_str(&render_slot_json(&rows, true)).unwrap();
        assert_eq!(
            probed,
            json!([
                {"slot": {"product": "sles", "version": "15-6", "arch": "x86_64", "addons": []},
                 "hosts": 3, "free": ["whale-00"], "locked": 0, "pool": 1, "unreachable": 1,
                 "verdict": "free"},
                {"slot": {"product": "sles", "version": "15-6", "arch": "aarch64", "addons": ["sdk"]},
                 "hosts": 1, "free": [], "locked": 1, "pool": 0, "unreachable": 0,
                 "verdict": "busy"},
                {"slot": {"product": "sles", "version": "15-6", "arch": "s390x", "addons": []},
                 "hosts": 0, "free": [], "locked": 0, "pool": 0, "unreachable": 0,
                 "verdict": "none"},
            ])
        );
        let offline: Value = serde_json::from_str(&render_slot_json(&rows, false)).unwrap();
        assert_eq!(
            offline,
            json!([
                {"slot": {"product": "sles", "version": "15-6", "arch": "x86_64", "addons": []},
                 "hosts": 3, "free": null, "locked": null, "pool": null, "unreachable": null,
                 "verdict": null},
                {"slot": {"product": "sles", "version": "15-6", "arch": "aarch64", "addons": ["sdk"]},
                 "hosts": 1, "free": null, "locked": null, "pool": null, "unreachable": null,
                 "verdict": null},
                {"slot": {"product": "sles", "version": "15-6", "arch": "s390x", "addons": []},
                 "hosts": 0, "free": null, "locked": null, "pool": null, "unreachable": null,
                 "verdict": "none"},
            ])
        );
    }

    /// Every mock connection [`mock_target`] built for a host named `ro-*`.
    static READ_ONLY_HANDLES: std::sync::Mutex<Vec<(String, mtui_hosts::MockConnection)>> =
        std::sync::Mutex::new(Vec::new());

    /// A [`Connector`] over mock connections, by host-name marker: `down`
    /// cannot read its operation lock, `locked` holds one, `claimed` is
    /// pool-claimed, anything else is free.
    fn mock_target(config: &mtui_config::Config, name: &str) -> Target {
        use mtui_hosts::{MockConnection, POOL_LOCK_PATH, TARGET_LOCK_PATH};
        let mut conn = MockConnection::new(name);
        if name.contains("down") {
            conn = conn.with_open_error(TARGET_LOCK_PATH);
        } else if name.contains("locked") {
            conn = conn.with_file(TARGET_LOCK_PATH, b"1700000000:alice:4242:busy".to_vec());
        } else if name.contains("claimed") {
            conn = conn.with_file(
                POOL_LOCK_PATH,
                b"1600000000:bob:99:mtui pool SUSE:Maintenance:9:9 [bob]".to_vec(),
            );
        }
        if name.starts_with("ro-") {
            READ_ONLY_HANDLES
                .lock()
                .unwrap()
                .push((name.to_owned(), conn.clone()));
        }
        Target::with_connection_and_config(config, name, TargetState::Enabled, Box::new(conn))
    }

    fn fleet(hosts: &[(&str, &str)]) -> Refhosts {
        Refhosts::from_hosts(
            hosts
                .iter()
                .map(|(name, arch)| host(name, arch, Some(VersionField::Num(6)), vec![]))
                .collect(),
        )
    }

    /// Runs `summarize` over `store` with mock probes; returns its output.
    async fn summary_output(store: &Refhosts, argv: &[&str]) -> String {
        use crate::commands::testkit::matches;
        let (mut session, buf, _dir) = session_with_refhosts_file(YAML);
        let args = matches(&ListRefhosts, argv);
        let config = session.config.clone();
        let filters = Filters {
            testplatform: args.get_one::<String>("testplatform").map(String::as_str),
            name: args.get_one::<String>("name").map(String::as_str),
            ..Default::default()
        };
        let survey = Survey {
            args: &args,
            config: &config,
            store,
            filters: &filters,
            connect: mock_target,
        };
        summarize(&mut session, &survey).await;
        buf.contents()
    }

    fn four_hosts() -> Refhosts {
        fleet(&[
            ("free-a", "x86_64"),
            ("down-b", "aarch64"),
            ("locked-c", "x86_64"),
            ("claimed-d", "s390x"),
        ])
    }

    #[tokio::test]
    async fn summary_rows_probe_every_host_of_each_kept_slot_only() {
        use crate::commands::testkit::matches;
        let args = matches(&ListRefhosts, &["--summary", "--free", "--limit", "2"]);
        let survey = Survey {
            args: &args,
            config: &mtui_config::Config::default(),
            store: &four_hosts(),
            filters: &Filters::default(),
            connect: mock_target,
        };
        let out = summary_rows(&survey).await;
        assert_eq!((out.total, out.total_hosts), (3, 4));
        let probed: Vec<_> = out
            .rows
            .iter()
            .map(|r| {
                (
                    slot_label(&r.slot, true),
                    r.hosts
                        .iter()
                        .map(|h| (h.name.as_str(), h.state()))
                        .collect::<Vec<_>>(),
                )
            })
            .collect();
        assert_eq!(
            probed,
            vec![
                (
                    "sles-15-6 x86_64".to_owned(),
                    vec![("free-a", HostState::Free), ("locked-c", HostState::Locked)]
                ),
                (
                    "sles-15-6 aarch64".to_owned(),
                    vec![("down-b", HostState::Unreachable)]
                ),
            ]
        );
    }

    #[tokio::test]
    async fn summary_free_json_folds_every_probed_host() {
        let out = summary_output(&four_hosts(), &["--summary", "--free", "--json"]).await;
        let rows: Value = serde_json::from_str(out.trim()).unwrap();
        let rows = rows.as_array().expect("nothing cut: a plain array");
        let got: Vec<_> = rows
            .iter()
            .map(|r| {
                (
                    r["slot"]["arch"].clone(),
                    r["free"].clone(),
                    r["locked"].clone(),
                    r["pool"].clone(),
                    r["unreachable"].clone(),
                    r["verdict"].clone(),
                )
            })
            .collect();
        assert_eq!(
            got,
            vec![
                (
                    json!("x86_64"),
                    json!(["free-a"]),
                    json!(1),
                    json!(0),
                    json!(0),
                    json!("free")
                ),
                (
                    json!("aarch64"),
                    json!([]),
                    json!(0),
                    json!(0),
                    json!(1),
                    json!("unknown")
                ),
                (
                    json!("s390x"),
                    json!([]),
                    json!(0),
                    json!(1),
                    json!(0),
                    json!("busy")
                ),
            ]
        );
    }

    #[tokio::test]
    async fn summary_probe_only_reads_and_reaps_nothing() {
        use mtui_hosts::MockSftpOp;
        let store = fleet(&[
            ("ro-locked-1", "x86_64"),
            ("ro-claimed-2", "aarch64"),
            ("ro-free-3", "s390x"),
        ]);
        let out = summary_output(&store, &["--summary", "--free"]).await;
        assert!(out.contains("3 slot(s)"), "{out}");
        let handles: Vec<_> = READ_ONLY_HANDLES.lock().unwrap().iter().cloned().collect();
        assert_eq!(handles.len(), 3);
        for (name, conn) in handles {
            let writes: Vec<_> = conn
                .sftp_ops()
                .into_iter()
                .filter(|op| {
                    matches!(
                        op,
                        MockSftpOp::Put { .. }
                            | MockSftpOp::PutBytes { .. }
                            | MockSftpOp::Write { .. }
                            | MockSftpOp::Append(_)
                            | MockSftpOp::Remove(_)
                    )
                })
                .collect();
            assert!(writes.is_empty(), "{name}: {writes:?}");
        }
    }

    #[tokio::test]
    async fn summary_offset_window_is_named_in_text_and_json() {
        let text = summary_output(&four_hosts(), &["--summary", "--offset", "1"]).await;
        assert!(!text.contains("sles-15-6 x86_64"), "{text}");
        assert!(text.contains("sles-15-6 aarch64"), "{text}");
        assert!(text.contains("\n2 of 3 slot(s)"), "{text}");
        assert!(
            !text.contains("…[probed"),
            "no probe, no probe note: {text}"
        );

        let json = summary_output(&four_hosts(), &["--summary", "--json", "--offset", "1"]).await;
        let env: Value = serde_json::from_str(json.trim()).unwrap();
        assert_eq!(env["window"], json!({"kept": 2, "total": 3}), "{json}");
        assert_eq!(env["rows"].as_array().unwrap().len(), 2);
        assert_eq!(env["rows"][0]["slot"]["arch"], "aarch64");
        assert!(env.get("truncation").is_none() && env.get("probe").is_none());
    }

    #[tokio::test]
    async fn summary_free_window_notes_the_unprobed_hosts() {
        let argv = ["--summary", "--free", "--limit", "1"];
        let text = summary_output(&four_hosts(), &argv).await;
        assert!(
            text.lines()
                .last()
                .is_some_and(|l| l.starts_with("…[probed 2 of 4 hosts")),
            "{text}"
        );
        let mut argv = argv.to_vec();
        argv.push("--json");
        let env: Value =
            serde_json::from_str(summary_output(&four_hosts(), &argv).await.trim()).unwrap();
        assert_eq!(env["probe"], json!({"probed": 2, "total": 4}));
        assert_eq!(env["window"], json!({"kept": 1, "total": 3}));
    }

    /// 150 one-host slots (distinct arches), `locked-075` and `down-080` mid-list.
    fn wide_fleet() -> Refhosts {
        let names: Vec<(String, String)> = (0..150)
            .map(|i| {
                let name = match i {
                    75 => "locked-075".to_owned(),
                    80 => "down-080".to_owned(),
                    _ => format!("free-{i:03}"),
                };
                (name, format!("arch{i:03}"))
            })
            .collect();
        let refs: Vec<(&str, &str)> = names
            .iter()
            .map(|(n, a)| (n.as_str(), a.as_str()))
            .collect();
        fleet(&refs)
    }

    #[tokio::test]
    async fn summary_over_the_row_budget_keeps_anomalies_and_says_so() {
        let text = summary_output(&wide_fleet(), &["--summary", "--free"]).await;
        assert!(text.contains("arch075"), "busy slot survives: {text}");
        assert!(text.contains("arch080"), "unknown slot survives: {text}");
        assert!(
            !text.contains("arch060"),
            "a free mid slot is dropped: {text}"
        );
        assert!(text.contains("\n52 of 150 slot(s)"), "{text}");
        assert!(
            text.contains("…[truncated 98 of 150 rows (2/2 anomalies kept)"),
            "{text}"
        );

        let json = summary_output(&wide_fleet(), &["--summary", "--free", "--json"]).await;
        let env: Value = serde_json::from_str(json.trim()).unwrap();
        assert_eq!(env["truncation"]["truncated"], 98, "{json}");
        assert_eq!(env["truncation"]["anomaly_total"], 2, "{json}");
        assert!(env.get("window").is_none() && env.get("probe").is_none());
        let verdicts: Vec<&str> = env["rows"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|r| r["verdict"] != "free")
            .map(|r| r["verdict"].as_str().unwrap())
            .collect();
        assert_eq!(verdicts, ["busy", "unknown"]);
    }

    #[tokio::test]
    async fn summary_with_no_matching_host_says_so() {
        let text = summary_output(&four_hosts(), &["--summary", "-n", "nope-*"]).await;
        assert_eq!(text.trim(), "no refhosts match");
        let json = summary_output(&four_hosts(), &["--summary", "-n", "nope-*", "--json"]).await;
        assert_eq!(
            serde_json::from_str::<Value>(json.trim()).unwrap(),
            json!([])
        );
    }

    #[tokio::test]
    async fn call_summary_names_an_unserved_arch() {
        use crate::commands::testkit::matches;
        let (mut session, buf, _dir) = session_with_refhosts_file(YAML);
        let args = matches(
            &ListRefhosts,
            &[
                "--testplatform",
                "base=sles(major=15,minor=6);arch=[x86_64,s390x]",
                "--summary",
            ],
        );
        ListRefhosts.call(&mut session, &args).await.unwrap();
        let out = buf.contents();
        let lines: Vec<&str> = out.lines().collect();
        assert!(
            lines.contains(&"sles-15-6 x86_64                       1"),
            "{out}"
        );
        assert!(
            lines.contains(&"sles-15-6 s390x                        0 none"),
            "{out}"
        );
        assert!(out.contains("2 slot(s)"), "{out}");
    }

    #[tokio::test]
    async fn call_summary_refuses_a_testplatform_with_an_empty_arch() {
        use crate::commands::testkit::matches;
        let (mut session, buf, _dir) = session_with_refhosts_file(YAML);
        let args = matches(
            &ListRefhosts,
            &[
                "--testplatform",
                "base=sles(major=15,minor=6);arch=[x86_64,]",
                "--summary",
            ],
        );
        let err = ListRefhosts.call(&mut session, &args).await.unwrap_err();
        assert!(err.to_string().contains("names an empty arch"), "{err}");
        assert_eq!(buf.contents(), "");
    }

    #[test]
    fn summary_refuses_pool_and_verbose() {
        for other in ["--pool", "-v"] {
            let parsed = ListRefhosts
                .configure(clap::Command::new("list_refhosts"))
                .try_get_matches_from(["list_refhosts", "--summary", other]);
            assert_eq!(
                parsed.unwrap_err().kind(),
                clap::error::ErrorKind::ArgumentConflict,
                "{other}"
            );
        }
    }
}
