# Introduction

**mtui-rs** is an improved, idiomatic Rust successor to
[openSUSE/mtui](https://github.com/openSUSE/mtui) — the **M**aintenance **T**est
**U**pdate **I**nstaller, SUSE QE's tool for validating maintenance updates: load
a request by RRID, install and test it on reference hosts over SSH in parallel,
then approve or reject. It drives the OBS/IBS and Gitea review workflows and
openQA/QEM under the hood.

This is a redesign, not a transpile: MTUI is the behavioral reference and source
of domain truth, but mtui-rs is memory-safe, async-native, and distributed as two
static binaries — while preserving the data-format and workflow contracts that
keep it interoperable with the SUSE maintenance ecosystem (RRID grammar, the
`refhosts.yml` schema, the testreport/export text format, and the remote-lock
wire format that lets a Rust and a Python mtui share a host fleet).

## Two surfaces

- **`mtui`** — the interactive REPL (line editing, tab completion, history). See
  [Invocation](invocation.md) for how to launch it and seed a session.
- **`mtui-mcp`** — a Model Context Protocol server whose tools are *synthesised
  from the same command registry* as the REPL, so the CLI and the MCP surface
  never drift. See [MCP server](mcp.md).

## This book

- [Installation](installation.md) — build from source, install the binaries,
  completions, man pages, and terminal-launcher scripts.
- [Invocation](invocation.md) — the `mtui` and `mtui-mcp` binary flags and how to
  start a session.
- [Configuration](configuration.md) — the TOML config file, its resolution order,
  and every option with its default.
- [Command reference](cli.md) — every REPL command and its arguments, generated
  directly from the command registry.
- [Workflow concepts](concepts.md) — fan-out across templates, reference-host pool
  selection, host state, and locking.
- [MCP server](mcp.md) — running `mtui-mcp`, the tool surface, the testreport and
  job tools, profiles, and the security boundary.
- [Architecture](architecture.md) — the crate layout and the data-format
  contracts.
- [Developer guide](developer.md) — the toolchain, quality gates, the command
  trait, how to add a command, and testing conventions.
- [FAQ](faq.md).

For contributor conventions and the definition of done, see `AGENTS.md`, which
lives in the repository root.
