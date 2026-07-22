# Developer guide

This guide covers the day-to-day mtui-rs development workflow: the toolchain, the
quality gates, how the command layer is structured, and how to add a command.
[Architecture](architecture.md) has the higher-level crate map and the data-format
contracts; `AGENTS.md` at the repo root is the authoritative contributor spec and
definition of done.

## Toolchain

mtui-rs uses the standard Rust toolchain — no `rustup` is assumed (the reference
dev box runs a Homebrew `rustc`). Edition 2024, **MSRV 1.96**, pinned via
`rust-version` in `Cargo.toml`.

| Task | Command |
|------|---------|
| Build everything | `cargo build --workspace` |
| Run the REPL | `cargo run -p mtui-cli -- --help` |
| Run the MCP server | `cargo run -p mtui-mcp --features mcp -- --help` |
| Format | `cargo fmt --all` |
| Lint (warnings are errors) | `cargo clippy --workspace --all-targets -- -D warnings` |
| Test | `cargo test --workspace` |
| Coverage | `cargo llvm-cov --workspace --lcov --output-path lcov.info` |

Repo automation lives in the `xtask` crate, invoked through the alias in
`.cargo/config.toml`:

| Task | Command | Effect |
|------|---------|--------|
| Regenerate packaging artifacts | `cargo xtask gen` | `dist/completions/{bash,zsh,fish}/…` + `dist/man/*.1` |
| Regenerate generated docs | `cargo xtask gen-docs` | `docs/src/cli.md` + `docs/src/invocation.md` |
| Build a release tarball | `cargo xtask package --version <VER> --target <TRIPLE>` | `dist/release/…tar.gz` |

## Quality gates

CI (`.gitlab-ci.yml`) mirrors the local gate. Run the **whole workspace** before
claiming done:

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace                       # default features only
cargo build --workspace --no-default-features # feature matrix (compile-only)
cargo build --workspace --all-features        # feature matrix (compile-only)
```

Notes that save time:

- **The cost is compilation, not test execution.** A cold
  `cargo build --workspace --tests` is ~80 s; the test *run* is ~20-25 s. Give the
  first `cargo test --workspace` of a session a generous timeout (≥5 min) and
  don't treat a cold-cache timeout as a failure.
- **Test default features only while iterating.** `--all-features` relinks the
  whole `mcp`/`axum` tree for no runtime signal beyond what the compile-only
  feature matrix already gives. CI only *compiles* `--all-features`; it does not
  *test* it.
- **Scope tight during dev:** `cargo test -p <crate>` for the crate you're
  touching; reserve `cargo test --workspace` for the final gate.
- `mtui-cli`'s lib suite is the slow one (its `edit`/`shell` tests spawn real
  editor/shell subprocesses) — don't rerun it when working elsewhere.

New or changed code needs **≥80% patch coverage**. For a genuinely un-coverable
best-effort network/error path, add a focused test or a justified `allow` — never
leave coverage silently red.

## Command architecture

Every interactive command implements one `Command` trait
(`crates/mtui-core/src/command.rs`) and is registered into a central registry by an
explicit `register_all()` (`crates/mtui-core/src/registry.rs`). The REPL dispatch,
tab-completion, the generated [command reference](cli.md), and the `mtui-mcp` tool
synthesiser all iterate that **one** registry — so the surfaces can't drift.

The trait has just **two required methods**; everything else has a default:

| Method | Signature | Default |
|--------|-----------|---------|
| `name` | `fn name(&self) -> &'static str` | *(required)* |
| `call` | `async fn call(&self, session: &mut Session, args: &ArgMatches) -> CommandResult` | *(required)* — the per-template command body |
| `aliases` | `fn aliases(&self) -> &'static [&'static str]` | `&[]` |
| `about` | `fn about(&self) -> Option<&'static str>` | `None` |
| `scope` | `fn scope(&self) -> Scope` | `Scope::Active` |
| `mutates_registry` | `fn mutates_registry(&self) -> bool` | `false` |
| `skip_hostless_templates` | `fn skip_hostless_templates(&self) -> bool` | `true` |
| `configure` | `fn configure(&self, cmd: clap::Command) -> clap::Command` | identity (no args) |
| `complete` | `fn complete(&self, session: &Session, text: &str, line: &str) -> Vec<String>` | `Vec::new()` |

> The command body is **`call()`**, not `run()`. The trait's provided `run()` is
> the fan-out driver that resolves the target template(s) (per
> [Workflow concepts](concepts.md#fan-out-across-templates)) and invokes `call()`
> once per template — commands normally do **not** override `run()`.

Commands operate on an explicit `Session` (config, host targets, loaded templates,
display) passed into each call. There are no hidden globals.

### How to add a command

1. **Implement the trait** in `crates/mtui-core/src/commands/<name>.rs`. A minimal
   command (model: `commands/whoami.rs`):

   ```rust
   use async_trait::async_trait;
   use clap::ArgMatches;

   use crate::command::Command;
   use crate::error::CommandResult;
   use crate::session::Session;

   /// One-line summary shown by `help` and in the generated CLI reference.
   pub struct Foo;

   #[async_trait]
   impl Command for Foo {
       fn name(&self) -> &'static str {
           "foo"
       }

       fn about(&self) -> Option<&'static str> {
           Some("Does the foo thing.")
       }

       async fn call(&self, session: &mut Session, _args: &ArgMatches) -> CommandResult {
           session.display.println("hello, foo");
           Ok(())
       }
   }
   ```

   For a command with arguments, override `configure` to add `clap::Arg`s and read
   them from `args` in `call`; for completion, override `complete`; for a
   non-default fan-out policy, override `scope`. `commands/switch.rs` is a compact
   model that exercises all three (a required positional, `Scope::Single`,
   `mutates_registry`, and a prefix completer).

2. **Re-export** the type from `commands/mod.rs` (the `pub use` block).

3. **Register** it with one line in `register_all()`
   (`crates/mtui-core/src/registry.rs`):

   ```rust
   registry.register(Arc::new(commands::Foo));
   ```

   It is now a REPL command **and** an MCP tool automatically.

4. **Decide MCP exposure.** If the command is REPL-only (needs a TTY) or executes
   locally, add it to `MCP_DENYLIST` in `registry.rs`. The deny-list is
   intersected with the live registry and consistency-tested, so a stale entry is
   warned about at boot.

5. **Add tests** (see below) with ≥80% patch coverage, and **snapshot** any new
   text output.

6. **Regenerate the docs:** `cargo xtask gen-docs` picks the new command up into
   [`cli.md`](cli.md) automatically (the drift-guard test fails until you commit
   the regenerated file). Add behavioural prose to
   [Workflow concepts](concepts.md) only if the command introduces cross-cutting
   behaviour the per-command help can't express.

## Testing conventions

- **Unit tests are colocated** in a `#[cfg(test)] mod tests` in the same file
  (see `commands/whoami.rs` and `commands/switch.rs`). Cover arg parsing, `call`
  against a mock session, the error path, and completion.
- **Shared scaffolding** lives in `crates/mtui-core/src/commands/mod.rs`'s
  `testkit` module: `empty_session()`, `session_with_hosts(...)`,
  `session_with_targets(...)`, a `FakeReport` test double, a capturing display
  `Buffer`, and `matches(command, argv)` — the helper that runs a command's own
  clap parser exactly as the engine does.
- **Mock, don't hit the network or real hosts.** SSH goes through
  `MockConnection` (`crates/mtui-hosts/src/connection/mock.rs`, implementing the
  `Connection` trait); HTTP goes through `wiremock`; `svn`/`osc` behaviour is
  driven through a command-runner trait or a stub on `PATH`. Real
  hosts/containers are gated behind `#[ignore]` + a CI env flag.
- **Snapshot text contracts** with `insta` (testreport/export rendering, metadata
  parsing, MCP schemas, the lock wire format, display output). Review new
  snapshots with `cargo insta review`.

### The one-`it.rs`-per-crate rule

To keep test-compile time down, each crate's integration tests are consolidated
into a **single binary**. A crate using this pattern has, in its `Cargo.toml`:

```toml
autotests = false

[[test]]
name = "it"
path = "tests/it.rs"
```

and `tests/it.rs` pulls each test file in as a module:

```rust
#[path = "gitea.rs"]
mod gitea;
```

So **add a new integration test as a `mod` line in `tests/it.rs`, not as a new
top-level `tests/*.rs`** (a new top-level file would be silently ignored under
`autotests = false`). Because all the crate's integration tests then share one
process, anything touching a process-global (env vars, the spinner sink) must be
serialised with `#[serial(...)]`, and tests must not assume per-binary isolation.
`insta` prefixes each snapshot file with the test-binary name, which is `it` for
these crates: `it__<module>__<name>.snap`.

> **Exception: `mtui-core`.** It does *not* use the `it.rs` pattern — its
> integration tests are per-file and its snapshots are named `<module>__<name>.snap`
> (no `it__` prefix). Add a `mtui-core` integration test as a normal top-level
> `tests/*.rs` there; use the `it.rs` `mod` convention in `mtui-hosts`,
> `mtui-datasources`, `mtui-testreport`, and `mtui-mcp`.

## Debugging the MCP server

The [MCP server](mcp.md) page is the operator reference; this covers iterating on
the server itself (schema synthesis, the deny-list, a new `testreport_*` tool).

Start it under HTTP with debug logging:

```sh
cargo run -p mtui-mcp --features mcp -- --transport http --port 8765 --debug
```

Then drive it with the MCP Inspector without an LLM in the loop:

```sh
npx @modelcontextprotocol/inspector
```

In the UI select **Streamable HTTP**, set the URL to
`http://127.0.0.1:8765/mcp`, connect, and use the *Tools* tab to list every
synthesised tool and call it with hand-crafted arguments. For a stdio smoke-test,
point the Inspector at the binary instead of a URL; it spawns the subprocess and
attaches to its stdio streams.

`whoami` is the cheapest end-to-end check — no loaded report, no hosts — so a
successful `whoami()` confirms schema synthesis, the dispatch wrapper, the session
lock, and the response envelope. The testreport tools refuse cleanly with a
"no testreport loaded" message until a `load_template` call has run, which is the
correct refusal path.

Common issues:

- **HTTP client gets a 404 at the root** — the endpoint is `/mcp`, not `/`.
- **A new command is missing from the tool surface** — check `MCP_DENYLIST`; if it
  isn't denied, confirm it's registered in `register_all()` and re-exported from
  `commands/mod.rs`. Run with `--debug` to see the synthesis loop.
- **Tool calls time out** — network-bound commands run for minutes; either raise
  the client's read timeout or use `background=true` (see
  [Background jobs](mcp.md#background-jobs)). The server emits progress heartbeats
  for clients that supply a `progressToken`.

## Documentation

The book lives under `docs/` (mdBook). Two pages are **generated** — `cli.md` from
the command registry and `invocation.md` from the binary parsers — via
`cargo xtask gen-docs`; a drift-guard test (`cargo test -p xtask`) fails if the
checked-in copies are stale. **Never hand-edit those two files** (or the `cli.md`
preamble, which is emitted by the generator). Every other page is hand-written.

Build the book locally (if `mdbook` is installed):

```sh
mdbook build docs
```

## Issue tracking and commits

Work is tracked in the project's issue tracker.

Commits follow **Conventional Commits** (`feat`/`fix`/`docs`/`refactor`/`test`/
`chore`/…), explaining **why** rather than restating the diff. Reference SUSE
Bugzilla as `bsc#NNNN` / `boo#NNNN` where applicable. Only commit when asked;
inspect `git status`/`git diff` first and stage only intended files.
