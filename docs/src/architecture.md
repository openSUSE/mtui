# Architecture

This page is a map, not a deep dive: it sketches how mtui is put together so
the rest of the book (and the source) is easier to navigate. For the day-to-day
contributor workflow — toolchain, quality gates, adding a command, testing — see
the [Developer guide](developer.md). For the authoritative contributor spec and
definition of done see `AGENTS.md`, which lives in the repository root.

## Workspace layout

mtui is a Cargo workspace of single-purpose crates. Lower crates never depend on
higher ones; `mtui-core` is the composition root that wires everything together.

| Crate | Job |
|-------|-----|
| `mtui-types` | Domain types + the error hierarchy. Pure, sync, no I/O. |
| `mtui-config` | TOML config + XDG path resolution. |
| `mtui-hosts` | SSH/SFTP (russh), the `Target`/`HostsGroup` model, locks, the pool arbiter. Async. |
| `mtui-datasources` | Shared HTTP; refhosts resolve/search/verify; the openQA/QEM/Gitea/OBS/oqa-search clients. Async. |
| `mtui-testreport` | Testreport lifecycle, metadata parsers, SVN/Gitea checkout, and the update workflow (actions/checks/export). |
| `mtui-core` | The `Command` trait + registry, `Session`, the dispatch engine, and the wiring that ties the crates together. |
| `mtui-cli` | The reedline REPL and the `mtui` binary. |
| `mtui-mcp` | The rmcp server and the `mtui-mcp` binary. |

## The command registry is the single source of truth

Every command implements one `Command` trait and is registered into a central
registry by an explicit `register_all()` (no auto-registration magic). Three
consumers iterate that **one** registry:

- the REPL's command dispatch and tab-completion;
- the generated [command reference](cli.md);
- the `mtui-mcp` tool synthesiser, which turns each non-denied command's `clap`
  arg spec into a JSON tool schema.

So adding, renaming, or removing a command updates the REPL, the docs, and the MCP
tool surface together — they cannot drift. See [MCP server](mcp.md) for the
deny-list that keeps REPL-only commands off the wire.

## Session state, not globals

Commands operate on an explicit `Session` (config, the `HostsGroup` targets, the
loaded templates/metadata, the display) passed into each call — there are no
hidden globals. This is the Rust replacement for MTUI's `CommandPrompt`
god-object.

## Composition root and the no-cycles rule

`mtui-core`'s wiring injects the update-workflow doer/check registries (which live
in `mtui-testreport`) into the host `Target` dispatch (in `mtui-hosts`) **via
traits**, so `mtui-hosts` never has to depend on `mtui-testreport`. The rule when
two lower crates need to cooperate: **define a trait and inject it at the
composition root — never introduce a crate cycle.**

## Contracts

These data-format and workflow contracts keep mtui interoperable with the SUSE
maintenance ecosystem and with a Python MTUI sharing the same fleet. They are
preserved deliberately; upstream `tests/` fixtures are the authority for the
formats.

- **RRID grammar** — `project:kind:maintenance_id:review_id` and its parse errors
  (see the [FAQ](faq.md#what-is-an-rrid)).
- **`refhosts.yml` schema** — location-grouped on disk, but rows are
  merged/flattened/de-duplicated at load; parses identically to upstream fixtures.
- **Testreport / export text format** — including the idempotent
  `overview_inject` BEGIN/END block under `regression tests:`.
- **Remote-lock wire format** — one line, `timestamp:user:pid[:comment]`, shared
  by the operation lock and the pool-claim lock (see
  [Workflow concepts](concepts.md#locking)).
- **MCP tool names/schemas** — downstream LLM configs depend on them; the
  synthesised, slimmed schemas are snapshot-tested.

## Intentional deviations from upstream

mtui is a redesign, not a transpile. The deliberate departures:

- **TOML config**, not INI (defaults still match upstream exactly).
- **Native OBS/IBS API** for the QAM review workflow, not an `osc` subprocess.
- **Two static binaries** (`mtui`, `mtui-mcp`), no Python runtime or virtualenv.
- **Async I/O** (`tokio`) with true parallel host fan-out.
