# Workflow concepts

The [command reference](cli.md) documents each command's own arguments. This page
covers the cross-cutting behaviour that sits *behind* those commands — how fan-out
works, how reference hosts are drawn from the pool, and how locking keeps
concurrent testers from colliding. It applies equally to the `mtui` REPL and the
`mtui-mcp` tools, since both dispatch through the same engine.

## Templates and the active template

Loading a testreport with [`load_template`](cli.md#load_template) *adds* it to the
session as a **template**; previously loaded templates stay loaded and the new one
becomes active. Each template owns its own reference hosts and its own report.
[`list_templates`](cli.md#list_templates) shows the loaded set (the active one
marked `*`), [`switch`](cli.md#switch) changes the active one, and
[`unload`](cli.md#unload) drops one (closing only *its* host connections).

Navigation and single-target commands (`load_template`, `edit`, `switch`,
`unload`, `quit`) always act on the active template.

## Fan-out across templates

When more than one template is loaded, **action commands fan out across every
loaded template by default**, each acting on that template's own hosts (or its own
report, for report-scoped commands). Each template's output is prefixed with an
`=== <RRID> ===` banner so results stay attributable.

Scope a single call two ways:

- **`-T <RRID>` / `--template <RRID>`** — act on one loaded template only.
- **`--all-templates`** — force fan-out explicitly (already the default; useful in
  scripts). Mutually exclusive with `-T`.

Failure handling is per-template: if a fanned-out command fails on one template it
**keeps running on the rest** and reports an aggregate failure at the end. When a
host-phase command (one that accepts `-t`) fans out *without* explicit `-t` hosts,
a loaded template with **no connected host** is skipped with a warning rather than
counted as a failure — so an unscoped `lock`/`run`/… still succeeds on the
templates that do have hosts. If *every* template is host-less the command ran
nowhere and fails with "No refhosts defined". Naming a disconnected host with
`-t <host>` is still a per-template failure, and a `-T`-scoped call keeps
single-template error behaviour.

The queue-browsing [`updates`](cli.md#updates) command is not template-scoped and
does not fan out. [`regenerate`](cli.md#regenerate) acts on the active template by
default.

Over MCP the same model applies through per-call `template=` / `all_templates=`
parameters — see [MCP server](mcp.md#multiple-templates-scoping-and-fan-out).

## Reference-host pool selection

When [`add_host`](cli.md#add_host) connects hosts from a testplatform (and during
the autoconnect that runs when a template loads), mtui connects **one** reference
host per test-target slot — a slot being a unique (product, version, arch, addons)
combination — rather than every matching candidate. So a testplatform that
resolves to several interchangeable hosts on the same architecture does not fan a
command out across all of them.

- The host drawn for each slot is chosen **at random** among the free candidates,
  spreading load across interchangeable refhosts.
- If the chosen host fails to connect, mtui automatically **tries another
  candidate** from the same slot; only when *every* candidate for a slot is
  unreachable is a single warning logged and that slot left unconnected.
- Under fan-out, each test-target slot draws a **distinct** free host from the
  shared pool, arbitrated in-process so two templates never collide on the same
  host. When every candidate for a slot is busy, the claim **queues** on the
  exhausted pool according to `[lock] wait` / `[lock] wait_poll` (see
  [Configuration](configuration.md)).

To search the inventory *without* connecting, use
[`list_refhosts`](cli.md#list_refhosts) — it reads the same source offline (no
SSH, no lock, no loaded report).

## Product-drift check on connect

When a host connects, mtui checks that the products actually installed on it (from
`/etc/products.d`) match what `refhosts.yml` records for that host. Any drift — a
wrong or wrong-version base product, a wrong architecture, missing/unexpected
addons, or a dangling `baseproduct` symlink — is logged as a **WARNING** and the
host is **kept** (the check never aborts a connect). The `qa` product is ignored,
and hosts not listed in `refhosts.yml` are skipped silently.

## Host state

Each connected host has a state, set with
[`set_host_state`](cli.md#set_host_state):

- **`enabled`** — runs all issued commands (the default).
- **`disabled`** — runs nothing and prints nothing.

Commands designed to run in parallel (like [`run`](cli.md#run)) always fan out
concurrently across enabled hosts.

## Locking

Two independent locks coordinate concurrent testers on a shared fleet. Both use
the same on-host wire format (`timestamp:user:pid[:comment]`), so a Rust `mtui` and
a Python MTUI interoperate.

- **Operation / zypper lock** (`/var/lock/mtui.lock`, PID-based) — guards
  serialized repository transactions (enabling/disabling the test repo,
  install/update/prepare/downgrade). Set it explicitly with
  [`lock`](cli.md#lock); it is applied automatically around the flows that need
  it. Enabled locks are removed when the session exits. To make the lock effective
  for *other* sessions too, give it a comment (`lock -c "<why>"`).
- **Pool-claim lock** (`/var/lock/mtui-pool.lock`, RRID-based) — taken during
  host-pool selection to reserve a host for a template. List and remove them with
  the `-p`/`--pool` flag on [`list_locks`](cli.md#list_locks) /
  [`unlock`](cli.md#unlock).

`unlock -f` force-removes a lock held by another user or session. mtui also reaps
a pre-existing lock older than `[lock] stale_age` on connect (almost always
left over from a crashed session); see [Configuration](configuration.md) for
`reap_stale`, `stale_age`, `wait`, and `wait_poll`.

### Product-Increment autolock

When testing a Product Increment (PI) and `[lock] pi_autolock` is enabled (the
default), mtui auto-locks all reference hosts on [`assign`](cli.md#assign) — with a
comment naming the request — and releases this session's locks at the end of
testing (`unassign` / `approve` / `reject`). Hosts added with `add_host` while the
assignment is active are locked too. A [`reboot`](cli.md#reboot) clears
`/var/lock`, so the per-host testing lock is re-applied after the host comes back.

### Assignment context on `assign`

After the assignment succeeds, [`assign`](cli.md#assign) prints best-effort
context from the TeReGen report API: the update's live priority and deadline
and, when the report already carries assignment state, who currently holds or
has decided each review group (one line per group). This is informational only
and never blocks the action — its absence can also mean the lookup simply
failed, and the server caches the state for a few minutes, so a line may lag by
up to ~5 minutes.
