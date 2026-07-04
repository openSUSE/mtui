# Phase 7 — `mtui-mcp` (MCP server + `mtui-mcp` binary) (detailed)

Goal: ship the second binary, `mtui-mcp` — a Model Context Protocol server that
**exposes the Phase-5 command layer as MCP tools**. The key architectural fact
(confirmed from upstream): the MCP server is a *thin adapter*, not a
reimplementation — it synthesises one tool per `Command.registry` entry, plus a
handful of testreport/job tools. Doing this last means every tool's behavior is
already built and tested.

**Size:** large but largely mechanical (wraps existing commands). **Prereqs:**
Phase 0–5 (needs the command registry, `Session`, all services); independent of
Phase 6 (REPL). **Blocks:** nothing.

Source grounding (upstream `main`, `mtui/mcp/`):

| Python file            | Size  | Rust target                                        |
| ---------------------- | ----- | -------------------------------------------------- |
| `session.py`           | 48.1K | `session.rs` — McpSession state + progress          |
| `testreport_tools.py`  | 32.2K | `testreport_tools.rs` — extra testreport tools      |
| `_schema.py`           | 18.7K | `schema.rs` — arg spec → JSON schema (clap-based)   |
| `tools.py`             | 18.6K | `tools.rs` — synthesise tools from registry + jobs  |
| `registry.py`          | 13.8K | `provider.rs` — SessionProvider (stdio + http)      |
| `_argv.py`             | 10.4K | `argv.rs` — kwargs → argv reconstruction            |
| `main.py`              | 9.1K  | `main.rs` — `mtui-mcp` binary entry                 |
| `_slim.py`             | 7.0K  | `slim.rs` — JSON-schema token slimming              |
| `profiles.py`          | 5.6K  | `profiles.rs` — tool-surface profiles               |
| `args.py`              | 2.6K  | `args.rs` — mtui-mcp CLI args                        |
| `deny.py`              | 1.7K  | `deny.rs` — REPL-only deny list                     |

---

## 7.1 The core insight: tools are synthesised from `Command.registry`

Confirmed (tools.py): for every concrete command **not on the deny-list**, build
one MCP tool whose **name** = the command name, **description** = its docstring,
**input schema** = inferred from its argument definition. This means Phase 7 does
**not** reimplement command behavior — it:
1. iterates the Phase-5 registry,
2. generates a JSON input schema from each command's `clap` arg spec,
3. on tool-call, reconstructs argv from the JSON kwargs and dispatches through the
   **same Phase-5 engine** used by the REPL,
4. captures output and returns it as the MCP tool result.

Plus three extra tool groups registered directly: **testreport tools**, **job
tools**, and the command-derived tools.

## 7.2 Rust MCP SDK: `rmcp`

Python uses `mcp.server.fastmcp.FastMCP` (schema inferred from a synthesised
Python signature via pydantic). Rust equivalent: **`rmcp`** (the official Rust
MCP SDK). Differences to plan for:
- rmcp typically derives tool schemas from Rust types (`#[tool]` macros +
  `schemars`). But mtui's tools are **dynamic** (built at runtime from the
  registry), so we likely need rmcp's **lower-level dynamic tool registration**
  (list_tools + call_tool handlers) rather than the static `#[tool]` macro path.
- **Decision gate (step 1):** spike whether `rmcp` supports registering tools
  with a **runtime-provided JSON schema** + a dynamic dispatch handler. If not,
  fall back to implementing the MCP protocol (stdio JSON-RPC) directly for the
  `initialize`/`tools/list`/`tools/call` surface (well-specified, small).

## 7.3 Module layout

```
crates/mtui-mcp/src/
├── main.rs            # mtui-mcp binary: args -> config -> server -> transport
├── server.rs          # build FastMCP-equivalent; register all tool groups
├── provider.rs        # SessionProvider trait + StdioProvider + HttpRegistry
├── session.rs         # McpSession: per-client Session + progress reporting
├── tools.rs           # synthesise command tools + job tools from registry
├── testreport_tools.rs# testreport-specific tools
├── schema.rs          # clap arg spec -> JSON schema
├── argv.rs            # tool kwargs (JSON) -> argv for the Phase-5 engine
├── slim.rs            # JSON-schema token-slimming pass
├── profiles.rs        # tool-surface profiles (full/allow/deny)
├── deny.rs            # REPL_ONLY deny list
└── args.rs            # mtui-mcp CLI parser
```

## 7.4 Transport + session isolation (`provider.rs`, `registry.py`)

Confirmed the **SessionProvider** abstraction makes the tool layer
transport-agnostic:
- **stdio** (`--transport stdio`, default): one process = one session; a single
  eagerly-built `McpSession` doubles as the degenerate provider.
- **http** (`--transport http`): one process serves many clients; a
  `SessionRegistry` mints **one fully isolated `McpSession` per client** (keyed on
  the request/session id), so concurrent clients never share `metadata`/`targets`.

### Rust plan
- `trait SessionProvider { async fn session(&self, key) -> Arc<Mutex<McpSession>> }`.
- `StdioProvider` (single session) + `HttpRegistry` (per-client map keyed on
  MCP session id).
- **stdio is the Phase-7 gating deliverable** (matches the success criterion:
  MCP stdio round-trip). http transport can be a follow-up sub-task if `rmcp`'s
  streamable-http support needs extra work.

## 7.5 `McpSession` (session.py, 48.1K — the largest file)

Holds the per-client equivalent of the REPL's `CommandPrompt` state (config,
`targets` HostsGroup, loaded `TestReport`/metadata) **plus MCP-specific concerns**:
progress reporting, output capture/cap, and lifecycle. Rust plan:
- Wrap a Phase-5 `Session` and add MCP progress notifications (rmcp progress
  tokens) + an **output cap** (see `test_mcp_output_cap.py` — bound tool result
  size). Build incrementally; most behavior delegates to Phase 5.

## 7.6 Tool synthesis (`tools.rs`, `schema.rs`, `argv.rs`)

- `schema.rs`: convert each command's **clap `Command` arg spec → JSON Schema**
  (types, required/optional, enums, help as description). This replaces the
  Python argparse→pydantic path. `schemars` can help for typed pieces, but the
  mapping is essentially "walk clap args → emit JSON Schema."
- `argv.rs`: inverse — take the tool-call JSON kwargs → reconstruct an argv
  vector (honoring an `argv_prefix` for subparser fan-out, e.g. `config_show` →
  `["config","show"]`) → feed the Phase-5 engine.
- `tools.rs`: for each non-denied registry command, register a dynamic tool
  (name, description=doc, schema from `schema.rs`, handler that
  `argv.rs`+dispatch). Also `register_job_tools` (openQA jobs) +
  subparser fan-out (one tool per `config` subcommand).

## 7.7 Deny list, slim, profiles (token-budget layer)

- `deny.rs`: `REPL_ONLY = {quit, exit, EOF, edit, shell, help, terms, switch}` —
  filtered out of tool synthesis. **Assert the deny-list ∩ registry at boot** so a
  renamed deny-listed command fails loudly (upstream does this).
- `slim.rs`: recursively strip redundant JSON-schema weight (drop `"title"`,
  collapse `anyOf:[{type:X},{type:null}]` → nullable) to cut per-request token
  cost. Port `slim_tool_schema` + `slim_registered_tools`.
- `profiles.rs`: narrow the tool surface to a configured profile
  (`cfg.mcp_tool_profile` + `mcp_tools_allow`/`mcp_tools_deny`); `full` with no
  override is a no-op (default unchanged). Port `apply_profile`.
- Applied in `main` **after** registration: `slim_registered_tools(server)` then
  `apply_profile(...)`.

## 7.8 `main.rs` — the `mtui-mcp` binary

Confirmed main.py flow → Rust:
- Parse `mtui-mcp` args (`args.rs`, mirrors REPL flags + transport/host/port).
- Build the same `Config` the REPL builds; set color/logging; `detect_system`.
- Choose provider by transport (stdio → single `McpSession`; http →
  `HttpRegistry`).
- `build_tools(server, provider)` + `register_testreport_tools` +
  `register_job_tools` → `slim` → `apply_profile` → serve on the transport.
- Friendly hint if the MCP feature/deps are missing (upstream lazily imports the
  SDK so a missing `[mcp]` extra prints a hint, not a traceback) — in Rust, gate
  the whole crate/binary behind an `mcp` feature and emit a clear message.

## 7.9 Test strategy

| Upstream test                        | Ports to                                        |
| ------------------------------------ | ----------------------------------------------- |
| `test_mcp_stdio_roundtrip.py` (3.6K) | **gating**: initialize + tools/list + tools/call over stdio |
| `test_mcp_tools.py` (30.9K)          | tool synthesis, names, schemas, dispatch         |
| `test_mcp_testreport_tools.py` (34.8K)| testreport tools                                |
| `test_mcp_session.py` (22.3K)        | McpSession state/lifecycle                       |
| `test_mcp_registry.py` (18.9K)       | provider isolation (http per-client)             |
| `test_mcp_main.py` (14.3K)           | boot/wiring/arg handling                         |
| `test_mcp_jobs.py` (9.4K)            | job tools                                        |
| `test_mcp_session_progress.py` (8.2K)| progress reporting                              |
| `test_mcp_slim.py` (5.5K)            | schema slimming                                  |
| `test_mcp_run.py` (3.6K)             | run tool end-to-end                             |
| `test_mcp_profiles.py` (3.0K)        | profile narrowing                               |
| `test_mcp_output_cap.py` (2.9K)      | output size cap                                 |

### Infrastructure
- **stdio round-trip**: spawn `mtui-mcp` (stdio), send JSON-RPC
  `initialize`/`tools/list`/`tools/call`, assert responses (the canonical
  contract test).
- Reuse Phase-2 `MockConnection` + Phase-3 `wiremock` for tool dispatch that
  touches hosts/HTTP; a fixture `TestReport` for testreport tools.
- **Schema golden tests** (`insta`): snapshot synthesised + slimmed schemas so
  token-budget regressions are visible.

## 7.10 Task breakdown

1. Spike `rmcp` dynamic tool registration (runtime schema + handler) vs raw
   JSON-RPC; pick the approach.
2. `provider.rs`: `SessionProvider` trait + `StdioProvider` (single session).
3. `session.rs`: `McpSession` wrapping Phase-5 `Session` (+ output cap).
4. `schema.rs`: clap arg spec → JSON Schema.
5. `argv.rs`: kwargs → argv (+ subparser prefix).
6. `tools.rs`: synthesise command tools from registry + job tools; `deny.rs`.
7. stdio server + `main.rs` → **stdio round-trip test passes (gate)**.
8. `testreport_tools.rs`.
9. `slim.rs` + `profiles.rs` (+ schema golden snapshots).
10. `HttpRegistry` per-client isolation (+ progress) — follow-up sub-task.

## 7.11 Deliverables (files)

- **Create:** `crates/mtui-mcp/src/{main.rs, server.rs, provider.rs, session.rs,`
  `tools.rs, testreport_tools.rs, schema.rs, argv.rs, slim.rs, profiles.rs,`
  `deny.rs, args.rs}` + `crates/mtui-mcp/tests/**`.
- **Modify:** `crates/mtui-mcp/Cargo.toml` — `[[bin]] name="mtui-mcp"`; deps
  `mtui-core` (+ lower crates), `rmcp`, `serde_json`, `schemars` (optional),
  `tokio`, `tracing`, `clap`, `thiserror`; feature `mcp` gating; dev `insta`.
- **Possibly modify:** `mtui-core` — expose the command registry + a
  `run_argv(&mut Session, argv)` dispatch entry the MCP layer can call (parallel
  to the REPL's `run_line`).

## 7.12 Risks / decisions to confirm

- **`rmcp` dynamic tools.** The whole design hinges on runtime tool registration
  with a runtime-built JSON schema. If `rmcp` only supports static `#[tool]`
  macros, implement the stdio JSON-RPC surface directly (small, well-specified).
  **Resolve in step 1 before committing.**
- **Schema fidelity / token budget.** LLM tool schemas must be compact and
  correct; the slim + profiles layers are load-bearing for cost. Snapshot-test
  schemas so regressions surface.
- **Session isolation (http).** Per-client `McpSession` must not share
  `targets`/`metadata` — a correctness+security property. stdio (single session)
  is simpler; ship it first, treat http as a follow-up.
- **argv reconstruction fidelity.** kwargs→argv must exactly reproduce what the
  command expects (flag names, REMAINDER, subparser prefixes). Drive from
  `test_mcp_tools.py`.
- **Deny-list correctness.** Assert deny ∩ registry at boot; a leaked REPL-only
  tool (`shell`, `quit`) in an MCP context is a real hazard.
- **Output cap.** Bound tool-result size (`test_mcp_output_cap.py`) so a huge
  `run` output can't blow the client's context — mirror upstream's cap.
- **Feature gating.** Keep `mtui-mcp` behind an `mcp` feature so the core build
  and the `mtui` REPL don't drag in the MCP SDK.

## 7.13 Out of scope for Phase 7

No new command behavior (tools wrap Phase-5 commands), no REPL, no packaging/
release (**Phase 8**). Phase 7 delivers the `mtui-mcp` binary serving synthesised
tools over stdio (http as a follow-up), verified by the stdio round-trip contract.
