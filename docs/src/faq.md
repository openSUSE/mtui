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

## Can a Rust mtui and a Python mtui share the same reference hosts?

Yes. The remote-lock wire format is identical across both implementations, so
they cooperate on a shared host fleet. There are two locks with the same
`timestamp:user:pid[:comment]` layout: the operation lock
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

## How do I install packages the update newly introduces?

Feature updates often add packages that only exist in the test-update repository,
so `prepare` can't install them yet. Run `update --newpackage` to install the new
packages right after the update applies.

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

## Where are the per-host install logs stored?

Under the loaded template's checkout, in
`template_dir/<RRID>/install_logs/<host>.log` (one file per refhost). The
`install_logs` sub-directory name is configurable under `[mtui]`; see
[Configuration](configuration.md).

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
updates --mine                # updates assigned to you
updates --status all          # every status and assignee
```
