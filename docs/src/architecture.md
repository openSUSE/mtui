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
| `mtui` (root) | Facade package owning `src/bin/{mtui,mtui-mcp}.rs` behind the `cli`/`mcp` features; its integration tests are in `tests/it.rs`. |
| `mtui-cli` | The reedline REPL library. |
| `mtui-mcp` | The rmcp server library. |

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
hidden globals.

## Trait injection and the no-cycles rule

The update-workflow doer/check registries live in `mtui-testreport`, but the
install/uninstall template that needs them lives in `mtui-hosts` — the lower
crate. `mtui-hosts` therefore declares a `PlanProvider` trait in terms of its own
types, and `mtui-testreport`'s `WorkflowRegistry` implements it. The rule when
two crates need to cooperate across that boundary: **define a trait in the lower
crate and inject an implementation from the higher one — never introduce a crate
cycle.**

**Inject at the point of use.** `update_flow::perform_install` /
`perform_uninstall` install the provider on the `HostsGroup` immediately before
driving the template. `OperationGroup::plans` has exactly one consumer, so there
is exactly one site that cannot forget. Injecting where the group is *built*
looks tidier but is not equivalent: an earlier design that wired one central
spot was never called at all, leaving `install` and `uninstall` running no
command while still reporting success.

## Contracts

These data-format and workflow contracts keep mtui interoperable with the SUSE
maintenance ecosystem. Each is owed to something live: the RRID grammar to
OBS/IBS/QEM, the `refhosts.yml` schema to the qam-metadata fleet database, the
testreport/export format to SVN/Gitea/TeReGen, the MCP tool surface to downstream
LLM clients, and the on-host formats to **other mtui processes sharing a fleet**
— including older releases, which is why they are a wire format and not an
implementation detail. The `crates/*/tests/` fixtures are the authority.

- **RRID grammar** — `project:kind:maintenance_id:review_id` and its parse errors
  (see the [FAQ](faq.md#what-is-an-rrid)).
- **`refhosts.yml` schema** — location-grouped on disk, but rows are
  merged/flattened/de-duplicated at load; parses identically to the golden
  fixtures.
- **Testreport / export text format** — including the idempotent
  `overview_inject` BEGIN/END block under `regression tests:`.
- **Remote-lock wire format** — one line, `timestamp:user:pid[:comment]`, shared
  by the operation lock and the pool-claim lock (see
  [Workflow concepts](concepts.md#locking)).
- **MCP tool names/schemas** — downstream LLM configs depend on them; the
  synthesised, slimmed schemas are snapshot-tested.
- **openQA `BUILD` query string** — `:{type}:{number}:{package}`
  (`crates/mtui-datasources/src/openqa/base.rs::OpenQABase::new`), owed to
  **openSUSE/qem-bot**, which writes it (`types/submissions.py:236`), not to
  TeReGen. Each component is qem-bot's own: `type` mirrors
  qem-dashboard's `incidents.type` column (`git`/`smelt`,
  `mtui_types::UpdateSource`); `number` is the dashboard's own incident
  number, which for **every** SLFO update (git- or OBS-served) is the
  review id, not the maintenance id
  (`crates/mtui-datasources/src/qem_dashboard/incident.rs::QemIncident::incident_number`);
  `package` is chosen by qem-bot's `sort_packages` ordering (demote
  arch-suffixed/`-livepatch-` names, then shortest, then alphabetical), not
  plain shortest-by-length. A Product Increment is connected to **neither**
  qem-dashboard nor openQA, so mtui skips the dashboard fetch for PI outright
  rather than querying a service that was never going to answer.

## Deliberate design choices

- **TOML config**, not INI.
- **Native OBS/IBS API** for the QAM review workflow, not an `osc` subprocess.
- **Two static binaries** (`mtui`, `mtui-mcp`), no runtime interpreter or
  virtualenv to install.
- **Async I/O** (`tokio`) with true parallel host fan-out.
- **Git-vs-OBS is decided per update, from the template, not from the RRID.**
  During the SL-Micro 6.0/6.1 cutover both workflows share the `SLFO:1.1` id
  space, so no rule over the RRID's shape can be correct — and an update can
  briefly be served *both* ways at once. `mtui_types::UpdateSource` is a
  **selection, not an observation**: `Git` when the template carries
  `gitea_commit_hash`, `Obs` otherwise, resolved once at load. On a
  dual-served update `Git` wins by design, and mtui leaves that update's OBS
  review request untouched — `assign`/`approve`/`reject`/`comment` only ever
  reach the Gitea side. See [`approve`](cli.md#approve) for the operator-facing
  consequence.
