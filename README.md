# mtui

<img src="docs/assets/logo.svg" align="right" width="130" alt="mtui logo">

An **improved, idiomatic Rust successor** to MTUI — the **M**aintenance **T**est
**U**pdate **I**nstaller, SUSE QE's tool for validating maintenance updates: load
a request by RRID, install and test it on reference hosts over SSH in parallel,
then approve or reject. It drives OBS/IBS and Gitea review workflows, `svn`, and
openQA/QEM under the hood.

mtui is memory-safe, async-native, and distributed as static binaries, while
preserving the data-format and workflow contracts that keep it interoperable with
the SUSE maintenance ecosystem.

## Design goals

- **Safety & robustness** — strong types, exhaustive error enums, no interpreter.
- **Performance** — async I/O (`tokio`), true parallel host fan-out, fast startup.
- **Distribution** — two static binaries (`mtui`, `mtui-mcp`), no runtime
  interpreter or virtualenv; generated shell completions and man pages.
- **Maintainability** — a Cargo workspace with clean crate boundaries and one
  composition root.

## Two surfaces

- `mtui` — interactive REPL (line editing, tab completion, history).
- `mtui-mcp` — a Model Context Protocol server whose tools are **synthesised from
  the command registry**, so the CLI and the MCP surface never drift.

### MCP security boundary

Interactive/REPL-only commands (`shell`, `edit`, `help`, …) are permanently
deny-listed: MCP synthesis and routing never expose them over stdio or HTTP,
under every MCP profile, and the deny cannot be reversed with
`[mcp] tools_allow`. Local process execution and terminal launching are not
exposed at all — the `lrun` and `terms` commands were removed from mtui
entirely.

MCP profiles reduce the advertised tool surface; they are not authentication or
authorization. HTTP session isolation is likewise not caller authentication.
Keep the HTTP transport on its default loopback interface or place it behind an
authenticated boundary trusted to operate the remaining maintenance tools.

## Features

- Parallel SSH command execution across reference hosts (`run`, `update`,
  `install`, `prepare`, `downgrade`, …) with per-host `enabled`/`disabled`
  states. **Pubkey auth only.**
- OBS/IBS and Gitea maintenance-request workflow (`assign`, `approve`, `reject`,
  `comment`, …) via the native OBS/IBS API (no `osc` subprocess).
- Optional Slack review-request integration (`request_review`, off by default):
  posts a review request to a channel, can watch for reviewer 👍/👎 reactions
  until a verdict, and — once enabled — gates `approve` on an acknowledged
  review.
- openQA / QEM Dashboard integration, incl. an `openqa_overview` (port of
  `oqa-search`) with `--export` into the testreport.
- TeReGen-backed maintenance-queue browsing (`updates`) and template
  regeneration (`regenerate`).
- Reference-host discovery via `refhosts.yml` (HTTPS- or filesystem-resolved,
  cached) and offline inventory search (`list_refhosts`).
- Cooperative reference-host locking so concurrent testers can share a fleet: a
  PID-based operation lock (`/var/lock/mtui.lock`) serializing repository
  transactions, and an RRID-based pool claim (`/var/lock/mtui-pool.lock`)
  reserving a host for a template.
- Test-report lifecycle: `load_template`, `checkout`, `commit`, `edit`, `export`
  (SVN and Gitea backends).
- File transfer (`put`/`get`) over SFTP.
- Vim syntax highlighting for testreport files, packaged separately as
  `mtui-vim-plugin`.

## Install

Each release carries x86_64 `.deb` and `.rpm` packages plus the portable
tarballs. The REPL and the MCP server are packaged separately, so a host that
only serves MCP clients need not carry the REPL:

- `mtui` — the REPL, completions, man page, example config. Recommends
  `mtui-mcp`, so installing it alone still gets both.
- `mtui-mcp` — the MCP server, completions, man page, example config.

The binaries are static, so the packages declare no libc floor and install on
any x86_64 distro.
**On openSUSE prefer the OBS package** — it covers every tier-1 arch, not just
x86_64. See [docs/installation](docs/src/installation.md).

## Build

Requires a Rust toolchain (edition 2024, **MSRV 1.96**). MSRV is pinned via
`rust-version` in `Cargo.toml`; there is no `rust-toolchain.toml` (the reference
dev environment uses a Homebrew rustc with no `rustup`). See [`docs/`](docs) for
build-from-source, install, and packaging details.

```sh
cargo build --workspace              # build all crates (produces mtui + mtui-mcp)
cargo run -p mtui -- --help                    # run the REPL binary (mtui)
cargo run -p mtui --bin mtui-mcp -- --help     # run the MCP server (mtui-mcp)
cargo test --workspace               # run tests
cargo fmt --all --check              # formatting gate
cargo clippy --workspace --all-targets --all-features -- -D warnings   # lint gate
```

## Runtime dependencies

One backend shells out to an external tool (kept optional; degrades gracefully
when absent):

- `svn` — testreport checkout/commit (SVN backend)

The QAM review workflow talks to the OBS/IBS API natively (no `osc` subprocess);
it reads credentials from the user's oscrc — located exactly like `osc` itself
(`$OSC_CONFIG`, then `$XDG_CONFIG_HOME/osc/oscrc`, then `~/.oscrc`) — and is
configured via the `[obs]` table (`api_url`, `request_timeout`).

## Documentation

- [`docs/`](docs) — the user guide (mdBook): installation, configuration, the
  generated command reference, the MCP server, and an FAQ. Build with
  `mdbook build docs`, or read the Markdown under `docs/src/` directly.
- [`AGENTS.md`](AGENTS.md) — contributor/agent guide: conventions, contracts, and
  the definition of done.

## License

GPL-3.0-or-later (MTUI is GPL-2.0-only; this is an intentional relicense). See `LICENSE`.
