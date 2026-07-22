# Agent Notes — mtui

## Mission
`mtui` is an **improved, idiomatic Rust successor to
[openSUSE/mtui](https://github.com/openSUSE/mtui)** (Maintenance Test Update
Installer) — the SUSE QE tool for validating maintenance updates: load a request
by RRID, install/test it on reference hosts over SSH, then approve/reject. It
drives `osc`/`svn`/Gitea and openQA/QEM under the hood.

This is a **rewrite/redesign, not a 1:1 transpile.** Use MTUI as the behavioral
reference and the source of domain truth, but build something better: memory-safe,
fast, async-native, single static binary, with a cleaner architecture. Break
compatibility only where it clearly improves the tool — and always preserve the
**data-format and workflow contracts** that let mtui interoperate with the SUSE
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
- `mtui` — the interactive REPL (`reedline`).
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
  mtui-datasources/  shared HTTP, refhosts resolve/search/verify, openQA/QEM/Gitea/native-OBS-QAM/oqa-search  [async]
  mtui-testreport/   TestReport lifecycle, metadata parsers, SVN/Gitea checkout, update workflow (actions/checks/export)
  mtui-core/         Command trait + registry + Session + engine + wiring (composition root)
  mtui-cli/          reedline REPL + `mtui` binary
  mtui-mcp/          rmcp server + `mtui-mcp` binary
```
Task breakdown is tracked in the project's issue tracker; check it for the
next actionable task before working on a subsystem.

## Setup & commands
- Build everything: `cargo build --workspace`
- Run the REPL: `cargo run -p mtui-cli -- --help`
- Run the MCP server: `cargo run -p mtui-mcp --features mcp -- --help`
- Format: `cargo fmt --all`
- Lint (warnings are errors): `cargo clippy --workspace --all-targets -- -D warnings`
- Test: `cargo test --workspace` — **the cost is compilation, not test
  execution.** A cold `cargo build --workspace --tests` is ~80s; the actual test
  *run*, once compiled, is only ~20-25s. The default 120s timeout is exceeded
  only when compiling from cold, so allow ≥300000 ms (5 min) on the first run of
  a session; a second run against a warm `target/` cache is seconds.
- Coverage: `cargo llvm-cov --workspace --lcov --output-path lcov.info`
- Feature matrix (catches feature-gate rot) — **compile-only, and ~doubles build
  time** (the `mcp` feature pulls in `axum` + `rmcp` streamable-http):
  `cargo build --workspace --no-default-features` and `--all-features`. Do **not**
  routinely *test* `--all-features`; CI only compiles it (`.gitlab-ci.yml`
  feature-matrix job).

### Fast local iteration
- **Keep the cache warm and scope tight.** During dev, run
  `cargo test -p <crate>` for the crate you're touching, not the whole workspace
  — reserve `cargo test --workspace` for the final gate.
- **Test default features only while iterating.** `--all-features` relinks the
  whole mcp/axum tree (~95s even warm) for no runtime signal beyond what the
  compile-only feature matrix already gives.
- **`mtui-cli`'s lib suite is the slow one (~8s).** Its `edit`/`shell` tests spawn
  real editor/shell subprocesses. When working elsewhere, don't rerun it.

## Definition of Done (hard rules)
- Run the **full gate on the whole workspace** before claiming done, mirroring
  CI: `cargo fmt --all --check` **and**
  `cargo clippy --workspace --all-targets -- -D warnings` **and**
  `cargo test --workspace` (default features) **and** the compile-only feature
  matrix `cargo build --workspace --no-default-features` +
  `cargo build --workspace --all-features`. Tests run against default features
  only — do **not** run `--all-features` *tests*. The long pole is cold
  compilation, not the test run itself, so give the first `cargo test --workspace`
  (and the feature-matrix builds) a generous timeout (≥300000 ms) and don't treat
  an early timeout as a failure.
- **"Done" means CI observed green, not predicted green.** Report status from the
  actual run.
- New/changed code needs **>=80% patch coverage**. If a line is genuinely
  un-coverable (best-effort network/error paths), add a focused test or a
  justified allow — never leave coverage silently red.
- Keep **both surfaces** (`mtui`, `mtui-mcp`) building and passing when touching
  commands, `Session`, the registry, or entrypoints.
- Preserve the **Contracts** below unless the task explicitly changes one.
- **Changelog:** add an entry to the top-most unreleased section of
  `CHANGELOG.md` for user-visible changes (new/changed/removed commands or
  flags, behavior a user or MCP client would notice, config format changes).
  Internal refactors, perf/implementation-only fixes, and chore/CI-only
  changes do not need one (`CONTRIBUTING.md` § Changelog is authoritative).

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
  CLI-arg merging (mirroring upstream `Config.merge_args`) is implemented via
  `Args::apply_to` (`crates/mtui-core/src/args.rs`), the highest-precedence
  config layer, applied after `Config::load`.
- **MCP is a thin adapter.** `mtui-mcp` builds one tool per non-denied command by
  converting the command's `clap` arg spec to a JSON schema, reconstructing argv
  from tool kwargs, and dispatching through the **same engine** as the REPL.
  REPL-only commands (`quit`, `exit`, `EOF`, `edit`, `shell`, `help`, `terms`,
  `switch`) are deny-listed; the deny-list ∩ registry is consistency-tested and
  drift is warned about at boot. Local process execution has no command at all —
  `lrun` was removed by design; do not reintroduce it.

## Contracts (do not break without intent — these enable ecosystem interop)
- **RRID grammar** `project:kind:maintenance_id:review_id` and its parse errors.
- **refhosts.yml schema** — the file is still location-grouped *on disk*, but
  location is a legacy grouping, not a live query dimension: rows are
  merged/flattened and de-duplicated at load (`version.minor` may be numeric or
  `spN`). Parse identically to upstream fixtures.
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
- **One integration-test binary per crate.** Each crate's integration tests are
  consolidated into a single `tests/it.rs` (`#[path = "<file>.rs"] mod <file>;`
  per file) with `autotests = false` + `[[test]] name = "it"` in `Cargo.toml`, so
  the crate + its heavy deps link **once**, not once per file (this is the main
  test-compile speedup). **Add a new integration test as a `mod` line in
  `tests/it.rs`, not as a new top-level `tests/*.rs`** (a new top-level file
  would be silently ignored under `autotests = false`, or reintroduce a per-file
  binary if you re-enable discovery). Because all a crate's integration tests now
  share one process, anything touching a **process-global** (env vars, the
  `set_test_sink` spinner sink) must be serialised with `#[serial(<name>)]`
  (`serial_test`), and tests must not assume per-binary isolation (e.g. no
  asserting on heap-address identity — a freed `Arc` address can be reused).
- **Mock, don't hit the network/hosts:** HTTP via `wiremock`; SSH via a
  `MockConnection` implementing the `Connection` trait; `svn`/`osc` via a
  command-runner trait or a stub on `PATH`.
- **Snapshot text contracts** (`insta`): testreport/export rendering, metadata
  parsing, MCP schemas, lock-file format, display output. `insta` prefixes each
  `.snap` file with the **test-binary** name, which is now `it` for every crate —
  so snapshot files are `it__<module>__<name>.snap`. A new snapshot test's file
  lands with that prefix automatically; don't hand-name it otherwise.
- **Gate real hosts/containers** behind `#[ignore]` + a CI env flag (sshd
  integration fixture); unit tests must run offline and fast.
- Port the corresponding upstream `tests/test_*.py` when porting a module; it
  encodes the exact expected behavior.

## Style & error handling
- Edition 2024, MSRV 1.96. `rustfmt` defaults; `clippy` clean with
  `-D warnings`.
- Errors: `thiserror` enums in library crates, `anyhow` only at binary
  boundaries. Prefer a typed `Result` over MTUI's `log + return None`; where you
  intentionally mirror best-effort degradation, make it explicit and test it.
- Logging: `tracing` (not `log`); levels configurable via CLI + `RUST_LOG`.
- Async everywhere I/O happens (`tokio`); keep pure logic (`mtui-types`) sync and
  I/O-free.
- **Never leak secrets to output or logs.** Secret config fields (currently just
  `gitea_token`, classified by `is_secret_attr` in the `config` command) are
  masked (`<set>`) by both `config show` and `config set` — never echo their
  value to the display buffer (it reaches terminal scrollback and MCP output).
  Configured datasource URLs may embed credentials (`scheme://user:pass@host`);
  always run a URL through `mtui_datasources::sanitize_url` before logging it or
  putting it in an error (it replaces userinfo with `***` while keeping the host
  for diagnosis). The Gitea token travels in an `Authorization` header and is
  never logged.
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
`svn` (testreport checkout) and terminal emulators for `terms/switch`. Declare as
packaging recommends; keep them optional and degrade gracefully when absent. The
QAM review workflow (`assign`/`unassign`/`approve`/`reject`/`comment`) no longer
shells out to `osc`/`osc-plugin-qam` — it talks to the OBS/IBS API natively (see
the native OBS backend and `[obs]` config), reading credentials from `oscrc`.

## Further reading
- `docs/src/architecture.md` — architecture map (crate layout, composition root,
  contracts) and the rest of the mdBook under `docs/src/` (installation,
  configuration, developer, invocation, mcp).
- The former Python implementation and its `Documentation/*.rst` (architecture,
  interactive command reference) remain the deepest behavioral references while
  porting; they are preserved on the `archive/python-main` tag and the
  `16.0.x`..`19.0.x` maintenance branches.
