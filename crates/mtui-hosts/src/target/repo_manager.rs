//! Per-target zypper repository manager.
//!
//! ## Reference
//!
//! Ported from upstream `mtui/hosts/target/repo_manager.py` (`RepoManager`).
//! Upstream extracts the two repository-shape methods that used to live directly
//! on [`Target`] into one collaborator so [`Target`] can stay focused on the
//! connection/lock skeleton:
//!
//! * [`set`](RepoManager::set) — the one-line forward into
//!   `testreport.set_repo(target, operation)`; kept on the collaborator so
//!   callers reach for `target.repo_manager().set(...)` instead of
//!   `target.set_repo(...)`.
//! * [`run_zypper`](RepoManager::run_zypper) — fans the zypper `ar`/`rr`
//!   add/remove loop out across the target's flattened system and finishes with
//!   a `zypper ref` (routed through `transactional-update` on read-only-root
//!   hosts). The unknown-cmd safeguard (force-unlock followed by an error) is
//!   preserved.
//!
//! ## `&mut Target` vs the immutable [`Reporter`]
//!
//! Unlike the sibling [`Reporter`](super::reporter::Reporter), which only
//! *reads* the target and so borrows it immutably, `RepoManager` must **mutate**
//! the target: it issues commands ([`Target::run`]) and, on the unknown-cmd
//! safeguard path, force-unlocks it ([`Target::unlock`]). It therefore borrows
//! `&mut Target`. Obtain one via [`Target::repo_manager`], which — like
//! upstream's per-access `Target.repo_manager` property — hands out a fresh
//! binding over the live target each time.
//!
//! ## The [`SetRepo`] seam
//!
//! Upstream's `set` forwards into `testreport.set_repo(...)`, but the concrete
//! test-report types live in `mtui-testreport` — a *higher* crate. A direct
//! dependency here would make `mtui-hosts` depend on `mtui-testreport` and
//! **break the acyclic crate graph**. So `set` dispatches through the
//! object-safe [`SetRepo`] trait defined here; the concrete
//! `impl SetRepo for SlReport` (and the other report types) live in
//! `mtui-testreport` and are driven via
//! [`HostsGroup::fanout_set_repo`](super::HostsGroup) — mirroring exactly how
//! [`operation`](super::operation) injects its doer/check registries through the
//! [`OperationGroup`](super::operation::OperationGroup) seam.

use std::collections::BTreeMap;

use mtui_types::rrid::RequestReviewID;
use mtui_types::shellquote::quote_args;
use mtui_types::system::SystemProduct;
use tracing::{debug, info, warn};

use super::Target;

/// Which repository change a [`set`](RepoManager::set) forwards: add or remove.
///
/// Upstream passes the bare strings `"add"` / `"remove"` into
/// `testreport.set_repo`. Modelling them as a two-variant enum keeps the seam
/// typed; [`as_str`](RepoOp::as_str) renders the exact upstream token for a
/// [`SetRepo`] implementer that wants it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepoOp {
    /// Add the update's repositories (upstream `"add"`).
    Add,
    /// Remove the update's repositories (upstream `"remove"`).
    Remove,
}

impl RepoOp {
    /// The upstream string token for this operation (`"add"` / `"remove"`).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Add => "add",
            Self::Remove => "remove",
        }
    }
}

/// The report-side hook a [`RepoManager::set`] forwards into — the injection
/// point for `testreport.set_repo(target, operation)`.
///
/// Object-safe (`&dyn SetRepo`) and `async` (the report's `set_repo` ultimately
/// drives async `zypper` fan-out through [`RepoManager::run_zypper`]), matching
/// the `#[async_trait]` convention used by
/// [`OperationGroup`](super::operation::OperationGroup). The concrete
/// implementations (`SlReport`, `ObsReport`, …) live in `mtui-testreport` and
/// are dispatched through this seam; keeping the trait here preserves the
/// acyclic crate graph.
#[async_trait::async_trait]
pub trait SetRepo: Send + Sync {
    /// Add or remove this report's repositories on `target`.
    ///
    /// Mirrors upstream `testreport.set_repo(target, operation)`.
    async fn set_repo(&self, target: &mut Target, operation: RepoOp);
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

    /// Asks `report` to add or remove repos on the bound target.
    ///
    /// Ports `RepoManager.set` (upstream the one-line forward that used to live
    /// as `Target.set_repo`): logs at debug and forwards to
    /// [`SetRepo::set_repo`], the report-side hook implemented in
    /// `mtui-testreport`.
    pub async fn set(&mut self, operation: RepoOp, report: &dyn SetRepo) {
        debug!(host = %self.target.hostname(), op = operation.as_str(), "changing repos");
        report.set_repo(self.target, operation).await;
    }

    /// Runs a fan-out `zypper` command across the target's repos.
    ///
    /// Ports `RepoManager.run_zypper`. Iterates `repos` filtered by what the
    /// target's flattened system actually carries; for each matching
    /// product/repo pair, `cmd` drives one of:
    ///
    /// * **`ar`** (add-repo, `cmd` contains `"ar"`) →
    ///   `zypper <cmd> <alias> <url> <alias>` with alias
    ///   `issue-<name>:<version>:p=<maintenance_id>:<review_id>`. A non-zero
    ///   exit is surfaced as a WARNING (the repo was *not* registered), not
    ///   swallowed.
    /// * **`rr`** (remove-repo, `cmd` contains `"rr"`) → `zypper <cmd> <url>`.
    ///
    /// The metadata-derived `alias` and `url` are shell-quoted at this exec
    /// boundary (they run as root), so a crafted value cannot inject a command;
    /// URLs are additionally validated at ingestion (see `repoparse`).
    /// * **anything else** → force-unlock the target ([`Target::unlock`] with
    ///   `force = true`) and return [`false`], the Rust analogue of upstream's
    ///   post-`unlock(True)` `ValueError` safeguard. (The typed error the caller
    ///   sees is out of scope for the safeguard itself; the boolean signals
    ///   "unknown command, bailed".)
    ///
    /// When no repo product matches the host's flattened system (`matched == 0`)
    /// a WARNING fires listing the update's products vs the host's — reproducing
    /// the fix for the silent-no-op bug where `set_repo --add` "succeeded" but
    /// registered nothing against a drifted host.
    ///
    /// Always finishes with a refresh so subsequent operations see the freshly
    /// (un)registered repos: `zypper -n ref` normally, or
    /// `transactional-update --continue --non-interactive run zypper
    /// --gpg-auto-import-keys -n ref` on a transactional (read-only-root) host,
    /// where a plain `zypper ref` of a not-yet-trusted signed repo would fail
    /// importing the key into the read-only rpmdb. A failed refresh is also
    /// surfaced as a WARNING.
    ///
    /// Returns `true` on the normal path and `false` on the unknown-cmd
    /// safeguard (after force-unlocking via [`Target::unlock`], and *without*
    /// running the refresh — matching upstream, which raises before reaching
    /// it).
    pub async fn run_zypper(
        &mut self,
        cmd: &str,
        repos: &BTreeMap<SystemProduct, String>,
        rrid: &RequestReviewID,
    ) -> bool {
        let is_ar = cmd.contains("ar");
        let is_rr = cmd.contains("rr");

        // Snapshot the flattened system once so the borrow of `self.target`
        // does not overlap the mutable `run`/`unlock` calls below.
        let flattened = self.target.system().flatten();
        let hostname = self.target.hostname().to_owned();

        // Products the host actually carries, paired with their repo URL, in the
        // deterministic order of the BTreeMap (upstream iterated a dict).
        let matched_repos: Vec<(SystemProduct, String)> = repos
            .iter()
            .filter(|(product, _)| flattened.contains(*product))
            .map(|(product, url)| (product.clone(), url.clone()))
            .collect();

        if matched_repos.is_empty() {
            // Unknown command still has to hit the force-unlock safeguard even
            // when nothing matched (upstream reaches the branch only inside the
            // loop, but an empty loop means the refresh runs; the safeguard is a
            // per-pair concern). Preserve upstream: with no matches, no per-pair
            // branch is taken, so an unknown cmd is a silent no-op except for
            // the matched==0 warning + refresh. We therefore only warn here.
            let op = if is_ar {
                "add"
            } else if is_rr {
                "remove"
            } else {
                cmd
            };
            let want: Vec<String> = {
                let mut v: Vec<String> = repos.keys().map(fmt_product).collect();
                v.sort();
                v
            };
            let have: Vec<String> = {
                let mut v: Vec<String> = flattened.iter().map(fmt_product).collect();
                v.sort();
                v
            };
            warn!(
                "set_repo {op} on {hostname} did nothing: none of the update's \
                 products {want:?} match the host's installed products {have:?}",
            );
        }

        for (product, url) in &matched_repos {
            if is_ar {
                let alias = issue_alias(product, rrid);
                info!("Adding repo {url} on {hostname}");
                // Quote the metadata-derived alias/url so a crafted value cannot
                // break out of its argument into the root command line.
                let args = quote_args(&[alias.as_str(), url.as_str(), alias.as_str()]);
                self.target.run(&format!("zypper {cmd} {args}")).await;
                // Surface a failed add instead of returning silent success: a
                // non-zero zypper exit here means the repo was NOT registered.
                if self.target.lastexit() != Some(0) {
                    let err = last_error_line(self.target);
                    warn!(
                        "adding repo {alias} on {hostname} failed: zypper exited {}{}",
                        exit_display(self.target.lastexit()),
                        err,
                    );
                }
            } else if is_rr {
                info!("Removing repo {url} on {hostname}");
                let args = quote_args(&[url.as_str()]);
                self.target.run(&format!("zypper {cmd} {args}")).await;
            } else {
                // Unknown sub-command: upstream force-unlocks the target
                // (`self.target._lock.unlock(True)`) and raises `ValueError`.
                // The `Target` now owns its `TargetLock` (built in
                // `Target::connect`, or the test seam in `with_connection`), so
                // we issue the force-unlock and bail with `false` — the Rust
                // analogue of upstream's post-unlock `ValueError` safeguard.
                warn!(
                    host = %hostname, %cmd,
                    "unknown zypper sub-command; force-unlocking target and bailing"
                );
                self.target.unlock(true).await;
                return false;
            }
        }

        // Refresh repo metadata. On a transactional (read-only root) host this
        // must go through transactional-update so zypper gets a writable rpmdb
        // in a snapshot (activated by the reboot the prepare/update flow already
        // performs) and can import a not-yet-trusted repo key non-interactively.
        let ref_cmd = if self.target.transactional() {
            "transactional-update --continue --non-interactive run \
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
        }

        true
    }
}

/// Builds the `issue-<name>:<version>:p=<maintenance_id>:<review_id>` repo alias,
/// byte-identical to upstream's `name(product, rrid)`.
fn issue_alias(product: &SystemProduct, rrid: &RequestReviewID) -> String {
    format!(
        "issue-{}:{}:p={}:{}",
        product.name, product.version, rrid.maintenance_id, rrid.review_id
    )
}

/// Renders a [`SystemProduct`] the way upstream's `str(product)` does for the
/// matched==0 diagnostic — `name-version.arch` is *not* what upstream prints;
/// upstream prints the `NamedTuple`'s repr. We surface the meaningful triple in
/// a stable, sortable form so the warning is legible and deterministic.
fn fmt_product(p: &SystemProduct) -> String {
    format!("{}-{}.{}", p.name, p.version, p.arch)
}

/// Formats the last exit code for a log line: the numeric code, or `?` when the
/// log is empty (upstream printed `""`/`-1`; a missing entry is `?` here).
fn exit_display(code: Option<i16>) -> String {
    code.map_or_else(|| "?".to_owned(), |c| c.to_string())
}

/// Extracts the trailing "( <last stderr/stdout line> )" suffix for a failure
/// warning, mirroring upstream's `f" ({err.splitlines()[-1]})"`. Prefers stderr,
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
    //! Ported from upstream `tests/test_repo_manager.py`. Upstream drives the
    //! manager against `MagicMock` targets; the Rust analogue exercises a real
    //! [`Target`] over a [`MockConnection`], asserting the exact `zypper`
    //! command strings, the alias format, the matched==0 warning, the
    //! unknown-cmd force-unlock safeguard, and the transactional refresh path.

    use std::collections::BTreeMap;
    use std::sync::{Arc, Mutex};

    use mtui_types::enums::{ExecutionMode, RequestKind, TargetState};
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
        let mut t = Target::with_connection(
            "host1.example.com",
            TargetState::Enabled,
            ExecutionMode::Parallel,
            Box::new(conn),
        );
        t.set_system(system, transactional);
        (t, handle)
    }

    /// A system whose flattened set is exactly `products`.
    fn system_of(base: SystemProduct, addons: &[SystemProduct]) -> System {
        System::new(base, addons.iter().cloned().collect(), false)
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
    async fn ar_alias_is_byte_identical_to_upstream() {
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
        let mut t = Target::with_connection(
            "host1.example.com",
            TargetState::Enabled,
            ExecutionMode::Parallel,
            Box::new(conn),
        );
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

    // --- run_zypper: transactional refresh routing -------------------------

    #[tokio::test]
    async fn transactional_host_routes_ref_through_transactional_update() {
        let micro = product("SL-Micro", "6-1");
        let (mut t, conn) = target_with(system_of(micro.clone(), &[]), true);
        let mut repos = BTreeMap::new();
        repos.insert(micro, "https://example/repo".to_owned());

        t.repo_manager().run_zypper("ar", &repos, &rrid()).await;

        let cmds = conn.commands();
        assert_eq!(
            cmds.last().map(String::as_str),
            Some(
                "transactional-update --continue --non-interactive run \
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
        let mut t = Target::with_connection(
            "host1.example.com",
            TargetState::Enabled,
            ExecutionMode::Parallel,
            Box::new(conn),
        );
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
    fn repo_op_as_str_matches_upstream_tokens() {
        assert_eq!(RepoOp::Add.as_str(), "add");
        assert_eq!(RepoOp::Remove.as_str(), "remove");
    }
}
