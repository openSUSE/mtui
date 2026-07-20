# MCP server

`mtui-mcp` is a [Model Context Protocol](https://modelcontextprotocol.io) server
that exposes mtui's functionality as tools an LLM client can call. Its tool
surface is *synthesised from the same command registry* the `mtui` REPL uses:
each non-denied command becomes one tool, with a JSON input schema derived from
the command's `clap` argument spec, dispatched through the same engine as the
REPL. Adding, renaming, or removing a command changes the MCP tools
automatically. The underlying semantics of each tool are the command semantics in
the [Command reference](cli.md).

## Building and running

The server lives behind the `mcp` feature so the default build and the `mtui`
REPL never pull in the MCP SDK:

```sh
cargo run -p mtui-mcp --features mcp -- --help
```

Two transports are served:

- **stdio** (default) — one process == one client. Use this for a local LLM
  client that spawns the server as a subprocess.

  ```sh
  mtui-mcp                       # stdio
  ```

- **streamable HTTP** — many clients, with per-client session isolation.

  ```sh
  mtui-mcp --transport http --host 127.0.0.1 --port 8000
  ```

  `--host`/`--port` bind the HTTP listener and are ignored under stdio. The bind
  address is **loopback only** — rmcp's DNS-rebinding guard rejects non-loopback
  binds.

Logs go to **stderr**; **stdout** is the transport, so never write to it.

## Tool profiles

The advertised tool surface can be narrowed with `[mcp] profile`:

- **`full`** (default) — every synthesised tool.
- **`core`** — a curated everyday subset; the rest are filtered out before they
  reach the wire.

Fine-tune with `[mcp] tools_allow` (add specific tools on top of the profile) and
`[mcp] tools_deny` (remove specific tools; deny wins last). See the `[mcp]` table
in [Configuration](configuration.md) for these and the resource caps
(`max_output_bytes`, `max_active_jobs`, session budget, …).

## Security boundary

**Profiles are not authentication or authorization.** They reduce the advertised
tool surface; they do not gate callers. HTTP session isolation is likewise not
caller authentication. Keep the HTTP transport on its default loopback interface,
or place it behind an authenticated boundary you trust to operate the remaining
maintenance tools.

### Permanent deny-list

Some commands are **never** synthesised into MCP tools, under every profile — this
cannot be re-enabled with `[mcp] tools_allow`:

- **`lrun`** — executes commands as the local `mtui-mcp` process user. It stays
  available through direct `mtui` use and trusted callers of the core engine, but
  is never exposed over stdio or HTTP.
- The interactive/REPL-only commands, which require a controlling terminal or
  have no headless meaning: `quit`, `exit`, `EOF`, `switch`, `shell`, `help`,
  `edit`, `terms`.

The deny-list is intersected with the live registry and consistency-tested; a
renamed or removed command that leaves a stale deny-list entry is warned about at
boot, so the boundary cannot drift silently.

## Session state and isolation

Session state is isolated per client. Under **stdio** one process serves one
client, so there is exactly one config / loaded template set / group of connected
hosts. Under **HTTP** each connected client gets its own isolated session (its own
config view, loaded templates, and hosts), so concurrent clients never see each
other's state. Clients load their own state at runtime via the `load_template` and
`add_host` tools — `mtui-mcp` takes no boot-time update/host flags (see
[Invocation](invocation.md)).

The HTTP registry refuses to create more than `[mcp] session_cap` concurrent
sessions and reaps a session after `[mcp] session_idle_timeout` seconds of
inactivity (disconnecting its hosts — the SDK gives no per-session teardown
callback, so this sweep is what releases a vanished client's SSH connections).

## Multiple templates: scoping and fan-out

A session can hold several loaded templates at once (call `load_template` more
than once; loading an already-loaded RRID reloads it). `list_templates` lists the
set, and each template keeps its own test report and SSH host group.

Because every command's parser carries the shared `-T/--template` and
`--all-templates` flags, every synthesised command tool exposes two optional
parameters in its schema:

- **`template="<RRID>"`** — scope this one call to a single loaded template (the
  analogue of the REPL `-T` flag). An unknown RRID returns a clean error.
- **`all_templates=true`** — force fan-out across every loaded template (already
  the default for fan-out tools, so this is only for explicitness).

Omitting both fans a fan-out tool out across every loaded template, prefixing each
template's output with an `=== <RRID> ===` banner. A fanned-out call that fails on
one template keeps running on the others and reports an aggregate failure at the
end.

> `switch` is **not** an MCP tool (moving the active-template pointer is REPL-only
> navigation): over MCP you target a template per call with `template=`.
> `load_template` and `unload <rrid>` **are** exposed — each names its own RRID.
> `list_templates` is available as a read-only listing.

## Non-interactive prompt behaviour

MCP sessions run with no interactive prompter installed, so a command that would
prompt at the REPL never blocks waiting for input:

- **`approve`** refuses non-interactively when it would otherwise ask for
  confirmation (e.g. a Gitea hash mismatch or a missing token) rather than
  proceeding — pass the explicit flags so the intent is unambiguous.
- A **command timeout** aborts immediately instead of offering the REPL's
  wait/retry prompt.
- **`comment`** and **`commit`** take their text/message as a required argument
  (no editor/stdin prompt), and **`regenerate`** gates overwrite on the `--force`
  flag rather than a prompt.

SSH authentication is public-key only in every surface. A failed key auth reports
the host as unreachable; `mtui-mcp` never falls back to a password prompt.

## Testreport editing tools

Five hand-written tools operate on the loaded test report's checkout, replacing
the REPL's `$EDITOR`-based `edit` flow (which is deny-listed). Each accepts an
optional **`template="<RRID>"`** selecting which loaded template's checkout to act
on; with more than one template loaded an unscoped call is refused (pass
`template=`), and with zero or one it may be omitted. All refuse cleanly when no
test report is loaded.

### `testreport_read` (read-only)

Reads a file from the checkout as UTF-8 (lossy).

- Parameters: `relpath` (optional; defaults to the report's `log` file),
  `offset` (optional, 1-based first line, default 1), `limit` (optional, max
  lines), `template` (optional).
- `relpath` is resolved **inside** the checkout and may not escape it — `..`
  traversal, absolute paths, and in-tree symlinks pointing outside are all
  rejected. Use it to read `build_checks/<pkg>.<arch>.log`,
  `install_logs/<host>.log`, `source.diff`, `patchinfo.xml`, etc.
- Returns `{ "path", "line_count", "content" }`; when a window is requested
  (`offset`/`limit`) it additionally returns `offset` and `returned_lines`.

### `testreport_logs` (read-only)

Lists the auxiliary log files the `log` file doesn't cover.

- Parameters: `template` (optional).
- Returns `{ "path", "build_checks": [{"name","size"}], "install_logs":
  [{"name","size"}] }`. Fetch one with `testreport_read(relpath=…)`.

### `testreport_patch`

Splices an **inclusive, 1-indexed** line range. Atomic write (temp file +
`fsync` + rename).

- Parameters (required): `start_line`, `end_line`, `replacement`. Plus optional
  `template`. `end_line == start_line - 1` is a pure insertion before
  `start_line`; an empty `replacement` deletes the range. A non-empty replacement
  is normalised to end with exactly one newline.
- Returns `{ "path", "new_line_count", "replaced_lines", "inserted_lines",
  "bytes_written" }`.

### `testreport_write`

Full-file overwrite (same atomic write). Use when line drift makes patching
unreliable.

- Parameters (required): `content`. Plus optional `template`.
- Returns `{ "path", "bytes_written", "line_count" }`.

### `testreport_fill`

Bulk-fills the unfilled placeholder tokens the report ships with, idempotently
(never clobbers a hand-filled value). At least one field is required.

- Parameters (all optional; at least one required): `reproducer` (`YES`/`NO`),
  `status` (one of `FIXED`, `NOT_FIXED`, `HYPOTHETICAL`, `NOT_REPRODUCIBLE`,
  `NO_ENVIRONMENT`, `TOO_COMPLEX`, `SKIPPED`, `OTHER`), `summary`
  (`PASSED`/`FAILED`), `template`.
- Returns `{ "path", "filled": {"summary","reproducer","status"}, "bytes_written",
  "line_count" }`.

> **Always call `testreport_read` immediately before `testreport_patch`.** Line
> numbers shift after every patch, so two patches computed against one read will
> land the second at the wrong offset.

### Worked example

Read the loaded report, replace lines 2–3 with three lines, then re-read to
confirm:

```text
> testreport_read()
{ "path": ".../log", "line_count": 5,
  "content": "header\nfoo\nbar\nfooter\ntrailer\n" }

> testreport_patch(start_line=2, end_line=3, replacement="X\nY\nZ\n")
{ "path": ".../log", "new_line_count": 6,
  "replaced_lines": 2, "inserted_lines": 3, "bytes_written": 34 }

> testreport_read()
{ "path": ".../log", "line_count": 6,
  "content": "header\nX\nY\nZ\nfooter\ntrailer\n" }
```

## Background jobs

Long-running commands run as background jobs so a tool call returns promptly with
a job id you can poll. The slow host commands — `run`, `update`, `downgrade`,
`prepare`, `install`, `uninstall`, `set_repo`, `reboot`, and `regenerate` — accept
a **`background=true`** parameter. Instead of holding the request open for the
minutes the operation takes, the call returns immediately with the job id(s); when
the command fans out across several templates it mints **one job per template**,
each independently pollable and cancellable.

Four job-control tools manage them:

- **`job_list`** (read-only) — every job in the session and its state.
- **`job_status(job_id)`** (read-only) — one job's state (`running` / `done` /
  `failed` / `cancelled`) and elapsed time.
- **`job_result(job_id)`** (read-only) — a finished job's captured output; it
  errors while the job is still running (poll `job_status` first) and surfaces the
  command's failure envelope if it failed.
- **`job_cancel(job_id)`** — cancel a running job. (A command already executing on
  a host may run to completion there even after cancel returns — the same caveat as
  Ctrl-C on a foreground `run`.)

Jobs are scoped to the session (under HTTP, the caller's isolated session, so one
client never sees another's jobs). The per-session job budget
(`max_active_jobs`, `max_completed_jobs`) and the single-result size cap
(`max_output_bytes`) bound resource use so one client cannot exhaust the server or
dwarf the client's context.

## Long-running calls: progress heartbeats

Many commands legitimately take minutes (a `run` against a slow refhost, an
`update`, an SVN `checkout`). To keep MCP clients from timing out, `mtui-mcp`
emits `notifications/progress` while a slow tool runs — for both synthesised
command tools and the testreport tools — provided the client supplied a
`progressToken` on the request. Spec-compliant clients (Claude Desktop, opencode,
the MCP Inspector, Cursor, …) reset their read deadline on each frame, so a
ten-minute command still returns cleanly. Clients that ignore progress
notifications should raise their own per-server read timeout instead. The fast
job-control tools do not emit heartbeats.

## Connecting a client

`mtui-mcp` speaks standard MCP framing. A stdio client spawns the binary as a
subprocess; an HTTP client connects to `http://HOST:PORT/mcp`.

Claude Desktop (stdio) — in `claude_desktop_config.json`:

```json
{
  "mcpServers": {
    "mtui": { "command": "mtui-mcp", "args": ["--transport", "stdio"] }
  }
}
```

opencode (remote HTTP) — start `mtui-mcp --transport http --port 8765`, then in
`opencode.json`:

```json
{
  "mcp": {
    "mtui": { "type": "remote", "url": "http://127.0.0.1:8765/mcp", "enabled": true }
  }
}
```

opencode also accepts `"type": "local"` with `"command": ["mtui-mcp",
"--transport", "stdio"]` to spawn it per-session instead.
