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

Session-level commands (`load_template`, `unload`, `switch`, `list_templates`,
`list_refhosts`, `updates`, `config`, `whoami`, `set_log_level`, `help`, `quit`)
take no template flag: each runs exactly once, never fans out, and accepts
neither `-T` nor `--all-templates`.

## Fan-out across templates

When more than one template is loaded, a command falls into one of two groups:

- **Read/annotate commands** (`list_*`, `show_*`, `openqa_*`, `checkers`,
  `checkout`, `export`, `set_workflow`, `comment`, …) **fan out across every
  loaded template by default** — none of them mutates shared remote state, and
  re-running one is always safe. Each template's output is prefixed with an
  `=== <RRID> ===` banner so results stay attributable.
- **Host-mutating and remote-write commands** (`update`, `prepare`, `downgrade`,
  `install`, `uninstall`, `set_repo`, `run`, `reboot`, `lock`, `unlock`,
  `add_host`, `remove_host`, `get`, `put`, `approve`, `reject`, `assign`,
  `unassign`, `request_review`, `commit`) **never implicitly fan out**: bare,
  they act on exactly one template — the active one in the REPL, or the sole
  loaded one — and refuse rather than guess when several are loaded headlessly
  and none is named.

Scope a single call three ways:

- **`-T <RRID>` / `--template <RRID>`** — act on one loaded template only.
- **`--all-templates`** — force fan-out explicitly, on either group. Mutually
  exclusive with `-T`.
- **`--all-templates=false`** — suppress fan-out on a read/annotate command,
  narrowing it to the same single-template resolution the other group uses by
  default.

A headless caller (MCP) with several templates loaded and neither flag named
gets [`AmbiguousTemplate`](cli.md), not a silent guess — the error names the
loaded RRIDs and how to scope the call.

Failure handling is per-template: if a fanned-out command fails on one template it
**keeps running on the rest** and reports an aggregate failure at the end. When a
host-phase command (one that accepts `-t`) fans out *without* explicit `-t` hosts,
a loaded template with **no connected host** is skipped with a warning rather than
counted as a failure — so an unscoped `lock`/`run`/… still succeeds on the
templates that do have hosts. If *every* template is host-less the command ran
nowhere and fails with "No refhosts defined". Naming a disconnected host with
`-t <host>` is still a per-template failure, and a `-T`-scoped call keeps
single-template error behaviour.

[`export`](cli.md#export) is the exception: it runs on a host-less template
too, since every workflow has openQA data to write. A Manual template with no
connected host writes only `testing.openqa` (install results and overview
rows); `testing.install` and the install logs wait for an export after
`add_host` and `update`.

[`regenerate`](cli.md#regenerate) also runs exactly once, on the active template
by default; `-T` names another loaded one.

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
  unreachable or claimed does `add_host` name the slot and why in its result and
  leave that slot unconnected.
- Under fan-out, each test-target slot draws a **distinct** free host from the
  shared pool, arbitrated in-process so two templates never collide on the same
  host. When every candidate for a slot is busy, the claim **queues** on the
  exhausted pool according to `[lock] wait` / `[lock] wait_poll` (see
  [Configuration](configuration.md)).
- Hosts named explicitly (`add_host -t`) take the same in-process claim and,
  once connected, the remote pool claim, so a later pool selection treats
  them as taken. A named host claimed by another template is refused with
  the holder's RRID instead of being attached silently. A fresh remote claim
  blocks `-t` with no override on that template: have the holding template
  run `unlock --pool --force [host]`, or wait for the stale reap
  (`[lock] pool_stale_age`); a force flag for `-t` itself does not exist yet.

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
the same on-host wire format (`timestamp:user:pid[:comment]`), so concurrent
`mtui` processes — and any other fleet tooling that reads the same files —
cooperate rather than clobber one another.

- **Operation / zypper lock** (`/var/lock/mtui.lock`, PID-based) — guards
  serialized repository transactions (enabling/disabling the test repo,
  install/update/prepare/downgrade). Set it explicitly with
  [`lock`](cli.md#lock); it is applied automatically around the flows that need
  it. Enabled locks are removed when the session exits. To make the lock effective
  for *other* sessions too, give it a comment (`lock -c "<why>"`). Such a
  reservation is never rewritten or released by `run`, `update`, `install` or
  `prepare` on the host; only `unlock` removes it. Templates loaded in the same
  process, and the sessions of one `mtui-mcp` server, hold the operation lock
  against each other like separate users: a second template's `run` or `update`
  waits for (or, at `[lock] wait = 0`, refuses on) the first one's hold.
- **Pool-claim lock** (`/var/lock/mtui-pool.lock`, RRID-based) — taken during
  host-pool selection to reserve a host for a template. List and remove them with
  the `-p`/`--pool` flag on [`list_locks`](cli.md#list_locks) /
  [`unlock`](cli.md#unlock).

Each lock file holds exactly one line, `timestamp:user:pid[:comment]` (Unix
seconds, user name, numeric process id, optional free text that may contain
colons). A pool claim's comment is `mtui pool <RRID> [<owner>]`. A missing file
means the host is free. Anything else that is not such a line — an empty or
whitespace-only file, the wrong number of fields, a non-numeric pid, an empty
user, bytes that are not UTF-8 — is an **invalid lock file**. It is never read
as free, because it usually marks a claim that died before writing its line.
Another tester's claim also reads empty for an instant between creating the
file and writing it, so mtui re-reads a file it cannot prove is older than five
seconds for up to five seconds, and only a file still invalid then is reported.
[`list_locks`](cli.md#list_locks) reports it as `invalid lock file`, and
`lock` and pool claims refuse the host with an error naming it, the file and
the way out (`unlock -f` is only right if no other mtui is claiming the host). `unlock -f` removes it (`unlock -p -f` for the pool claim); a plain
`unlock` leaves it alone. It is also reaped like any stale lock once older than
`[lock] stale_age` (`pool_stale_age` for the pool claim), except that the age is
the file's modification time, since there is no timestamp inside; a file whose
modification time is missing or in the future is never reaped.

By default, `lock`/`unlock` act on the whole active template's hosts; pass
`-t <host>` (repeatable) to scope either to a subset. Plain `unlock` releases
only the operation lock **this session took**: a lock it never took — another
user's, or a live sibling template's on a refhost shared inside the same
process — is left alone and reported `skipped`. A foreign lock points at
`list_locks` or `--force`; a sibling's is named as held by this mtui process and
is released from the template that took it. `unlock -f` force-removes a lock held by another user or
session regardless of who took it. mtui also reaps a pre-existing lock older
than `[lock] stale_age` on connect (almost always left over from a crashed
session); see [Configuration](configuration.md) for `reap_stale`, `stale_age`,
`wait`, and `wait_poll`. The one exception is
[`list_refhosts --free`](cli.md#list_refhosts): its connect is a read-only
survey, so it never reaps. Its `lock` column reports one of `free`, `locked`,
`pool` (free but pool-claimed), `locked+pool`, `unreachable` (the host could
not be probed), or `unknown` (the probe never completed for it); `--json`
keeps this vocabulary and adds a `pool` key: the claiming RRID, or `null` when
unclaimed.

### Product-Increment autolock

When testing a Product Increment (PI) and `[lock] pi_autolock` is enabled (the
default), mtui locks each reference host — with a comment naming the request —
as it connects, for the life of the loaded report; the lock is released when
the report is unloaded or the session quits, matching how every non-PI
report's pool claims already behave. A host added later with `add_host`
(including a pool-selected host, with no `-t`) is locked too, since the lock
comment is seeded on load and every connect path applies it. Review actions
(`assign`/`unassign`/`approve`/`reject`) do not lock or unlock hosts
themselves. A [`reboot`](cli.md#reboot) clears `/var/lock`, so the per-host
testing lock is re-applied after the host comes back.

### Assignment context on `assign`

After the assignment succeeds, [`assign`](cli.md#assign) prints best-effort
context from the TeReGen report API: the update's live priority and deadline
and, when the report already carries assignment state, who currently holds or
has decided each review group (one line per group). This is informational only
and never blocks the action — its absence can also mean the lookup simply
failed, and the server caches the state for a few minutes, so a line may lag by
up to ~5 minutes.

### Kernel export file names

A kernel [`export`](cli.md#export) saves each openQA job's install log as
`<host>-zypper-<arch>.log` and each LTP result as
`<host>-<arch>-<test>.json`. When several jobs of one export would write the
same file, each keeps its own file with the job ID inserted before the
extension (`<host>-zypper-<arch>-job-<id>.log`); a name only one job claims
never changes. Files from an earlier export are not deleted, so a plain
`<host>-zypper-<arch>.log` written before this naming existed can remain next to
the job-ID files.
