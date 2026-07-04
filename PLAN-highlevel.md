# mtui-rs — High-Level Plan

Rust reimplementation of [openSUSE/mtui](https://github.com/openSUSE/mtui)
(Maintenance Test Update Installer).

**Approach:** full-scope **idiomatic rewrite** (redesign, not a 1:1 transpile),
covering all five subsystems: SSH host layer + commands, test-report lifecycle,
refhosts + data sources, MCP server, and the interactive REPL.

---

## Current state

`mtui-rs` is an empty Rust scaffold (single-crate `Cargo.toml`, edition 2024,
stub `src/main.rs`, no commits). Upstream `mtui` is a mature Python 3.13+ tool:
**~176 source files / ~910 KB** across 9 modules — a parallel-SSH maintenance
update installer with an interactive shell, test-report lifecycle, refhost/data
integrations, and a self-contained MCP server.

## Scope map (what we are rewriting)

| Python module      | Size            | Role                                                    |
| ------------------ | --------------- | ------------------------------------------------------- |
| `hosts/`           | 172 KB / 22 f   | SSH connections, host groups, locks, targets            |
| `commands/`        | 166 KB / 46 f   | User commands (run/update/install/prepare/…)            |
| `mcp/`             | 164 KB / 12 f   | MCP server (tools, registry, schema, session)           |
| `data_sources/`    | 102 KB / 18 f   | refhosts, OBS/IBS, qem_dashboard, oqa-search, gitea     |
| `cli/`             | 73 KB / 16 f    | REPL, completer, history, lexer, display, prompter      |
| `test_reports/`    | 70 KB / 13 f    | test-report lifecycle, SVN/Gitea checkout               |
| `update_workflow/` | 58 KB / 19 f    | update/export logic                                     |
| `support/`         | 51 KB / 11 f    | config, exceptions, utilities                           |
| `types/`           | 45 KB / 15 f    | domain types (updateid, product, version, enums)        |

## Dependency mapping (Python → Rust)

| Python                    | Rust crate                          | Notes                                              |
| ------------------------- | ----------------------------------- | -------------------------------------------------- |
| `paramiko` (SSH)          | **`russh`** + `russh-sftp`          | pure-Rust async SSH/SFTP; alt: `ssh2` (libssh2)    |
| `prompt_toolkit` (REPL)   | **`reedline`**                      | completion, history, reverse-search, hints         |
| `ruamel.yaml`             | **`serde_yaml`** (or `serde_yml`)   | refhosts.yml / config parsing                      |
| `requests`                | **`reqwest`**                       | + `tokio` async runtime                            |
| `openqa-client`           | thin wrapper over `reqwest`         | no direct crate; reimplement client                |
| `pyxdg`                   | **`directories`** / `etcetera`      | XDG config/cache paths                             |
| `argparse` / `argcomplete`| **`clap`** (derive) + `clap_complete` | subcommands + shell completion                   |
| `mcp[cli]`                | **`rmcp`** (official Rust MCP SDK)  | stdio transport, tool registry                     |
| `keyring`                 | **`keyring`** crate                 | optional feature                                   |
| `notify-py`               | **`notify-rust`**                   | optional desktop notifications                     |
| threads / pool            | **`tokio`** tasks + `futures`       | parallel-vs-serial host execution                  |
| logging                   | **`tracing`** + `tracing-subscriber`| structured, configurable levels                    |
| errors                    | **`thiserror`** (lib) + `anyhow` (bin) | replaces `support/exceptions.py`                |
| tests                     | `#[test]` + **`insta`** + `wiremock`| snapshots + mocked HTTP; mirror `tests/*.py`       |

## Proposed workspace architecture

A Cargo **workspace** — the redesign lets us split cleanly and keep the MCP
server optional and independently testable.

```
mtui-rs/                    (workspace root)
└── crates/
    ├── mtui-types/         ← types/ + support/exceptions   (foundation, no I/O)
    ├── mtui-config/        ← support/config + XDG paths
    ├── mtui-hosts/         ← hosts/ (russh, host groups, locks)   [async]
    ├── mtui-datasources/   ← data_sources/ + refhost store        [async, reqwest]
    ├── mtui-testreport/    ← test_reports/ + update_workflow/
    ├── mtui-core/          ← command engine, session state, orchestration
    ├── mtui-cli/           ← reedline REPL + clap + display   → binary `mtui`
    └── mtui-mcp/           ← rmcp server                       → binary `mtui-mcp`
```

## Phased roadmap

| Phase | Size   | Deliverable                                    | Verify                                                                 |
| ----- | ------ | ---------------------------------------------- | --------------------------------------------------------------------- |
| 0     | small  | Workspace bootstrap + CI (fmt/clippy/test), MSRV 1.96 (Homebrew rustc, no rustup) | `cargo build --workspace` + empty `cargo test` green in CI            |
| 1     | medium | `mtui-types` + `mtui-config`                    | round-trip parse of sample `refhosts.yml` + config; port `test_products*`/`test_config*` |
| 2     | large  | `mtui-hosts` (SSH core, parallel/serial, locks) | integration test vs local `sshd` running `run` across N hosts; lock reap tests |
| 3     | large  | `mtui-datasources` (refhosts, OBS/IBS, oqa, gitea) | `wiremock` HTTP tests (`test_openqa_connector`, `test_obs_report`); offline `list_refhosts` filtering |
| 4     | large  | `mtui-testreport` + update workflow             | `insta` snapshots of rendered testreports vs fixtures                  |
| 5     | large  | `mtui-core` + 46 commands (run/update/… first)  | per-command unit tests; golden end-to-end `run` vs Phase-2 fixture     |
| 6     | large  | `mtui-cli` REPL + `mtui` binary                 | completion/history unit tests; REPL smoke; `mtui --help` + one command |
| 7     | large  | `mtui-mcp` + `mtui-mcp` binary                  | stdio round-trip test; registry/schema tests                          |
| 8     | small  | Packaging & docs (completions, features, README)| `cargo build --release` → `mtui` + `mtui-mcp`; feature matrix builds    |

### Command port order (Phase 5)

1. Core workflow first: `run`, `update`, `install`, `prepare`, `downgrade`,
   `reboot`, `zypper`, `localrun`.
2. Host/state: `addhost`, `removehost`, `hoststate`, `hostslock`, `hostsunlock`,
   `switch`, `list_refhosts`.
3. Test-report: `load_template`/`loadtemplate`, `checkout`, `commit`, `edit`,
   `export`, `templates`, `unload`.
4. Data/query: `openqa_overview`, `openqa_jobs`, `updates`, `approve`,
   `apicall`, `showdiff`, `listpackages`, `setrepo`, `showrepos`.
5. Trivial last: `whoami`, `products`, `config`, `terms`, `help`, `quit`, `shell`.

## Files

- **Modify:** `Cargo.toml` (→ workspace), relocate `src/main.rs` into `crates/mtui-cli`.
- **Create:** the `crates/*` tree (each with its own `Cargo.toml` + `src/`), root
  CI config, `README.md`.
- **Delete:** the current single-crate `src/main.rs` stub (folded into `mtui-cli`).

## Risks

- **SSH parity is the crux.** `russh` is async and lower-level than `paramiko`;
  agent auth, known_hosts, keyboard-interactive, and SFTP edge cases need care.
  Fallback: `ssh2` (blocking, libssh2) if `russh` friction is high.
- **Undocumented data formats.** refhosts.yml, testreport, and OBS/openQA
  response shapes are defined only by Python code + fixtures. Mitigation: port
  the upstream `tests/` fixtures first and treat them as the contract.
- **MCP SDK maturity.** `rmcp` is newer than Python's `mcp[cli]`; schema/registry
  ergonomics may differ. Isolated in the last phase so it cannot block core.
- **Scope size (~910 KB).** Realistically many weeks. Phase boundaries let a
  useful `mtui` ship (Phases 0–6) before MCP.

## Alternatives considered

- **Single-crate binary** — rejected: 176-file surface + optional MCP server
  benefits from workspace boundaries and independent testing.
- **1:1 transpile** — rejected per project choice; Python idioms (dynamic
  dispatch, duck typing, threads) map poorly to Rust ownership/async.
- **`ssh2` as primary SSH** — kept as fallback, not default: blocking FFI
  complicates the `tokio` parallel-execution model.
- **`rustyline` for REPL** — rejected in favor of `reedline`, a closer fit to
  prompt_toolkit's feature set.

## Success criteria

- [ ] `cargo build --workspace` and `cargo clippy` clean; CI green.
- [ ] `mtui` runs the REPL and a non-interactive command; `mtui-mcp` responds to
      an MCP stdio round-trip.
- [ ] Parallel `run` executes across ≥2 hosts against an sshd fixture with
      correct per-host state handling.
- [ ] refhosts.yml, config, and testreport fixtures from upstream `tests/` parse
      identically (snapshot-verified).
- [ ] Ported test suite (SSH, datasources, testreport, MCP, commands) passes.
- [ ] Both console entry points (`mtui`, `mtui-mcp`) build in `--release` with
      shell completions generated.
