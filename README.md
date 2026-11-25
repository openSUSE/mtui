# mtui

<img src="docs/assets/logo.svg" align="right" width="130" alt="mtui logo">

mtui — the **M**aintenance **T**est **U**pdate **I**nstaller — is SUSE QE's tool
for validating maintenance updates: load a request by RRID, install and test it
on reference hosts over SSH in parallel, then approve or reject. It drives OBS/IBS and Gitea review workflows and
openQA/QEM; reports load from and upload to TeReGen's v2 API.

mtui is a memory-safe, async-native Rust program distributed as static binaries.
Its data-format and workflow contracts keep it interoperable with the SUSE
maintenance ecosystem.

## Design goals

- **Safety & robustness** — strong types, exhaustive error enums, no interpreter.
- **Performance** — async I/O (`tokio`), true parallel host fan-out, fast startup.
- **Distribution** — two static binaries (`mtui`, `mtui-mcp`), no runtime
  interpreter; generated shell completions and man pages.
- **Maintainability** — a Cargo workspace with clean crate boundaries and one
  composition root.

## Two surfaces

- `mtui` — interactive REPL (line editing, tab completion, history).
- `mtui-mcp` — a Model Context Protocol server whose tools are **synthesised from
  the command registry**, so the CLI and the MCP surface never drift.

### MCP security boundary

Interactive/REPL-only commands (`shell`, `edit`, `help`, …) are permanently
deny-listed, as are `config_set` and `teregen_login`: MCP synthesis and routing
never expose them over stdio or HTTP, under every MCP profile, and the deny
cannot be reversed with `[mcp] tools_allow`. `get` and `put` are deny-listed
too; the MCP server re-serves them as in-band transfer tools. mtui has no
command for local process execution or terminal launching, so neither is
exposed.

MCP profiles reduce the advertised tool surface; they are not authentication or
authorization. HTTP session isolation is likewise not caller authentication.
Keep the HTTP transport on its default loopback interface or place it behind an
authenticated boundary trusted to operate the remaining maintenance tools.

Tool calls can be recorded to a durable audit log and shipped to an
OpenTelemetry collector — see [Shipping the audit
log](docs/src/mcp.md#shipping-the-audit-log).

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
  `oqa-search`) with `--export` into the report document
  (`testing.openqa.extra`), plus `openqa_jobs` and `openqa_summary`.
- TeReGen-backed maintenance-queue browsing (`updates`) and template
  regeneration (`regenerate`).
- Reference-host discovery via `refhosts.yml` (HTTPS- or filesystem-resolved,
  cached) and inventory search (`list_refhosts`); `list_refhosts --free` also
  probes each host's live operation-lock and pool-claim state.
- Cooperative reference-host locking so concurrent testers can share a fleet: a
  PID-based operation lock (`/var/lock/mtui.lock`) serializing repository
  transactions, and an RRID-based pool claim (`/var/lock/mtui-pool.lock`)
  reserving a host for a template.
- Test-report lifecycle: `load_template`, `unload`, `checkout`, `commit`, `edit`,
  `export`, backed by TeReGen's v2 report document. `edit` with no argument
  opens the full-screen report-document editor, where testers also file and
  delete their own bugs.
- Multi-template sessions: several requests can be loaded at once, commands
  take `-T/--template` and `--all-templates`, `list_templates` shows what is
  loaded and `switch` changes the active template.
- SL Micro and SLES 16 hosts with a read-only root are driven through
  `transactional-update`.
- File transfer (`put`/`get`) over SFTP. The MCP server exposes in-band
  `get`/`put` (including `put source=`) and the `report_*` document tools.

## Install

Each release carries x86_64 `.deb` and `.rpm` packages plus the portable
tarballs. The REPL and the MCP server are packaged separately, so a host that
only serves MCP clients need not carry the REPL:

- `mtui` — the REPL, completions, man page, example config. Recommends
  `mtui-mcp`, so installing it alone still gets both.
- `mtui-mcp` — the MCP server, completions, man page, example config.

The binaries are static, so the packages declare no libc floor and install on
any x86_64 distro.

Other channels:

- Release tarballs `mtui-<ver>-x86_64-unknown-linux-musl.tar.gz` and
  `mtui-<ver>-aarch64-apple-darwin.tar.gz`, each with a `.sha256`.
- `cargo install --locked suse-mtui` installs both binaries.
- The multi-arch container image `ghcr.io/opensuse/mtui`.

**On openSUSE prefer the OBS package** — it covers every tier-1 arch, not just
x86_64. See [docs/installation](docs/src/installation.md).

## Build

Requires a Rust toolchain (edition 2024, **MSRV 1.96**). MSRV is pinned via
`rust-version` in `Cargo.toml`; there is no `rust-toolchain.toml` (the reference
dev environment uses a Homebrew rustc with no `rustup`). See [`docs/`](docs) for
build-from-source, install, and packaging details.

```sh
cargo build --workspace              # build all crates (produces mtui + mtui-mcp)
cargo run -p suse-mtui -- --help                    # run the REPL binary (mtui)
cargo run -p suse-mtui --bin mtui-mcp -- --help     # run the MCP server (mtui-mcp)
cargo test --workspace               # run tests
cargo fmt --all --check              # formatting gate
cargo clippy --workspace --all-targets --all-features -- -D warnings   # lint gate
```

## Runtime dependencies

mtui needs no external programs beyond its own binaries.

The QAM review workflow talks to the OBS/IBS API natively (no `osc` subprocess);
it reads credentials from the user's oscrc — located exactly like `osc` itself
(`$OSC_CONFIG`, then `$XDG_CONFIG_HOME/osc/oscrc`, then `~/.oscrc`) — and is
configured via the `[obs]` table (`api_url`, `request_timeout`).

## Documentation

- [User guide](https://opensuse.github.io/mtui/) — installation, configuration, the
  generated command reference, the MCP server, and an FAQ.
- [`docs/`](docs) — the guide's mdBook source. Build with `mdbook build docs` (needs
  `mdbook-linkcheck2`), or read the Markdown under `docs/src/` directly.
- [`AGENTS.md`](AGENTS.md) — contributor/agent guide: conventions, contracts, and
  the definition of done.

## License

GPL-3.0-or-later (MTUI is GPL-2.0-only; this is an intentional relicense). See `LICENSE`.
