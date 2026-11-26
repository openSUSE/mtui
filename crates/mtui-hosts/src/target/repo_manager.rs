//! Per-target zypper repository manager.
//!
//! [`RepoManager`] owns the per-target zypper repository lifecycle, keeping [`Target`]
//! focused on the connection/lock skeleton: `set` forwards into
//! `testreport.set_repo(target, operation)`, and
//! [`run_zypper`](RepoManager::run_zypper) runs the `ar`/`rr` loop over the target's
//! flattened system and finishes with a `zypper ref` (via `transactional-update` on
//! read-only-root hosts). An unknown command triggers the force-unlock safeguard.
//!
//! Unlike the read-only `Reporter`, `RepoManager` borrows `&mut Target`, because it
//! issues commands and may force-unlock ([`Target::unlock`]). Obtain one via
//! [`Target::repo_manager`].
//!
//! The concrete test-report types live in `mtui-testreport`, a higher crate, so `set`
//! dispatches through the object-safe [`SetRepo`] trait defined here to keep the crate
//! graph acyclic. The `impl SetRepo for SlReport` (and the other report types) are
//! driven via [`HostsGroup::fanout_set_repo`](super::HostsGroup), as
//! [`operation`](super::operation) does with the
//! [`OperationGroup`](super::operation::OperationGroup) seam.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use mtui_types::rrid::RequestReviewID;
use mtui_types::shellquote::quote_args;
use mtui_types::system::SystemProduct;
use mtui_types::url_redaction::sanitize_url;
use tracing::{debug, info, warn};

use super::Target;

/// Which repository change a `set` forwards: add or remove.
///
/// A two-variant enum keeps the seam typed; `as_str` renders the wire token for a
/// [`SetRepo`] implementer that wants it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepoOp {
    /// Add the update's repositories (wire token `"add"`).
    Add,
    /// Remove the update's repositories (wire token `"remove"`).
    Remove,
}

impl RepoOp {
    /// The wire token for this operation (`"add"` / `"remove"`).
    #[must_use]
    const fn as_str(self) -> &'static str {
        match self {
            Self::Add => "add",
            Self::Remove => "remove",
        }
    }
}

/// What a [`RepoManager::run_zypper`] fan-out that did not fail did on one
/// host — see [`Target::last_repo`](super::Target::last_repo).
///
/// `Display` renders the line `set_repo` prints after the host's name, and the
/// reason the update flows give for leaving out a host an add would not reach.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RepoOutcome {
    /// Every repository of the update that the host carries a product for was
    /// changed. Also recorded for a remove that matched nothing: there was
    /// nothing to remove.
    Done,
    /// An add whose repository map names none of the host's products, so no
    /// repository was added.
    NoMatch {
        /// The host's installed products.
        products: BTreeSet<SystemProduct>,
    },
    /// An add whose repository map is empty, so no repository was added.
    NoRepositories,
    /// An add on a host none of whose products has a version or arch (its
    /// system was never read, or its product files are missing), so no
    /// repository could match it.
    ProductsUnread,
}

impl std::fmt::Display for RepoOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Done => f.write_str("ok"),
            Self::NoMatch { products } => {
                // An unparsed product has no version or arch to render.
                let products: Vec<String> = products
                    .iter()
                    .map(|p| {
                        if p.version.is_empty() && p.arch.is_empty() {
                            p.name.clone()
                        } else {
                            p.to_string()
                        }
                    })
                    .collect();
                write!(
                    f,
                    "no repository of this update matches the host's products ({})",
                    products.join(", ")
                )
            }
            Self::NoRepositories => f.write_str("this update has no repository to add"),
            Self::ProductsUnread => f.write_str("its products could not be read"),
        }
    }
}

impl RepoOutcome {
    /// The repositories of `repos` an add registers on a host carrying
    /// `products`.
    ///
    /// The one matching rule: [`RepoManager::run_zypper`] changes exactly these
    /// repositories and records the outcome this returns, so a caller can tell
    /// before the add which hosts it would not reach.
    ///
    /// # Errors
    ///
    /// The outcome an add records when it registers no repository there:
    /// [`NoRepositories`](Self::NoRepositories),
    /// [`ProductsUnread`](Self::ProductsUnread) or
    /// [`NoMatch`](Self::NoMatch).
    pub fn reach<'a>(
        repos: impl IntoIterator<Item = (&'a SystemProduct, &'a String)>,
        products: &BTreeSet<SystemProduct>,
    ) -> Result<Vec<(&'a SystemProduct, &'a String)>, Self> {
        let mut any = false;
        let matched: Vec<_> = repos
            .into_iter()
            .inspect(|_| any = true)
            .filter(|(product, _)| products.contains(*product))
            .collect();
        if !matched.is_empty() {
            Ok(matched)
        } else if !any {
            Err(Self::NoRepositories)
        } else if products
            .iter()
            .all(|p| p.version.is_empty() && p.arch.is_empty())
        {
            Err(Self::ProductsUnread)
        } else {
            Err(Self::NoMatch {
                products: products.clone(),
            })
        }
    }
}

/// Why a [`RepoManager::run_zypper`] fan-out failed on one host.
///
/// [`run_zypper`](RepoManager::run_zypper) drives several `zypper` commands per host
/// and records the *first* failure as the run's verdict (see
/// [`Target::last_repo`](super::Target::last_repo)). Consumers route on the variant;
/// `Display` renders the wording `set_repo` prints.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RepoFailure {
    /// `zypper ar` did not register the alias.
    Add {
        /// The repo alias that failed to register.
        alias: String,
    },
    /// `zypper rr` did not remove the URL.
    Remove {
        /// The repo URL that failed to be removed, with any userinfo masked.
        url: String,
    },
    /// The trailing `zypper ref` (or its `transactional-update` wrapper)
    /// failed.
    Refresh {
        /// The refresh command that failed.
        command: String,
    },
    /// `run_zypper` was asked to drive a `cmd` that is neither an `ar` nor an
    /// `rr` — the safeguard path, which also force-unlocks the target.
    UnknownCommand {
        /// The unrecognised sub-command.
        command: String,
    },
    /// The host's root mode is undecided ([`Target::root_mode_undecided`]), so
    /// no command was run on it.
    RootModeUnknown,
}

impl std::fmt::Display for RepoFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Add { alias } => write!(f, "adding repo {alias} failed"),
            Self::Remove { url } => write!(f, "removing repo {} failed", sanitize_url(url)),
            Self::Refresh { command } => write!(f, "refreshing repos failed: {command}"),
            Self::UnknownCommand { command } => {
                write!(f, "unknown zypper sub-command: {command}")
            }
            Self::RootModeUnknown => write!(
                f,
                "transactional-update.conf but root mode unknown; no repo command run"
            ),
        }
    }
}

/// The report-side hook a `RepoManager::set` forwards into
/// (`testreport.set_repo(target, operation)`), and the seam the update flow reads the
/// report's composition and repository map through.
///
/// Object-safe and `async`, as with
/// [`OperationGroup`](super::operation::OperationGroup). The concrete
/// implementations (`SlReport`, `ObsReport`, …) live in `mtui-testreport`, which keeps
/// the crate graph acyclic.
#[async_trait::async_trait]
pub trait SetRepo: Send + Sync {
    /// Add or remove this report's repositories on `target`.
    async fn set_repo(&self, target: &mut Target, operation: RepoOp);

    /// The package names this update composes, per product and architecture.
    ///
    /// Borrowed, not cloned: a prepare reads the index once and a large
    /// update's is the whole binaries block.
    ///
    /// `None` by default, and a report whose metadata carries no composition
    /// answers `None` or an empty map; a consumer reading either must treat
    /// every package as composed for every host.
    fn composition(&self) -> Option<&HashMap<SystemProduct, BTreeSet<String>>> {
        None
    }

    /// The repository URL an add registers, per product.
    ///
    /// `None` by default, and a consumer must then judge no host by it. A
    /// report whose `set_repo` adds from such a map must return it: `update`
    /// reads it to leave out, before anything runs, a host the add would not
    /// reach.
    fn repositories(&self) -> Option<&HashMap<SystemProduct, String>> {
        None
    }
}

/// Adapter that owns the per-target zypper-repo lifecycle for one [`Target`].
///
/// Obtain one via [`Target::repo_manager`]; it borrows the target mutably so it
/// can issue commands and, on the safeguard path, force-unlock.
pub struct RepoManager<'a> {
    target: &'a mut Target,
}

impl<'a> RepoManager<'a> {
    /// Binds a repo manager to `target`.
    ///
    /// Prefer [`Target::repo_manager`] over calling this directly.
    #[must_use]
    pub(super) fn new(target: &'a mut Target) -> Self {
        Self { target }
    }

    /// Asks `report` to add or remove repos on the bound target via
    /// [`SetRepo::set_repo`], logging at debug.
    pub(crate) async fn set(&mut self, operation: RepoOp, report: &dyn SetRepo) {
        debug!(host = %self.target.hostname(), op = operation.as_str(), "changing repos");
        report.set_repo(self.target, operation).await;
    }

    /// Runs a fan-out `zypper` command across the target's repos.
    ///
    /// For each product/repo pair [`RepoOutcome::reach`] matches to the target's
    /// flattened system, `cmd` drives one of:
    ///
    /// * **`ar`** (`cmd` contains `"ar"`) → `zypper <cmd> <alias> <url> <alias>` with
    ///   alias `issue-<name>:<version>:p=<maintenance_id>:<review_id>`. A non-zero exit
    ///   does not by itself mean the repo is absent (zypper also exits non-zero on an
    ///   already-registered alias), so a `zypper -n lr <alias>` probe decides: `0` is a
    ///   benign collision (WARNING only), non-zero a recorded failure.
    /// * **`rr`** (`cmd` contains `"rr"`) → `zypper <cmd> <url>`. zypper exits 0 when the
    ///   repo was never registered, so a non-zero exit is always a genuine failure:
    ///   warned and recorded.
    /// * **anything else** → force-unlock the target ([`Target::unlock`] with
    ///   `force = true`) and return [`false`] as a safeguard against an unrecognised
    ///   command.
    ///
    /// The metadata-derived `alias` and `url` are shell-quoted at this exec boundary
    /// (they run as root); URLs are additionally validated at ingestion (see
    /// `repoparse`).
    ///
    /// When no repo product matches the host (`matched == 0`) a WARNING lists the
    /// update's products vs the host's, and an add records the outcome
    /// [`RepoOutcome::reach`] gives, even if the refresh then fails (its own WARNING still
    /// fires).
    ///
    /// Every path but the two bails finishes with a refresh: `zypper -n ref`, or
    /// `transactional-update --non-interactive run zypper --gpg-auto-import-keys -n ref`
    /// on a read-only-root host, where a plain refresh of a not-yet-trusted signed repo
    /// would fail importing the key into the read-only rpmdb.
    ///
    /// Returns `true` normally and `false` when it bails *without* refreshing: on the
    /// unknown-cmd safeguard, and on a host whose root mode is undecided
    /// ([`Target::root_mode_undecided`]), where it runs no command and records
    /// [`RepoFailure::RootModeUnknown`].
    pub async fn run_zypper(
        &mut self,
        cmd: &str,
        repos: &BTreeMap<SystemProduct, String>,
        rrid: &RequestReviewID,
    ) -> bool {
        let is_ar = cmd.contains("ar");
        let is_rr = cmd.contains("rr");

        // Neither refresh form is safe on a root that may be read-write or not.
        if self.target.root_mode_undecided() {
            warn!(
                host = %self.target.hostname(),
                "transactional-update.conf but root mode unknown; not touching repos"
            );
            self.target.set_last_repo(Err(RepoFailure::RootModeUnknown));
            return false;
        }

        // Aggregate the run's verdict for `Target::last_repo`: any non-zero zypper exit
        // (a failed `ar`/`rr`, or the trailing `ref`) is a per-host failure, though only
        // the last command lands in the `HostLog`.
        let mut failure: Option<RepoFailure> = None;

        // Snapshot the flattened system so its borrow does not overlap the mutable calls
        // below.
        let flattened = self.target.system().flatten();
        let hostname = self.target.hostname().to_owned();

        // Products the host actually carries, paired with their repo URL, in the
        // deterministic order of the BTreeMap.
        let reach = RepoOutcome::reach(repos, &flattened);
        let matched_repos = reach.as_deref().unwrap_or_default();

        if matched_repos.is_empty() {
            // An unknown cmd with no matches takes no per-pair branch (the safeguard lives in
            // the loop), so it is a silent no-op beyond this warning + refresh.
            let op = if is_ar {
                "add"
            } else if is_rr {
                "remove"
            } else {
                cmd
            };
            let want: Vec<String> = {
                let mut v: Vec<String> = repos.keys().map(ToString::to_string).collect();
                v.sort();
                v
            };
            let have: Vec<String> = {
                let mut v: Vec<String> = flattened.iter().map(ToString::to_string).collect();
                v.sort();
                v
            };
            warn!(
                "set_repo {op} on {hostname} did nothing: none of the update's \
                 products {want:?} match the host's installed products {have:?}",
            );
        }

        for &(product, url) in matched_repos {
            // Only the executed argument keeps the userinfo; every log line and
            // failure value below carries the masked form.
            let safe_url = sanitize_url(url);
            if is_ar {
                let alias = issue_alias(product, rrid);
                info!("Adding repo {safe_url} on {hostname}");
                // Quote the metadata-derived alias/url so a crafted value cannot
                // break out of its argument into the root command line.
                let args = quote_args(&[alias.as_str(), url.as_str(), alias.as_str()]);
                self.target.run(&format!("zypper {cmd} {args}")).await;
                // Surface a failed add instead of returning silent success.
                if self.target.lastexit() != Some(0) {
                    let err = last_error_line(self.target);
                    warn!(
                        "adding repo {alias} on {hostname} failed: zypper exited {}{}",
                        exit_display(self.target.lastexit()),
                        err,
                    );
                    // A bare non-zero `ar` cannot tell "not added" from "already there" (zypper's
                    // RepoAlreadyExistsException also exits non-zero, and a `update --noprepare` retry
                    // re-runs `ar` on a correctly registered host), so probe `zypper -n lr <alias>`
                    // before calling it a failure.
                    let probe_args = quote_args(&[alias.as_str()]);
                    self.target.run(&format!("zypper -n lr {probe_args}")).await;
                    if self.target.lastexit() == Some(0) {
                        warn!(
                            "repo {alias} on {hostname} is already registered; \
                             treating the failed add as a benign collision"
                        );
                    } else {
                        failure.get_or_insert_with(|| RepoFailure::Add {
                            alias: alias.clone(),
                        });
                    }
                }
            } else if is_rr {
                info!("Removing repo {safe_url} on {hostname}");
                let args = quote_args(&[url.as_str()]);
                self.target.run(&format!("zypper {cmd} {args}")).await;
                // Unlike `ar`, a non-zero `rr` is never benign: zypper exits 0 when the repo was
                // not registered.
                if self.target.lastexit() != Some(0) {
                    let err = last_error_line(self.target);
                    warn!(
                        "removing repo {safe_url} on {hostname} failed: zypper exited {}{}",
                        exit_display(self.target.lastexit()),
                        err,
                    );
                    failure.get_or_insert(RepoFailure::Remove { url: safe_url });
                }
            } else {
                // Unknown sub-command: force-unlock the target and bail with `false` as the
                // safeguard.
                warn!(
                    host = %hostname, %cmd,
                    "unknown zypper sub-command; force-unlocking target and bailing"
                );
                self.target.unlock(true).await;
                self.target.set_last_repo(Err(RepoFailure::UnknownCommand {
                    command: cmd.to_owned(),
                }));
                return false;
            }
        }

        // Refresh repo metadata. A transactional host must go through
        // transactional-update so zypper gets a writable rpmdb in a snapshot (activated by
        // the reboot the prepare/update flow performs) and can import an untrusted repo
        // key non-interactively. No `--continue`: it branches off the booted snapshot,
        // dropping any staged leftover.
        let ref_cmd = if self.target.transactional() {
            "transactional-update --non-interactive run \
             zypper --gpg-auto-import-keys -n ref"
        } else {
            "zypper -n ref"
        };
        self.target.run(ref_cmd).await;
        // Surface a failed refresh instead of returning silent success.
        if self.target.lastexit() != Some(0) {
            let err = last_error_line(self.target);
            warn!(
                "refreshing repos on {hostname} failed: {ref_cmd} exited {}{}",
                exit_display(self.target.lastexit()),
                err,
            );
            failure.get_or_insert_with(|| RepoFailure::Refresh {
                command: ref_cmd.to_owned(),
            });
        }

        // With nothing matched no repository was touched, so a failed refresh
        // (the only failure possible then) is not what the caller must hear.
        let untouched = if is_ar {
            reach.as_ref().err().cloned()
        } else {
            None
        };
        let recorded = match untouched {
            Some(outcome) => Ok(outcome),
            None => failure.map_or(Ok(RepoOutcome::Done), Err),
        };
        self.target.set_last_repo(recorded);
        true
    }
}

/// Builds the `issue-<name>:<version>:p=<maintenance_id>:<review_id>` repo
/// alias — the format other tools on the fleet also produce, so a repo
/// registered by one is recognised by another.
fn issue_alias(product: &SystemProduct, rrid: &RequestReviewID) -> String {
    format!(
        "issue-{}:{}:p={}:{}",
        product.name, product.version, rrid.maintenance_id, rrid.review_id
    )
}

/// Formats the last exit code for a log line: the numeric code, or `?` when the
/// log is empty.
fn exit_display(code: Option<i16>) -> String {
    code.map_or_else(|| "?".to_owned(), |c| c.to_string())
}

/// Extracts the trailing "( <last stderr/stdout line> )" suffix for a failure
/// warning. Prefers stderr,
/// falls back to stdout; empty output yields an empty suffix.
fn last_error_line(target: &Target) -> String {
    let err = target.lasterr().trim();
    let src = if err.is_empty() {
        target.lastout().trim()
    } else {
        err
    };
    match src.lines().next_back() {
        Some(line) if !src.is_empty() => format!(" ({line})"),
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    //! Exercises a real
    //! [`Target`] over a [`MockConnection`], asserting the exact `zypper`
    //! command strings, the alias format, the matched==0 warning, the
    //! unknown-cmd force-unlock safeguard, and the transactional refresh path.

    use std::collections::BTreeMap;
    use std::sync::{Arc, Mutex};

    use mtui_types::enums::{RequestKind, TargetState};
    use mtui_types::rrid::RequestReviewID;
    use mtui_types::system::{System, SystemProduct};

    use super::*;
    use crate::connection::MockConnection;

    /// An `RRID` fixture: `SUSE:Maintenance:1:2`.
    fn rrid() -> RequestReviewID {
        RequestReviewID {
            project: "SUSE".to_owned(),
            kind: RequestKind::Maintenance,
            maintenance_id: "1".to_owned(),
            review_id: 2,
        }
    }

    /// A `SystemProduct` shorthand.
    fn product(name: &str, version: &str) -> SystemProduct {
        SystemProduct::new(name, version, "x86_64")
    }

    /// Builds an enabled target over a mock connection whose every command
    /// succeeds (exit 0, empty output — [`MockConnection::new`]'s default), with
    /// `system` as its parsed system. Returns the target plus a clone of the
    /// mock (shared state) so the ordered issued-command list can be asserted:
    /// the mock's *response* log carries an empty command field, so the issued
    /// commands live on the connection handle, not on `Target::out`.
    fn target_with(system: System, transactional: bool) -> (Target, MockConnection) {
        let conn = MockConnection::new("host1.example.com");
        let handle = conn.clone();
        let mut t =
            Target::with_connection("host1.example.com", TargetState::Enabled, Box::new(conn));
        t.set_system(system, transactional);
        (t, handle)
    }

    /// A system whose flattened set is exactly `products`.
    fn system_of(base: SystemProduct, addons: &[SystemProduct]) -> System {
        System::new(base, addons.iter().cloned().collect(), false)
    }

    #[test]
    fn remove_failure_display_masks_credentials_in_a_directly_built_variant() {
        let failure = RepoFailure::Remove {
            url: "https://audituser:CANARY@repo.example/r/".to_owned(),
        };
        assert_eq!(
            failure.to_string(),
            "removing repo https://***@repo.example/r/ failed"
        );
    }

    // --- run_zypper: ar / rr -----------------------------------------------

    #[tokio::test]
    async fn ar_emits_add_command_with_issue_alias() {
        let sles = product("SLES", "15-SP5");
        let (mut t, conn) = target_with(system_of(sles.clone(), &[]), false);
        let mut repos = BTreeMap::new();
        repos.insert(sles, "https://example/repo".to_owned());

        let ok = t.repo_manager().run_zypper("ar", &repos, &rrid()).await;

        assert!(ok);
        let cmds = conn.commands();
        assert!(
            cmds.iter()
                .any(|c| c.contains("zypper ar") && c.contains("issue-SLES")),
            "expected a `zypper ar ... issue-SLES ...` command, got {cmds:?}"
        );
        assert_eq!(cmds.last().map(String::as_str), Some("zypper -n ref"));
    }

    #[tokio::test]
    async fn ar_alias_is_byte_identical_to_contract() {
        let sles = product("SLES", "15-SP5");
        let (mut t, conn) = target_with(system_of(sles.clone(), &[]), false);
        let mut repos = BTreeMap::new();
        repos.insert(sles, "https://example/repo".to_owned());

        t.repo_manager().run_zypper("ar", &repos, &rrid()).await;

        let cmds = conn.commands();
        // The alias contains `=` (`:p=1:2`), which shlex quotes at the exec
        // boundary; the URL has no shell-special chars so it stays bare. The
        // single-quoted alias is the identical zypper alias.
        assert_eq!(
            cmds[0],
            "zypper ar 'issue-SLES:15-SP5:p=1:2' https://example/repo 'issue-SLES:15-SP5:p=1:2'"
        );
    }

    #[tokio::test]
    async fn ar_shell_quotes_malicious_url_and_alias() {
        // A crafted repo URL (and a product name that feeds the alias) must reach
        // the host as single quoted arguments, never as injected root commands.
        let evil = product("SLES;reboot", "15-SP5");
        let (mut t, conn) = target_with(system_of(evil.clone(), &[]), false);
        let mut repos = BTreeMap::new();
        repos.insert(evil, "https://x/repo;rm -rf /".to_owned());

        t.repo_manager().run_zypper("ar", &repos, &rrid()).await;

        let cmd = conn
            .commands()
            .into_iter()
            .find(|c| c.starts_with("zypper ar"))
            .expect("an ar command ran");
        // Re-splitting must yield exactly the verbs plus three literal args
        // (alias, url, alias) — no injected `reboot`/`rm` words.
        let tokens = shlex::split(&cmd).expect("command re-splits");
        assert_eq!(
            tokens,
            vec![
                "zypper".to_owned(),
                "ar".to_owned(),
                "issue-SLES;reboot:15-SP5:p=1:2".to_owned(),
                "https://x/repo;rm -rf /".to_owned(),
                "issue-SLES;reboot:15-SP5:p=1:2".to_owned(),
            ],
            "injection leaked into argv: {cmd:?}"
        );
    }

    #[tokio::test]
    async fn rr_shell_quotes_malicious_url() {
        let evil = product("SLES", "15-SP5");
        let (mut t, conn) = target_with(system_of(evil.clone(), &[]), false);
        let mut repos = BTreeMap::new();
        repos.insert(evil, "https://x/repo;reboot".to_owned());

        t.repo_manager().run_zypper("rr", &repos, &rrid()).await;

        let cmd = conn
            .commands()
            .into_iter()
            .find(|c| c.starts_with("zypper rr"))
            .expect("an rr command ran");
        let tokens = shlex::split(&cmd).expect("command re-splits");
        assert_eq!(
            tokens,
            vec![
                "zypper".to_owned(),
                "rr".to_owned(),
                "https://x/repo;reboot".to_owned(),
            ],
            "injection leaked into argv: {cmd:?}"
        );
    }

    #[tokio::test]
    async fn rr_emits_remove_command_per_repo() {
        let sles = product("SLES", "15-SP5");
        let (mut t, conn) = target_with(system_of(sles.clone(), &[]), false);
        let mut repos = BTreeMap::new();
        repos.insert(sles, "https://example/repo".to_owned());

        t.repo_manager().run_zypper("rr", &repos, &rrid()).await;

        let cmds = conn.commands();
        assert!(
            cmds.iter().any(|c| c == "zypper rr https://example/repo"),
            "expected `zypper rr <url>`, got {cmds:?}"
        );
        assert_eq!(cmds.last().map(String::as_str), Some("zypper -n ref"));
    }

    #[tokio::test]
    async fn skips_products_not_in_flattened_system() {
        let sles = product("SLES", "15-SP5");
        let other = product("opensuse", "15.4");
        let (mut t, conn) = target_with(system_of(sles.clone(), &[]), false);
        let mut repos = BTreeMap::new();
        repos.insert(sles, "https://wanted/repo".to_owned());
        repos.insert(other, "https://other/repo".to_owned());

        t.repo_manager().run_zypper("ar", &repos, &rrid()).await;

        let cmds = conn.commands();
        assert!(cmds.iter().any(|c| c.contains("https://wanted/repo")));
        assert!(!cmds.iter().any(|c| c.contains("https://other/repo")));
        assert_eq!(cmds.last().map(String::as_str), Some("zypper -n ref"));
    }

    // --- run_zypper: unknown-cmd safeguard ---------------------------------

    #[tokio::test]
    async fn unknown_command_bails_and_returns_false() {
        let sles = product("SLES", "15-SP5");
        let (mut t, conn) = target_with(system_of(sles.clone(), &[]), false);
        let mut repos = BTreeMap::new();
        repos.insert(sles, "https://example/repo".to_owned());

        let ok = t.repo_manager().run_zypper("nosuch", &repos, &rrid()).await;

        assert!(!ok, "unknown command must signal failure");
        // The safeguard bails before the refresh — no `zypper -n ref` ran.
        let cmds = conn.commands();
        assert!(
            !cmds.iter().any(|c| c == "zypper -n ref"),
            "unknown-cmd safeguard must not reach the refresh, got {cmds:?}"
        );
    }

    #[tokio::test]
    async fn unknown_command_force_unlocks_a_foreign_lock() {
        use std::path::PathBuf;

        use crate::connection::MockSftpOp;
        use crate::target::locks::TARGET_LOCK_PATH;

        // Seed a *foreign* operation lock on the host (different user/pid), so
        // the unknown-cmd safeguard's `unlock(force=true)` has something to
        // remove. The mock shares state with the lock's cloned connection, so
        // the lock reads this file and the force-remove is observable here.
        let sles = product("SLES", "15-SP5");
        let conn = MockConnection::new("host1.example.com").with_file(
            TARGET_LOCK_PATH,
            b"1700000000:someone-else:99999:held".to_vec(),
        );
        let handle = conn.clone();
        let mut t =
            Target::with_connection("host1.example.com", TargetState::Enabled, Box::new(conn));
        t.set_system(system_of(sles.clone(), &[]), false);
        let mut repos = BTreeMap::new();
        repos.insert(sles, "https://example/repo".to_owned());

        let ok = t.repo_manager().run_zypper("nosuch", &repos, &rrid()).await;

        assert!(!ok, "unknown command must signal failure");
        // The force-unlock removed the foreign lockfile.
        assert!(
            handle
                .sftp_ops()
                .contains(&MockSftpOp::Remove(PathBuf::from(TARGET_LOCK_PATH))),
            "expected a force-unlock sftp_remove of {TARGET_LOCK_PATH}, got {:?}",
            handle.sftp_ops()
        );
    }

    // --- run_zypper: matched == 0 warning ----------------------------------

    #[tokio::test]
    async fn no_matching_product_only_refreshes() {
        // Host carries SLES 16.0; the update targets opensuse 15.4 — no overlap.
        let host = product("SLES", "16.0");
        let update = product("opensuse", "15.4");
        let (mut t, conn) = target_with(system_of(host, &[]), false);
        let mut repos = BTreeMap::new();
        repos.insert(update, "https://other/repo".to_owned());

        let ok = t.repo_manager().run_zypper("ar", &repos, &rrid()).await;

        assert!(ok);
        // Only the refresh ran; no `zypper ar`.
        assert_eq!(conn.commands(), vec!["zypper -n ref".to_owned()]);
    }

    #[tokio::test]
    async fn a_no_match_add_records_no_match_with_the_hosts_products() {
        let base = product("SLES", "16.1");
        let addon = product("SLE-Module-Basesystem", "16.1");
        let (mut t, _conn) =
            target_with(system_of(base.clone(), std::slice::from_ref(&addon)), false);
        let mut repos = BTreeMap::new();
        repos.insert(
            SystemProduct::new("SLES", "16.1", "aarch64"),
            "https://other/repo".to_owned(),
        );

        t.repo_manager()
            .run_zypper("-n ar -ckn", &repos, &rrid())
            .await;

        assert_eq!(
            t.last_repo(),
            Some(&Ok(RepoOutcome::NoMatch {
                products: BTreeSet::from([base, addon])
            }))
        );
        assert_eq!(
            t.last_repo().unwrap().as_ref().unwrap().to_string(),
            "no repository of this update matches the host's products \
             (SLE-Module-Basesystem-16.1.x86_64, SLES-16.1.x86_64)"
        );
    }

    #[tokio::test]
    async fn an_add_on_a_host_whose_products_were_never_read_says_so() {
        let mut t = Target::with_connection(
            "host1.example.com",
            TargetState::Enabled,
            Box::new(MockConnection::new("host1.example.com")),
        );
        let mut repos = BTreeMap::new();
        repos.insert(product("SLES", "16.0"), "https://other/repo".to_owned());

        t.repo_manager()
            .run_zypper("-n ar -ckn", &repos, &rrid())
            .await;

        assert_eq!(
            t.last_repo().unwrap().as_ref().unwrap().to_string(),
            "its products could not be read"
        );
    }

    #[tokio::test]
    async fn a_no_match_add_names_an_unread_base_product_by_name() {
        // A dangling baseproduct link leaves the base's name only.
        let addon = product("SLE-Module-Basesystem", "16.1");
        let (mut t, _conn) = target_with(
            system_of(
                SystemProduct::new("SLES", "", ""),
                std::slice::from_ref(&addon),
            ),
            false,
        );
        let mut repos = BTreeMap::new();
        repos.insert(product("SLES", "16.0"), "https://other/repo".to_owned());

        t.repo_manager()
            .run_zypper("-n ar -ckn", &repos, &rrid())
            .await;

        assert_eq!(
            t.last_repo().unwrap().as_ref().unwrap().to_string(),
            "no repository of this update matches the host's products \
             (SLE-Module-Basesystem-16.1.x86_64, SLES)"
        );
    }

    /// An add registers exactly the repositories [`RepoOutcome::reach`]
    /// returns and records the outcome it returns, so a caller judging the add
    /// beforehand cannot disagree with it.
    #[tokio::test]
    async fn run_zypper_adds_what_reach_returns_and_records_its_outcome() {
        let sles = product("SLES", "15.5");
        let micro = product("SL-Micro", "6.0");
        let repos: BTreeMap<SystemProduct, String> = [
            (sles.clone(), "https://repo/sles".to_owned()),
            (micro.clone(), "https://repo/micro".to_owned()),
        ]
        .into();
        let systems = [
            system_of(sles.clone(), &[]),
            system_of(sles.clone(), std::slice::from_ref(&micro)),
            system_of(SystemProduct::new("SLES", "15.5", "aarch64"), &[]),
            system_of(SystemProduct::new("unknown", "", ""), &[]),
        ];
        for system in systems {
            for map in [&repos, &BTreeMap::new()] {
                let flattened = system.flatten();
                let reach = RepoOutcome::reach(map, &flattened);
                let (mut t, conn) = target_with(system.clone(), false);

                t.repo_manager()
                    .run_zypper("-n ar -ckn", map, &rrid())
                    .await;

                let added: BTreeSet<&SystemProduct> = repos
                    .keys()
                    .filter(|p| {
                        let alias = issue_alias(p, &rrid());
                        conn.commands()
                            .iter()
                            .any(|c| c.starts_with("zypper -n ar") && c.contains(&alias))
                    })
                    .collect();
                let reached: BTreeSet<&SystemProduct> = reach
                    .as_deref()
                    .unwrap_or_default()
                    .iter()
                    .map(|(p, _)| *p)
                    .collect();
                assert_eq!(added, reached, "{flattened:?} over {map:?}");
                assert_eq!(
                    t.last_repo(),
                    Some(&Ok(reach.err().unwrap_or(RepoOutcome::Done))),
                    "{flattened:?} over {map:?}"
                );
            }
        }
    }

    #[tokio::test]
    async fn a_no_match_remove_records_done() {
        let (mut t, _conn) = target_with(system_of(product("SLES", "16.0"), &[]), false);
        let mut repos = BTreeMap::new();
        repos.insert(product("opensuse", "15.4"), "https://other/repo".to_owned());

        t.repo_manager().run_zypper("-n rr", &repos, &rrid()).await;

        assert_eq!(t.last_repo(), Some(&Ok(RepoOutcome::Done)));
    }

    #[tokio::test]
    async fn an_add_with_no_repos_records_no_repositories() {
        let (mut t, _conn) = target_with(system_of(product("SLES", "16.0"), &[]), false);

        t.repo_manager()
            .run_zypper("-n ar -ckn", &BTreeMap::new(), &rrid())
            .await;

        assert_eq!(t.last_repo(), Some(&Ok(RepoOutcome::NoRepositories)));
        assert_eq!(
            t.last_repo().unwrap().as_ref().unwrap().to_string(),
            "this update has no repository to add"
        );
    }

    #[tokio::test]
    async fn a_remove_with_no_repos_records_done() {
        let (mut t, _conn) = target_with(system_of(product("SLES", "16.0"), &[]), false);

        t.repo_manager()
            .run_zypper("-n rr", &BTreeMap::new(), &rrid())
            .await;

        assert_eq!(t.last_repo(), Some(&Ok(RepoOutcome::Done)));
    }

    #[tokio::test]
    async fn a_no_match_add_whose_refresh_fails_still_records_no_match() {
        use mtui_types::hostlog::CommandLog;

        let conn = MockConnection::new("host1.example.com").with_response(
            "zypper -n ref",
            CommandLog::new("zypper -n ref", "", "", 1, 0),
        );
        let mut t =
            Target::with_connection("host1.example.com", TargetState::Enabled, Box::new(conn));
        t.set_system(system_of(product("SLES", "16.0"), &[]), false);
        let mut repos = BTreeMap::new();
        repos.insert(product("opensuse", "15.4"), "https://other/repo".to_owned());

        t.repo_manager()
            .run_zypper("-n ar -ckn", &repos, &rrid())
            .await;

        // Nothing was added, so the refresh cannot have mattered to this update.
        assert_eq!(
            t.last_repo(),
            Some(&Ok(RepoOutcome::NoMatch {
                products: BTreeSet::from([product("SLES", "16.0")])
            }))
        );
    }

    // --- run_zypper: transactional refresh routing -------------------------

    #[tokio::test]
    async fn transactional_refresh_branches_off_the_booted_snapshot() {
        let micro = product("SL-Micro", "6-1");
        let (mut t, conn) = target_with(system_of(micro.clone(), &[]), true);
        let mut repos = BTreeMap::new();
        repos.insert(micro, "https://example/repo".to_owned());

        t.repo_manager().run_zypper("ar", &repos, &rrid()).await;

        let cmds = conn.commands();
        assert_eq!(
            cmds.last().map(String::as_str),
            Some(
                "transactional-update --non-interactive run \
                 zypper --gpg-auto-import-keys -n ref"
            )
        );
    }

    #[tokio::test]
    async fn an_unconfirmed_sles_16_root_gets_no_repo_command() {
        // A read-write SLES 16 host carries transactional-update.conf too, so
        // neither refresh form is safe to pick for it, nor the add before it.
        let sles = product("SLES", "16.1");
        let (mut t, conn) = target_with(system_of(sles.clone(), &[]), false);
        t.set_system(system_of(sles.clone(), &[]), crate::Transactional::Assumed);
        let mut repos = BTreeMap::new();
        repos.insert(sles, "https://example/repo".to_owned());

        let ran = t.repo_manager().run_zypper("ar", &repos, &rrid()).await;

        assert!(!ran, "a bail without a refresh returns false");
        assert!(conn.commands().is_empty(), "{:?}", conn.commands());
        assert!(
            matches!(t.last_repo(), Some(Err(_))),
            "the refusal is a failure: {:?}",
            t.last_repo()
        );
    }

    #[tokio::test]
    async fn an_unconfirmed_sl_micro_root_still_refreshes_through_transactional_update() {
        let micro = product("SL-Micro", "6.1");
        let (mut t, conn) = target_with(system_of(micro.clone(), &[]), false);
        t.set_system(system_of(micro.clone(), &[]), crate::Transactional::Assumed);
        let mut repos = BTreeMap::new();
        repos.insert(micro, "https://example/repo".to_owned());

        t.repo_manager().run_zypper("ar", &repos, &rrid()).await;

        assert_eq!(
            conn.commands().last().map(String::as_str),
            Some(
                "transactional-update --non-interactive run \
                 zypper --gpg-auto-import-keys -n ref"
            )
        );
    }

    // --- run_zypper: failed add is surfaced --------------------------------

    #[tokio::test]
    async fn ar_failure_still_reaches_refresh_and_returns_true() {
        use mtui_types::hostlog::CommandLog;

        let sles = product("SLES", "15-SP5");
        // The add fails (exit 4) but the refresh must still run. The exact `ar`
        // command string is deterministic from the alias helper; the alias is
        // shlex-quoted at the exec boundary because it contains `=`.
        let ar_cmd =
            "zypper ar 'issue-SLES:15-SP5:p=1:2' https://example/repo 'issue-SLES:15-SP5:p=1:2'";
        let conn = MockConnection::new("host1.example.com").with_response(
            ar_cmd,
            CommandLog::new(ar_cmd, "", "Repository already exists.", 4, 0),
        );
        let handle = conn.clone();
        let mut t =
            Target::with_connection("host1.example.com", TargetState::Enabled, Box::new(conn));
        t.set_system(system_of(sles.clone(), &[]), false);
        let mut repos = BTreeMap::new();
        repos.insert(sles, "https://example/repo".to_owned());

        let ok = t.repo_manager().run_zypper("ar", &repos, &rrid()).await;

        assert!(ok);
        assert_eq!(
            handle.commands().last().map(String::as_str),
            Some("zypper -n ref")
        );
    }

    // --- run_zypper: ar failure verification via `zypper lr` probe --------

    #[tokio::test]
    async fn a_failed_add_of_an_already_registered_repo_is_not_a_failure() {
        use mtui_types::hostlog::CommandLog;

        // `ar` exits 4 (zypper's RepoAlreadyExistsException) but the alias IS
        // registered, per the `lr` postcondition probe — the documented
        // `update --noprepare` retry hits exactly this collision.
        let sles = product("SLES", "15-SP5");
        let ar_cmd =
            "zypper ar 'issue-SLES:15-SP5:p=1:2' https://example/repo 'issue-SLES:15-SP5:p=1:2'";
        let lr_cmd = "zypper -n lr 'issue-SLES:15-SP5:p=1:2'";
        let conn = MockConnection::new("host1.example.com")
            .with_response(
                ar_cmd,
                CommandLog::new(ar_cmd, "", "Repository already exists.", 4, 0),
            )
            .with_response(lr_cmd, CommandLog::new(lr_cmd, "some repo info", "", 0, 0));
        let handle = conn.clone();
        let mut t =
            Target::with_connection("host1.example.com", TargetState::Enabled, Box::new(conn));
        t.set_system(system_of(sles.clone(), &[]), false);
        let mut repos = BTreeMap::new();
        repos.insert(sles, "https://example/repo".to_owned());

        t.repo_manager().run_zypper("ar", &repos, &rrid()).await;

        assert_eq!(t.last_repo(), Some(&Ok(RepoOutcome::Done)));
        assert!(
            handle.commands().iter().any(|c| c == lr_cmd),
            "expected the `lr` probe to run, got {:?}",
            handle.commands()
        );
    }

    #[tokio::test]
    async fn a_failed_add_of_a_missing_repo_is_a_failure() {
        use mtui_types::hostlog::CommandLog;

        // `ar` exits 4 and the `lr` probe also fails: the repo is genuinely
        // absent, not an already-exists collision.
        let sles = product("SLES", "15-SP5");
        let ar_cmd =
            "zypper ar 'issue-SLES:15-SP5:p=1:2' https://example/repo 'issue-SLES:15-SP5:p=1:2'";
        let lr_cmd = "zypper -n lr 'issue-SLES:15-SP5:p=1:2'";
        let conn = MockConnection::new("host1.example.com")
            .with_response(
                ar_cmd,
                CommandLog::new(ar_cmd, "", "Repository already exists.", 4, 0),
            )
            .with_response(
                lr_cmd,
                CommandLog::new(lr_cmd, "", "Repository not found.", 3, 0),
            );
        let handle = conn.clone();
        let mut t =
            Target::with_connection("host1.example.com", TargetState::Enabled, Box::new(conn));
        t.set_system(system_of(sles.clone(), &[]), false);
        let mut repos = BTreeMap::new();
        repos.insert(sles, "https://example/repo".to_owned());

        t.repo_manager().run_zypper("ar", &repos, &rrid()).await;

        assert_eq!(
            t.last_repo(),
            Some(&Err(RepoFailure::Add {
                alias: "issue-SLES:15-SP5:p=1:2".to_owned()
            }))
        );
        assert!(
            handle.commands().iter().any(|c| c == lr_cmd),
            "expected the `lr` probe to run, got {:?}",
            handle.commands()
        );
    }

    // --- run_zypper: last_repo outcome aggregation -------------------------

    #[tokio::test]
    async fn last_repo_records_success_on_normal_run() {
        let sles = product("SLES", "15-SP5");
        let (mut t, _conn) = target_with(system_of(sles.clone(), &[]), false);
        let mut repos = BTreeMap::new();
        repos.insert(sles, "https://example/repo".to_owned());

        t.repo_manager().run_zypper("ar", &repos, &rrid()).await;

        assert_eq!(t.last_repo(), Some(&Ok(RepoOutcome::Done)));
    }

    #[tokio::test]
    async fn last_repo_records_failure_when_add_fails_but_ref_succeeds() {
        use mtui_types::hostlog::CommandLog;

        // The `ar` fails (exit 4) and the `lr` postcondition probe confirms the
        // repo is genuinely absent (exit 3), but the trailing `ref` succeeds
        // (exit 0), so `lastexit()` reads 0 — yet `last_repo` must still
        // record the failure the masked `ar` produced. This is the exact case
        // `lasterr()`/`lastexit()` cannot report.
        let sles = product("SLES", "15-SP5");
        let ar_cmd =
            "zypper ar 'issue-SLES:15-SP5:p=1:2' https://example/repo 'issue-SLES:15-SP5:p=1:2'";
        let lr_cmd = "zypper -n lr 'issue-SLES:15-SP5:p=1:2'";
        let conn = MockConnection::new("host1.example.com")
            .with_response(
                ar_cmd,
                CommandLog::new(ar_cmd, "", "Repository already exists.", 4, 0),
            )
            .with_response(
                lr_cmd,
                CommandLog::new(lr_cmd, "", "Repository not found.", 3, 0),
            );
        let mut t =
            Target::with_connection("host1.example.com", TargetState::Enabled, Box::new(conn));
        t.set_system(system_of(sles.clone(), &[]), false);
        let mut repos = BTreeMap::new();
        repos.insert(sles, "https://example/repo".to_owned());

        t.repo_manager().run_zypper("ar", &repos, &rrid()).await;

        // The trailing refresh succeeded, so the log's last exit is 0 …
        assert_eq!(t.lastexit(), Some(0));
        // … but the aggregated verdict still reports the masked add failure.
        assert_eq!(
            t.last_repo(),
            Some(&Err(RepoFailure::Add {
                alias: "issue-SLES:15-SP5:p=1:2".to_owned()
            }))
        );
    }

    #[tokio::test]
    async fn last_repo_records_a_ref_only_failure_as_refresh() {
        use mtui_types::hostlog::CommandLog;

        // The `ar` succeeds but the trailing `ref` fails: a distinct variant
        // from a masked `ar` failure, since a consumer treats the two
        // differently (on a read-write host Refresh is a degradation, not an
        // exclusion — see `update_flow::repo_add_failures`).
        let sles = product("SLES", "15-SP5");
        let conn = MockConnection::new("host1.example.com").with_response(
            "zypper -n ref",
            CommandLog::new("zypper -n ref", "", "", 1, 0),
        );
        let mut t =
            Target::with_connection("host1.example.com", TargetState::Enabled, Box::new(conn));
        t.set_system(system_of(sles.clone(), &[]), false);
        let mut repos = BTreeMap::new();
        repos.insert(sles, "https://example/repo".to_owned());

        t.repo_manager().run_zypper("ar", &repos, &rrid()).await;

        assert_eq!(
            t.last_repo(),
            Some(&Err(RepoFailure::Refresh {
                command: "zypper -n ref".to_owned()
            }))
        );
    }

    #[tokio::test]
    async fn last_repo_records_a_failed_rr_as_remove() {
        use mtui_types::hostlog::CommandLog;

        // `rr` exits non-zero but the trailing `ref` succeeds, so `lastexit()`
        // reads 0 — masking the removal failure the same way a masked `ar`
        // does. Fact 1 (see plans/551): a non-zero `rr` is always a real
        // problem, since zypper exits 0 even when the repo was never there.
        let sles = product("SLES", "15-SP5");
        let rr_cmd = "zypper rr https://example/repo";
        let conn = MockConnection::new("host1.example.com").with_response(
            rr_cmd,
            CommandLog::new(rr_cmd, "", "Repository not found.", 1, 0),
        );
        let mut t =
            Target::with_connection("host1.example.com", TargetState::Enabled, Box::new(conn));
        t.set_system(system_of(sles.clone(), &[]), false);
        let mut repos = BTreeMap::new();
        repos.insert(sles, "https://example/repo".to_owned());

        t.repo_manager().run_zypper("rr", &repos, &rrid()).await;

        assert_eq!(t.lastexit(), Some(0));
        assert_eq!(
            t.last_repo(),
            Some(&Err(RepoFailure::Remove {
                url: "https://example/repo".to_owned()
            }))
        );
    }

    #[tokio::test]
    async fn last_repo_records_failure_on_unknown_command() {
        let sles = product("SLES", "15-SP5");
        let (mut t, _conn) = target_with(system_of(sles.clone(), &[]), false);
        let mut repos = BTreeMap::new();
        repos.insert(sles, "https://example/repo".to_owned());

        t.repo_manager().run_zypper("nosuch", &repos, &rrid()).await;

        assert_eq!(
            t.last_repo(),
            Some(&Err(RepoFailure::UnknownCommand {
                command: "nosuch".to_owned()
            }))
        );
    }

    // --- set: forwards to SetRepo ------------------------------------------

    #[tokio::test]
    async fn set_forwards_operation_to_report() {
        /// Records the `(hostname, op)` it was asked to set.
        struct RecordingReport {
            seen: Arc<Mutex<Vec<(String, RepoOp)>>>,
        }

        #[async_trait::async_trait]
        impl SetRepo for RecordingReport {
            async fn set_repo(&self, target: &mut Target, operation: RepoOp) {
                self.seen
                    .lock()
                    .unwrap()
                    .push((target.hostname().to_owned(), operation));
            }
        }

        let sles = product("SLES", "15-SP5");
        let (mut t, _conn) = target_with(system_of(sles, &[]), false);
        let seen = Arc::new(Mutex::new(Vec::new()));
        let report = RecordingReport { seen: seen.clone() };

        t.repo_manager().set(RepoOp::Add, &report).await;

        assert_eq!(
            *seen.lock().unwrap(),
            vec![("host1.example.com".to_owned(), RepoOp::Add)]
        );
    }

    #[test]
    fn repo_op_as_str_tokens() {
        assert_eq!(RepoOp::Add.as_str(), "add");
        assert_eq!(RepoOp::Remove.as_str(), "remove");
    }
}
