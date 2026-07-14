# mtui-rs

> **Status: early work in progress (Phase 0 complete).** This is a ground-up Rust
> rewrite of [openSUSE/mtui](https://github.com/openSUSE/mtui). The architecture
> and phased plan are defined (`PLAN-highlevel.md`; per-phase tasks tracked in beads). The
> Cargo workspace skeleton builds and is CI-gated; no domain logic has landed yet
> (Phase 1+). Commands and flags below describe the **intended** interface, not a
> shipped one.

An **improved, idiomatic Rust successor** to MTUI — the **M**aintenance **T**est
**U**pdate **I**nstaller, SUSE QE's tool for validating maintenance updates: load
a request by RRID, install and test it on reference hosts over SSH in parallel,
then approve or reject. It drives `osc`/`svn`/Gitea and openQA/QEM under the hood.

This is a **redesign, not a transpile**: MTUI is the behavioral reference and
source of domain truth, but mtui-rs aims to be memory-safe, async-native, and
distributable as a single static binary — while preserving the data-format and
workflow contracts that keep it interoperable with the SUSE maintenance
ecosystem.

## Why a rewrite

- **Safety & robustness** — strong types, exhaustive error enums, no interpreter.
- **Performance** — async I/O (`tokio`), true parallel host fan-out, fast startup.
- **Distribution** — two static binaries (`mtui`, `mtui-mcp`), no Python runtime
  or virtualenv; generated shell completions and man pages.
- **Maintainability** — a Cargo workspace with clean crate boundaries and one
  composition root.

## Two surfaces

- `mtui` — interactive REPL (line editing, tab completion, history) **and**
  non-interactive single-command mode.
- `mtui-mcp` — a Model Context Protocol server whose tools are **synthesised from
  the command registry**, so the CLI and the MCP surface never drift.

## Planned features

- Parallel SSH command execution across reference hosts (`run`, `update`,
  `install`, `prepare`, `downgrade`, …) with per-host `enabled`/`disabled`/
  `dryrun` states and `parallel`/`serial` modes. **Pubkey auth only.**
- OBS/IBS and Gitea maintenance-request workflow (`assign`, `approve`, `reject`,
  `comment`, …) via the native OBS/IBS API (no `osc` subprocess).
- openQA / QEM Dashboard integration, incl. an `openqa_overview` (port of
  `oqa-search`) with `--export` into the testreport.
- Reference-host discovery via `refhosts.yml` (HTTPS- or filesystem-resolved,
  cached) and offline inventory search (`list_refhosts`).
- Cooperative reference-host locking (`/var/lock/mtui.lock`), interoperable with
  Python MTUI on a shared fleet.
- Test-report lifecycle: `load_template`, `checkout`, `commit`, `edit`, `export`
  (SVN and Gitea backends).
- File transfer (`put`/`get`) over SFTP.

## Build

Requires a Rust toolchain (edition 2024, **MSRV 1.96**). MSRV is pinned via
`rust-version` in `Cargo.toml`; there is no `rust-toolchain.toml` (the reference
dev environment uses a Homebrew rustc with no `rustup`).

```sh
cargo build --workspace              # build all crates
cargo run -p mtui-cli -- --help      # run the REPL binary (mtui)
cargo run -p mtui-mcp -- --help      # run the MCP server (mtui-mcp)
cargo test --workspace               # run tests
cargo fmt --all --check              # formatting gate
cargo clippy --workspace --all-targets -- -D warnings   # lint gate
```

## Runtime dependencies

Some backends shell out to external tools (kept optional; degrade gracefully when
absent):

- `svn` — testreport checkout/commit (SVN backend)
- a terminal emulator — for the `terms`/`switch` commands

The QAM review workflow talks to the OBS/IBS API natively (no `osc` subprocess);
it reads credentials from `oscrc` and is configured via the `[obs]` table
(`api_url`, `conffile`, `request_timeout`).

## Documentation

- [`PLAN-highlevel.md`](PLAN-highlevel.md) — architecture, crate layout,
  dependency mapping, and the 8-phase roadmap.
- Per-phase task breakdown is tracked in [beads](https://github.com/Dicklesworthstone/beads_rust)
  (`br ready`, `br epic status`, `br show <id>`); the detailed per-phase plans were
  migrated from the former `PLAN-phase0..8.md` files into beads epics + tasks.
- [`AGENTS.md`](AGENTS.md) — contributor/agent guide: conventions, contracts, and
  the definition of done.

## License

GPL-2.0-only, matching upstream MTUI. See `LICENSE`.
