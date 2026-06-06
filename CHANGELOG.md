# Changelog

All notable user-visible changes to MTUI are documented in this file.

The format is based on [Keep a Changelog 1.1.0](https://keepachangelog.com/en/1.1.0/),
and this project follows [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- New `mtui-mcp` console script (optional `mcp` extra) ships a
  FastMCP server that exposes every non-interactive mtui command as
  a Model Context Protocol tool, plus dedicated `testreport_read` /
  `testreport_patch` / `testreport_write` tools, so LLM clients can
  drive a headless mtui session over `stdio` or `http`. See
  `Documentation/mcp.rst` for the deny-list, single-session caveat,
  and a `read → patch → read` worked example.

### Fixed

- `lrun` now captures child stdout/stderr and propagates the real exit
  code when invoked through `mtui-mcp` (or any non-interactive prompt).
  Previously a failing local command surfaced to the MCP client as
  `exit_code=1` with no output; the child's streams went to the server's
  TTY and `CalledProcessError.returncode` was discarded. The interactive
  REPL path is unchanged — output still streams live to the terminal.
- `openqa_overview` now shows build check logs for all packages in a
  multi-package update, not just one. Previously only logs matching a
  single package name extracted from the build string were displayed,
  causing build check results for other packages in the update to be
  silently omitted.
- `mtui-mcp` now exposes the full surface of `set_repo` and
  `load_template`. The previous schema-synthesis collapsed each
  command's mutually exclusive flag pair onto a single MCP parameter,
  silently hiding the `--remove` operation of `set_repo` and the
  `--kernel-review-id` path of `load_template` from clients (visible at
  boot as `duplicate dest` warnings). `set_repo` now takes a required
  `operation` enum (`add`/`remove`); `load_template` now takes two
  optional strings `auto_review_id` / `kernel_review_id` with an
  "exactly one required" runtime check.
- `mtui-mcp` now preserves real argparse defaults for optional
  list-shaped arguments. Previously the schema layer replaced every
  optional list default with `[]`, so invoking `openqa_overview`
  without `aggregated_groups` emitted a bare `--aggregated-groups`
  flag and crashed with *"expected at least one argument"*. Non-empty
  argparse defaults (like `["core"]`) now flow through to the MCP
  schema and match the REPL behaviour.
- `config show` with no attributes no longer crashes with
  `TypeError: 'ConfigOption' object is not subscriptable`. The handler
  was still indexing config entries as tuples after they were
  refactored into a `ConfigOption` dataclass, so any caller (REPL or
  `mtui-mcp`'s `config_show` tool) that omitted attribute names hit
  the error instead of getting the sorted option list.
