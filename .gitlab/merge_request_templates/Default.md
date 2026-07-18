<!--
Thanks for contributing! Make sure this MR follows the checklist below.
See AGENTS.md for the full engineering conventions and Definition of Done.
-->

## Summary

<!-- One or two sentences: what this MR does and, above all, *why*
(the diff already shows *what*). -->

## Changes

<!-- Bullet list of the notable changes. -->

-

## Checklist (mirrors the AGENTS.md Definition of Done)

- [ ] Commits follow [Conventional Commits](https://www.conventionalcommits.org/).
- [ ] `cargo fmt --all --check` is clean.
- [ ] `cargo clippy --workspace --all-targets -- -D warnings` is clean.
- [ ] `cargo test --workspace` passes (default features).
- [ ] Feature matrix compiles: `cargo build --workspace --no-default-features`
      and `cargo build --workspace --all-features`.
- [ ] Both surfaces (`mtui`, `mtui-mcp`) still build and pass when commands,
      `Session`, the registry, or entrypoints changed.
- [ ] New/changed code has >=80% patch coverage; new text output is snapshotted.
- [ ] Docs under `docs/` updated where relevant.

## Related issues

<!-- e.g. "Closes #123", "bsc#NNNN". -->
