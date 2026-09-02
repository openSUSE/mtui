# Agent Notes — mtui

## Mission
`mtui` (Maintenance Test Update Installer) is the SUSE QE tool for validating
maintenance updates: load a request by RRID, install/test it on reference hosts
over SSH, then approve/reject. It drives OBS/IBS and Gitea review workflows,
`svn`, and openQA/QEM under the hood. It is a memory-safe, async-native Rust
workspace that ships as two single static binaries (`mtui`, `mtui-mcp`), with no
runtime interpreter.

Preserve the **data-format and workflow contracts** that let mtui interoperate
with the SUSE maintenance ecosystem (see "Contracts" below); break compatibility
only when the task explicitly calls for it.

### The Python implementation is no longer a reference
mtui was originally ported from a Python implementation, removed in `ec80791c`
(the old tree is readable at `git show ec80791c^:mtui/...`, and on the
`archive/python-main` tag and the `16.0.x`–`19.0.x` branches). **Do not treat it
as an authority.** Matching its behaviour is not a goal and "upstream does X" is
not a justification. Use the archive to understand *why* a shape exists — then
record what you learn as a statement about the constraint, not as a citation.

- **Never** preserve a bug, a typo, or an awkward shape for parity with it.
  Fixing one is welcome — **in its own commit**, with a CHANGELOG entry if a user
  or MCP client would notice. Retiring a *rationale* is prose; changing the
  *bytes* is not, and the two must not ride in the same commit.
- **Check first that the shape is not a contract in disguise.** If it is written
  to a host (`/var/lock/mtui*.lock`, `/var/log/mtui.log`, a zypper repo alias —
  `repo_manager.rs::issue_alias`), signed or transmitted to a live service (the
  openQA HMAC path encoding, `openqa/client.rs::encode_path_for_signing`), or is
  user-facing text an LLM client consumes, it is load-bearing **even when the
  only comment about it says "upstream did X"**. Replace the stale rationale;
  do not delete the constraint.
- A note recording a **deliberate departure** is a guard-rail, not a citation —
  it stays. Reframe it as a positive statement of the choice ("mtui is XDG-first:
  history lives at `$XDG_DATA_HOME/mtui/history`") rather than a comparison.
  Keep the constraint, drop the comparison. Likewise for a bare `Ports upstream
  mtui.commands.foo`: keep the behavioural description, drop the citation, and if
  the line was *only* a citation, drop the line.
- **Never strip a licence or provenance citation**, even when it routes through
  the old tree — `obs/inference.rs`'s GPL-2.0-only attribution chain to
  openSUSE/osc-plugin-qam is a legal requirement, not archaeology, and
  `Cargo.toml`'s relicensing note explains a deliberate difference.
- `upstream` is **not always** the old implementation, and the distinction is by
  *referent, not by path* — the same file mixes both. Where the sentence cites a
  `.py` file or a Python identifier it means the removed tree; where it names a
  live external it stays: `openSUSE/osc-plugin-qam`, `mjdonis/oqa-search`, the
  `osc` tool's own `oscrc` lookup (`obs/oscrc.rs`), the openQA server's signing
  rules (`openqa/client.rs`), and Rust crates such as `russh`/`rsa`. Read each
  site; do not sweep the term.
- "Python" is also a **domain word** here. Never rename or delete it in SUSE
  package flavours (`pythonNNN-foo` → `python-foo`,
  `oqa_search/heuristics.rs::PYTHON_FLAVOR_RE` — live openQA build-check
  matching), product names in fixtures (`sle-module-python2-*`), or `typos.toml`
  entries that keep the append-only CHANGELOG spell-checkable. Note `typos` is a
  CI-only job the local gate does not run.

### Design invariants (do not regress)
- **Safety & robustness:** strong types, exhaustive error enums (`thiserror`);
  prefer a typed `Result` over silent `None`-swallowing.
- **Performance:** async I/O (`tokio`), true parallel host fan-out; fast startup;
  single binary.
- **Distribution:** one static `mtui` + one `mtui-mcp`, no runtime deps beyond a
  couple of subprocesses; shell completions and man pages are generated.
- **Maintainability:** a Cargo workspace with clear crate boundaries and a single
  composition root, so hosts/datasources/testreport stay decoupled.

## Two driving surfaces (keep both working)
There are **two entrypoints**, and command/entrypoint changes must keep both
green:
- `mtui` — the interactive REPL (`reedline`).
- `mtui-mcp` — the MCP server, which **synthesises its tools from the command
  registry**. Adding/renaming/removing a command affects MCP tools automatically.
  Exception: a deny-listed command may be re-served under the same name as a
  hand-written tool (`edit` → `testreport_*`; `get`/`put` → the in-band
  transfer tools, #434).

## Workspace layout
Cargo workspace; each crate has one job. Lower crates never depend on higher ones;
`mtui-core` is the composition root that wires everything.

```
mtui (root)         facade package owning `src/bin/{mtui,mtui-mcp}.rs` behind
                    the `cli`/`mcp` features; integration tests in `tests/it.rs`
crates/
  mtui-types/        domain types + error hierarchy (no I/O)
  mtui-config/       TOML config + XDG paths
  mtui-hosts/        SSH/SFTP (russh), Target/HostsGroup, locks, arbiter   [async]
  mtui-datasources/  shared HTTP, refhosts resolve/search/verify, openQA/QEM/Gitea/native-OBS-QAM/oqa-search  [async]
  mtui-testreport/   TestReport lifecycle, metadata parsers, SVN/Gitea checkout, update workflow (actions/checks/export)
  mtui-core/         Command trait + registry + Session + engine + dispatch
  mtui-cli/          reedline REPL library
  mtui-mcp/          rmcp server library
fuzz/                cargo-fuzz harness over the untrusted-input parsers.
                     Detached from the workspace (own empty [workspace] table):
                     fuzzing needs nightly and must not affect the MSRV,
                     Cargo.lock, or stable CI. Run: cargo +nightly fuzz run
                     <target>. CI runs it via ClusterFuzzLite
                     (.clusterfuzzlite/ + .github/workflows/cflite_fuzz.yml).
```
Task breakdown is tracked in the project's issue tracker; check it for the
next actionable task before working on a subsystem.

## Setup & commands
- Build everything: `cargo build --workspace` (produces both `mtui` and `mtui-mcp`)
- Run the REPL: `cargo run -p mtui -- --help`
- Run the MCP server: `cargo run -p mtui --bin mtui-mcp -- --help`
- Format: `cargo fmt --all`
- Lint (warnings are errors): `cargo clippy --workspace --all-targets --all-features -- -D warnings`
- Docs (broken/private links are errors): `RUSTDOCFLAGS="-D warnings" cargo doc
  --workspace --no-deps --all-features --document-private-items`
- Test: `cargo test --workspace` — **the cost is compilation, not test
  execution.** A cold `cargo build --workspace --tests` is ~80s; the actual test
  *run*, once compiled, is only ~20-25s. The default 120s timeout is exceeded
  only when compiling from cold, so allow ≥300000 ms (5 min) on the first run of
  a session; a second run against a warm `target/` cache is seconds.
- Coverage: `cargo llvm-cov --workspace --lcov --output-path lcov.info`
- Feature matrix (catches feature-gate rot) — **compile-only**. On a 10-core,
  32 GiB Mac16,10, switching from the default graph took 25 s for
  `--no-default-features` and 4 s for `--all-features`: the former changes feature
  unification by dropping `cli`/`mcp`, while the latter adds only `notify-rust`:
  `cargo build --workspace --no-default-features` and `--all-features`. Do **not**
  routinely *test* `--all-features`; CI only compiles it
  (`.github/workflows/ci.yml` feature-matrix job).

### Fast local iteration
- **Keep the cache warm and scope tight.** During dev, run
  `cargo test -p <crate>` for the crate you're touching, not the whole workspace
  — reserve `cargo test --workspace` for the final gate.
- **Test default features only while iterating.** On a 10-core, 32 GiB Mac16,10,
  `cargo test --workspace --all-features` took 36 s versus 28 s with default
  features; it adds only `notify-rust` and no extra runtime signal beyond the
  compile-only feature matrix.
- **`mtui-cli`'s lib suite is the slow one (~8s).** Its `edit`/`shell` tests spawn
  real editor/shell subprocesses. When working elsewhere, don't rerun it.

## Definition of Done (hard rules)
- Run the **full gate on the whole workspace** before claiming done, mirroring
  CI: `cargo fmt --all --check` **and**
  `cargo clippy --workspace --all-targets --all-features -- -D warnings` **and**
  `RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --all-features
  --document-private-items` **and** `cargo test --workspace` (default features —
  the root `mtui` facade's `mcp` feature is on by default, so this run already
  covers `mtui-mcp`'s server/transport suite) **and** the compile-only feature
  matrix `cargo build --workspace --no-default-features` +
  `cargo build --workspace --all-features`. Tests run against default features
  only — do **not** run `--all-features` *tests*. The long pole is cold
  compilation, not the test run itself, so give the first `cargo test --workspace`
  (and the feature-matrix builds) a generous timeout (≥300000 ms) and don't treat
  an early timeout as a failure.
- **"Done" means CI observed green, not predicted green.** Report status from the
  actual run.
- **A regression test must be observed failing** against the unfixed code before
  the fix is claimed done. Revert the fix (or hand-break the line), run that one
  test, see red, restore. A pinning test that was never red pins nothing.
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

## Architecture (non-obvious bits)
- **Command registry.** Every command implements the `Command` trait and is
  registered into a central registry (explicit `register_all()`). The REPL
  dispatch, tab-completion, and the MCP tool synthesiser all iterate this one
  registry — it is the single source of the command surface.
- **Session state.** Commands operate on a `Session` (config, `HostsGroup`
  targets, loaded `TestReport`/metadata, display) passed explicitly. No hidden
  globals.
- **Trait injection instead of crate cycles.** `mtui-hosts` drives the
  install/uninstall template through the `PlanProvider` trait, declared in
  `mtui-hosts` in terms of its own types; `mtui-testreport`'s `WorkflowRegistry`
  implements it, so the doer/check tables reach the host dispatch without
  `mtui-hosts` depending on `mtui-testreport`. Do not create crate cycles — add
  a trait and inject.
  **Inject at the point of use, not at construction.** `update_flow::
  perform_install` / `perform_uninstall` install the provider immediately before
  driving the template. `OperationGroup::plans` has exactly one consumer, so
  there is exactly one place that cannot forget to wire it. An earlier design
  injected at a composition root instead; nothing ever called it, every
  `HostsGroup` was built without a provider, and `install` reported success
  while running no command on any host for the whole life of the Rust port.
- **Config.** **TOML** file `mtui.toml`, resolved highest-precedence first from
  `--config` → `$MTUI_CONF` → `$XDG_CONFIG_HOME/mtui/mtui.toml` → `~/.mtui.toml`
  → `/etc/mtui.toml`; the default set is merged **lowest-precedence first** so
  per-user files override `/etc` on shared keys. Sectioned tables (`[mtui]`,
  `[connection]`, `[refhosts]`, `[url]`, ...) map to typed options. Loading is
  **lenient**: a missing/malformed file (or a bad value) is logged at ERROR and
  skipped, falling back to defaults; it never hard-fails. CLI-arg merging is
  implemented via `Args::apply_to` (`crates/mtui-core/src/args.rs`), the
  highest-precedence config layer, applied after `Config::load`.
- **MCP is a thin adapter.** `mtui-mcp` builds one tool per non-denied command by
  converting the command's `clap` arg spec to a JSON schema, reconstructing argv
  from tool kwargs, and dispatching through the **same engine** as the REPL.
  REPL-only commands (`quit`, `exit`, `EOF`, `edit`, `shell`, `help`, `switch`)
  are deny-listed, as are `get`/`put`, which are re-served under the same names
  as hand-written in-band transfer tools (#434) — a deny-listed command may be
  replaced by a richer hand-written tool (`edit` → the `testreport_*` tools is
  the same pattern). The deny-list ∩ registry is consistency-tested and drift is
  warned about at boot. Local process execution and terminal launching have no
  entry because they have no command: `lrun` and `terms` were removed by design;
  do not reintroduce either.
- **Cancellation is cooperative-first.** `Session` carries a
  `CancellationToken` (the seam); the `Command::run` driver checks it before
  dispatch and between fan-out templates. `Session::activate` pushes the token
  down onto the active report's `HostsGroup` so the flows can consult it.
  **Never gate a parallel fan-out on it**: `run_parallel` returns `()`, so a
  host skipped mid-batch is indistinguishable from one that ran — its stale
  `last*` snapshot sails through the post-run checks and the update reports
  success on a host it never touched (and every teardown is a fan-out too, so
  gating strands remote locks exactly when a cancel needs them released).
  Long serial flows poll `targets.cancel_requested()` at their own boundaries
  (`downgrade`'s non-transactional per-package loop, `prepare --installed`'s
  pre- and post-probe checkpoints, the `update` step sequence) — but **never
  past a point of no return**: `update`
  makes its last check before dispatching the patch command, and the
  post-failure rollback runs under `HostsGroup::suspend_cancellation` (its own
  per-package checkpoint would otherwise abort the recovery at package 0 and
  strand the half-applied update it exists to undo). `UpdateFailure::Cancelled`
  skips the rollback because the update never ran. A checkpoint must `break`
  into its function's normal fall-through, never early-`return` past cleanup —
  and a real failure collected before the cancel always outranks it. MCP `job_cancel` cancels the job's token, waits a short grace for a
  cooperative stop, then hard-aborts the worker — its reply distinguishes the
  two and never claims to cancel a job that had already finished. The REPL is
  the seam's second producer: a Ctrl-C *during* a command (the terminal is
  cooked then, so it is a real SIGINT rather than reedline's key event) is
  forwarded onto the session token instead of killing the process, and a second
  press force-exits 130 with a warning about the locks that may be left behind.
  It installs a **fresh token per dispatched line, unconditionally** — the token
  is one-shot, so a cancelled one left in place would kill every later dispatch
  at the pre-flight check, `quit`'s teardown included, stranding exactly the
  locks the cancel had to release. The teardown dispatch gets the fresh token but
  **no cancel arm**: a press there only escalates, because cancelling the
  cleanup is what strands the locks. That is a property of the *dispatch*, not of
  the key — a typed `quit`/`exit` is routed to it by resolving the line's command
  position through the registry, so it is protected exactly as Ctrl-D is.
  A new interrupt hook belongs on this seam,
  never on its own `tokio::signal::ctrl_c` — the REPL arms SIGINT process-wide
  from the first prompt onward (startup seeding is still outside that window),
  and a headless tool call has no terminal to press Ctrl-C at, while a stdio
  server that *does* share one would fire every listener at once, interrupting
  work nobody asked to stop. A flow that
  stops on a cancel must surface as `CommandError::Cancelled`, not a generic
  failure — unless stopping *is* its documented success (`request_review
  --watch` returns `Ok` with "the request is still posted"; `regenerate`
  abandons the wait and reports the state it last saw, since the server keeps
  building) — and that verdict must come from the flow's own `cancelled` flag,
  never from sniffing the session token, which would mask a real host failure
  that merely coincided with a cancel (see
  `commands/perform.rs::map_flow_error`). New long-running command bodies
  should observe the seam at their own step boundaries, and report what they
  completed before stopping.

## Contracts (do not break without intent — these enable ecosystem interop)
- **RRID grammar** `project:kind:maintenance_id:review_id` and its parse errors.
- **refhosts.yml schema** — the file is still location-grouped *on disk*, but
  location is a legacy grouping, not a live query dimension: rows are
  merged/flattened and de-duplicated at load (`version.minor` may be numeric or
  `spN`).
- **Testreport / export text format**, incl. the `overview_inject` BEGIN/END
  idempotent block under `regression tests:`.
- **Remote lock wire format** — one line `timestamp:user:pid[:comment]` (parsed
  with a 3-way split so the comment keeps embedded colons). Two locks share this
  layout: the operation lock `/var/lock/mtui.lock` (PID-based ownership, guards
  serialized zypper transactions) and the pool-claim lock
  `/var/lock/mtui-pool.lock` (RRID-based ownership; the comment carries
  `mtui pool <RRID> [<owner>]`). Every mtui sharing the fleet parses this layout,
  including older releases — snapshot-test it
  (`crates/mtui-hosts/tests/lock_format.rs`).
- **Remote history format** — one `timestamp:user:field1[:field2…]` line appended
  to `/var/log/mtui.log` on each enabled host, written with `sftp_append`
  (`O_APPEND|O_CREAT`, never read-modify-write) so concurrent testers on one host
  do not clobber one another, and read back by `list_history`. The append-only
  primitive exists *for* this contract — do not collapse it into `sftp_write`.
- **MCP tool names/schemas** — downstream LLM configs depend on them; snapshot the
  synthesised + slimmed schemas.

The `tests/` fixtures are the authority for these formats; treat them as golden.

## Testing conventions
- Unit tests colocated (`#[cfg(test)]`); integration tests in `crates/*/tests/` and
  the facade's root `tests/`.
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
  `MockConnection` implementing the `Connection` trait; `svn` via the
  `SvnRunner` command-runner trait/stub; OBS/IBS and every other HTTP
  datasource via `wiremock`.
- **Snapshot text contracts** (`insta`): testreport/export rendering, metadata
  parsing, MCP schemas, lock-file format, display output. `insta` prefixes each
  `.snap` file with the **test-binary** name, which is now `it` for every crate —
  so snapshot files are `it__<module>__<name>.snap`. A new snapshot test's file
  lands with that prefix automatically; don't hand-name it otherwise.
- **Gate real hosts/containers** behind `#[ignore]` + a CI env flag (sshd
  integration fixture); unit tests must run offline and fast.
- **Capturing `tracing` output: install the subscriber globally, scope the
  sink.** `tracing::subscriber::set_default`'s guard is thread-local, but
  callsite *interest* is cached **process-wide**: a callsite first reached from a
  thread with no subscriber installed is cached `Interest::never()` and stays
  silent for every later capture, so with libtest running in parallel whichever
  test got there first decides whether a log assertion can even fail. Install
  once with `set_global_default` and move the scoping to a thread-local sink.
  The pattern is settled and already written up three times — canonical copy:
  `crates/mtui-datasources/tests/log_capture.rs`; also
  `mtui-datasources::teregen` and `mtui-testreport::reports::update_flow`'s test
  modules. Copy it, do not re-derive it. Known cost: an unfiltered `Registry`
  reports no `max_level_hint`, so `LevelFilter::current()` goes to `TRACE` for
  that whole test binary and every `debug!`/`trace!` in it starts evaluating its
  arguments; bound the layer with a `LevelFilter` if that ever matters.
- **A test that cannot fail is not evidence.** Twice a real regression has sailed
  through a green workspace run: once because the fixture disarmed the assertion
  (a `MockConnection` answering empty stdout builds no downgrade command, so
  "assert no `--oldpackage`" could not fail; a fresh report "clears" a field that
  was already `None`), once because the fix landed one layer too low
  (`Target::reboot` gated on `TargetState`) — which half-gated the lifecycle and
  silently broke the `reboot` command while every test stayed green. Green CI is
  a claim about the suite, not a proof about the code. For each new assertion,
  name the mutation it must catch and check the fixture can express it: a
  *changing* boot id passes the disabled-host test even when only the dispatch is
  skipped, so that test pins a *fixed* one. Then break the code and watch it go
  red. Apply the same scepticism to the fix — when the leaf you are gating has
  other callers (`Target::reboot` is also reached by `close` and by the explicit
  `reboot` command), gate on the layer that owns the state and probe the sibling
  paths before believing the gate landed where you meant it.

## Style & error handling
- Edition 2024, MSRV 1.96. `rustfmt` defaults; `clippy` clean with
  `-D warnings`.
- Errors: `thiserror` enums in library crates, `anyhow` only at binary
  boundaries. Prefer a typed `Result` over a `log + return None`; where you
  intentionally rely on best-effort degradation, make it explicit and test it.
- Logging: `tracing` (not `log`); levels configurable via CLI + `RUST_LOG`.
- Async everywhere I/O happens (`tokio`); keep pure logic (`mtui-types`) sync and
  I/O-free.
- **Never leak secrets to output or logs.** Secret config fields (currently just
  `gitea_token`, classified by `is_secret_attr` in the `config` command) are
  masked (`<set>`) by both `config show` and `config set` — never echo their
  value to the display buffer (it reaches terminal scrollback and MCP output).
  Configured datasource URLs may embed credentials (`scheme://user:pass@host`);
  always run a URL through `sanitize_url` (crate-internal to `mtui-datasources`,
  not a public export) before logging it or putting it in an error (it replaces
  userinfo with `***` while keeping the host for diagnosis). Never render a
  `reqwest::Error` directly — its `Display` appends the request URL verbatim, and
  a redirect can put credentials back into it; `impl From<reqwest::Error> for
  HttpError` strips the URL where reqwest errors enter the crate's hierarchy, so
  convert first and add request context yourself. Outside `mtui-datasources`
  `sanitize_url` is not reachable (it is crate-internal), so the rule there is:
  never interpolate a datasource URL into a message at all — log the host alone,
  or rely on the already-stripped error types. The Gitea token travels in an
  `Authorization` header and is never logged.
- Never add SSH password auth — MTUI is **pubkey-only by design**; preserve that.
- **Comments must not outweigh the code.** A one-line change gets at most a
  one-line comment, and usually none — a three-line preamble explaining a
  one-line fix is noise that goes stale the moment the line moves. Where the
  code says what it does (a named function call, an obvious guard, a `match`
  arm), no comment is needed. Comment only what the code cannot say: *why* a
  non-obvious choice was made, a contract or invariant that must not be
  regressed, or a subtlety that cost real debugging time.

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
`svn` (testreport checkout). Declare as a packaging recommends; keep it optional
and degrade gracefully when absent. The QAM review workflow (`assign`/`unassign`/
`approve`/`reject`/`comment`) no longer shells out to `osc`/`osc-plugin-qam` — it
talks to the OBS/IBS API natively (see the native OBS backend and `[obs]`
config), reading credentials from `oscrc`.

## Further reading
- `docs/src/architecture.md` — architecture map (crate layout, trait injection,
  contracts) and the rest of the mdBook under `docs/src/` (installation,
  configuration, developer, invocation, mcp).
- `CONTRIBUTING.md` — changelog policy; `docs/src/developer.md` — dev workflow.
