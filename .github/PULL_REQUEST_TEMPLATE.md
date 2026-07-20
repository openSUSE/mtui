<!--
Thanks for contributing! Please make sure your PR follows the
checklist below. See CONTRIBUTING.md for the full workflow.
-->

## Summary

<!-- One or two sentences describing what this PR does and why. -->

## Changes

<!-- Bullet list of the notable changes. -->

-

## Checklist

<!-- CI runs the same gates; see CONTRIBUTING.md "Quality gates". -->

- [ ] Commits follow [Conventional Commits](https://www.conventionalcommits.org/).
- [ ] `cargo fmt --all --check` is clean.
- [ ] `cargo clippy --workspace --all-targets -- -D warnings` is clean.
- [ ] `cargo test --workspace` passes.
- [ ] `cargo test -p mtui-mcp -F mcp` passes (when touching commands, `Session`, the registry, or entrypoints).
- [ ] The compile-only feature matrix builds: `cargo build --workspace --no-default-features` and `--all-features`.
- [ ] New/changed code is covered (>= 80% patch coverage).
- [ ] User-visible changes are recorded in `CHANGELOG.md`.
- [ ] Documentation under `docs/src/` updated where relevant; generated pages re-run with `cargo xtask gen-docs`.

## Related issues

<!-- e.g. "Closes #123", "bsc#NNNN". -->
