# Phase 0 — Workspace Bootstrap (detailed)

Goal: turn the empty single-crate scaffold into a **buildable, CI-gated Cargo
workspace** with all eight member crates stubbed, shared dependencies pinned at
the root, and quality gates (fmt, clippy, test, coverage) green. No mtui logic
yet — this phase establishes the skeleton every later phase builds on.

**Size:** small. **Blocks:** all later phases. **Prereqs:** none.

Local toolchain confirmed: `rustc 1.96.0`, `cargo 1.96.0`, edition 2024,
installed via Homebrew (**no `rustup`**). MSRV is therefore pinned to **1.96**
(the installed floor) rather than 1.85; a `rust-toolchain.toml` channel pin is
omitted because Homebrew provides a single fixed toolchain and cannot switch
channels. The CI matrix relies on rustup being present on GitHub runners.

---

## 0.1 Objectives / definition of done

- [ ] Repo is a Cargo **workspace**; `cargo build --workspace` succeeds.
- [ ] All 8 member crates exist and compile (empty `lib.rs` / stub `main.rs`).
- [ ] Shared deps declared once in `[workspace.dependencies]`, inherited by members.
- [ ] `cargo fmt --check`, `cargo clippy -- -D warnings`, `cargo test --workspace` all pass.
- [ ] CI runs the above on push/PR; coverage reported.
- [ ] MSRV pinned + documented in `Cargo.toml` (`rust-version`) + README. No
      `rust-toolchain.toml` (local toolchain is Homebrew rustc, no rustup).
- [ ] Initial git commit(s) landed; `README.md` states project intent + status.

## 0.2 Task breakdown

### T0 — Pre-flight
- Confirm `git status` clean-ish; the current untracked `src/`, `Cargo.toml`,
  `Cargo.lock`, `.gitignore` will be restructured.
- MSRV: **1.96** (the installed Homebrew floor). Edition 2024 needs ≥1.85, so
  1.96 comfortably satisfies it. CI tests `stable` (≥1.96) + `beta`. Document in
  README. No local `rustup`, so no channel-pinned `rust-toolchain.toml`.

### T1 — Workspace root `Cargo.toml`
Replace the single-package manifest with a virtual workspace:

```toml
[workspace]
resolver = "3"
members = ["crates/*"]

[workspace.package]
version = "0.1.0"
edition = "2024"
rust-version = "1.96"
license = "GPL-2.0-only"       # match upstream mtui
repository = "https://gitlab.suse.de/osukup/mtui-rs"

[workspace.dependencies]
# runtime
tokio      = { version = "1", features = ["full"] }
futures    = "0.3"
# cli
clap          = { version = "4", features = ["derive", "env"] }
clap_complete = "4"
reedline      = "0.<latest>"
# ser/de + config
serde      = { version = "1", features = ["derive"] }
serde_json = "1"
serde_yaml = "0.9"          # or serde_yml; decide in Phase 1
directories = "5"
# http
reqwest = { version = "0.12", features = ["json", "rustls-tls"], default-features = false }
# ssh (Phase 2)
russh       = "0.<latest>"
russh-sftp  = "0.<latest>"
# mcp (Phase 7)
rmcp = "0.<latest>"
# observability + errors
tracing            = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter"] }
thiserror = "2"
anyhow    = "1"
# dev / test
insta    = "1"
wiremock = "0.6"
# internal crates (path deps)
mtui-types       = { path = "crates/mtui-types" }
mtui-config      = { path = "crates/mtui-config" }
mtui-hosts       = { path = "crates/mtui-hosts" }
mtui-datasources = { path = "crates/mtui-datasources" }
mtui-testreport  = { path = "crates/mtui-testreport" }
mtui-core        = { path = "crates/mtui-core" }

[profile.release]
lto = "thin"
strip = true
```

> Version pins marked `<latest>` are resolved during execution (`cargo add`)
> and captured in `Cargo.lock`.

### T2 — Create the 8 member crates
Each member inherits shared metadata via `.workspace = true`.

| Crate              | Kind | Depends on (internal)                              | Stub content            |
| ------------------ | ---- | -------------------------------------------------- | ----------------------- |
| `mtui-types`       | lib  | —                                                  | `lib.rs` + 1 smoke test |
| `mtui-config`      | lib  | types                                              | `lib.rs` + smoke test   |
| `mtui-hosts`       | lib  | types, config                                      | `lib.rs`                |
| `mtui-datasources` | lib  | types, config                                      | `lib.rs`                |
| `mtui-testreport`  | lib  | types, config                                      | `lib.rs`                |
| `mtui-core`        | lib  | types, config, hosts, datasources, testreport      | `lib.rs`                |
| `mtui-cli`         | bin  | core (+ all)                                        | `main.rs` → `mtui`      |
| `mtui-mcp`         | bin  | core (+ all)                                        | `main.rs` → `mtui-mcp`  |

Example member manifest (`crates/mtui-types/Cargo.toml`):

```toml
[package]
name = "mtui-types"
version.workspace = true
edition.workspace = true
rust-version.workspace = true
license.workspace = true

[dependencies]
serde.workspace = true
thiserror.workspace = true
```

Example binary manifest (`crates/mtui-cli/Cargo.toml`) declares the binary name:

```toml
[[bin]]
name = "mtui"
path = "src/main.rs"
```

Stub `main.rs` for both binaries: parse `--help` via clap, print version, exit 0
(enough to prove the entry point links).

### T3 — Relocate existing stub
- Move logic from `src/main.rs` into `crates/mtui-cli/src/main.rs`.
- Delete the root `src/` directory.

### T4 — Toolchain + gitignore
- **No `rust-toolchain.toml`.** The local toolchain is Homebrew rustc 1.96 with
  no `rustup`, so a channel-pinned toolchain file cannot be honored locally and
  would only mislead. MSRV is instead pinned via `rust-version = "1.96"` in
  `[workspace.package]` and documented in the README.
- Extend `.gitignore`: keep `/target`; add `**/*.rs.bk`, `.DS_Store`.
- Add `rustfmt.toml` (edition 2024, defaults) and `clippy` lint policy
  (optional `[lints]` table in workspace: `unsafe_code = "warn"`, etc.).

### T5 — CI (GitLab CI)
The project lives on `gitlab.suse.de`, so CI is GitLab CI (`.gitlab-ci.yml`),
not GitHub Actions. Base image: `rust:latest` (Docker Hub).

- **stages / jobs:**
  - `fmt` — `rustup component add rustfmt` then `cargo fmt --all --check`
  - `clippy` — `rustup component add clippy` then
    `cargo clippy --workspace --all-targets -- -D warnings`
  - `test` — `cargo build --workspace` + `cargo test --workspace`
  - `coverage` — install `cargo-llvm-cov` + `llvm-tools-preview`, emit Cobertura
    XML as a GitLab `coverage_report` artifact and expose the total via the
    `coverage:` regex (native MR diff coverage; no external service). CI-only —
    `cargo-llvm-cov` is not installed locally.
- **triggers:** default GitLab pipeline on push / merge request.
- **caching:** `.cargo/` + `target/` keyed on `Cargo.lock`.
- Optional: `.pre-commit-config.yaml` with `cargo fmt`/`clippy` hooks.

### T6 — Docs + housekeeping
- `README.md`: one-paragraph intent (Rust rewrite of openSUSE/mtui), status
  badge placeholders, build/test instructions, link to `PLAN-highlevel.md`.
- Add `LICENSE` (GPL-2.0-only, matching upstream).
- Consider `AGENTS.md` port later (not required for Phase 0).

### T7 — Commit
- Logical commits: (1) workspace + crates skeleton, (2) toolchain + gitignore,
  (3) CI, (4) docs. Conventional Commit style.

## 0.3 Verification

```sh
cargo build --workspace            # all 8 crates compile
cargo test  --workspace            # smoke tests pass
cargo fmt   --all --check          # formatting clean
cargo clippy --workspace --all-targets -- -D warnings
cargo run -p mtui-cli -- --help    # prints usage, exits 0
cargo run -p mtui-mcp -- --help    # prints usage, exits 0
```

CI must show all jobs green on the opening PR.

## 0.4 Deliverables (files)

- **Modify:** `Cargo.toml` (→ virtual workspace), `.gitignore`, `README.md`.
- **Create:** `crates/{mtui-types,mtui-config,mtui-hosts,mtui-datasources,`
  `mtui-testreport,mtui-core,mtui-cli,mtui-mcp}/{Cargo.toml,src/{lib,main}.rs}`,
  `rustfmt.toml`, `.gitlab-ci.yml`, `LICENSE`.
- **Delete:** root `src/main.rs` (relocated).
- **Explicitly not created:** `rust-toolchain.toml` (no rustup; MSRV documented
  via `rust-version` + README instead).

## 0.5 Risks / decisions to confirm

- **`serde_yaml` is in maintenance mode.** Alternatives: `serde_yml` (fork) or
  `saphyr`. Defer final choice to Phase 1 where YAML fidelity vs `ruamel.yaml`
  matters; the workspace dep is a placeholder.
- **MSRV vs edition 2024.** Edition 2024 requires ≥1.85, but the local floor is
  Homebrew rustc 1.96 with no rustup, so MSRV is pinned to **1.96**. Revisit only
  if a lower floor is needed and a rustup-based toolchain is adopted.
- **Crate granularity.** 8 crates is deliberate for compile isolation + optional
  MCP. If build times or path-dep churn become painful, `mtui-cli`/`mtui-mcp`
  could later re-merge, but split is preferred now.
- **License.** Assumed GPL-2.0-only to match upstream (derivative work); confirm
  before first public push.

## 0.6 Out of scope for Phase 0

No domain types, no SSH, no HTTP clients, no REPL behavior, no MCP tools — only
the buildable, gated skeleton. First real logic lands in Phase 1 (`mtui-types`).
