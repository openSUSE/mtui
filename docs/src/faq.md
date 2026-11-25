# FAQ

## What is an RRID?

An **RRID** is the `project:kind:maintenance_id:review_id` identifier that names a
maintenance request across the SUSE ecosystem — for example
`SUSE:Maintenance:12345:678901`. It is parsed by splitting on `:` (empty tokens
dropped, so leading/trailing/doubled colons are ignored) into exactly four
positional components; more than four is rejected, and a missing component is a
parse error. This grammar and its errors are a stable contract with the rest of
the maintenance ecosystem.

## Can I set the template directory without passing it every time?

Yes. Set `template_dir` under `[mtui]` in your config file, or export
`$TEMPLATE_DIR` (the built-in default reads it). See
[Configuration](configuration.md).

## Can I work on several updates at once?

Yes. Load more than one testreport and each is a *template* in the session:
`list_templates` shows the loaded set and `switch` changes the active one. A
command that acts on a template takes its scope from the command: read/annotate
commands (`list_*` other than `list_products`, `show_*`, `openqa_*`, `export`,
`checkout`, `comment`, …) cover every loaded template, and the rest act on the
active one (session-level commands
such as `list_templates`, `updates` or `config` take no template flag). Scope a
call with `-T <RRID>`/`--template <RRID>`, or fan out with `--all-templates`.
Under fan-out each template gets its own `=== <RRID> ===` banner. See
[Workflow concepts](concepts.md#fan-out-across-templates).

## Can I run mtui without loading a testreport?

Yes, for the commands that do not need one. `list_refhosts` searches the
reference-host inventory without a loaded template (and without SSH, unless you
pass `--free`, which connects to read each host's lock state). `run`, `reboot`,
`lock`/`unlock`, `put`/`get` and `reload_products` act on hosts you added with
`add_host`. `install`, `uninstall`, `prepare`, `update` and `downgrade` refuse
without a loaded template.

## Does mtui support SSH password authentication?

No — and it never will. mtui is **pubkey-only by design**: it authenticates from
your SSH agent, the `IdentityFile` entries of `~/.ssh/config` or `~/.ssh/id_*`.

## Can several testers use the same reference hosts at once?

Yes, but the locks are **advisory and fail-fast by default**. Every mtui on the
fleet writes the same `timestamp:user:pid[:comment]` layout, so sessions see one
another's claims — a pool draw skips a host whose pool claim or live
operation lock someone else holds (another mtui process of yours included), and
an install/update aborts with "Hosts locked" rather than waiting. Raise
`[lock] wait` (default `0`) to poll for a busy host during pool arbitration;
`install`/`update` still refuse at once on a host someone else already holds.
Naming a held host explicitly (`add_host -t`) is refused with the holder's
RRID instead of attaching it, and a fresh remote claim blocks `-t` with no
override on that template: have the holding template run
`unlock --pool --force -t <host>`, or wait for the stale reap. A named host under
another owner's operation lock still attaches, and its lock-taking calls fail
until that lock is released.
There are two locks with that layout: the operation lock
(`/var/lock/mtui.lock`, PID-based, guards serialized zypper transactions) and the
pool-claim lock (`/var/lock/mtui-pool.lock`, RRID-based). Stale-lock reaping is
configurable under `[lock]` — see [Configuration](configuration.md).

## Where does mtui find the reference-host inventory?

From `refhosts.yml`, resolved by the ordered `[refhosts] resolvers` list
(default `https,path`): the HTTPS database (`[refhosts] https_uri`, cached with a
`https_expiration` TTL) and/or a local file (`[refhosts] path`). The file is
grouped by location on disk, but location is a legacy grouping — rows are merged,
flattened, and de-duplicated at load.

## How do I change the editor used by the `edit` command?

`edit` spawns your `$EDITOR` (`vim` when unset) on the controlling terminal.
Set it in your shell environment. When the loaded report is a JSON document, a bare
`edit` opens the document editor instead (see the next question); `edit FILE` always
uses `$EDITOR`.

## How do I fill in a report document by hand?

With a document loaded, `edit` opens a full-screen form over it, one tab per
section: `summary` (verdict and comment), `people`, `issues`, `testing` and
`review`, then `update` and `install`, which are read-only. `Tab`/`Shift-Tab`
switch sections, `Up`/`Down` move, `Enter` edits a field or opens a group,
`Left`/`Right` step an enum or yes/no field, `n` sets a field to null, `Ctrl-S`
saves and `q`/`Ctrl-C` quits (asking first when there are unsaved changes); `?`
lists the keys. On the `issues` tab, `a` (or Enter on the `new_issues`
row) files a bug you found (it asks for the id,
the title and an optional comment) and `d` deletes a bug you filed; pipeline issues
cannot be deleted.

Only the tester's answers are editable: verdicts, comments, issue
reproducer/status/comment, the title and comment of a bug you filed, the reviewer name and the review questions. Fields the
pipeline or `export` fills in are shown greyed. Every field is checked against the
report schema as you edit it, and `Ctrl-S` re-checks the whole document; a refusal
leaves the editor open on the offending field.

A save marks the document as edited but not uploaded — `commit` uploads it, and
`unload`/`regenerate` refuse to drop it without `--discard-authored`. `regenerate` also
looks at the server's copy: in the REPL it asks before discarding tester content found
there or in the loaded document, and over MCP it refuses. `export` keeps the verdicts
and comments you entered under `testing.install` and `testing.regression`; it rewrites
only the install checks (with `-t`, only the selected hosts' rows) and the kernel results
block inside the regression comment. The editor needs a terminal on both stdin and
stdout.

## How do I export results into the testreport?

`export` authors the collected update data (and, for the openQA-sourced
workflows, openQA data) onto the loaded report document's `testing.*` sections and
writes the install logs to the report directory. A Manual export also fetches the
QEM Dashboard results into `testing.openqa`. The document stays local until
`commit` uploads it. Re-exporting updates the same sections in place.

A `Manual` export must run from the session that ran `update`: the before/after
package versions exist only in that process, so from any other session `export`
refuses rather than write a report with no verdict. It also refuses when no
package of the update is tracked for any selected host, for example because
the update ships packages for none of the products installed there; running
`update` does not change what a host tracks, but once the missing product is
registered on the host, `reload_products` makes the host track its packages.
`--allow-unverified` writes the unverified scaffold anyway.

## Where do OBS/Gitea credentials come from?

- **OBS/IBS**: from your `oscrc`, located like `osc` itself — `$OSC_CONFIG`, then
  `$XDG_CONFIG_HOME/osc/oscrc`, then `~/.oscrc`. There is no mtui-side path
  option; point `$OSC_CONFIG` at a non-default file. The API to act against is
  `[obs] api_url`.
- **Gitea**: the `[gitea] token` config option. It is treated as a secret —
  masked as `<set>` in `config` output and sent only in an `Authorization`
  header, never logged.

## Do I still need `osc` installed?

No. The QAM review workflow (`assign`/`unassign`/`approve`/`reject`/`comment`)
talks to the OBS/IBS API natively — no `osc` subprocess.

## How do I install packages the update newly introduces?

Feature updates often add packages that only exist in the test-update repository,
so `prepare` can't install them yet. Run `update --newpackage` to install the new
packages right after the update applies.

## Why does `prepare` (or `update`) refuse to run?

When the loaded report's metadata names no package versions, `prepare` and
`update` refuse with an error rather than reporting `prepare completed on
<hosts>` over a no-op that installs nothing (#396). Check
`list_packages -w` and the report metadata; a host the fan-out cannot build a
command for (unresolvable release, no matching template) likewise fails the
run by name. That includes a SLES 16 host whose root mode could not be
determined, reported as `missing preparer: <host> (<product version>,
transactional-update.conf but root mode unknown)`.

When a host action completes on some hosts and fails on others, the command prints
`<verb> completed on <hosts>` for the hosts it completed on before it reports the
failure.

## How do I control what `prepare` installs?

`prepare` has three switches:

- `prepare -f` / `--force` — force installation even on package conflicts.
- `prepare -i` / `--installed` — narrow the package list to what each host
  already carries (skip pulling in additional patchinfo packages). Each host
  still installs its own list in a single transaction.
- `prepare -u` / `--update` — enable the test-update repositories and install from
  there.

Independently of those switches, on a report whose metadata carries a
`binaries` block the list is narrowed per host to what that host's products
actually compose (base product plus addons). A host whose products compose
none of the list is failed by name rather than sent a list zypper will refuse
with "capability not found" (104); a host no product of which the metadata
names keeps the full list and logs a warning. An architecture a product
declares but the `binaries` block never mentions at all composes nothing
rather than falling back to the full list — it is treated as known-and-empty,
not unknown, so it gets the same named refusal as any other host whose
products compose none of the requested packages.

`update`'s own prepare narrows the same way, and a host it refuses is left out
of the patch — no prepare ran on it, so there is no package baseline to patch
— and named in the update's error instead of being patched anyway. `update`'s
own operation lock, repository add/remove and pre/post package version check
are scoped to the hosts that actually compose it, so a refused host draws none
of them either: it cannot contend the group's lock, have its repository
configuration changed, or block a composing peer's patch behind its own
unresolvable updater.

`update` also leaves out a host the update's repositories do not reach, on any
report: one whose products match none of them, one whose products could not be
read, or every host when the update has none. The prepare of a classic
maintenance report narrows nothing, and a host no product of which the
metadata names keeps the full list, so neither refuses it, yet the patch would
find nothing to install there and pass. Such a host is left out before
anything runs on it: no prepare, lock, repository change, patch or package
check. The update's error names it: `<host>: no repository of this update
matches the host's products (…); it was excluded from the update`, `<host>:
its products could not be read; it was excluded from the update`, or `<host>:
this update has no repository to add; it was excluded from the update`.

These exclusions cover the automatic rollback too. When a host that was patched
then fails its check, or a reachable host's reboot does not take effect, the
rollback reaches only the hosts the patch was dispatched to, so an excluded
host gets no lock, repository change, downgrade, history row or reboot from it.
When a peer's patch never ran, or a host does not reconnect after its reboot,
nothing is rolled back: the hosts the update completed on get the same finishing
steps as a successful run instead.

`prepare -u` fails such a host by name too, on any report: no test repository
would be added to it, so its install would draw only on the repositories it
already had. It is left out before anything runs on it (no preparer lookup,
lock, repository change or install), so neither a missing preparer nor another
tester's lock on it can fail its peers' prepare. The other hosts are prepared,
and the prepare's error names it: `<host>: no repository of this update matches
the host's products (…); nothing was installed`, `<host>: its products could
not be read; nothing was installed`, or `<host>: this update has no repository
to add; nothing was installed`. It is named even when a peer stops the prepare
first (a missing preparer, another tester's lock, a failed repository add), in
one `prepare failed on <hosts> (…)` summary with the error that stopped it.

## Can I run a command on only some of the connected hosts?

Yes — temporarily disable the rest with `set_host_state`, then re-enable:

```
set_host_state -t hostA -t hostC disabled
run zypper lr            # runs only on the still-enabled hosts
set_host_state enabled   # re-enable all
```

`disabled` hosts run nothing (and print nothing).

## Can I export the update log from a specific refhost?

Yes: `export -t <host>`. It rewrites that host's install check and keeps the
rows of every other host already in `testing.install.checks`, a host no longer
connected included, so a host tested after the others were exported can be
added on its own; the answer names the hosts whose rows it kept. By default
`export` writes the collected data for every host in the list — including
disabled ones — and drops the row of a host no longer connected (the answer
names it), so to keep a temporarily-added host out of the report,
`remove_host -t <host>` before exporting.

## Why doesn't `put *.rpm` upload anything?

Inside the REPL nothing expands the `*`, so `put` looks for a file literally
named `*.rpm` and errors with `File *.rpm not found`. That is deliberate, not
a gap ([#399](https://github.com/openSUSE/mtui/issues/399)): `put` pushes to
**every enabled refhost in the group**, which is exactly where an over-broad
pattern does damage, so mtui refuses to guess how wide a pattern was meant to
be. To upload several files, `put` the directory containing them (it is
walked recursively), or expand the glob in your own shell before invoking
mtui.

## Where are the per-host install logs stored?

Under the loaded report's directory, in `template_dir/<RRID>/install_logs/`. The
file names depend on the workflow: a Manual export writes `<host>.log` (one file
per refhost), an Auto export `<distri>_<version>_<arch>[_<flavor>].log` (one per
openQA job), and a Kernel export `<host>-zypper-<arch>.log`, where `<host>` is the
last segment of the openQA instance URL and jobs that would write the same name
get `-job-<id>` inserted before the extension. The `install_logs`
sub-directory name is configurable under `[mtui]`; see
[Configuration](configuration.md).

## What does Ctrl-C do?

It depends on whether a command is running.

At the prompt, Ctrl-C clears the line you were typing and reprompts. It never
exits mtui — use `quit`, `exit`, or Ctrl-D for that, so the session's hosts are
unlocked and closed on the way out.

While a command is running, the first Ctrl-C asks it to **stop at its next
checkpoint**. Every path releases the hosts' operation locks on the way out, so
nothing is left behind — but how soon the command stops depends on where its
checkpoints are:

- `update` checks between its steps, and for the last time just before the point
  of no return: once the patch command has been dispatched the update runs to its
  end (rolling back on failure) rather than leaving a half-applied update behind.
- `prepare --installed` checks before and after its installed-package probe,
  and stops without installing anything; `downgrade` on non-transactional hosts
  checks between packages. A transactional `downgrade` (SL Micro, or SLES 16
  with a read-only root) applies in one transaction, so it finishes first like
  the commands below.
- `reload_products` checks before each host; a host already reloading finishes
  or times out first, and the reply names the hosts it reloaded and those it
  did not reach.
- A command applying to several loaded templates stops at the next template
  boundary, and reports how many it got through.
- `install`, `uninstall`, `run`, and `reboot` finish the host operation already
  under way first, then stop.

A command that *did* stop at a checkpoint reports `error: cancelled`, naming what
it completed first where it can. A command with no checkpoint left to reach
simply finishes and reports normally — the cancel arrived too late to change
anything, and saying "cancelled" would be a lie about work that was in fact done.
Two long waits are their own case: `request_review --watch` and `regenerate`
stop watching and tell you so (the review request stays posted, the
regeneration keeps running on the server), and both count as success. Either
way the session stays usable — the next command starts with a clean slate.

A second Ctrl-C **force-quits** (exit status 130). This is the escape hatch for a
host that has stopped responding, and it comes at a cost: the running command is
abandoned where it stands, so the operation locks it holds are left behind. Your
command history is still saved. mtui warns when this happens; see the next entry
for the cleanup.

Ctrl-C during the **teardown** behaves the same way but cancels nothing: the
teardown is what releases the pool claims and closes the hosts, so it always runs
to completion. A press there warns that it is in progress, and a second one
force-quits — leaving both the operation locks and the pool claims
(`unlock --force` and `unlock --pool`). This holds however you asked to leave:
Ctrl-D and a typed `quit`/`exit` dispatch the same teardown and get the same
protection.

Two carve-outs. Ctrl-C during the **initial load** at startup (`-a`/`-k`/`--sut`,
before the first prompt appears) still exits immediately, without teardown. Any
locks or pool claims that partial load had already taken need
`unlock --force`/`unlock --pool`, as after any crash.

And in a **non-interactive (piped) session** — mtui reading commands from a pipe
rather than a terminal — there is no line editor holding the keyboard, so a
Ctrl-C arriving between commands is queued and then discarded when the next
command starts: it interrupts nothing. Interrupt such a session with a signal to
the process (`kill -INT`) *while a command is running*, or end it with `kill`.

`SIGHUP` (the terminal closed) and `SIGTERM` (a plain `kill`) end the session
through the same teardown as `quit`: a running command is cancelled at its next
checkpoint first, as on the first Ctrl-C, then the pool claims and operation
locks are released and mtui exits with 129 (`SIGHUP`) or 143 (`SIGTERM`). A
repeated `SIGHUP`/`SIGTERM` does not force-quit; a second Ctrl-C still does,
and `SIGKILL` skips the teardown. A host command already under way, such as
`run`, finishes before the teardown starts, so a supervisor that kills after a
short grace period (a container stop allows 10 seconds by default) can cut the
teardown off and leave the locks behind. Once a signal has arrived, a
command-timeout question ("keep waiting?") is answered no without reading the
terminal; one already on screen when it arrives still waits for its answer, or
for the terminal to close. If the terminal fails while mtui waits for the next
command, the same teardown runs before mtui reports the error. Signals during
the initial load (`-a`/`-k`/`--sut`) still end mtui at once, without teardown.
Signals that arrive while `shell` or `edit` holds the terminal are not acted on
until that command returns. An unresponsive host cannot hold the teardown:
releasing its pool claim and closing it each give up after 45 seconds, and mtui
names the host. The claims and locks left on it need `unlock --pool` and
`unlock --force` once it answers again.

## How do I remove a dangling lock left by a crashed session?

`list_refhosts --free` shows who holds each lock, since when, and its comment,
without attaching the host. A session that ends normally — `quit`, Ctrl-D,
`SIGHUP` or `SIGTERM` in the REPL, or any end of an `mtui-mcp` session — removes
its own locks, commented or not, so a lock that outlives its session means that
session crashed or was killed (`SIGKILL`, a container stop) before its teardown
reached the host.

Reconnect to the same hosts and run `unlock -f` (force) to remove locks left by
another user or session. mtui also reaps locks older than `[lock] stale_age`
automatically on connect (see [Configuration](configuration.md)); `unlock -p`
removes a pool-claim lock instead of the operation lock.

## Which update should I pick up next?

`updates` lists the queue live from the TeReGen API, sorted by priority. By
default it shows the actionable pickup queue — **unassigned** updates that are **in
testing**. Widen or filter it:

```
updates --review-group qam-sle --limit 5
updates -G qam-sle -G qam-teradata                           # either group, one query
updates --mine                # assigned to your oscrc or Gitea login
updates --status all          # every status and assignee
updates -F Rating -F 'Assigned Roles' -F 'Unassigned Roles'  # osc-qam field names
updates --json                # the raw TeReGen rows, for scripting
```

`--mine` asks for the logins your review actions are recorded under: the
`user` of the oscrc section for `[obs] api_url` (read even when the section
has no usable `sshkey`, so a password or keyring oscrc works) and, with
`[gitea] token` set, that token's Gitea login. It never asks for the local
session user (`[mtui] user`), so a Gitea assignment recorded under that name,
which `assign` falls back to when it cannot resolve the token's owner, needs
`--assignee`. When one of the two sources fails, a `note:` line names that source and why,
and the other login is listed alone; under `--json` that fails the call
instead, and with neither login there is nothing to list. An empty answer
names the logins it asked for.

`-F` accepts the field names `osc qam list -F` uses — case-insensitively, with
spaces, hyphens and underscores treated as equivalent: `ReviewRequestID`,
`Incident Priority`, `Rating`, `Category`, `Status`, `Kind`, `Deadline`,
`Assignee`, `Assigned Roles`, `Unassigned Roles`, `Title`, `URL`, `Products`,
`SRCRPMs`, `Bugs`, `Creator`, `Package-Streams`, `Issues`, `Comments`. The
last three are not in the TeReGen queue listing: they are read live from OBS
per shown row (after `--limit`/`--offset`), so narrow large queues with
`--limit`. SLFO rows render `n/a` there (their review ids are Gitea PR
numbers, not OBS requests); a row OBS refuses renders `unavailable` in all
three, and an unreadable comments document renders `unavailable` for
`Comments` while the row's packages and issues still print; comments are terminal-sanitised and capped to
the newest five per row with the omitted count marked. `Unassigned Roles`
renders `n/a` when it cannot be answered honestly: on any row served
without review groups or without assignment data, where "every group open"
would be indistinguishable from the truth. `--json` and `-F` are mutually
exclusive, and `Assignee`/`Assigned Roles` are refused under any `--status`
other than `testing`, where TeReGen omits the assignment data they would
render.
