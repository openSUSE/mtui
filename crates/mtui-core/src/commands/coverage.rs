//! The `coverage` command.

use std::collections::HashSet;

use async_trait::async_trait;
use clap::ArgMatches;
use mtui_config::Config;
use mtui_datasources::http::VerifyPolicy;
use mtui_datasources::refhost::{Attributes, RefhostsFactory, ResolveConfig};
use mtui_datasources::{RefhostError, Refhosts};
use mtui_types::enums::Workflow;
use mtui_types::report_document::{InstallCheck, PlatformRef, ReportDocument, TestPlatform};

use super::support::one_line;
use crate::command::{Command, Scope};
use crate::error::{CommandError, CommandResult};
use crate::session::{Session, match_slots};

/// Reports, per declared test-platform slot, whether an install check covers it.
///
/// A slot is one `install.test_platforms[]` entry and one of its archs. It is
/// `checked` when an install check's `refhost` is a host the refhosts
/// inventory resolves for that slot (the testplatform query `add_host` runs),
/// `no-refhost` when the inventory resolves no host for it, else `owed`. A
/// check counts for at most one slot, as a host serves one slot in `add_host`.
/// Reads the loaded document and the inventory only; no host is contacted.
pub struct Coverage;

/// Where one declared slot stands.
#[derive(Debug, Clone, PartialEq, Eq)]
enum SlotState {
    /// A check from a host the inventory resolves for this slot counts for it.
    Checked { host: String },
    /// The inventory resolves no host for this slot.
    NoRefhost,
    /// Neither: a check is still owed.
    Owed,
    /// Neither, in a workflow that records no install check.
    NotExpected,
    /// The platform's query could not be read back from the document, so
    /// nothing is matched against it.
    Unreadable,
}

impl SlotState {
    fn label(&self) -> &'static str {
        match self {
            Self::Checked { .. } => "checked",
            Self::NoRefhost => "no-refhost",
            Self::Owed => "owed",
            Self::NotExpected => "not expected",
            Self::Unreadable => "unreadable",
        }
    }
}

/// One declared slot: a test platform and one of its archs, or a whole
/// platform whose archs could not be read.
#[derive(Debug, Clone, PartialEq, Eq)]
struct SlotRow {
    platform: String,
    arch: String,
    state: SlotState,
}

/// Why a recorded install check counts for no slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Unmatched {
    /// The inventory could not be read, so no check is matched.
    InventoryUnavailable,
    /// Its `refhost` is not a host in the inventory.
    NotInInventory,
    /// Its host serves none of the declared slots.
    NoDeclaredSlot,
    /// Every slot its host serves is counted for another check.
    SlotTaken,
}

impl std::fmt::Display for Unmatched {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::InventoryUnavailable => "refhosts inventory unavailable",
            Self::NotInInventory => "host not in the refhosts inventory",
            Self::NoDeclaredSlot => "host serves no declared slot",
            Self::SlotTaken => "every slot its host serves is counted for another check",
        })
    }
}

/// A recorded check that counts for no slot.
#[derive(Debug, Clone, PartialEq, Eq)]
struct UnmatchedCheck {
    refhost: String,
    target: String,
    why: Unmatched,
}

/// The coverage of one document's declared slots.
#[derive(Debug, Clone, PartialEq, Eq)]
struct CoverageReport {
    rows: Vec<SlotRow>,
    unmatched: Vec<UnmatchedCheck>,
}

impl CoverageReport {
    fn count(&self, wanted: fn(&SlotState) -> bool) -> usize {
        self.rows.iter().filter(|r| wanted(&r.state)).count()
    }
}

/// `{class} {major} {minor}`, the minor left out when empty.
fn platform_ref_label(p: &PlatformRef) -> String {
    let mut label = format!("{} {}", p.class, p.major);
    if !p.minor.is_empty() {
        label.push(' ');
        label.push_str(&p.minor);
    }
    label
}

fn platform_label(tp: &TestPlatform) -> String {
    let mut label = platform_ref_label(&tp.base);
    if let Some(addon) = &tp.addon {
        label.push_str(" +");
        label.push_str(&platform_ref_label(addon));
    }
    label
}

/// `tp`'s per-arch queries, one per distinct arch, or `None` when parsing its
/// query `line` does not give back the platform's own base, addon and archs:
/// a lost base or addon would match any host, an empty arch any arch.
fn read_platform(tp: &TestPlatform, line: &str) -> Option<Vec<Attributes>> {
    let mut attrs = Attributes::from_testplatform(line);
    let mut want: Vec<&str> = tp.archs.iter().map(String::as_str).collect();
    want.sort_unstable();
    want.dedup();
    let mut got: Vec<&str> = attrs.iter().map(Attributes::arch).collect();
    got.sort_unstable();
    got.dedup();
    let names_read = attrs.iter().all(|a| {
        a.product().is_some_and(|p| p.name == tp.base.class)
            && a.addons()
                .iter()
                .map(|x| x.name.as_str())
                .eq(tp.addon.iter().map(|x| x.class.as_str()))
    });
    if want.contains(&"") || want != got || !names_read {
        return None;
    }
    let mut seen = HashSet::new();
    attrs.retain(|a| seen.insert(a.arch().to_owned()));
    Some(attrs)
}

/// One row of the document's slot list before matching.
enum Slot {
    Readable(String, Attributes),
    Unreadable(String, String),
}

/// Matches `doc`'s declared slots against its recorded install checks through
/// `inventory`; `None` (unreadable) excuses no slot and matches no check. In
/// the kernel workflow an uncovered slot is `not expected` rather than `owed`.
fn assess(
    doc: &ReportDocument,
    inventory: Option<&Refhosts>,
    workflow: Workflow,
) -> CoverageReport {
    let lines = mtui_testreport::testplatform_lines(&doc.install.test_platforms);
    let mut slots: Vec<Slot> = Vec::new();
    for (tp, line) in doc.install.test_platforms.iter().zip(&lines) {
        let label = platform_label(tp);
        match read_platform(tp, line) {
            Some(attrs) => slots.extend(
                attrs
                    .into_iter()
                    .map(|attr| Slot::Readable(label.clone(), attr)),
            ),
            None => slots.push(Slot::Unreadable(label, tp.archs.join(","))),
        }
    }
    let checks: &[InstallCheck] = doc
        .testing
        .install
        .as_ref()
        .map_or(&[], |install| install.checks.as_slice());
    let target_label = |c: &InstallCheck| {
        format!(
            "{} {} {}",
            c.target.product, c.target.version, c.target.arch
        )
    };
    let uncovered = if workflow == Workflow::Kernel {
        SlotState::NotExpected
    } else {
        SlotState::Owed
    };

    let mut hosts: Vec<&String> = Vec::new();
    for check in checks {
        if !hosts.contains(&&check.refhost) {
            hosts.push(&check.refhost);
        }
    }
    // Per slot, the inventory's hosts for it; `None` for an unreadable slot.
    let served: Vec<Option<Vec<String>>> = slots
        .iter()
        .map(|slot| match (slot, inventory) {
            (Slot::Readable(_, attr), Some(store)) => {
                Some(store.search(std::slice::from_ref(attr)))
            }
            _ => None,
        })
        .collect();
    let free: Vec<Vec<&String>> = served
        .iter()
        .map(|names| {
            hosts
                .iter()
                .copied()
                .filter(|h| names.as_ref().is_some_and(|n| n.contains(h)))
                .collect()
        })
        .collect();
    let matched = match_slots(&free, |_, _| false);

    let rows = slots
        .iter()
        .zip(&served)
        .zip(&matched)
        .map(|((slot, names), host)| match slot {
            Slot::Unreadable(platform, archs) => SlotRow {
                platform: platform.clone(),
                arch: archs.clone(),
                state: SlotState::Unreadable,
            },
            Slot::Readable(platform, attr) => {
                let state = match (names, host) {
                    (Some(names), _) if names.is_empty() => SlotState::NoRefhost,
                    (_, Some(host)) => SlotState::Checked {
                        host: (*host).clone(),
                    },
                    _ => uncovered.clone(),
                };
                SlotRow {
                    platform: platform.clone(),
                    arch: attr.arch().to_owned(),
                    state,
                }
            }
        })
        .collect();

    // The first check of a counted host is the one that counts.
    let mut counted: HashSet<&String> = matched.iter().flatten().copied().collect();
    let unmatched = checks
        .iter()
        .filter(|c| !counted.remove(&c.refhost))
        .map(|c| {
            let why = match inventory {
                None => Unmatched::InventoryUnavailable,
                Some(store) if store.host_by_name(&c.refhost).is_none() => {
                    Unmatched::NotInInventory
                }
                Some(_) if served.iter().flatten().any(|n| n.contains(&c.refhost)) => {
                    Unmatched::SlotTaken
                }
                Some(_) => Unmatched::NoDeclaredSlot,
            };
            UnmatchedCheck {
                refhost: c.refhost.clone(),
                target: target_label(c),
                why,
            }
        })
        .collect();
    CoverageReport { rows, unmatched }
}

/// The reply: warnings first, then a summary, the rows, the checks that count
/// for no slot and what the workflow means for them. Every line passes through
/// [`one_line`], as the document and the inventory supply most of its text.
fn render(
    report: &CoverageReport,
    workflow: Workflow,
    inventory_error: Option<&str>,
) -> Vec<String> {
    let mut lines = Vec::new();
    if let Some(err) = inventory_error {
        lines.push(format!(
            "WARNING: refhosts inventory unavailable ({err}): no slot is excused as \
             no-refhost and no check is matched"
        ));
    }
    let unreadable = report.count(|s| *s == SlotState::Unreadable);
    if unreadable > 0 {
        lines.push(format!(
            "WARNING: {unreadable} test platforms could not be read from the document; \
             nothing is matched against them"
        ));
    }
    lines.push(format!(
        "install coverage: {} slots: {} checked, {} owed, {} no-refhost, {} not expected, \
         {unreadable} unreadable; {} unmatched checks",
        report.rows.len(),
        report.count(|s| matches!(s, SlotState::Checked { .. })),
        report.count(|s| *s == SlotState::Owed),
        report.count(|s| *s == SlotState::NoRefhost),
        report.count(|s| *s == SlotState::NotExpected),
        report.unmatched.len()
    ));
    for row in &report.rows {
        let mut line = format!("  {:<12}  {} {}", row.state.label(), row.platform, row.arch);
        if let SlotState::Checked { host } = &row.state {
            line.push_str(&format!(": {host}"));
        }
        lines.push(line);
    }
    if !report.unmatched.is_empty() {
        lines.push("checks counting for no slot:".to_owned());
        for check in &report.unmatched {
            lines.push(format!(
                "  {} ({}): {}",
                check.refhost, check.target, check.why
            ));
        }
    }
    match workflow {
        Workflow::Kernel => {
            lines.push("kernel workflow: export records no install check".to_owned());
        }
        Workflow::Auto => lines.push(
            "auto workflow: openQA install jobs (testing.openqa.install) are not counted \
             against the slots"
                .to_owned(),
        ),
        Workflow::Manual => {}
    }
    lines.iter().map(|l| one_line(l, None)).collect()
}

/// The refhosts inventory, resolved as `list_refhosts` does.
async fn resolve_inventory(config: &Config) -> Result<Refhosts, RefhostError> {
    RefhostsFactory::production(
        config.refhosts_path.clone(),
        VerifyPolicy::from_config(&config.ssl_verify),
    )?
    .resolve(ResolveConfig {
        refhosts_resolvers: &config.refhosts_resolvers,
        refhosts_path: &config.refhosts_path,
        refhosts_https_uri: &config.refhosts_https_uri,
        refhosts_https_expiration: config.refhosts_https_expiration,
    })
    .await
}

#[async_trait]
impl Command for Coverage {
    fn name(&self) -> &'static str {
        "coverage"
    }

    fn about(&self) -> Option<&'static str> {
        Some("Lists which declared test platforms of the report have an install check.")
    }

    fn scope(&self) -> Scope {
        Scope::Fanout
    }

    async fn call(&self, session: &mut Session, _args: &ArgMatches) -> CommandResult {
        let Some(doc) = session.metadata().base().document.clone() else {
            return Err(CommandError::NoDocument);
        };
        if doc.install.test_platforms.is_empty() {
            return Err(CommandError::Other(
                "the report declares no test platforms (install.test_platforms is empty), \
                 so there is no install coverage to report"
                    .to_owned(),
            ));
        }
        let workflow = session.metadata().workflow();
        let config = session.config.clone();
        let inventory = resolve_inventory(&config).await;
        let report = assess(&doc, inventory.as_ref().ok(), workflow);
        let error = inventory.as_ref().err().map(ToString::to_string);
        for line in render(&report, workflow, error.as_deref()) {
            session.display.println(&line);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::testkit::{Buffer, empty_session, matches, session_with_hosts};
    use mtui_types::report_document::{Req, TargetRef, TestingInstall};
    use mtui_types::version::VersionField;
    use mtui_types::{Addon, Host, Product, Version};

    const RRID: &str = "SUSE:Maintenance:1:1";

    /// The shared `refhosts.yml` fixture: `sles` 15.5 hosts on x86_64 (one
    /// with the `sdk` 15.5 addon), aarch64 and ppc64le, and a `sles` 12 sp4
    /// x86_64 host.
    fn fixture_inventory() -> Refhosts {
        Refhosts::from_hosts(
            mtui_types::load_refhosts(mtui_types::fixtures::REFHOSTS_YML).expect("fixture parses"),
        )
    }

    /// `SLES 15-SP5` x86_64 hosts carrying the `SLES-LTSS 15-SP5` addon, the
    /// extension-on-a-SLES-base shape `Refhosts::product_satisfied` documents
    /// and `session.rs`'s `hpc_ltss_store` builds.
    fn ltss_inventory(names: &[&str]) -> Refhosts {
        let sp5 = || Some(Version::new(15, Some(VersionField::Text("SP5".to_owned()))));
        Refhosts::from_hosts(
            names
                .iter()
                .map(|name| Host {
                    name: (*name).to_owned(),
                    arch: "x86_64".to_owned(),
                    product: Product {
                        name: "SLES".to_owned(),
                        version: sp5(),
                    },
                    addons: vec![Addon {
                        name: "SLES-LTSS".to_owned(),
                        version: sp5(),
                    }],
                })
                .collect(),
        )
    }

    fn pref(class: &str, major: &str, minor: &str) -> PlatformRef {
        PlatformRef {
            class: class.to_owned(),
            major: major.to_owned(),
            minor: minor.to_owned(),
        }
    }

    fn platform(base: PlatformRef, addon: Option<PlatformRef>, archs: &[&str]) -> TestPlatform {
        TestPlatform {
            base,
            addon,
            archs: archs.iter().map(|a| (*a).to_owned()).collect(),
        }
    }

    /// `sles 15 5` on `archs`, the fixture inventory's main platform.
    fn sles155(archs: &[&str]) -> TestPlatform {
        platform(pref("sles", "15", "5"), None, archs)
    }

    /// `SLES-LTSS 15 SP5` x86_64, `session.rs`'s `LTSS_TP`.
    fn ltss_sp5() -> TestPlatform {
        platform(pref("SLES-LTSS", "15", "SP5"), None, &["x86_64"])
    }

    /// `doc_raw` with its test platforms replaced and `checks` recorded.
    fn doc_with(
        doc_raw: &str,
        platforms: Vec<TestPlatform>,
        checks: Vec<InstallCheck>,
    ) -> ReportDocument {
        let mut d: ReportDocument = doc_raw.parse().expect("fixture parses");
        d.install.test_platforms = platforms;
        d.testing.install = Some(TestingInstall {
            verdict: Req(None),
            checks,
            comment: Req(None),
        });
        d
    }

    fn obs_doc(platforms: Vec<TestPlatform>, checks: Vec<InstallCheck>) -> ReportDocument {
        doc_with(mtui_types::fixtures::MAINTENANCE_OBS, platforms, checks)
    }

    fn check_on(refhost: &str, version: &str) -> InstallCheck {
        InstallCheck {
            target: TargetRef {
                product: "SLE-Product-SLES".to_owned(),
                version: version.to_owned(),
                arch: "x86_64".to_owned(),
            },
            refhost: refhost.to_owned(),
            verdict: Req(None),
            before: Default::default(),
            after: Default::default(),
        }
    }

    fn check(refhost: &str) -> InstallCheck {
        check_on(refhost, "15-SP5")
    }

    fn labels(report: &CoverageReport) -> Vec<(&str, &str)> {
        report
            .rows
            .iter()
            .map(|r| (r.arch.as_str(), r.state.label()))
            .collect()
    }

    /// Mutation: matching the check by its `target` product/version instead of
    /// the inventory leaves this slot `owed`, since `SLE-Product-SLES` is not
    /// `SLES-LTSS`.
    #[test]
    fn ltss_platform_is_checked_by_a_sles_host_carrying_the_addon() {
        let d = obs_doc(vec![ltss_sp5()], vec![check("ltss-a")]);
        let report = assess(&d, Some(&ltss_inventory(&["ltss-a"])), Workflow::Manual);
        assert_eq!(
            report.rows,
            vec![SlotRow {
                platform: "SLES-LTSS 15 SP5".to_owned(),
                arch: "x86_64".to_owned(),
                state: SlotState::Checked {
                    host: "ltss-a".to_owned()
                },
            }]
        );
        assert!(report.unmatched.is_empty(), "{:?}", report.unmatched);
    }

    /// The addon is part of the slot: a host without it serves no declared
    /// slot, one with it checks the slot.
    #[test]
    fn platform_with_an_addon_needs_a_host_carrying_it() {
        let tp = platform(
            pref("sles", "15", "5"),
            Some(pref("sdk", "15", "5")),
            &["x86_64"],
        );
        let store = fixture_inventory();
        let doc = |host: &str| {
            doc_with(
                mtui_types::fixtures::MAINTENANCE_ADDON,
                vec![tp.clone()],
                vec![check(host)],
            )
        };

        let plain = assess(&doc("host-nbg-x86"), Some(&store), Workflow::Manual);
        assert_eq!(plain.rows[0].platform, "sles 15 5 +sdk 15 5");
        assert_eq!(labels(&plain), vec![("x86_64", "owed")]);
        assert_eq!(plain.unmatched[0].why, Unmatched::NoDeclaredSlot);

        let sdk = assess(&doc("host-default-x86"), Some(&store), Workflow::Manual);
        assert_eq!(
            sdk.rows[0].state,
            SlotState::Checked {
                host: "host-default-x86".to_owned()
            }
        );
    }

    /// Per arch: the arch no inventory host serves is `no-refhost`, the served
    /// one with no check is `owed`.
    #[test]
    fn slot_with_no_inventory_host_is_no_refhost() {
        let d = obs_doc(vec![sles155(&["s390x", "x86_64"])], Vec::new());
        let report = assess(&d, Some(&fixture_inventory()), Workflow::Manual);
        assert_eq!(
            labels(&report),
            vec![("s390x", "no-refhost"), ("x86_64", "owed")]
        );
    }

    /// A check from a host the inventory does not know counts for no slot.
    #[test]
    fn check_from_a_host_outside_the_inventory_is_unmatched() {
        let d = obs_doc(vec![sles155(&["x86_64"])], vec![check("stray.example.com")]);
        let report = assess(&d, Some(&fixture_inventory()), Workflow::Manual);
        assert_eq!(report.rows[0].state, SlotState::Owed);
        assert_eq!(
            report.unmatched,
            vec![UnmatchedCheck {
                refhost: "stray.example.com".to_owned(),
                target: "SLE-Product-SLES 15-SP5 x86_64".to_owned(),
                why: Unmatched::NotInInventory,
            }]
        );
    }

    /// One host serving two declared slots counts for one; its second check
    /// counts for none, and the first is the one that counts.
    #[test]
    fn a_check_counts_for_at_most_one_slot() {
        let sles = platform(pref("SLES", "15", "SP5"), None, &["x86_64"]);
        let platforms = vec![ltss_sp5(), sles];
        let store = ltss_inventory(&["ltss-a", "ltss-b"]);

        let one = assess(
            &obs_doc(platforms.clone(), vec![check("ltss-a")]),
            Some(&store),
            Workflow::Manual,
        );
        assert_eq!(
            labels(&one),
            vec![("x86_64", "checked"), ("x86_64", "owed")]
        );
        assert!(one.unmatched.is_empty());

        let repeated = assess(
            &obs_doc(
                platforms.clone(),
                vec![check("ltss-a"), check_on("ltss-a", "15-SP5-LTSS")],
            ),
            Some(&store),
            Workflow::Manual,
        );
        assert_eq!(
            labels(&repeated),
            vec![("x86_64", "checked"), ("x86_64", "owed")]
        );
        assert_eq!(
            repeated.unmatched,
            vec![UnmatchedCheck {
                refhost: "ltss-a".to_owned(),
                target: "SLE-Product-SLES 15-SP5-LTSS x86_64".to_owned(),
                why: Unmatched::SlotTaken,
            }]
        );

        let both = assess(
            &obs_doc(platforms, vec![check("ltss-a"), check("ltss-b")]),
            Some(&store),
            Workflow::Manual,
        );
        assert_eq!(
            labels(&both),
            vec![("x86_64", "checked"), ("x86_64", "checked")]
        );
    }

    /// Mutation: dropping the `SlotTaken` arm reports a host serving a
    /// declared slot as serving none.
    #[test]
    fn redundant_check_for_a_covered_slot_names_the_slot_taken() {
        let d = obs_doc(
            vec![sles155(&["x86_64"])],
            vec![check("host-default-x86"), check("host-nbg-x86")],
        );
        let report = assess(&d, Some(&fixture_inventory()), Workflow::Manual);
        assert_eq!(report.unmatched.len(), 1);
        assert_eq!(report.unmatched[0].refhost, "host-nbg-x86");
        assert_eq!(report.unmatched[0].why, Unmatched::SlotTaken);
    }

    /// A repeated arch is one slot, not two.
    #[test]
    fn a_repeated_arch_is_one_slot() {
        let d = obs_doc(vec![sles155(&["x86_64", "x86_64"])], Vec::new());
        let report = assess(&d, Some(&fixture_inventory()), Workflow::Manual);
        assert_eq!(labels(&report), vec![("x86_64", "owed")]);
    }

    /// An unreadable inventory: every slot owed, none excused, every check
    /// unmatched for that reason.
    #[test]
    fn unreadable_inventory_excuses_no_slot_and_matches_no_check() {
        let d = obs_doc(
            vec![sles155(&["s390x", "x86_64"])],
            vec![check("host-nbg-x86")],
        );
        let report = assess(&d, None, Workflow::Manual);
        assert_eq!(labels(&report), vec![("s390x", "owed"), ("x86_64", "owed")]);
        assert_eq!(report.unmatched.len(), 1);
        assert_eq!(report.unmatched[0].why, Unmatched::InventoryUnavailable);
    }

    /// A platform with no arch matches nothing and is not dropped: a document
    /// declaring only such platforms never reads as nothing owed.
    #[test]
    fn platform_without_archs_is_an_unreadable_row() {
        let blank = assess(
            &obs_doc(vec![sles155(&[""])], vec![check("host-nbg-x86")]),
            Some(&fixture_inventory()),
            Workflow::Manual,
        );
        assert_eq!(labels(&blank), vec![("", "unreadable")]);
        let d = obs_doc(vec![sles155(&[])], vec![check("host-nbg-x86")]);
        let report = assess(&d, Some(&fixture_inventory()), Workflow::Manual);
        assert_eq!(
            report.rows,
            vec![SlotRow {
                platform: "sles 15 5".to_owned(),
                arch: String::new(),
                state: SlotState::Unreadable,
            }]
        );
        let out = render(&report, Workflow::Manual, None);
        assert_eq!(
            out[0],
            "WARNING: 1 test platforms could not be read from the document; nothing is \
             matched against them"
        );
        assert!(out[1].contains(" 1 unreadable;"), "{out:?}");
    }

    /// A base that does not survive the query (here a `;` splits the segment)
    /// would query any product: the slot must be unreadable, not `checked` by
    /// whatever host of that arch has a check.
    #[test]
    fn unparseable_base_matches_nothing() {
        let tp = platform(pref("sles;x", "15", "5"), None, &["x86_64"]);
        let d = obs_doc(vec![tp], vec![check("host-nbg-x86")]);
        let report = assess(&d, Some(&fixture_inventory()), Workflow::Manual);
        assert_eq!(labels(&report), vec![("x86_64", "unreadable")]);
        assert_eq!(report.unmatched[0].why, Unmatched::NoDeclaredSlot);
    }

    /// Same for an addon lost from the query, which would drop the constraint.
    #[test]
    fn unparseable_addon_matches_nothing() {
        let tp = platform(
            pref("sles", "15", "5"),
            Some(pref("sdk;x", "15", "5")),
            &["x86_64"],
        );
        let d = obs_doc(vec![tp], vec![check("host-default-x86")]);
        let report = assess(&d, Some(&fixture_inventory()), Workflow::Manual);
        assert_eq!(labels(&report), vec![("x86_64", "unreadable")]);
    }

    /// In the kernel workflow an uncovered slot is not owed, and the summary
    /// does not count it as owed; a slot without a host stays `no-refhost`.
    #[test]
    fn kernel_workflow_owes_no_slot() {
        let d = obs_doc(vec![sles155(&["s390x", "x86_64"])], Vec::new());
        let report = assess(&d, Some(&fixture_inventory()), Workflow::Kernel);
        assert_eq!(
            labels(&report),
            vec![("s390x", "no-refhost"), ("x86_64", "not expected")]
        );
        let out = render(&report, Workflow::Kernel, None);
        assert!(
            out[0].starts_with(
                "install coverage: 2 slots: 0 checked, 0 owed, 1 no-refhost, 1 not expected,"
            ),
            "{out:?}"
        );
        assert_eq!(
            out.last().map(String::as_str),
            Some("kernel workflow: export records no install check")
        );
    }

    /// SLFO: `SLES_SAP 16 0` has no host in the shared inventory fixture.
    #[test]
    fn slfo_document_reads_its_platform() {
        let d: ReportDocument = mtui_types::fixtures::SLFO_GITEA.parse().unwrap();
        let report = assess(&d, Some(&fixture_inventory()), Workflow::Manual);
        assert_eq!(report.rows[0].platform, "SLES_SAP 16 0");
        assert_eq!(
            labels(&report),
            vec![("ppc64le", "no-refhost"), ("x86_64", "no-refhost")]
        );
    }

    /// Every state and unmatched reason over the shared inventory fixture.
    #[test]
    fn render_snapshot() {
        let d = obs_doc(
            vec![
                sles155(&["aarch64", "ppc64le", "s390x", "x86_64"]),
                platform(pref("sles", "12", "sp4"), None, &["x86_64"]),
                platform(pref("sles;x", "15", "5"), None, &["x86_64"]),
            ],
            vec![
                check("host-nbg-x86"),
                check_on("host-default-noaddon", "12-SP4"),
                check("host-default-x86"),
                check("stray.example.com"),
            ],
        );
        let report = assess(&d, Some(&fixture_inventory()), Workflow::Auto);
        insta::assert_snapshot!(render(&report, Workflow::Auto, None).join("\n"));
    }

    /// Document text cannot break a line of the reply.
    #[test]
    fn render_keeps_document_text_on_one_line() {
        let d = obs_doc(vec![sles155(&["x86_64"])], vec![check("stray\nhost")]);
        let report = assess(&d, Some(&fixture_inventory()), Workflow::Manual);
        let out = render(&report, Workflow::Manual, None);
        assert!(out.iter().all(|l| !l.contains('\n')), "{out:?}");
        assert!(
            out.iter().any(|l| l.starts_with("  stray host (")),
            "{out:?}"
        );
    }

    #[test]
    fn render_puts_the_inventory_warning_first() {
        let report = assess(
            &obs_doc(vec![sles155(&["x86_64"])], Vec::new()),
            None,
            Workflow::Manual,
        );
        let out = render(&report, Workflow::Manual, Some("boom"));
        assert!(
            out[0].starts_with("WARNING: refhosts inventory unavailable (boom)"),
            "{out:?}"
        );
    }

    #[test]
    fn name_and_fanout_scope() {
        assert_eq!(Coverage.name(), "coverage");
        assert_eq!(Coverage.scope(), Scope::Fanout);
    }

    #[tokio::test]
    async fn no_document_is_an_error() {
        let (mut session, buf) = empty_session();
        let err = Coverage
            .call(&mut session, &matches(&Coverage, &[]))
            .await
            .unwrap_err();
        assert!(matches!(err, CommandError::NoDocument), "{err}");
        assert!(buf.contents().is_empty());
    }

    /// PI documents declare no test platforms: never "nothing owed".
    #[tokio::test]
    async fn document_without_test_platforms_is_an_error() {
        let (mut session, _buf) = session_with_hosts(RRID, &[], "ok");
        session.metadata_mut().base_mut().document =
            Some(mtui_types::fixtures::PI.parse().unwrap());
        let err = Coverage
            .call(&mut session, &matches(&Coverage, &[]))
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("declares no test platforms"),
            "{err}"
        );
    }

    fn session_with_inventory(present: bool) -> (tempfile::TempDir, Session, Buffer) {
        let dir = tempfile::tempdir().unwrap();
        let (mut session, buf) = session_with_hosts(RRID, &[], "ok");
        let path = dir.path().join("refhosts.yml");
        if present {
            std::fs::write(&path, mtui_types::fixtures::REFHOSTS_YML).unwrap();
        }
        session.config.refhosts_resolvers = "path".to_owned();
        session.config.refhosts_path = path;
        (dir, session, buf)
    }

    #[tokio::test]
    async fn reads_the_inventory_and_reports_the_checked_slot() {
        let (_dir, mut session, buf) = session_with_inventory(true);
        session.metadata_mut().base_mut().document = Some(obs_doc(
            vec![sles155(&["x86_64"])],
            vec![check("host-nbg-x86")],
        ));
        Coverage
            .call(&mut session, &matches(&Coverage, &[]))
            .await
            .unwrap();
        let out = buf.contents();
        assert!(
            out.contains("checked       sles 15 5 x86_64: host-nbg-x86"),
            "{out}"
        );
    }

    #[tokio::test]
    async fn unreadable_inventory_is_named_first_and_every_slot_owed() {
        let (_dir, mut session, buf) = session_with_inventory(false);
        session.metadata_mut().base_mut().document = Some(obs_doc(
            vec![sles155(&["x86_64"])],
            vec![check("host-nbg-x86")],
        ));
        Coverage
            .call(&mut session, &matches(&Coverage, &[]))
            .await
            .unwrap();
        let out = buf.contents();
        let first = out.lines().next().unwrap_or_default();
        assert!(
            first.starts_with("WARNING: refhosts inventory unavailable ("),
            "{out}"
        );
        assert!(out.contains("owed          sles 15 5 x86_64"), "{out}");
        assert!(
            out.contains(
                "host-nbg-x86 (SLE-Product-SLES 15-SP5 x86_64): refhosts inventory unavailable"
            ),
            "{out}"
        );
    }

    #[tokio::test]
    async fn kernel_workflow_reads_the_session_workflow() {
        let (_dir, mut session, buf) = session_with_inventory(true);
        session.metadata_mut().base_mut().document =
            Some(obs_doc(vec![sles155(&["x86_64"])], Vec::new()));
        session.metadata_mut().base_mut().workflow = Workflow::Kernel;
        Coverage
            .call(&mut session, &matches(&Coverage, &[]))
            .await
            .unwrap();
        let out = buf.contents();
        assert!(out.contains("not expected  sles 15 5 x86_64"), "{out}");
        assert_eq!(
            out.lines().last(),
            Some("kernel workflow: export records no install check"),
            "{out}"
        );
    }
}
