# Phase 5 — `mtui-core` + 46 commands (detailed)

Goal: build the **command engine** (the trait, registry, shared session state,
and display layer) and port all **46 commands**. This is the phase that makes
mtui *do* things — every prior crate is wired together here behind a uniform
command surface consumed by both the REPL (Phase 6) and MCP (Phase 7).

**Size:** large (the biggest phase by breadth). **Prereqs:** Phase 0–4.
**Blocks:** Phase 6 (REPL), Phase 7 (MCP).

Source grounding (upstream `main`): `mtui/commands/` (46 files, ~166K),
`mtui/main.py` (entry), plus the parts of `mtui/cli/` that are engine-level
(`display.py`, `args.py`, `argparse.py`, `term.py`, `prompter.py`,
`completion.py`) — the interactive REPL shell itself (`repl.py`, `_completer.py`,
`_history.py`, `_lexer.py`, colors) is **Phase 6**.

---

## 5.1 The command framework (confirmed mechanics)

From `commands/_command.py` + `commands/__init__.py`:
- `Command` is an **ABC** with a `command` class attribute (the name) and
  **auto-registration** via `__init_subclass__` into a class-level
  `registry: dict[str, type[Command]]`.
- `commands/__init__.py` walks the package (`pkgutil.iter_modules`) importing
  every non-underscore module so registration happens as an import side-effect.
- Each command provides: `add_arguments(parser)` (argparse), `run()`, and a
  **static `complete(state, text, line, begidx, endidx)`** for tab completion.
- A command instance is built with `(args, sys, config, prompt)` and reads shared
  state: `prompt.templates`, `prompt.metadata` (the loaded `TestReport`),
  `prompt.display`, `prompt.targets` (the `HostsGroup`), `prompt.config`.
- `main.py`: parse args → configure color/logging → dispatch a **single command
  (non-interactive)** or launch the **REPL**.

## 5.2 `mtui-core` design

```
crates/mtui-core/src/
├── lib.rs
├── command/
│   ├── mod.rs        # Command trait + CommandResult + errors
│   ├── registry.rs   # static registry (see 5.3) + name/alias lookup
│   └── args.rs       # per-command arg parsing (clap builder or manual)
├── session.rs        # Session state (was `CommandPrompt` fields)
├── display.rs        # CommandPromptDisplay port (colored output)
├── engine.rs         # dispatch: parse line -> command -> run
├── wiring.rs         # inject Phase-4 Doer/Check registries into Phase-2 hosts
└── commands/         # the 46 ported commands (see 5.5)
```

### `Command` trait
```rust
trait Command {
    const NAME: &'static str;
    const ALIASES: &'static [&'static str] = &[];
    fn configure(cmd: clap::Command) -> clap::Command; // add_arguments
    async fn run(&self, ctx: &mut Session, args: &ArgMatches) -> Result<()>;
    fn complete(state: &Session, line: &str, word: &str) -> Vec<String>; // tab completion
}
```
- **Async** `run` — commands fan out over hosts (Phase 2 is async) and call HTTP
  clients (Phase 3 is async).
- `Session` (`session.rs`) holds `config`, `targets: HostsGroup`,
  `metadata: Box<dyn TestReport>` (Null by default), `display`, `templates`,
  history handle. Replaces the Python `CommandPrompt` god-object; keep it a plain
  struct passed by `&mut`.

## 5.3 Registry: Python side-effect import → explicit Rust registry

Rust has no `__init_subclass__`. Options:
- **(a) `inventory` / `linkme` crate** — distributed slice; each command file
  registers itself via a macro at link time (closest to Python's auto-register).
- **(b) explicit registration** — one `register_all()` fn listing every command.
  Simpler, greppable, no proc-macro magic.
**Decision:** start with **(b) explicit** for clarity; revisit `inventory` if the
list becomes a maintenance burden. Registry maps `name/alias → CommandFactory`.

## 5.4 Display + args (engine-level cli/)

- `display.rs`: port `CommandPromptDisplay` — formats `HostsGroup` results,
  `System`, `RPMVersion`, `TargetState`, `ExecutionMode` with color (green/red/
  yellow). Use `owo-colors`/`nu-ansi-term`; respect a color-mode toggle
  (port `cli/colors/`). Output goes through a pager (`term.page`) — port with a
  `less`-style pager or `minus` crate (feature-gated; plain print fallback).
- `args.rs` (top-level, from `cli/args.py`): the global `mtui` argument parser
  (RRID/SUT parsing via `AutoOBSUpdateID`/`KernelOBSUpdateID`, `--config`,
  log level, color mode, version incl. dep versions). Build with `clap`.
- `prompter.rs`/`term.rs` bits used by commands (timeout prompt, `prompt_user`) —
  port the non-REPL pieces here; full-line-editor REPL is Phase 6.

## 5.5 The 46 commands — grouping & port order

Bundled files (upstream groups several commands per module — preserve or split):
- `simplelists.py` → multiple `List*` commands (bugs, packages, hosts, …).
- `simpleset.py` → multiple `Set*` commands (workflow, repo, host state, …).
- `apicall.py` → `BaseApiCall` ABC + `assign`/`unassign`/`reject`/`comment`
  (dispatches OSC vs Gitea backend); `approve.py` is separate.

Port order (deliver a usable core early):

**Wave 1 — core workflow (gates the phase):**
`run`, `localrun`, `update`, `install`(via updates/simpleset), `prepare`,
`downgrade`, `reboot`, `zypper`, `setrepo`, `showrepos`.

**Wave 2 — host & session management:**
`addhost`, `removehost`, `hoststate`, `hostslock`, `hostsunlock`, `switch`,
`list_refhosts`, `loadtemplate`(`load_template`), `unload`, `reload`,
`templates`, `whoami`, `products`, `config`, `quit`, `shell`.

**Wave 3 — test-report & files:**
`checkout`, `commit`, `edit`, `export`, `sftpcmd`(`put`/`get`), `listpackages`,
`showdiff`, `checkers`, `terms`.

**Wave 4 — data/query (heaviest):**
`openqa_overview` (14.9K — port of `oqa-search` overview), `openqa_jobs`,
`updates`, `approve`, `apicall` (assign/unassign/reject/comment), `regenerate`,
`reloadoqa`, `help`.

Each command: define struct → `configure` (args) → `run` (calls Phase 2/3/4
services via `Session`) → `complete` → port its `test_cmd_*.py`.

## 5.6 Wiring the cross-crate seams (`wiring.rs`)

Phase 4 flagged the **hosts↔testreport** action/check injection. `mtui-core` is
where it's resolved: at startup, register the Phase-4 `Doer`/`Check` registries
into the Phase-2 `Target` dispatch (trait objects / function pointers). Also
constructs the shared HTTP client (Phase 3), the refhosts factory, and the
default `NullReport`. Keep lower crates dependency-free of each other; `core` is
the composition root.

## 5.7 Test strategy

- **39 `test_cmd_*.py` files (~112K)** → per-command Rust tests. Each exercises
  `run` against a `Session` built with Phase-2 `MockConnection` + Phase-3
  `wiremock` + a fixture `TestReport`.
- Command **arg-parsing** tests (accept/reject flags, defaults).
- **Completion** tests (`complete()` returns expected candidates) — feeds Phase 6.
- **Display** snapshots (`insta`) for `CommandPromptDisplay` output.
- **Golden end-to-end:** `run "uname -a"` across ≥2 hosts on the Phase-2 sshd
  fixture, asserting aggregated + colored output shape.

Key upstream tests to port: `test_cmd_run`, `test_cmd_apicall`, `test_cmd_help`,
`test_cmd_updates`, `test_cmd_add/removehost`, `test_cmd_showdiff`,
`test_cmd_regenerate`, `test_cmd_approve`, `test_cmd_loadtemplate`,
`test_cmd_export/commit/checkout`, `test_cmd_simplelists`, `test_cmd_localrun`,
`test_cmd_quit`, `test_cmd_config`, `test_cmd_products`, `test_cmd_hostslock/
hoststate/hostsunlock`, `test_cmd_templates`.

## 5.8 Task breakdown

1. `Command` trait + `CommandResult`/errors + `Session` struct.
2. `registry.rs` (explicit registration) + `engine.rs` (line → dispatch).
3. `display.rs` (`CommandPromptDisplay`) + color mode + pager.
4. `args.rs` top-level parser (`clap`) + RRID/SUT parsing hookup.
5. `wiring.rs` composition root (inject Doer/Check, build HTTP/refhosts).
6. Wave 1 commands + tests (gates phase: e2e `run`).
7. Wave 2 commands + tests.
8. Wave 3 commands + tests.
9. Wave 4 commands + tests (openqa_overview, apicall, approve, updates…).
10. Non-interactive `main`-style dispatch entrypoint (single-command mode) —
    the binary itself is Phase 6, but core exposes the dispatch API here.

## 5.9 Deliverables (files)

- **Create:** `crates/mtui-core/src/{lib.rs, command/**, session.rs, display.rs,`
  `engine.rs, wiring.rs, commands/*.rs (≈46)}` + `crates/mtui-core/tests/**`.
- **Modify:** `crates/mtui-core/Cargo.toml` — deps on all lower crates + `clap`,
  `owo-colors`/`nu-ansi-term`, `minus` (feature `pager`), `tokio`, `tracing`,
  `thiserror`; dev `insta`, `wiremock`.
- **Possibly modify:** `mtui-hosts` (finalize Doer/Check traits),
  `mtui-testreport` (expose registry constructors).

## 5.10 Risks / decisions to confirm

- **Registry mechanism** — explicit vs `inventory`. Explicit chosen for
  greppability; ~46 entries is manageable. Confirm before step 2.
- **`Session` shape** — the Python `CommandPrompt` is a large mutable object;
  resist replicating it verbatim. Model as a plain struct with clear ownership;
  commands take `&mut Session`. Async + `&mut` can fight the borrow checker for
  concurrent host fan-out — may need `Arc<Mutex<_>>` around the hostgroup.
- **Bundled command files** — decide whether to keep `simplelists`/`simpleset`/
  `apicall` as grouped modules or split one-command-per-file (Rust convention
  leans split; grouping eases the ABC-dispatch commands like `apicall`).
- **argparse → clap fidelity** — mtui uses `REMAINDER`, custom `ArgumentParser`
  (no-exit-on-error for REPL), per-command `--help`. Ensure clap replicates
  no-process-exit parsing inside the REPL (Phase 6 depends on this).
- **Pager** — interactive paging (`term.page`) needs a TTY; feature-gate and
  fall back to plain stdout for non-interactive/MCP use.
- **`apicall` backend dispatch** — OSC (subprocess, Phase 3 oscqam) vs Gitea
  (HTTP, Phase 3) selected at runtime; preserve the `BaseApiCall` dispatch logic.

## 5.11 Out of scope for Phase 5

No interactive line editor / REPL loop (completion *logic* is here, but the
`reedline` shell, history, lexer, toolbar are **Phase 6**); no MCP server
(**Phase 7**). Phase 5 delivers the command engine + all 46 commands, dispatchable
non-interactively and unit-tested.
