//! The explicit command registry.
//!
//! Port of upstream `mtui.commands.Command.registry`, which is auto-populated by
//! the `Command.__init_subclass__` hook at class-creation time. This is a
//! redesign, not a 1:1 port: per `AGENTS.md`, the implicit registration magic is
//! replaced by an **explicit** [`register_all`] composition point. The REPL
//! dispatch, tab-completion, and the MCP tool synthesiser all iterate this one
//! [`Registry`] — it is the single source of the command surface.
//!
//! A command answers to its [`name`](crate::Command::name) and any
//! [`aliases`](crate::Command::aliases); every one of those strings maps to the
//! same command instance. Two commands claiming the same name (or alias) is a
//! programming error, caught the way upstream catches it — at wiring time.
//! Upstream raises `CommandAlreadyBoundError` when the duplicate class is
//! created; here [`Registry::register`] **panics** when it is wired, so the
//! composition root ([`register_all`]) fails fast at boot.

use std::sync::Arc;

use indexmap::IndexMap;

use crate::command::Command;

/// A name→command lookup that preserves registration order.
///
/// Both the canonical name and every alias key the same [`Arc<dyn Command>`], so
/// lookup is uniform. Iteration order ([`names`](Registry::names)) follows
/// registration order (canonical names only), giving the REPL and MCP a stable,
/// deterministic command listing.
#[derive(Default)]
pub struct Registry {
    /// name-or-alias → command. Insertion-ordered so the first-inserted keys
    /// (canonical names, since a command registers its name before its aliases)
    /// drive a deterministic listing.
    by_key: IndexMap<&'static str, Arc<dyn Command>>,
    /// Canonical names in registration order (the subset of `by_key` keys that
    /// are a command's own `name()`), so [`names`](Registry::names) never lists
    /// aliases.
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
    /// Panics if the command's name or any alias is already claimed by another
    /// command. This mirrors upstream `CommandAlreadyBoundError`: a duplicate
    /// command string is a static programming error, so the composition root
    /// fails fast at boot rather than silently shadowing a command.
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

    /// Every command key — canonical names **and** aliases — in insertion
    /// order (a command's own name precedes its aliases).
    ///
    /// This mirrors upstream `_completer.py`, whose first-token completion
    /// iterates `prompt.commands` (which carries aliases as distinct keys). Use
    /// this for alias-aware first-token completion; contrast with [`names`],
    /// which is canonical-only and drives the REPL/MCP command listing.
    ///
    /// [`names`]: Registry::names
    pub fn keys(&self) -> impl Iterator<Item = &'static str> + '_ {
        self.by_key.keys().copied()
    }

    /// The number of distinct commands registered (aliases not counted).
    #[must_use]
    pub fn len(&self) -> usize {
        self.canonical.len()
    }

    /// `true` if no commands are registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.canonical.is_empty()
    }
}

/// Commands that are **REPL-only** and must not be synthesised into MCP tools.
///
/// Ports upstream's MCP deny-list (`AGENTS.md`): these commands drive the
/// interactive shell (exit the loop, move the hidden active-template pointer,
/// attach a PTY) or otherwise have no meaning for a headless client. The Phase-7
/// `mtui-mcp` tool synthesiser skips every registry command whose name or alias
/// appears here, and asserts at boot that the deny-list ∩ registry is exactly
/// this set (so a renamed/removed command can't silently leak an interactive
/// tool). Kept here, beside [`register_all`], so the deny-list and the command
/// surface it filters live in one place.
///
/// Names not yet backed by a registered command (`terms`) are reserved for their
/// later waves; [`mcp_denylist_is_consistent`] tolerates them so adding the
/// command later does not require touching this list.
pub const MCP_DENYLIST: &[&str] = &[
    "quit", "exit", "EOF",    // session exit (Wave 2)
    "switch", // active-template pointer, REPL-only (Wave 2)
    "shell",  // interactive PTY attach, Phase 6 (Wave 2)
    "help",   // registry listing / per-command help, REPL-only (Phase 6)
    "edit",   // $EDITOR spawn on the controlling TTY, REPL-only (Phase 6)
    "terms",  // later wave; reserved
];

/// Builds the process-wide command registry — the single, explicit place every
/// command is wired (replacing upstream's `__init_subclass__` auto-discovery).
///
/// The command waves (Phase 5.6+) register their commands here; until then this
/// returns an empty registry. Both the REPL (`mtui`) and MCP (`mtui-mcp`) build
/// their command surface from the [`Registry`] this returns, so a command added
/// here becomes a REPL command **and** an MCP tool automatically.
#[must_use]
pub fn register_all() -> Registry {
    use crate::commands;

    let mut registry = Registry::new();
    // Wave 1 — core workflow (gates the phase).
    registry.register(Arc::new(commands::Run));
    registry.register(Arc::new(commands::LocalRun));
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
    // Phase 5 follow-ups — deferred commands now unblocked.
    registry.register(Arc::new(commands::Export));
    registry.register(Arc::new(commands::ListRefhosts));
    registry.register(Arc::new(commands::LoadTemplate));
    // Phase 6 — REPL-only command-surface additions.
    registry.register(Arc::new(commands::Help));
    registry.register(Arc::new(commands::Edit));
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
        assert!(r.is_empty());
        assert_eq!(r.len(), 0);
        assert!(r.get("run").is_none());
    }

    #[test]
    fn register_all_wires_wave1_commands() {
        let r = register_all();
        // Wave 1 — core workflow.
        for name in [
            "run",
            "lrun",
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
        // Wave 2 — host & session management.
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
        // `quit` also answers to its REPL aliases.
        assert!(r.contains("exit"));
        assert!(r.contains("EOF"));
    }

    #[test]
    fn register_all_wires_wave3_commands() {
        let r = register_all();
        // Wave 3 — testreport lifecycle, metadata & host-info.
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
        // Wave 4 — backend APIs, openQA/QEM queue & workflow.
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
        ] {
            assert!(r.contains(name), "expected {name} to be registered");
        }
    }

    #[test]
    fn register_all_command_count() {
        // 10 Wave 1 + 14 Wave 2 + 17 Wave 3 + 11 Wave 4 + 4 P5 follow-ups
        // (export, list_refhosts, load_template, list_locks) + 2 openQA-holder
        // follow-ups (reload_openqa, set_workflow: mtui-rs-zs4/plt) + 2 Phase 6
        // (help: mtui-rs-lhz.9; edit: mtui-rs-lhz.10) = 60 canonical commands.
        assert_eq!(register_all().len(), 60);
    }

    #[test]
    fn register_all_wires_phase5_followups() {
        let r = register_all();
        for name in ["export", "list_refhosts", "load_template"] {
            assert!(r.contains(name), "expected {name} to be registered");
        }
    }

    #[test]
    fn load_template_is_not_mcp_denylisted() {
        // load_template is a valid headless tool (it names its own RRID), so it
        // must NOT be deny-listed — it should synthesise an MCP tool.
        assert!(!MCP_DENYLIST.contains(&"load_template"));
    }

    #[test]
    fn mcp_denylist_covers_wave2_repl_only_commands() {
        // The REPL-only Wave 2 commands must be denied MCP tool synthesis.
        for name in ["quit", "exit", "EOF", "switch", "shell"] {
            assert!(
                MCP_DENYLIST.contains(&name),
                "{name} must be on the MCP deny-list"
            );
        }
    }

    #[test]
    fn mcp_denylist_is_consistent() {
        // Every deny-listed name that is *registered* must be an interactive
        // command (present as a name or alias); names not yet backed by a
        // command (reserved for later waves) are tolerated. Nothing in the
        // deny-list may be a duplicate.
        let r = register_all();
        let mut seen = std::collections::HashSet::new();
        for name in MCP_DENYLIST {
            assert!(seen.insert(*name), "duplicate deny-list entry: {name}");
            // A registered deny-listed name resolves; an unregistered one is a
            // reserved placeholder for a later wave.
            let _reserved_or_registered = r.contains(name);
        }
        // Sanity: the currently-registered deny-listed commands are the Wave 2
        // REPL-only set (quit+aliases, switch, shell) plus `help` and `edit`
        // (Phase 6). The reserved name `terms` is not yet registered.
        let registered_denied: Vec<&str> = MCP_DENYLIST
            .iter()
            .copied()
            .filter(|n| r.contains(n))
            .collect();
        assert_eq!(
            registered_denied,
            vec!["quit", "exit", "EOF", "switch", "shell", "help", "edit"]
        );
    }

    #[test]
    fn name_and_alias_resolve_to_same_command() {
        let mut r = Registry::new();
        r.register(stub("run", &["r", "exec"]));
        assert!(r.contains("run"));
        assert!(r.contains("r"));
        assert!(r.contains("exec"));
        // aliases don't inflate the canonical count.
        assert_eq!(r.len(), 1);
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
        // Each command's canonical name precedes its own aliases (upstream
        // `prompt.commands` dict-iteration order); commands stay in
        // registration order.
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
