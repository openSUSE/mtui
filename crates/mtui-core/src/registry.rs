//! The explicit command registry.
//!
//! Every command is wired through the **explicit** [`register_all`] composition
//! point. The REPL dispatch, tab-completion and the MCP tool synthesiser all
//! iterate this one [`Registry`] — it is the single source of the command
//! surface.
//!
//! A command answers to its [`name`](crate::Command::name) and any
//! [`aliases`](crate::Command::aliases), each mapping to the same instance. Two
//! commands claiming one name (or alias) is a programming error:
//! [`Registry::register`] **panics**, so the composition root fails fast at boot.

use std::sync::Arc;

use indexmap::IndexMap;

use crate::command::Command;

/// A name→command lookup that preserves registration order, giving the REPL and
/// MCP a deterministic command listing.
#[derive(Default)]
pub struct Registry {
    /// name-or-alias → command; both key the same [`Arc<dyn Command>`], so
    /// lookup is uniform. Insertion-ordered, and a command registers its name
    /// before its aliases.
    by_key: IndexMap<&'static str, Arc<dyn Command>>,
    /// Canonical names in registration order, so [`names`](Registry::names)
    /// never lists aliases.
    canonical: Vec<&'static str>,
}

impl Registry {
    /// An empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers `command` under its name and every alias.
    ///
    /// # Panics
    ///
    /// Panics if the name or any alias is already claimed: a duplicate is a
    /// static programming error, and failing fast at boot beats silently
    /// shadowing a command.
    pub fn register(&mut self, command: Arc<dyn Command>) {
        let name = command.name();
        assert!(
            !self.by_key.contains_key(name),
            "command name already registered: {name}"
        );
        self.by_key.insert(name, Arc::clone(&command));
        self.canonical.push(name);
        for &alias in command.aliases() {
            assert!(
                !self.by_key.contains_key(alias),
                "command alias already registered: {alias}"
            );
            self.by_key.insert(alias, Arc::clone(&command));
        }
    }

    /// Looks up a command by its name or any alias.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<&Arc<dyn Command>> {
        self.by_key.get(key)
    }

    /// `true` if `key` names a command or one of its aliases.
    #[must_use]
    pub fn contains(&self, key: &str) -> bool {
        self.by_key.contains_key(key)
    }

    /// The canonical command names, in registration order (aliases excluded).
    pub fn names(&self) -> impl Iterator<Item = &'static str> + '_ {
        self.canonical.iter().copied()
    }

    /// Every command key — canonical names **and** aliases — in insertion order.
    ///
    /// For alias-aware first-token completion; [`names`](Registry::names) is the
    /// canonical-only listing.
    pub fn keys(&self) -> impl Iterator<Item = &'static str> + '_ {
        self.by_key.keys().copied()
    }
}

/// Commands that must not be synthesised into MCP tools.
///
/// They either drive the interactive shell / need a controlling terminal, or are
/// replaced by richer hand-written tools: `edit` → the `testreport_*` tools, and
/// `get`/`put` → the in-band transfer tools (#434), because their synthesised
/// forms exchange server-local paths a remote `--transport http` client cannot
/// reach. Local process execution is not a category here: `lrun` was removed
/// outright rather than denied.
///
/// The synthesiser skips every registry command whose name or alias appears
/// here and warns at boot if an entry no longer resolves; the
/// `mcp_denylist_is_consistent` test pins the exact expected set. Kept beside
/// [`register_all`] so the list and the surface it filters live in one place.
pub const MCP_DENYLIST: &[&str] = &[
    "quit", "exit", "EOF",    // session exit (Wave 2)
    "switch", // active-template pointer, REPL-only (Wave 2)
    "shell",  // interactive PTY attach, REPL-only (Wave 2)
    "help",   // registry listing / per-command help, REPL-only
    "edit",   // $EDITOR spawn on the controlling TTY, REPL-only
    "terms",  // spawn terminal-launcher scripts to hosts, REPL-only
    "get",
    "put", // path-based SFTP transfers; replaced by hand-written in-band MCP tools (#434)
];

/// Builds the process-wide command registry — the single, explicit place every
/// command is wired.
///
/// Both the REPL (`mtui`) and MCP (`mtui-mcp`) build their command surface from
/// the [`Registry`] this returns, so a command added here becomes a REPL command
/// **and** an MCP tool automatically.
#[must_use]
pub fn register_all() -> Registry {
    use crate::commands;

    let mut registry = Registry::new();
    // Wave 1 — core workflow.
    registry.register(Arc::new(commands::Run));
    registry.register(Arc::new(commands::Update));
    registry.register(Arc::new(commands::Install));
    registry.register(Arc::new(commands::Uninstall));
    registry.register(Arc::new(commands::Prepare));
    registry.register(Arc::new(commands::Downgrade));
    registry.register(Arc::new(commands::Reboot));
    registry.register(Arc::new(commands::SetRepo));
    registry.register(Arc::new(commands::ShowUpdateRepos));
    // Wave 2 — host & session management.
    registry.register(Arc::new(commands::AddHost));
    registry.register(Arc::new(commands::RemoveHost));
    registry.register(Arc::new(commands::HostState));
    registry.register(Arc::new(commands::HostLock));
    registry.register(Arc::new(commands::HostsUnlock));
    registry.register(Arc::new(commands::Switch));
    registry.register(Arc::new(commands::Unload));
    registry.register(Arc::new(commands::ListTemplates));
    registry.register(Arc::new(commands::Whoami));
    registry.register(Arc::new(commands::ListProducts));
    registry.register(Arc::new(commands::ReloadProducts));
    registry.register(Arc::new(commands::ConfigCmd));
    registry.register(Arc::new(commands::Quit));
    registry.register(Arc::new(commands::Shell));
    // Wave 3 — testreport lifecycle, metadata & host-info commands.
    registry.register(Arc::new(commands::Checkout));
    registry.register(Arc::new(commands::Commit));
    registry.register(Arc::new(commands::ShowDiff));
    registry.register(Arc::new(commands::AnalyzeDiff));
    registry.register(Arc::new(commands::ListBugs));
    registry.register(Arc::new(commands::ListMetadata));
    registry.register(Arc::new(commands::ListHosts));
    registry.register(Arc::new(commands::ListTimeout));
    registry.register(Arc::new(commands::ListUpdateCommands));
    registry.register(Arc::new(commands::ListSessions));
    registry.register(Arc::new(commands::ListLocks));
    registry.register(Arc::new(commands::ListHistory));
    registry.register(Arc::new(commands::ShowLog));
    registry.register(Arc::new(commands::ListVersions));
    registry.register(Arc::new(commands::ListPackages));
    registry.register(Arc::new(commands::SetTimeout));
    registry.register(Arc::new(commands::SftpPut));
    registry.register(Arc::new(commands::SftpGet));
    // Wave 4 — backend APIs, openQA/QEM queue & workflow.
    registry.register(Arc::new(commands::Checkers));
    registry.register(Arc::new(commands::Updates));
    registry.register(Arc::new(commands::OpenQAOverview));
    registry.register(Arc::new(commands::OpenQAJobs));
    registry.register(Arc::new(commands::ReloadOpenQA));
    registry.register(Arc::new(commands::SetWorkflow));
    registry.register(Arc::new(commands::SetLogLevel));
    registry.register(Arc::new(commands::Assign));
    registry.register(Arc::new(commands::Unassign));
    registry.register(Arc::new(commands::Reject));
    registry.register(Arc::new(commands::Comment));
    registry.register(Arc::new(commands::Approve));
    registry.register(Arc::new(commands::Regenerate));
    registry.register(Arc::new(commands::RequestReview));
    registry.register(Arc::new(commands::Export));
    registry.register(Arc::new(commands::ListRefhosts));
    registry.register(Arc::new(commands::LoadTemplate));
    // REPL-only command-surface additions.
    registry.register(Arc::new(commands::Help));
    registry.register(Arc::new(commands::Edit));
    registry.register(Arc::new(commands::Terms));
    registry
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::Command;
    use crate::session::Session;
    use async_trait::async_trait;
    use clap::ArgMatches;

    struct Stub {
        name: &'static str,
        aliases: &'static [&'static str],
    }

    #[async_trait]
    impl Command for Stub {
        fn name(&self) -> &'static str {
            self.name
        }
        fn aliases(&self) -> &'static [&'static str] {
            self.aliases
        }
        async fn call(
            &self,
            _session: &mut Session,
            _args: &ArgMatches,
        ) -> crate::error::CommandResult {
            Ok(())
        }
    }

    fn stub(name: &'static str, aliases: &'static [&'static str]) -> Arc<dyn Command> {
        Arc::new(Stub { name, aliases })
    }

    #[test]
    fn empty_registry_has_no_commands() {
        let r = Registry::new();
        assert_eq!(r.names().count(), 0);
        assert!(r.get("run").is_none());
    }

    #[test]
    fn register_all_wires_wave1_commands() {
        let r = register_all();
        for name in [
            "run",
            "update",
            "install",
            "uninstall",
            "prepare",
            "downgrade",
            "reboot",
            "set_repo",
            "show_update_repos",
        ] {
            assert!(r.contains(name), "expected {name} to be registered");
        }
    }

    #[test]
    fn register_all_wires_wave2_commands() {
        let r = register_all();
        for name in [
            "add_host",
            "remove_host",
            "set_host_state",
            "lock",
            "unlock",
            "switch",
            "unload",
            "list_templates",
            "whoami",
            "list_products",
            "reload_products",
            "config",
            "quit",
            "shell",
        ] {
            assert!(r.contains(name), "expected {name} to be registered");
        }
        assert!(r.contains("exit"));
        assert!(r.contains("EOF"));
    }

    #[test]
    fn register_all_wires_wave3_commands() {
        let r = register_all();
        for name in [
            "checkout",
            "commit",
            "show_diff",
            "analyze_diff",
            "list_bugs",
            "list_metadata",
            "list_hosts",
            "list_timeout",
            "list_update_commands",
            "list_sessions",
            "list_locks",
            "list_history",
            "show_log",
            "list_versions",
            "list_packages",
            "set_timeout",
            "put",
            "get",
        ] {
            assert!(r.contains(name), "expected {name} to be registered");
        }
    }

    #[test]
    fn register_all_wires_wave4_commands() {
        let r = register_all();
        for name in [
            "checkers",
            "updates",
            "openqa_overview",
            "openqa_jobs",
            "set_log_level",
            "assign",
            "unassign",
            "reject",
            "comment",
            "approve",
            "regenerate",
            "request_review",
        ] {
            assert!(r.contains(name), "expected {name} to be registered");
        }
    }

    #[test]
    fn register_all_command_count() {
        // 9 Wave 1 + 14 Wave 2 + 17 Wave 3 + 12 Wave 4 + 4 follow-ups
        // (export, list_refhosts, load_template, list_locks) + reload_openqa +
        // set_workflow + 3 REPL-only (help, edit, terms) = 61.
        assert_eq!(register_all().names().count(), 61);
    }

    #[test]
    fn register_all_wires_export_and_template_commands() {
        let r = register_all();
        for name in ["export", "list_refhosts", "load_template"] {
            assert!(r.contains(name), "expected {name} to be registered");
        }
    }

    /// Every command that must dodge the per-call fork, reached through the
    /// registry the way the MCP gate reaches it. `config` answers per invocation
    /// (#523) and only a recognised `show` is scoped — an unknown subcommand and
    /// an empty argv fall through to the canonical session, so a subcommand added
    /// later cannot silently start writing a fork. The fallthrough is pinned
    /// *here* and not on the lock shape: `resolve_command_rrids` also gives up on
    /// an argv it cannot parse, so `command_lock` would answer `Exclusive` for
    /// these rows whichever way the predicate went.
    #[test]
    fn requires_canonical_session_per_invocation() {
        let r = register_all();
        for (name, args, expected) in [
            ("load_template", &["SUSE:Maintenance:1:1"][..], true),
            ("unload", &[][..], true),
            ("switch", &[][..], true),
            ("regenerate", &[][..], true),
            ("config", &["set", "session_user", "x"][..], true),
            ("config", &["show"][..], false),
            ("config", &["-T", "SUSE:Maintenance:1:1", "show"][..], false),
            ("config", &[][..], true),
            ("config", &["frobnicate"][..], true),
            ("list_hosts", &[][..], false),
        ] {
            let argv: Vec<String> = args.iter().map(|s| (*s).to_owned()).collect();
            let cmd = r.get(name).expect("registered");
            assert_eq!(
                cmd.requires_canonical_session(&argv),
                expected,
                "{name} {args:?}"
            );
        }
    }

    #[test]
    fn load_template_is_not_mcp_denylisted() {
        // A valid headless tool: it names its own RRID.
        assert!(!MCP_DENYLIST.contains(&"load_template"));
    }

    #[test]
    fn mcp_denylist_covers_wave2_repl_only_commands() {
        for name in ["quit", "exit", "EOF", "switch", "shell"] {
            assert!(
                MCP_DENYLIST.contains(&name),
                "{name} must be on the MCP deny-list"
            );
        }
    }

    #[test]
    fn lrun_is_fully_removed() {
        // Arbitrary local execution is removed by design, not merely denied:
        // absent from the registry, and absent from the deny-list too, which
        // would otherwise imply it still exists somewhere.
        assert!(
            !register_all().contains("lrun"),
            "lrun must not be a registered command"
        );
        assert!(
            !MCP_DENYLIST.contains(&"lrun"),
            "a removed command must not linger on the deny-list"
        );
    }

    #[test]
    fn mcp_denylist_is_consistent() {
        // The loop only rules out duplicates; that every entry resolves is
        // asserted below against the full expected list.
        let r = register_all();
        let mut seen = std::collections::HashSet::new();
        for name in MCP_DENYLIST {
            assert!(seen.insert(*name), "duplicate deny-list entry: {name}");
            let _reserved_or_registered = r.contains(name);
        }
        // The REPL-only set (quit+aliases, switch, shell, help, edit, terms)
        // plus the transfer pair re-served as in-band MCP tools (#434).
        let registered_denied: Vec<&str> = MCP_DENYLIST
            .iter()
            .copied()
            .filter(|n| r.contains(n))
            .collect();
        assert_eq!(
            registered_denied,
            vec![
                "quit", "exit", "EOF", "switch", "shell", "help", "edit", "terms", "get", "put"
            ]
        );
    }

    #[test]
    fn name_and_alias_resolve_to_same_command() {
        let mut r = Registry::new();
        r.register(stub("run", &["r", "exec"]));
        assert!(r.contains("run"));
        assert!(r.contains("r"));
        assert!(r.contains("exec"));
        assert_eq!(r.names().count(), 1);
        let by_name = r.get("run").unwrap();
        let by_alias = r.get("r").unwrap();
        assert!(Arc::ptr_eq(by_name, by_alias));
    }

    #[test]
    fn names_lists_canonical_in_registration_order() {
        let mut r = Registry::new();
        r.register(stub("run", &["r"]));
        r.register(stub("list", &[]));
        r.register(stub("add", &["a"]));
        let names: Vec<&str> = r.names().collect();
        assert_eq!(names, vec!["run", "list", "add"]);
    }

    #[test]
    fn keys_lists_names_and_aliases_in_insertion_order() {
        let mut r = Registry::new();
        r.register(stub("run", &["r"]));
        r.register(stub("list", &[]));
        r.register(stub("add", &["a"]));
        let keys: Vec<&str> = r.keys().collect();
        assert_eq!(keys, vec!["run", "r", "list", "add", "a"]);
    }

    #[test]
    #[should_panic(expected = "command name already registered: run")]
    fn duplicate_name_panics() {
        let mut r = Registry::new();
        r.register(stub("run", &[]));
        r.register(stub("run", &[]));
    }

    #[test]
    #[should_panic(expected = "command alias already registered: r")]
    fn duplicate_alias_panics() {
        let mut r = Registry::new();
        r.register(stub("run", &["r"]));
        r.register(stub("remove", &["r"]));
    }

    #[test]
    #[should_panic(expected = "command alias already registered: list")]
    fn alias_colliding_with_existing_name_panics() {
        let mut r = Registry::new();
        r.register(stub("list", &[]));
        r.register(stub("ls", &["list"]));
    }
}
