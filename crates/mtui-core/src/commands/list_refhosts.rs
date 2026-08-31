//! The `list_refhosts` command: query/search the refhost inventory offline.
//!
//! Reads the refhost inventory — the same source `add_host` resolves through
//! [`RefhostsFactory`] — and prints matching hosts **without connecting**: no
//! SSH, no lock, no loaded template, so refhosts can be found through mtui
//! instead of by parsing `refhosts.yml`. Results are de-duplicated by name.
//! `--free` additionally connects to report live operation-lock and pool-claim
//! state — the only part that goes on the wire — without reaping: it is a
//! survey, not a session connect, so a matched host's stale operation lock is
//! left alone.
//!
//! # Testability seam
//! [`gather`], [`render_table`] and [`render_json`] are pure over an in-memory
//! [`Refhosts`] store; only [`call`](ListRefhosts::call) touches the network,
//! and its `--free` probe builds real [`Target`]s, so that half is exercised by
//! the gated sshd integration path.

use async_trait::async_trait;
use clap::{Arg, ArgAction, ArgMatches};
use mtui_datasources::Refhosts;
use mtui_datasources::http::VerifyPolicy;
use mtui_datasources::refhost::{Attributes, RefhostsFactory, ResolveConfig};
use mtui_hosts::Target;
use mtui_types::Host;
use mtui_types::enums::TargetState;
use serde_json::{Value, json};
use tokio::task::JoinSet;

use crate::command::{Command, Scope};
use crate::error::{CommandError, CommandResult};
use crate::session::Session;

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
}

impl Record {
    /// Omits `lock`/`pool` when unset (only set under `--free`).
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
        }
        obj
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
    let slot = with_slot.then(|| {
        let (name, ver, arch, _addons) = Refhosts::slot_of(host);
        format!("{name}-{ver} {arch}")
    });
    Record {
        name: host.name.clone(),
        arch: host.arch.clone(),
        product: host.product.name.clone(),
        version: ver_str(host),
        addons: host.addons.iter().map(|a| a.name.clone()).collect(),
        slot,
        lock: None,
        pool: None,
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
    match lock {
        Some("free") if pool.is_some() => "pool".to_owned(),
        Some("locked") if pool.is_some() => "locked+pool".to_owned(),
        Some(other) => other.to_owned(),
        None => String::new(),
    }
}

/// Render `records` as one aligned multi-line table, grouped by slot when
/// `pool`.
#[must_use]
pub fn render_table(records: &[Record], pool: bool, free: bool, verbose: bool) -> String {
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
    out.push_str(&format!("\n{} refhost(s)", records.len()));
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
        cmd.arg(
            Arg::new("testplatform")
                .short('T')
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
            Arg::new("json")
                .long("json")
                .action(ArgAction::SetTrue)
                .help("emit JSON"),
        )
        .arg(
            Arg::new("free")
                .long("free")
                .action(ArgAction::SetTrue)
                .help(
                    "also probe live operation-lock and pool-claim state \
                     (connects to each matched host)",
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
                &["-T", "--testplatform"],
                &["-n", "--name"],
                &["-a", "--arch"],
                &["-p", "--product"],
                &["--version"],
                &["--addon"],
                &["--pool"],
                &["--json"],
                &["--free"],
                &["-v", "--verbose"],
            ],
            Vec::new(),
            line,
            text,
        )
    }

    async fn call(&self, session: &mut Session, args: &ArgMatches) -> CommandResult {
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
                ssl_verify: &config.ssl_verify,
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

        let mut records = gather(&store, &filters);

        if free && !records.is_empty() {
            probe_locks(&ProbeConfig::new(&config), &mut records).await;
        }

        if as_json {
            session.display.println(&render_json(&records));
            return Ok(());
        }
        if records.is_empty() {
            session.display.println("no refhosts match");
            return Ok(());
        }
        session
            .display
            .println(&render_table(&records, pool, free, verbose));
        Ok(())
    }
}

/// A `Config` that provably cannot reap: the only constructor clears both
/// reap flags, so a `--free` survey cannot force-remove another tester's
/// operation lock or pool claim just for listing the host (#573).
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

/// Probes one connected target's operation-lock and pool-claim state.
///
/// An `Err` from *either* read collapses to `("unreachable", None)`: a
/// half-answered probe is not a trustworthy `free`/`locked` verdict.
async fn probe_state(target: &mut Target) -> (String, Option<String>) {
    match (target.is_locked().await, target.pool_claim_rrid().await) {
        (Ok(locked), Ok(pool)) => ((if locked { "locked" } else { "free" }).to_owned(), pool),
        _ => ("unreachable".to_owned(), None),
    }
}

/// Connect to each matched host in parallel and record its live operation-lock
/// and pool-claim state under [`Record::lock`] / [`Record::pool`].
/// Connects without reaping ([`ProbeConfig`]): a survey must not force-remove
/// another tester's lock just for being listed.
///
/// The only on-wire part of the command; failures are swallowed per host so one
/// dead host never aborts the listing.
async fn probe_locks(config: &ProbeConfig, records: &mut [Record]) {
    // Caps peak concurrent connections on large inventories: one task per
    // record, but at most `[connection] max_parallel` connecting at once.
    let bound = (config.get().max_parallel as usize).max(1);
    let sem = std::sync::Arc::new(tokio::sync::Semaphore::new(bound));
    let mut set: JoinSet<(String, String, Option<String>)> = JoinSet::new();
    for r in records.iter() {
        let config = config.get().clone();
        let name = r.name.clone();
        let sem = std::sync::Arc::clone(&sem);
        set.spawn(async move {
            // Held for the task's lifetime, so at most `bound` probes run.
            let _permit = sem.acquire_owned().await.expect("semaphore is not closed");
            let mut target = Target::new(&config, name.clone(), TargetState::Enabled);
            let (state, pool) = match target.connect().await {
                Ok(()) => probe_state(&mut target).await,
                Err(_) => ("unreachable".to_owned(), None),
            };
            (name, state, pool)
        });
    }
    let mut states = std::collections::HashMap::new();
    while let Some(res) = set.join_next().await {
        if let Ok((name, state, pool)) = res {
            states.insert(name, (state, pool));
        }
    }
    for r in records.iter_mut() {
        match states.get(&r.name) {
            Some((state, pool)) => {
                r.lock = Some(state.clone());
                r.pool = pool.clone();
            }
            None => r.lock = Some("unknown".to_owned()),
        }
    }
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
    fn render_table_plain_lists_and_counts() {
        let recs = gather(&store(), &Filters::default());
        let out = render_table(&recs, false, false, false);
        assert!(out.contains("whale-01"));
        assert!(out.contains("sles 15-6"));
        assert!(out.ends_with("3 refhost(s)"));
        // addons hidden without -v
        assert!(!out.contains("sdk"));
    }

    #[test]
    fn render_table_verbose_shows_addons() {
        let recs = gather(&store(), &Filters::default());
        let out = render_table(&recs, false, false, true);
        assert!(out.contains("sdk"));
    }

    #[test]
    fn render_table_free_column_present() {
        let mut recs = gather(&store(), &Filters::default());
        recs[0].lock = Some("locked".to_owned());
        let out = render_table(&recs, false, true, false);
        assert!(out.contains("locked"));
    }

    #[test]
    fn lock_label_combines_op_and_pool_state() {
        let cases: &[(Option<&str>, Option<&str>, &str)] = &[
            (Some("free"), None, "free"),
            (Some("free"), Some("SUSE:Maintenance:9:9"), "pool"),
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
        let out = render_table(&recs, true, false, false);
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

        let (state, pool) = probe_state(&mut target).await;

        assert_eq!(state, "free");
        assert_eq!(pool.as_deref(), Some("SUSE:Maintenance:9:9"));
        assert_eq!(lock_label(Some(&state), pool.as_deref()), "pool");
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
            "-T",
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
                "-T",
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
    async fn call_offline_no_match_message() {
        use crate::commands::testkit::matches;
        let (mut session, buf, _dir) = session_with_refhosts_file(YAML);
        let args = matches(&ListRefhosts, &["-n", "nope-*"]);
        ListRefhosts.call(&mut session, &args).await.unwrap();
        assert!(buf.contents().contains("no refhosts match"));
    }
}
