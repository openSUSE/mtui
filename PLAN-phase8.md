# Phase 8 — Packaging & Docs (detailed)

Goal: turn the working workspace into a **shippable product**: two release
binaries (`mtui`, `mtui-mcp`), generated shell completions, bundled data files,
wired optional features, user/developer docs, and distro packaging. This is the
"make it installable and maintainable" phase — small in code, broad in surface.

**Size:** small–medium. **Prereqs:** Phases 0–7 (a functioning `mtui` +
`mtui-mcp`). **Blocks:** nothing (final phase).

Source grounding (upstream `main`) — what "packaged mtui" ships:

| Upstream artifact                     | Phase-8 Rust equivalent                          |
| ------------------------------------- | ------------------------------------------------ |
| `[project.scripts] mtui`, `mtui-mcp`  | two `[[bin]]` targets (already from Phase 6/7)   |
| 6 optional extras (see 8.4)           | Cargo `[features]`                               |
| `mtui/terms/*.sh` (7 term wrappers)   | bundled data files (install + `terms_path`)      |
| `argcomplete` (shell completion)      | `clap_complete` generated completions            |
| `Documentation/*.rst` (Sphinx)        | ported user/dev docs (mdBook or `docs/*.md`)     |
| `README.md`, `SECURITY.md`, `CONTRIBUTING.md`, `AGENTS.md` | ported/updated       |
| `.github/workflows/ci.yml`, dependabot, templates | CI + release workflow + templates    |
| `codecov.yml`, `.pre-commit-config`, `.mergify.yml` | coverage + hooks + merge policy    |
| (distro) rpm spec for openSUSE        | `mtui-rs.spec` / OBS packaging                   |

---

## 8.1 Objectives / definition of done

- [ ] `cargo build --release` produces `mtui` and `mtui-mcp`.
- [ ] `cargo build --workspace --all-features` and the default feature set both
      compile; a feature matrix is CI-verified.
- [ ] Shell completions (bash/zsh/fish) generated for both binaries.
- [ ] `mtui/terms/*.sh` data files bundled and resolvable at runtime
      (`terms_path` analog).
- [ ] Optional features (`keyring`, `notify`, `mcp`, `pager`, `librpm`) toggle
      cleanly; default build stays lean.
- [ ] User + developer docs published; README reflects real usage.
- [ ] Release workflow builds cross-target binaries and publishes artifacts.
- [ ] Distro packaging (openSUSE rpm/OBS) drafted.

## 8.2 Binaries & release profile

- Confirm `[[bin]] mtui` (Phase 6) and `[[bin]] mtui-mcp` (Phase 7) names match
  upstream console scripts exactly.
- Finalize `[profile.release]` (lto=thin, strip, opt-level) from Phase 0.
- **Cross-compilation targets** (mtui is Linux-only per upstream classifiers):
  `x86_64-unknown-linux-gnu` + `aarch64-unknown-linux-gnu` (matches the arches in
  refhosts). Consider `musl` static builds for portable distribution.
- Reproducible/version stamping: embed `git describe` + crate version into
  `--version` (matches upstream's dep-version-aware version output).

## 8.3 Shell completions (`clap_complete`)

Upstream ships `argcomplete`. Rust: generate completions from the Phase-5/6 clap
parsers.
- A tiny `xtask` (or build step) that calls `clap_complete::generate` for
  bash/zsh/fish for both `mtui` and `mtui-mcp`.
- Install to standard locations (packaging step); also offer a hidden
  `mtui completions <shell>` subcommand (common Rust idiom) for ad-hoc use.
- **Note:** the *interactive* REPL tab-completion (Phase 6 reedline) is separate
  from *shell* completion — this phase only adds the latter.

## 8.4 Optional features (map extras → Cargo features)

Upstream extras → Rust `[features]`:

| Upstream extra   | Cargo feature | Crate/impl                              | Default? |
| ---------------- | ------------- | --------------------------------------- | -------- |
| `keyring`        | `keyring`     | `keyring` crate                         | off      |
| `mcp[cli]`       | `mcp`         | `rmcp` (Phase 7)                        | off*     |
| `notify-py`      | `notify`      | `notify-rust` (Phase 6)                 | off      |
| `rpm`/`norpm`    | `librpm`      | librpm FFI vs pure-Rust (Phase 1)       | off (pure default) |
| `argcomplete`    | (built-in)    | `clap_complete` — always available      | n/a      |
| —                | `pager`       | `minus` (Phase 5/6)                     | on?      |

*`mcp` off by default keeps the `mtui` REPL build from pulling the MCP SDK; the
`mtui-mcp` binary enables it. Decide whether the workspace default build includes
`mcp` or whether `mtui-mcp` is a `--features mcp` opt-in.

- **CI feature matrix**: build `--no-default-features`, default, and
  `--all-features` to catch feature-gate rot (a common Rust packaging bug).

## 8.5 Bundled data files (`mtui/terms/*.sh`)

Confirmed: 7 terminal-wrapper shell scripts (`term.{gnome,kde,sakura,screen,
tmux,urxvt,xterm}.sh`) referenced via `support/paths.py::terms_path`.
- Ship them under a data dir (e.g. `/usr/share/mtui-rs/terms/` when packaged;
  a repo `assets/terms/` in dev). Resolve at runtime via the `terms_path` port
  (Phase 1 `mtui-config`), honoring an install-prefix / XDG data lookup.
- The `terms`/`switch` commands (Phase 5, deny-listed in MCP) consume these.

## 8.6 Documentation

Upstream has substantial Sphinx docs (`cfg.rst`, `cli.rst`, `developer.rst`,
`faq.rst`, `installation.rst`, `iui.rst` 36K, `mcp.rst` 29K). Plan:
- **README.md** (via `readme-reviser`): what it is (Rust rewrite), install, quick
  start (`mtui <RRID>`, key commands), feature flags, status.
- **docs/** as Markdown (or mdBook): port the user-facing subset —
  installation, config (`cfg.rst` → config reference matching Phase-1 options),
  CLI/command reference, MCP usage (`mcp.rst`), FAQ. The huge `iui.rst`
  (interactive UI guide) can be condensed to the reedline REPL's actual behavior.
- **CONTRIBUTING.md / AGENTS.md / SECURITY.md**: port/adapt (build with cargo,
  test layout, MSRV, feature matrix; security contact from upstream SECURITY.md).
- **Man pages** (optional): `clap_mangen` for `mtui`/`mtui-mcp`.
- Do **not** auto-generate docs that drift — keep the config/command reference
  close to the code (consider generating the command list from the registry).

## 8.7 CI / release automation

Extend Phase-0 CI:
- **Release workflow** (tag-triggered): build `mtui` + `mtui-mcp` for the target
  matrix, run completions/mangen, package (tarball + rpm), attach to a GitHub
  Release. Cache with `Swatinem/rust-cache`.
- **Dependabot** for cargo (port `.github/dependabot.yml` to the cargo ecosystem).
- **Community files**: port issue/PR templates, `codecov.yml`,
  `.pre-commit-config.yaml` (cargo fmt/clippy hooks), and a merge policy
  (`.mergify.yml` equivalent, if used).
- **Coverage gate**: keep `cargo llvm-cov` → codecov from Phase 0.

## 8.8 Distro packaging (openSUSE)

mtui is an openSUSE/SUSE-QA tool → an rpm/OBS package is the real distribution
channel:
- Draft `mtui-rs.spec`: build with cargo (vendored deps for offline OBS builds via
  `cargo vendor`), install both binaries, completions, man pages, and
  `terms/*.sh` data files; declare runtime deps (`svn`, `osc`/`osc-plugin-qam`
  as recommends/optional — they're subprocess deps from Phases 3–4).
- Document `cargo vendor` + `cargo build --release --offline` for OBS.
- License: **GPL-2.0-only** (match upstream), `LICENSE` present from Phase 0.

## 8.9 Task breakdown

1. Finalize release profile + `--version` stamping (git describe + deps).
2. Feature wiring audit: map all 6 extras → features; ensure default build is lean.
3. `clap_complete` completions (+ optional `completions` subcommand); `clap_mangen`.
4. Bundle `terms/*.sh`; verify runtime `terms_path` resolution when installed.
5. CI feature matrix (`--no-default-features` / default / `--all-features`).
6. Release workflow: cross-target build → package → GitHub Release.
7. Docs: README (readme-reviser) + `docs/` (install/config/CLI/MCP/FAQ) + man.
8. Community files: templates, dependabot(cargo), codecov, pre-commit, mergify.
9. openSUSE `mtui-rs.spec` + `cargo vendor` offline-build notes.
10. Final full-matrix verification (8.1 checklist).

## 8.10 Deliverables (files)

- **Create:** `xtask/` (or build scripts) for completions/mangen;
  `assets/terms/*.sh`; `docs/**` (or `book/`); `mtui-rs.spec`;
  `.github/workflows/release.yml`; ported `.github/` templates,
  `dependabot.yml` (cargo), `codecov.yml`, `.pre-commit-config.yaml`,
  `CONTRIBUTING.md`, `SECURITY.md`, `AGENTS.md`, man pages.
- **Modify:** root + member `Cargo.toml`s (`[features]`, metadata, release
  profile); `README.md`; Phase-0 `ci.yml` (add feature matrix).

## 8.11 Risks / decisions to confirm

- **`mcp` default on/off.** Off keeps the `mtui` build lean but means the default
  workspace build doesn't produce `mtui-mcp`. Decide the default and document it.
- **`librpm` feature.** Pure-Rust RPM compare is the default (Phase 1); the
  `librpm` FFI feature needs system `librpm` at build+run time — CI must build
  both paths, and packaging must declare the dep when enabled.
- **Runtime subprocess deps** (`svn`, `osc`, terminal emulators). Not Cargo deps;
  declare as rpm recommends/requires and document. `terms/*.sh` need the target
  terminal installed to actually work.
- **Data-file path resolution.** Dev (`assets/terms/`) vs installed
  (`/usr/share/...`) must both resolve; align `terms_path` with an install prefix
  or XDG data dirs. Test both.
- **Doc drift.** The command/config reference should track the code; prefer
  generating the command list from the registry over hand-maintaining it.
- **Static (musl) vs dynamic builds.** musl gives portable binaries but complicates
  `librpm`/native TLS; decide per-target (rustls avoids OpenSSL, easing musl).
- **OBS offline build.** `cargo vendor` bloats the package source; confirm the
  openSUSE packaging workflow accepts vendored crates.

## 8.12 Out of scope for Phase 8

No new functionality — every capability ships from Phases 1–7. Phase 8 only
packages, documents, and automates distribution of what already works.

---

## Project completion criteria (all phases)

- [ ] `cargo build --release` → `mtui` + `mtui-mcp`; feature matrix CI-green.
- [ ] Parallel `run` across ≥2 hosts vs sshd fixture; correct per-host state.
- [ ] refhosts.yml / config / testreport fixtures parse identically to upstream.
- [ ] Ported test suites (types, config, hosts, datasources, testreport,
      commands, MCP) pass.
- [ ] MCP stdio round-trip verified; deny-list enforced; schemas slimmed.
- [ ] Completions, man pages, `terms/*.sh`, and docs shipped; rpm/OBS drafted.
