# Contributing to MTUI

Thanks for your interest in improving MTUI. This document covers the
day-to-day workflow; see `docs/src/developer.md` for a deeper walk-through
of the codebase and `AGENTS.md` for the full architecture and Definition
of Done.

## Branching and pull requests

- Base all work on `main`; one logical change per PR.
- Larger refactors should be split into reviewable commits or, if they
  cross subsystems, into a series of PRs.
- Rebase rather than merge when bringing your branch up to date.
- CI must be green before review (fmt, clippy, test, and the feature matrix).
- Keep **both surfaces** (`mtui` and `mtui-mcp`) building and passing when
  touching commands, `Session`, the registry, or the entrypoints.

## Commit messages

We use [Conventional Commits](https://www.conventionalcommits.org/).
Common prefixes used in this repository:

- `feat:` for user-visible new behaviour
- `fix:` for a bug fix
- `docs:` for documentation only
- `test:` for test-only changes
- `refactor:` for internal restructuring with no behaviour change
- `chore:` / `build:` / `ci:` for tooling, packaging, CI

Reference SUSE Bugzilla / Bugzilla.opensuse.org issues with the standard
keywords (`bsc#NNNN`, `boo#NNNN`) in the commit body when applicable.

## Development setup

The project is a Cargo workspace and uses the standard Rust toolchain
(edition 2024, MSRV 1.96); no `rustup` is assumed.

Build everything:

```sh
cargo build --workspace
```

Run mtui from the checkout:

```sh
cargo run -p mtui-cli -- --help
cargo run -p mtui-mcp --features mcp -- --help
```

## Quality gates

Run these locally before opening a PR (CI runs the same commands):

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Warnings are errors under clippy. Tests run against default features; the
long pole is cold compilation, not the test run itself.

Also run the compile-only feature matrix, which catches feature-gate rot:

```sh
cargo build --workspace --no-default-features
cargo build --workspace --all-features
```

Do **not** routinely *test* `--all-features` — CI only compiles it.

Auto-format:

```sh
cargo fmt --all
```

### pre-commit (optional)

```sh
pre-commit install
```

Installs hooks for `cargo fmt` and `cargo clippy`. CI runs the same gates,
so pre-commit is purely a convenience.

## Tests

- Unit tests are colocated (`#[cfg(test)]`); integration tests live in
  `crates/*/tests/`.
- Add a new integration test as a `mod` line in the crate's `tests/it.rs`
  (one integration-test binary per crate), not as a new top-level
  `tests/*.rs`.
- Mock, don't hit the network/hosts: HTTP via `wiremock`, SSH via a
  `MockConnection`, `svn`/`osc` via a command-runner trait or a stub on
  `PATH`. Gate real hosts/containers behind `#[ignore]` + a CI env flag.
- Snapshot text contracts (testreport/export rendering, metadata parsing,
  MCP schemas, lock-file format) with `insta`.
- New/changed code needs >=80% patch coverage.

## Changelog

User-visible changes belong in the top-most unreleased section of
`CHANGELOG.md`. Internal refactors do not need an entry.

## Code of Conduct

By participating you agree to abide by the
[Code of Conduct](CODE_OF_CONDUCT.md).
