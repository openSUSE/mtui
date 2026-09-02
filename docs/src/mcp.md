# MCP server

`mtui-mcp` is a [Model Context Protocol](https://modelcontextprotocol.io) server
that exposes mtui's functionality as tools an LLM client can call. Its tool
surface is *synthesised from the same command registry* the `mtui` REPL uses:
each non-denied command becomes one tool, with a JSON input schema derived from
the command's `clap` argument spec, dispatched through the same engine as the
REPL. Adding, renaming, or removing a command changes the MCP tools
automatically. Two commands, `get` and `put`, are denied synthesis and
re-served under the same names as hand-written tools that carry file content
in-band (see [File transfer tools](#file-transfer-tools)) — a path on the
server's filesystem would be meaningless to a remote `--transport http` client,
and is never needed on stdio either. The underlying semantics of each tool are the command semantics in
the [Command reference](cli.md).

## Building and running

The server lives behind the `mcp` feature, enabled by default alongside the
`mtui` REPL's `cli` feature (see [Installation](installation.md)):

```sh
cargo run -p mtui --bin mtui-mcp -- --help
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

- The interactive/REPL-only commands, which require a controlling terminal or
  have no headless meaning: `quit`, `exit`, `EOF`, `switch`, `shell`, `help`,
  `edit`.
- The path-based transfer commands `get` and `put`, replaced by the
  hand-written in-band tools of the same names (#434): their synthesized forms
  exchanged server-local paths a remote client cannot read or create. (Only
  the synthesized command forms are permanently gone; the replacement
  hand-written `get`/`put` are ordinary tools — present in `full`, restorable
  under `core` via `tools_allow`, removable via `tools_deny`.)

Local process execution and terminal launching are not on this list because they
are not in mtui at all. The former `lrun` (run a command as the local process
user) was removed outright — an MCP client already has its own local execution,
and a REPL user is already at a shell. The former `terms` (spawn a terminal
emulator per refhost) went the same way: it duplicated `shell` (#566).

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

This isolation depends on rmcp's legacy `Mcp-Session-Id` session lifecycle, so
`mtui-mcp` advertises protocol revisions `2024-11-05` through `2025-11-25` and
declines `2026-07-28`: that revision removes protocol-level sessions and is
served statelessly (a throwaway session per request) regardless of config, which
would defeat per-client isolation under HTTP. A client asking for it falls back
to the latest revision `mtui-mcp` does support.

## Multiple templates: scoping and fan-out

A session can hold several loaded templates at once (call `load_template` more
than once; loading an already-loaded RRID reloads it). `list_templates` lists the
set, and each template keeps its own test report and SSH host group.

Because every command's parser carries the shared `-T/--template` and
`--all-templates` flags, every synthesised command tool exposes two optional
parameters in its schema:

- **`template="<RRID>"`** — scope this one call to a single loaded template (the
  analogue of the REPL `-T` flag). An unknown RRID returns a clean error.
- **`all_templates=true`** — force fan-out across every loaded template.
- **`all_templates=false`** — suppress fan-out on a tool whose command fans out
  by default, narrowing it to one template instead.

Omitting both parameters resolves per command: a read/annotate tool
(`list_*`/`show_*`/`openqa_*`/`checkers`/`checkout`/`export`/`set_workflow`/
`comment`) fans out across every loaded template, prefixing each template's
output with an `=== <RRID> ===` banner. A host-mutating or remote-write tool
(`update`, `prepare`, `downgrade`, `install`, `uninstall`, `set_repo`, `run`,
`reboot`, `lock`, `unlock`, `add_host`, `remove_host`, `approve`, `reject`,
`assign`, `unassign`, `request_review`, `commit`, plus the hand-written `get`/
`put` transfer tools) **never implicitly fans out**: with several templates
loaded and neither parameter set, the call is refused rather than guessed at —
the error names the loaded RRIDs and the `template=`/`all_templates=true`
escape hatches. This is the same rule the `testreport_*` tools use for their own
required-with-several-loaded `template` parameter (below).

A fanned-out call that fails on one template keeps running on the others and
reports an aggregate failure at the end.

> `switch` is **not** an MCP tool (moving the active-template pointer is REPL-only
> navigation): over MCP you target a template per call with `template=`.
> `load_template` and `unload <rrid>` **are** exposed — each names its own RRID.
> `list_templates` is available as a read-only listing.

## Non-interactive prompt behaviour

MCP sessions run with no interactive prompter installed, so a command that would
prompt at the REPL never blocks waiting for input:

- **`approve`** refuses non-interactively on a Gitea checkout-hash mismatch
  (at the REPL it prompts for confirmation, default no) rather than proceeding;
  a missing Gitea token or a failed Gitea call refuses on every surface, since
  the check never produced a verdict to confirm.
- **`load_template`**'s interactive "Force continue loading template ?"
  fallback — for a stale checkout hash that TeReGen also refuses to
  regenerate (already hand-edited) — is exposed as the `--force-continue`
  argument; without it the load is unconditionally abandoned, since the
  question's non-interactive default is "no" (openSUSE/mtui#517). A
  force-continued load neither repairs nor bypasses the recorded
  mismatch, so it does not touch `approve`'s refusal above —
  and `commit`/`export` refuse the same way, unless given their own
  `--allow-stale`, so a stale-loaded report cannot be published or
  overwritten non-interactively either. `approve`'s `check_hash()` re-query
  is live (asks Gitea again, every call); the other three instead read
  whether *the load itself* saw a mismatch, so a report that loaded clean
  and only goes stale once the session is already open (a maintainer
  pushes to the PR while `mtui-mcp` still holds it loaded) passes all
  three silently — `commit` is the one of the three where that gap is most
  likely to matter, since it is the publish step.
- A **command timeout** aborts immediately instead of offering the REPL's
  wait/retry prompt.
- **`comment`** and **`commit`** take their text/message as a required argument
  (no editor/stdin prompt), and **`regenerate`** gates overwrite on the `--force`
  flag rather than a prompt.

SSH authentication is public-key only in every surface. A failed key auth reports
the host as unreachable; `mtui-mcp` never falls back to a password prompt.

## File transfer tools

The hand-written `get` and `put` tools replace the synthesized commands of the
same names (#434), carrying content in-band in both directions. The REPL
`get`/`put` commands work on literal paths instead, with downloads landing in
`{report_wd}/downloads/{name}.{host}`.

### `get` (read-only)

Downloads `remote` (an absolute file path; folders are not supported in-band)
from every enabled host and returns, per host, the full remote `size`, a
`truncated` flag, and the content: UTF-8 as `content`, binary as `content_b64`
(standard base64). Reads are capped per host at `[mcp] max_input_bytes`
(applied after the transfer — the whole remote file is buffered per host), and
each host gets an equal share of the `[mcp] max_output_bytes` wire budget.
Truncation never splices a notice into the content: the `truncated` flag and
the full remote `size` are the signal. Pass `hosts` (a list of connected
hostnames) to retry or page a subset when one host's file was truncated;
unknown or disabled names refuse the call. Any host failure fails the whole
call with the host named — partial content is never returned as success.

### `put`

Uploads a payload carried in the call — `content` (UTF-8) or `content_b64`,
exactly one of the two — to `<target tempdir>/<filename>` on every enabled
host, the same destination the REPL `put` uses. `filename` must be a bare name
(no path separators). A payload above `[mcp] max_input_bytes` is refused
outright: truncating an upload would corrupt it. Any host failure fails the
call with the host named.

## Command behaviour notes

- The `prepare` tool fails — instead of reporting success — when the loaded
  report's metadata names no package versions, and when no prepare command
  could be built for a connected host (#396), so automation keying on
  `prepare`'s success does not see a false positive on a metadata-empty report.
- On a report whose metadata carries a `binaries` block, `prepare` narrows the
  package list per host to what that host's products compose, and fails by name
  a host whose products compose none of it — rather than sending a list zypper
  refuses with "capability not found" (104). An architecture a product
  declares but the metadata never mentions composes nothing, not the full
  list, so it gets the same named refusal. `update` excludes such a host from
  its patch, scopes its own lock/repository/package-check to the hosts that do
  compose it, and reports a failure naming it, instead of patching a host its
  own prepare established no baseline on.
- `update`'s tool result carries diagnostics for every non-fatal degradation of
  the run — a composition refusal, a host excluded because its test update
  repository genuinely failed to register (its partial add is undone and its
  lock released before the patch reaches its peers), a repository refresh that
  may have left stale metadata on a host that still gets patched, a prepare
  host failure it continues past, a failed `--newpackage` step, a failed
  update leaving the test repositories configured, a failed automatic
  rollback, and the three ways the post-update test-repo cleanup can fail — on
  both the success and the failure path. The server's tracing goes to its own
  stderr, not the tool result, so a `tracing::warn!` alone would never reach an
  MCP client.
- Every workflow fan-out (`install`, `uninstall`, `prepare`, `downgrade`,
  `update`) confirms success as `<verb> completed on <hosts>`, so a clean run is
  never an empty tool result. `update`'s confirmation is printed *above* its
  diagnostics — the head of the buffer is what survives `max_output_bytes` — and
  is qualified whenever the run recorded a *degradation*: `update completed on
  h1: the patch passed its checks, with 2 degradations reported below`. The
  count is of degradations only; a check's own recognised sections (the
  vendor-support notice, extra rpm output) ride in the same diagnostics and are
  routine on a healthy update, so they leave the confirmation bare.
- `export` emits `WARNING: no package version data recorded for <host>...`
  lines in its tool output when a host has no recorded package data and its
  install-result block was therefore left unverified in the report.
- `run` prints its own verdict above its per-host output, for the same
  head-survives-`max_output_bytes` reason: `run completed on h1 (exit 0), h2
  (exit 1)` names every host it ran on with its exit code, followed by `FAILED
  on h2 (exit 1)` when any host exited non-zero. `run` returns `Ok` on a
  non-zero remote exit — a non-zero exit is often expected — so this verdict,
  not the tool's success/failure status, is the signal to check.
- The `run` tool's `command` is **argv tokens, not a shell line**: the tokens
  are re-quoted before dispatch, so `["cat /etc/os-release"]` reaches the host
  as one quoted word (exit 127) and `["zypper","lr","|","grep","x"]` passes a
  literal `|` as an argument. Split the words yourself
  (`["cat","/etc/os-release"]`), and for anything needing a shell — pipes,
  redirection, `;` — ask for one explicitly: `["sh","-c","zypper lr | grep x"]`.

## Testreport editing tools

Five hand-written tools operate on the loaded test report's checkout, replacing
the REPL's `$EDITOR`-based `edit` flow (which is deny-listed). Each accepts an
optional **`template="<RRID>"`** selecting which loaded template's checkout to act
on; with more than one template loaded an unscoped call is refused — the same
"one template, or refuse" rule described under [Multiple templates: scoping and
fan-out](#multiple-templates-scoping-and-fan-out) above, pass `template=` to
resolve it — and with zero or one loaded it may be omitted. All refuse cleanly
when no test report is loaded.

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
  `relpath` and `template`. `end_line == start_line - 1` is a pure insertion
  before `start_line`; an empty `replacement` deletes the range. A non-empty
  replacement is normalised to end with exactly one newline.
- `relpath` targets another checkout file instead of the report's `log` file,
  with the same traversal guard as `testreport_read` — but the file must
  already exist; a missing `relpath` refuses.
- Returns `{ "path", "new_line_count", "replaced_lines", "inserted_lines",
  "bytes_written" }`.

### `testreport_write`

Full-file overwrite (same atomic write). Use when line drift makes patching
unreliable.

- Parameters (required): `content`. Plus optional `relpath` and `template`.
- `relpath` targets another checkout file instead of the report's `log` file,
  with the same traversal guard as `testreport_read` — and unlike
  `testreport_patch`, it **may name a not-yet-existing file**. Its parent
  directory must already exist, though: a `relpath` whose parent is missing
  refuses rather than silently creating a new directory in the checkout.
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
`prepare`, `install`, `uninstall`, `set_repo`, `reboot`, `regenerate`, `add_host`,
and `load_template` — accept a **`background=true`** parameter. Instead of
holding the request open for the minutes the operation takes, the call returns
immediately with the job id(s); when the command fans out across several
templates it mints **one job per template**, each independently pollable and
cancellable.

`add_host` and `load_template -a` connect to a whole batch of reference hosts, and
one unreachable candidate among them can hold the call open far past any client's
patience — backgrounding is the only way to keep such a call cancellable via
`job_cancel`.

Four job-control tools manage them:

- **`job_list`** (read-only) — every job in the session and its state.
- **`job_status(job_id)`** (read-only) — one job's state (`running` / `done` /
  `failed` / `cancelled`) and elapsed time.
- **`job_result(job_id)`** (read-only) — a finished job's captured output; it
  errors while the job is still running (poll `job_status` first) and surfaces the
  command's failure envelope if it failed.
- **`job_cancel(job_id)`** — cancel a running job. (A command already executing on
  a host may run to completion there even after cancel returns — the same caveat as
  Ctrl-C on a foreground `run`.) Two commands treat a cancel as a normal ending
  rather than a failure: `request_review --watch` stops watching (the request
  stays posted) and `regenerate` stops waiting (the server keeps building). Both
  return success, so their job ends `done`, not `cancelled`, with the reply text
  saying what was and was not finished.

A job blocked mid host-operation cannot stop at a checkpoint, so cancelling it
force-aborts the dispatch — which skips the operation's own `unlock()`. A forced
cancel therefore releases `/var/lock/mtui.lock` on the job's behalf and reports
the outcome in its reply.

What it releases is deliberately narrow: **only the locks the cancelled job's own
host group actually took**, and never a comment-marked (exclusive) hold such as a
Product Increment assignment lock or an operator's `lock -c <text>` reservation.
Locks are owned per user + PID on the wire, so a broader sweep would remove a
sibling template's — or another MCP session's — live hold on a shared refhost and
report it as released.

The reply distinguishes: which hosts are now unlocked; which are held by another
owner; which release failed (with `unlock --force` as the remedy); and which
**templates** could not be reached inside the release budget, whose lock state is
therefore unknown. A cancel with no held lock to act on says nothing extra.

The job's template scope is recorded when the job is minted. A job started
without a resolvable scope falls back to every loaded template — the conservative
reading for a dispatch that may have run across all of them.

Nothing else is done at the hosts. The remote command may still be running there;
where it is a package transaction, the package manager's own system-wide lock
keeps serialising it, but a `run` or `reboot` has no such second layer and the
release is purely mtui-side bookkeeping.

A *cooperative* cancel — a body that unwound at a checkpoint — is unchanged: it
ran its own unlock discipline, and the cancel does not second-guess it.

Jobs are scoped to the session (under HTTP, the caller's isolated session, so one
client never sees another's jobs). The per-session job budget
(`max_active_jobs`, `max_completed_jobs`) and the single-result size cap
(`max_output_bytes`) bound resource use so one client cannot exhaust the server or
dwarf the client's context.

## Cancelling a foreground call

A client that sends an explicit `notifications/cancelled` for an in-flight
foreground tool call drops the dispatch and releases the `CommandLock` it was
holding — a follow-up call on the same RRID is not left queued behind it. What
happens next depends on whether the call could be holding a **host** operation
lock.

A testreport tool or a transfer tool (`get`/`put`) never dispatches through the
engine, so it cannot hold `/var/lock/mtui.lock`: the cancel drops the dispatch
immediately and the tool call resolves to an error rather than a fabricated
success.

A synthesised command tool (`run`, `update`, `install`, …) can be mid
host-operation when the cancel arrives, and dropping it outright would strand
that lock exactly as an unhandled `job_cancel` would. It instead runs the same
two-stage sequence `job_cancel` uses: the dispatch's own cancellation token is
signalled first, giving it a short grace period to unwind at a checkpoint (a
body that does — for example one already watching the session's cancel
token — returns its **own** verdict, success or failure, not a synthetic
cancellation error). Only if the grace elapses is the dispatch force-aborted;
the abort then best-effort releases `/var/lock/mtui.lock` on every host the
call's own group actually took (never a comment-marked reservation) and the
error text names what it unlocked, what is still held by another owner, and
what could not be reached inside the release budget.

**This only helps a client that sends the notification.** mtui-mcp's default
transport is stdio, where there is no per-request connection to drop — a
client that simply stops waiting (e.g. because it hit its own timeout) sends
nothing, and the server has no way to learn the caller gave up. In that case
the lock is still released, just on the ordinary schedule: the `connect_timeout`
budget (commit A) and the shared backup-retry budget bound how long that takes
rather than leaving it unbounded. Backgrounding a slow command
(`background=true`, see above) plus `job_cancel` remains the reliable way to
recover a stuck call regardless of transport.

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
