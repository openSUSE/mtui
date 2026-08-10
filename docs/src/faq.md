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
command runs against the active template by default; scope it to one with
`-T <RRID>`/`--template <RRID>`, or fan out across all loaded templates with
`--all-templates`. Under fan-out each template gets its own `=== <RRID> ===`
banner.

## Can I run mtui without loading a testreport?

Yes, for the commands that do not need one — notably `list_refhosts`, which
searches the reference-host inventory offline (no SSH, no lock, no loaded
template). Host-action commands need a loaded template with connected hosts.

## Does mtui support SSH password authentication?

No — and it never will. mtui is **pubkey-only by design**: it authenticates from
your SSH agent or `~/.ssh/id_*`. This is preserved from MTUI.

## Can several testers use the same reference hosts at once?

Yes, but the locks are **advisory and fail-fast, not a queue**. Every mtui on the
fleet writes the same `timestamp:user:pid[:comment]` layout, so sessions see one
another's claims — a host held by someone else is *skipped*, and an
install/update aborts with "Hosts locked" rather than waiting. Raise
`[lock] wait` (default `0`) to poll for a busy host during pool arbitration.
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

`edit` spawns your `$EDITOR` (or `$VISUAL`) on the controlling terminal, as usual.
Set it in your shell environment.

## Can I spawn a terminal emulator on all refhosts?

Yes — that is what `terms`/`switch` do, using the `term.*.sh` launcher scripts.
See [Installation](installation.md#terminal-launcher-scripts) for installing them
and the `$MTUI_TERMS_DIR` override.

## How do I export results into the testreport?

`export` writes the collected run/update logs (and, for the openQA-sourced
workflows, openQA data) into the testreport's text format. Its `regression tests:`
section uses an idempotent `overview_inject` BEGIN/END block, so re-exporting
updates in place rather than duplicating.

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
talks to the OBS/IBS API natively — no `osc` subprocess. `svn` is still used for
the SVN testreport backend, and a terminal emulator for `terms`/`switch`; both
are optional and mtui degrades gracefully when they are absent.

## Why does `assign`/`approve` go to Gitea when I expected OBS (or vice versa)?

mtui decides per update, from the loaded template's own metadata, not from the
RRID: an update whose `metadata.json` carries a `gitea_commit_hash` is
Gitea-served, otherwise it is OBS-served. This matters because the RRID's shape
cannot answer the question during the SL-Micro 6.0/6.1 cutover — both workflows
share the `SLFO:1.1` id space, and an update can briefly be served **both**
ways at once. When that happens mtui always drives the Gitea workflow and
**leaves that update's OBS review request alone** — `assign`/`unassign`/
`reject`/`comment`/`approve` never touch it. If an update's OBS request looks
stuck open, check whether its template carries a Gitea commit hash; if so,
that is expected, not a bug.

## How do I install packages the update newly introduces?

Feature updates often add packages that only exist in the test-update repository,
so `prepare` can't install them yet. Run `update --newpackage` to install the new
packages right after the update applies.

## Why does `prepare` (or `update`) refuse to run?

When the loaded report's metadata names no package versions, `prepare` and
`update` refuse with an error instead of printing an unqualified success over
a no-op (#396) — an empty-metadata report used to produce
`prepare completed on <hosts>` while installing nothing. Check
`list_packages -w` and the report metadata; a host the fan-out cannot build a
command for (unresolvable release, no matching template) likewise fails the
run by name.

## How do I control what `prepare` installs?

`prepare` has three switches:

- `prepare -f` / `--force` — force installation even on package conflicts.
- `prepare -i` / `--installed` — only prepare packages already installed (skip
  pulling in additional patchinfo packages).
- `prepare -u` / `--update` — enable the test-update repositories and install from
  there.

## Can I run a command on only some of the connected hosts?

Yes — temporarily disable the rest with `set_host_state`, then re-enable:

```
set_host_state -t hostA -t hostC disabled
run zypper lr            # runs only on the still-enabled hosts
set_host_state enabled   # re-enable all
```

`disabled` hosts run nothing (and print nothing).

## Can I export the update log from a specific refhost?

Yes: `export -t <host>`. By default `export` writes the collected data for every
host in the list — including disabled ones — so to keep a temporarily-added host
out of the report, `remove_host -t <host>` before exporting.

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

Under the loaded template's checkout, in
`template_dir/<RRID>/install_logs/<host>.log` (one file per refhost). The
`install_logs` sub-directory name is configurable under `[mtui]`; see
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
- `prepare --installed-only`, and `downgrade` on non-transactional hosts, check
  between packages. A transactional (SL-Micro) `downgrade` applies in one
  transaction, so it finishes first like the commands below.
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

Ctrl-C during the **Ctrl-D/`quit` teardown** behaves the same way but cancels
nothing: the teardown is what releases the pool claims and closes the hosts, so
it always runs to completion. A press there warns that it is in progress, and a
second one force-quits — leaving both the operation locks and the pool claims
(`unlock --force` and `unlock --pool`).

One carve-out: Ctrl-C during the **initial load** at startup (`-a`/`-k`/`--sut`,
before the first prompt appears) still exits immediately, without teardown. Any
locks or pool claims that partial load had already taken need
`unlock --force`/`unlock --pool`, as after any crash.

## How do I remove a dangling lock left by a crashed session?

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
updates -G qam-sle -G qam-teradata                           # groups OR together
updates --mine                # updates assigned to you
updates --status all          # every status and assignee
updates -F Rating -F 'Assigned Roles' -F 'Unassigned Roles'  # osc-qam field names
updates --json                # the raw TeReGen rows, for scripting
```

`-F` accepts the field names `osc qam list -F` uses — case-insensitively, with
spaces, hyphens and underscores treated as equivalent: `ReviewRequestID`,
`Incident Priority`, `Rating`, `Category`, `Status`, `Kind`, `Deadline`,
`Assignee`, `Assigned Roles`, `Unassigned Roles`, `Title`, `URL`. Fields the
TeReGen queue listing does not carry (`Products`, `SRCRPMs`, `Bugs`,
`Package-Streams`, `Creator`, `Issues`, `Comments`) are named in the error
rather than silently absent. `Unassigned Roles` renders `n/a` when it cannot
be answered honestly: on SLFO rows (TeReGen does not expose review groups on
them) and on any row served without assignment data, where "every group open"
would be indistinguishable from the truth. `--json` and `-F` are mutually
exclusive, and `Assignee`/`Assigned Roles` are refused under `--status all`,
where TeReGen omits the assignment data they would render.
