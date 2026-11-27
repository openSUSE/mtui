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
`list_refhosts`, `updates`, `config`, `whoami`, `teregen_login`, `set_log_level`,
`help`, `quit`)
take no template flag: each runs exactly once, never fans out, and accepts
neither `-T` nor `--all-templates`.

## Fan-out across templates

When more than one template is loaded, a command has one of three scopes:

- **Fanout** — read/annotate commands (`list_*`, `show_*`, `openqa_*`,
  `checkers`, `checkout`, `export`, `set_workflow`, `comment`, …) **fan out
  across every loaded template by default**. None of them mutates host state,
  and re-running one is safe, with one exception: `comment` posts its comment
  again. Each template's output is prefixed with an `=== <RRID> ===` banner so
  results stay attributable.
- **Explicit** — host-mutating and remote-write commands (`update`,
  `prepare`, `downgrade`, `install`, `uninstall`, `set_repo`, `run`, `reboot`,
  `lock`, `unlock`, `add_host`, `remove_host`, `get`, `put`, `approve`,
  `reject`, `assign`, `unassign`, `request_review`, `commit`) **never
  implicitly fan out**: bare, they act on exactly one template — the active
  one in the REPL, or the sole loaded one — and refuse rather than guess when
  several are loaded headlessly and none is named.
- **Active** — commands that act on the active template in the REPL
  (`list_products`, `reload_products`, `set_host_state`, `set_timeout`). A headless caller with
  several templates loaded and none named fans out instead, because there is no
  active template to address.

Scope a single call three ways:

- **`-T <RRID>` / `--template <RRID>`** — act on one loaded template only.
- **`--all-templates`** — force fan-out explicitly, on any scope. Mutually
  exclusive with `-T`.
- **`--all-templates=false`** — suppress fan-out on a Fanout command, narrowing
  it to the same single-template resolution an Explicit command uses by
  default.

A headless caller (MCP) with several templates loaded and neither flag named,
on an Explicit command or under `--all-templates=false`, gets an
`AmbiguousTemplate` error rather than a silent guess; the error names the loaded
RRIDs and how to scope the call (see
[MCP server](mcp.md#multiple-templates-scoping-and-fan-out)).

Failure handling is per-template: if a fanned-out command fails on one template it
**keeps running on the rest** and reports an aggregate failure at the end. When a
host-phase command (one that accepts `-t`) fans out *without* explicit `-t` hosts,
a loaded template with **no connected host** is skipped with a warning rather than
counted as a failure — so an unscoped `lock`/`run`/… still succeeds on the
templates that do have hosts. If *every* template is host-less the command ran
nowhere and fails with "No refhosts defined". With `-t <host>`, a template that
owns none of the named hosts is skipped with a warning; a template that owns at
least one of them is dispatched, and fails for any other named host that is not
connected in it. If no template owns any named host, the result is again "No
refhosts defined". A `-T`-scoped call keeps single-template
error behaviour.

When a host action completes on some hosts and fails on others, the command
prints `<verb> completed on <hosts>` — naming the hosts it completed on — before
it reports the failure. After a failed `update`, only a check failure or a
reboot that did not take effect triggers an automatic rollback; if a peer's
patch never ran, nothing is rolled back and the hosts that completed get the
same finishing steps as a successful `update`.

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
- Slots are matched to hosts so that each gets a host of its own whenever the
  free candidates allow it. A host can match two slots, as an `SLE_HPC` host
  carrying the `SLES-LTSS` addon matches both an `SLE_HPC` and a `SLES-LTSS`
  slot; it serves only the slot it was drawn for, on later `add_host` calls
  too, a candidate no other slot needs is preferred, and a slot left without a
  host because every candidate already serves another slot is named in
  `add_host`'s result.
- If the chosen host fails to connect, mtui automatically **tries another
  candidate** from the same slot; only when *every* candidate for a slot is
  unreachable, claimed or locked does `add_host` name the slot and why in its result and
  leave that slot unconnected.
- A host the update workflow cannot drive counts as a failed draw, and the slot
  moves on to a sibling: one whose key has no command set (`no command set for
  <Product version>, <root mode>`, the root mode as detected, such as `SLES
  15-SP6, transactional`), one whose base product mtui does not map (SLE
  Micro 5.x, Leap, Leap Micro: `unrecognised product …`), and one whose
  `baseproduct` link is missing or dangling (`base product unreadable …`). The
  check runs after connecting and before any claim or lock is written. A pool
  draw needs a loaded update, since its testplatforms come from one.
  `load_template` shows no unserved-slot lines; it prints `skipped <host>:
  <reason>` for each host refused this way, including a pool host whose slot a
  sibling then served. A refused host's `refhosts.yml` drift is still printed.
- A pool-drawn host whose live operation lock another owner holds (another
  mtui process of yours included) is a failed draw as well, checked at the
  same point, after a stale lock is reaped: its slot moves on to a sibling,
  and the reason reads `operation lock held by <user> since <time>`. A named
  host is attached regardless.
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
  run `unlock --pool --force -t <host>`, or wait for the stale reap
  (`[lock] pool_stale_age`); a force flag for `-t` itself does not exist yet.
  While an update is loaded, a named host the update workflow cannot drive is
  refused too, with the reason, and there is no override. With nothing loaded
  it attaches as before, for `run`, `get` and `put`.

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

## Removal guard

`install`, `uninstall`, `prepare` and `downgrade` let the package solver decide
what else a transaction takes with it, and `-n` accepts the result. Before
committing, each checked host first resolves the same transaction with a dry
run (`zypper -x … --dry-run`) and reads the packages it would remove. The host
is refused — its transaction is not run, it is not rebooted, it gets no history
row, and it fails the command by name while the other hosts carry on — when
that set holds a protected package the command did not name, or when the dry
run fails, times out, or its output cannot be read. Every other removal is
allowed and reported as `<host>: the transaction also removes …`.

The check is best-effort. The real command resolves the transaction again,
after its own refresh, and PackageKit or another zypper user on the host can
change what it resolves to between the dry run and the commit.

A refused host has still been through the steps before the dry run:

- `install`, `uninstall`: the operation lock was taken and is released; nothing
  else changed.
- `prepare`: the update repository was added and refreshed, and stays.
- `downgrade`: the update repository was removed and the repositories
  refreshed; it is not added back.

In `install`, `uninstall` and `prepare`, a cancel (Ctrl-C, MCP `job_cancel`)
that lands while the dry run is running stops the command before the
transaction is dispatched on any host.

Protected, by exact name: the base system (`aaa_base`, `bash`, `bash-sh`,
`coreutils`, `filesystem`, `glibc`, `pam`, `systemd`, `udev`, `util-linux`),
the boot chain (`dracut`, `kmod`, `shim`, and the kernels `kernel-default`,
`kernel-default-base`, `kernel-64kb`, `kernel-azure`, `kernel-kvmsmall`,
`kernel-preempt`, `kernel-rt`, `kernel-zfcpdump`), the package and snapshot
stack (`libzypp`, `rpm`, `rpm-ndb`, `snapper`, `transactional-update`, `tukit`,
`zypper`), and what keeps the host reachable (`NetworkManager`, `wicked`,
`openssh`, `openssh-common`, `openssh-server`, `sudo`); and `grub2` with every
`grub2-*` package. The list is built in; `[target] protected_packages` (see
[Configuration](configuration.md#target)) adds more names, matched exactly,
and cannot remove any. Known gaps: the `libtukit` library
packages and the products' `*-release` packages are not on it, so a
transaction that removes them without also removing a listed package is not
refused.

Pass `--allow-removal` (the `allow_removal` property of the MCP tools) to
commit the transaction anyway; it skips the dry run.

Not checked — each such host is named in the command output and the MCP
result as `<host>: no dry run exists for this host, …`, counted as a
degradation:

- Transactional hosts (SL Micro, transactional SLES 16): the transaction runs
  in a new snapshot whose repositories and trusted keys can differ from the
  booted root's, and no dry run on such a host has been verified to resolve the
  same transaction.
- SLE 11 hosts: their zypper predates the releases whose dry-run output the
  check was verified against.
- `YUM` hosts: yum/dnf have no machine-readable transaction summary.

`run` is not checked either, nor are the prepare and rollback inside `update`.

`show_log` omits the XML of a zypper dry run that succeeded, whoever ran it (the
guard, or a `run` command with the same `zypper -x -n in|rm --dry-run` prefix);
a failed or unreadable dry run's XML is shown, as it holds zypper's reason.

On a non-transactional host `downgrade` downgrades one package per
transaction, but the guard judges one dry run of all the host's packages
together. The single transactions can resolve differently from the combined
one, so the guard can refuse a downgrade none of them would make harmful, and
can miss a protected package that one of them removes while the combined
solve keeps it.

## Locking

Two independent locks coordinate concurrent testers on a shared fleet. Both use
the same on-host wire format (`timestamp:user:pid[:comment]`), so concurrent
`mtui` processes — and any other fleet tooling that reads the same files —
cooperate rather than clobber one another.

- **Operation / zypper lock** (`/var/lock/mtui.lock`, PID-based) — guards
  serialized repository transactions (enabling/disabling the test repo,
  install/update/prepare/downgrade). Set it explicitly with
  [`lock`](cli.md#lock); it is applied automatically around the flows that need
  it. Enabled locks are removed when the session exits, commented or not. To
  keep the lock between operations, give it a comment (`lock -c "<why>"`). Such
  a reservation is never rewritten or released by `run`, `update`, `install` or
  `prepare` on the host; `unlock` or the end of the session removes it.
  Templates loaded in the same process, and the sessions of one `mtui-mcp` server, hold the operation lock
  against each other like separate users: a second template's `run` or `update`
  waits for (or, at `[lock] wait = 0`, refuses on) the first one's hold.
- **Pool-claim lock** (`/var/lock/mtui-pool.lock`, RRID-based) — taken during
  host-pool selection to reserve a host for a template. List and remove them with
  the `-p`/`--pool` flag on [`list_locks`](cli.md#list_locks) /
  [`unlock`](cli.md#unlock).

A reboot wipes `/var/lock` (tmpfs), so after the host reconnects mtui
re-applies the reservation and pool claim this session held — in `reboot`
itself, in the transactional reboot inside `update`/`downgrade`/`prepare`,
and in the `install`/`uninstall` operation template. Re-taking is idempotent
and never overwrites someone else's lock: a hold another tester claimed in
the gap is left in place, not replaced. The `reboot` command prints one line
per lost hold naming the host and the hold; the `update`/`prepare`/`downgrade`
flows record the same as a degradation diagnostic, as do `install`/`uninstall`.
The reboot outcome itself stays a success either way. A hold another owner
claimed in the gap also drops its in-memory record, so a later plain `unlock`
reports the session holds nothing instead of trying to release someone else's
lock; a hold the re-take could not reach (transport error, or a corrupt line)
keeps its record — dropping a corrupt line would strand it under a later plain
`unlock`'s skip, while the kept record surfaces it on the next release
attempt.

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

`remove_host`, `unload`, `quit`, and a `load_template` (or `regenerate`) that
reloads an already-loaded update never refuse to tear a host down, but they
warn about what this session left on it, one `warning: <host>: …` line per
item, in the command's reply (so an MCP client sees it too):

- an operation lock or pool claim this session still holds after the close,
  with why — `link down` (the release was skipped), `release failed`, or
  `teardown abandoned` (the close ran out of time) — and how it goes away: a
  reboot clears it, otherwise it stays until it is older than
  `[lock] stale_age` (`pool_stale_age`) or someone runs `unlock -f`
  (`unlock -f -p`). A lock or claim that now belongs to another owner (this
  session's hold lapsed, e.g. it was reaped as stale and taken) is named as
  such: the close did not release it, and there is no `unlock -f` advice;
- the update's repositories, when the last repository change this session
  made on the host was an add (`set_repo -A`, `prepare --update`, or an
  `update` that stopped before removing them): with the update loaded,
  reconnect the host and run `set_repo --remove`;
- packages this session's `install` put there that no later `uninstall`
  removed. mtui does not check beforehand which of them were already
  installed, so the line says some may have been.

The check reads only what the session recorded; it runs nothing on the host.

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
not be probed), or `unknown` (the probe never completed for it), followed by
who holds it: `since <time> by <user> : <comment>` for the operation lock and
`pool <RRID> since <time> by <user>` for the pool claim. `--json` carries the
raw probe state in `lock` (`free`, `locked`, `unreachable` or `unknown`) and
adds a `pool` key (the claiming RRID, or `null` when unclaimed),
`lock_holder` (`user`, `since`, `comment`) and `pool_holder` (`user`,
`since`); each holder is `null` when nothing is held or the lock file is
invalid. `pool` and `locked+pool` are table-only labels.

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
testing lock is re-applied after the host comes back — through the same
re-apply as reservations and pool claims (see above), with the same lost-hold
reporting when another tester claimed the host in the gap.

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
