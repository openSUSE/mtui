# Agent Notes — mtui-rs

## Mission
`mtui-rs` is an **improved, idiomatic Rust successor to
[openSUSE/mtui](https://github.com/openSUSE/mtui)** (Maintenance Test Update
Installer) — the SUSE QE tool for validating maintenance updates: load a request
by RRID, install/test it on reference hosts over SSH, then approve/reject. It
drives `osc`/`svn`/Gitea and openQA/QEM under the hood.

This is a **rewrite/redesign, not a 1:1 transpile.** Use MTUI as the behavioral
reference and the source of domain truth, but build something better: memory-safe,
fast, async-native, single static binary, with a cleaner architecture. Break
compatibility only where it clearly improves the tool — and always preserve the
**data-format and workflow contracts** that let mtui-rs interoperate with the SUSE
maintenance ecosystem (see "Contracts" below).

### What "improved successor" means here
- **Safety & robustness:** no interpreter, strong types, exhaustive error enums
  (`thiserror`), no silent `None`-swallowing where a typed `Result` is clearer.
- **Performance:** async I/O (`tokio`), true parallel host fan-out without the
  GIL/threadpool overhead; fast startup; single binary.
- **Distribution:** one static `mtui` + one `mtui-mcp`, no Python runtime, no
  virtualenv, shell completions and man pages generated.
- **Maintainability:** a Cargo workspace with clear crate boundaries and a single
  composition root, so hosts/datasources/testreport stay decoupled.
- **Parity where it matters, better where it helps:** keep the commands, RRID
  grammar, refhosts/testreport formats, and MCP tool surface; modernize the
  internals, the CLI ergonomics, and the packaging.

## Two driving surfaces (keep both working)
Like upstream, there are **two entrypoints**, and command/entrypoint changes must
keep both green:
- `mtui` — the interactive REPL (`reedline`) + non-interactive single-command mode.
- `mtui-mcp` — the MCP server, which **synthesises its tools from the command
  registry**. Adding/renaming/removing a command affects MCP tools automatically.

## Workspace layout
Cargo workspace; each crate has one job. Lower crates never depend on higher ones;
`mtui-core` is the composition root that wires everything.

```
crates/
  mtui-types/        domain types + error hierarchy (no I/O)
  mtui-config/       INI config + XDG paths
  mtui-hosts/        SSH/SFTP (russh), Target/HostsGroup, locks, arbiter   [async]
  mtui-datasources/  shared HTTP, refhosts resolve/search/verify, openQA/QEM/Gitea/osc-qam/oqa-search  [async]
  mtui-testreport/   TestReport lifecycle, metadata parsers, SVN/Gitea checkout, update workflow (actions/checks/export)
  mtui-core/         Command trait + registry + Session + engine + wiring (composition root)
  mtui-cli/          reedline REPL + `mtui` binary
  mtui-mcp/          rmcp server + `mtui-mcp` binary
```
The high-level roadmap (architecture, crate layout, phase overview) lives in
`PLAN-highlevel.md`. The **per-phase task breakdown is tracked in beads** (`br`):
each phase is an epic (`Phase N: …`) whose child tasks carry the confirmed
upstream behavior, crate mapping, DoD, and test strategy migrated from the former
`PLAN-phaseN.md` files. **Before working on a subsystem, run `br ready` for the
next actionable task and `br show <id>` for its detail;** use `br epic status` to
see phase progress. Phase 0 (workspace bootstrap) is already complete (closed).

## Setup & commands
- Build everything: `cargo build --workspace`
- Run the REPL: `cargo run -p mtui-cli -- --help`
- Run the MCP server: `cargo run -p mtui-mcp --features mcp -- --help`
- Format: `cargo fmt --all`
- Lint (warnings are errors): `cargo clippy --workspace --all-targets -- -D warnings`
- Test: `cargo test --workspace`
- Coverage: `cargo llvm-cov --workspace --lcov --output-path lcov.info`
- Feature matrix (catches feature-gate rot):
  `cargo build --workspace --no-default-features` and `--all-features`

## Definition of Done (hard rules)
- Run the **full gate on the whole workspace** before claiming done:
  `cargo fmt --all --check` **and** `cargo clippy --workspace --all-targets -- -D warnings`
  **and** `cargo test --workspace`.
- **"Done" means CI observed green, not predicted green.** Report status from the
  actual run.
- New/changed code needs **>=80% patch coverage**. If a line is genuinely
  un-coverable (best-effort network/error paths), add a focused test or a
  justified allow — never leave coverage silently red.
- Keep **both surfaces** (`mtui`, `mtui-mcp`) building and passing when touching
  commands, `Session`, the registry, or entrypoints.
- Preserve the **Contracts** below unless the task explicitly changes one.

## Architecture (non-obvious bits — mirror these from MTUI)
- **Command registry.** Every command implements the `Command` trait and is
  registered into a central registry (explicit `register_all()`, not Python's
  `__init_subclass__` magic). The REPL dispatch, tab-completion, and the MCP tool
  synthesiser all iterate this one registry — it is the single source of the
  command surface.
- **Session state.** Commands operate on a `Session` (config, `HostsGroup`
  targets, loaded `TestReport`/metadata, display) passed explicitly — the Rust
  replacement for MTUI's `CommandPrompt` god-object. No hidden globals.
- **Composition root.** `mtui-core::wiring` injects the update-workflow
  Doer/Check registries (in `mtui-testreport`) into the host `Target` dispatch
  (in `mtui-hosts`) via traits, so `mtui-hosts` never depends on
  `mtui-testreport`. Do not create crate cycles — add a trait and inject.
- **Config.** **TOML** (intentional deviation from upstream INI — this is a
  redesign, not a 1:1 port), resolved from `--config` → `$MTUI_CONF` →
  `$XDG_CONFIG_HOME/mtui/config.toml` → `/etc/mtui.toml`. When the default pair
  is used, files are merged **lowest-precedence first** so the per-user XDG file
  overrides `/etc` on shared keys. Sectioned tables (`[mtui]`, `[connection]`,
  `[refhosts]`, `[url]`, ...) map to typed options whose defaults match upstream
  mtui exactly. Loading is **lenient**: a missing/malformed file (or a bad value)
  is logged at ERROR and skipped, falling back to defaults; it never hard-fails.
  CLI-arg merging (`merge_args`) lands with the `clap` args in Phase 6.
- **MCP is a thin adapter.** `mtui-mcp` builds one tool per non-denied command by
  converting the command's `clap` arg spec to a JSON schema, reconstructing argv
  from tool kwargs, and dispatching through the **same engine** as the REPL.
  REPL-only commands (`quit`, `exit`, `EOF`, `edit`, `shell`, `help`, `terms`,
  `switch`) are deny-listed; the deny-list ∩ registry is asserted at boot.

## Contracts (do not break without intent — these enable ecosystem interop)
- **RRID grammar** `project:kind:maintenance_id:review_id` and its parse errors.
- **refhosts.yml schema** (location-grouped, merged + deduped; `version.minor` may
  be numeric or `spN`) — parse identically to upstream fixtures.
- **Testreport / export text format**, incl. the `overview_inject` BEGIN/END
  idempotent block under `regression tests:`.
- **Remote lock wire format** — one line `timestamp:user:pid[:comment]` (parsed
  with a 3-way split so the comment keeps embedded colons). Two locks share this
  layout: the operation lock `/var/lock/mtui.lock` (PID-based ownership, guards
  serialized zypper transactions) and the pool-claim lock
  `/var/lock/mtui-pool.lock` (RRID-based ownership; the comment carries
  `mtui pool <RRID> [<owner>]`). A Rust mtui and a Python mtui may share a host
  fleet; snapshot-test this (`crates/mtui-hosts/tests/lock_format.rs`).
- **MCP tool names/schemas** — downstream LLM configs depend on them; snapshot the
  synthesised + slimmed schemas.

Upstream `tests/` fixtures are the authority for these formats. Port the fixtures
and treat them as golden.

## Testing conventions
- Unit tests colocated (`#[cfg(test)]`); integration tests in `crates/*/tests/`.
- **Mock, don't hit the network/hosts:** HTTP via `wiremock`; SSH via a
  `MockConnection` implementing the `Connection` trait; `svn`/`osc` via a
  command-runner trait or a stub on `PATH`.
- **Snapshot text contracts** (`insta`): testreport/export rendering, metadata
  parsing, MCP schemas, lock-file format, display output.
- **Gate real hosts/containers** behind `#[ignore]` + a CI env flag (sshd
  integration fixture); unit tests must run offline and fast.
- Port the corresponding upstream `tests/test_*.py` when porting a module; it
  encodes the exact expected behavior.

## Style & error handling
- Edition 2024, MSRV 1.85. `rustfmt` defaults; `clippy` clean with
  `-D warnings`.
- Errors: `thiserror` enums in library crates, `anyhow` only at binary
  boundaries. Prefer a typed `Result` over MTUI's `log + return None`; where you
  intentionally mirror best-effort degradation, make it explicit and test it.
- Logging: `tracing` (not `log`); levels configurable via CLI + `RUST_LOG`.
- Async everywhere I/O happens (`tokio`); keep pure logic (`mtui-types`) sync and
  I/O-free.
- Never add SSH password auth — MTUI is **pubkey-only by design**; preserve that.

## When adding or changing a command
1. Implement the `Command` trait (name, aliases, `configure` args, async `run`,
   `complete`).
2. Register it in `register_all()`.
3. It is now a REPL command **and** an MCP tool automatically — verify both, and
   check whether it belongs on the MCP deny-list.
4. Add unit tests (arg parsing, `run` against mocks, completion) with >=80%
   patch coverage; snapshot any new text output.
5. Update the command reference docs (prefer generating from the registry).

## Runtime dependencies (subprocess, not crates)
`svn` (testreport checkout), `osc` + `osc-plugin-qam` (osc-qam backend), terminal
emulators for `terms/switch`. Declare as packaging recommends; keep them optional
and degrade gracefully when absent.

## Further reading
- `PLAN-highlevel.md` — the implementation roadmap (architecture, crate layout,
  phased overview). The per-phase task detail now lives in beads (`br epic status`,
  `br show <id>`); the former `PLAN-phase0..8.md` files were migrated into beads.
- Upstream `Documentation/developer.rst` (architecture) and `iui.rst` (interactive
  command reference) remain the deepest behavioral references while porting.

<!-- br-agent-instructions-v1 -->

---

## Beads Workflow Integration

This project uses [beads_rust](https://github.com/Dicklesworthstone/beads_rust) (`br`/`bd`) for issue tracking. Issues are stored in `.beads/` and tracked in git.

### Essential Commands

```bash
# View ready issues (open, unblocked, not deferred)
br ready              # or: bd ready

# List and search
br list --status=open # All open issues
br show <id>          # Full issue details with dependencies
br search "keyword"   # Full-text search

# Create and update
br create --title="..." --description="..." --type=task --priority=2
br update <id> --status=in_progress
br close <id> --reason="Completed"
br close <id1> <id2>  # Close multiple issues at once

# Sync with git
br sync --flush-only  # Export DB to JSONL
br sync --status      # Check sync status
```

### Workflow Pattern

1. **Start**: Run `br ready` to find actionable work
2. **Claim**: Use `br update <id> --status=in_progress`
3. **Work**: Implement the task
4. **Complete**: Use `br close <id>`
5. **Sync**: Always run `br sync --flush-only` at session end

### Key Concepts

- **Dependencies**: Issues can block other issues. `br ready` shows only open, unblocked work.
- **Priority**: P0=critical, P1=high, P2=medium, P3=low, P4=backlog (use numbers 0-4, not words)
- **Types**: task, bug, feature, epic, chore, docs, question
- **Blocking**: `br dep add <issue> <depends-on>` to add dependencies

### Session Protocol

**Before ending any session, run this checklist:**

```bash
git status              # Check what changed
git add <files>         # Stage code changes
br sync --flush-only    # Export beads changes to JSONL
git commit -m "..."     # Commit everything
git push                # Push to remote
```

### Best Practices

- Check `br ready` at session start to find available work
- Update status as you work (in_progress → closed)
- Create new issues with `br create` when you discover tasks
- Use descriptive titles and set appropriate priority/type
- Always sync before ending session

<!-- end-br-agent-instructions -->
