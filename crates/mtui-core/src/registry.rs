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
        assert_eq!(r.len(), 10);
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
