# Changelog

All notable user-visible changes to MTUI are documented in this file.

The format is based on [Keep a Changelog 1.1.0](https://keepachangelog.com/en/1.1.0/),
and this project follows [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]:

### Added

- Groundwork for a native OBS/IBS QAM review backend that calls the OBS API
  directly (no `osc` library, no `osc qam` subprocess), reading credentials
  from the user's existing `~/.oscrc`. A new `[obs]` config section
  (`api_url`, `conffile`, `request_timeout`, `backend`) is introduced; the
  backend defaults to `plugin`, so behaviour is unchanged for now. When the
  `native` backend is enabled in a later release, OBS TLS is governed by
  `[mtui] ssl_verify` rather than oscrc's own TLS knobs, and `request_timeout`
  is a coarse between-call budget layered over the per-call HTTP timeout.

- `mtui-mcp` is now much more token-efficient. Tool schemas are automatically
  slimmed of redundant pydantic boilerplate (the per-field `title` keys and the
  `anyOf: [T, null]` optional-field unions), shrinking the tool-list payload
  sent on every request by roughly a quarter with no change to what the model
  can call. A new `[mcp] tool_profile` setting selects the exposed tool surface:
  `full` (default, every tool — unchanged behaviour) or `core` (a curated
  everyday subset that roughly halves the payload), fine-tunable with the
  `[mcp] tools_allow` / `tools_deny` lists. A new `[mcp] max_output_bytes`
  setting (default `100000`, `0` to disable) caps a single tool result, with
  oversized output truncated and a notice pointing at the testreport read
  paging.
- `unload` is now exposed as an `mtui-mcp` tool. It takes an RRID and drops
  exactly that loaded template (closing only its host connections), leaving the
  others loaded — the addressable counterpart to `load_template` for MCP
  clients. It is single-target by design and never fans out, even with several
  templates loaded. (`switch` stays REPL-only; select a template per call with
  the `template` parameter.)
- New `regenerate` command — regenerates the loaded update's test-report
  template via the TeReGen API (`POST /reports/{id}/regenerate`), waits for the
  generation job to finish, and reloads the fresh template. Supports `--force`
  (overwrite an existing unedited template), `--ignore-inconsistent`
  (regenerate despite inconsistent metadata, e.g. an arch mismatch), and
  `--no-wait`. The wait shows a TTY spinner interactively and can be
  interrupted with Ctrl-C, and over `mtui-mcp`
  it is a slow command that can be backgrounded (`background=true`). The
  stale-template loader (when a checked-out template's hash no longer matches its
  Gitea PR) now offers to regenerate via TeReGen, delete the local checkout, and
  wait for the rebuild in place of the old static hint.
- New `updates` command — lists the update queue, fetched live from the TeReGen
  API (`GET /api/v1/updates`, fed from SMELT) and sorted by priority. Each row
  shows priority, status, kind (SLFO / Maintenance / ...), deadline and the
  RRID; the queue merges gitea-sourced updates (SLFO/SL-Micro) with the classic
  Maintenance updates in QAM testing. By default it shows the actionable pickup
  queue — **unassigned** updates that are **in testing** — so bare `updates`
  is usable without wading through released entries; pass `--status all` for the
  full queue (every status and assignee). Optional `--review-group`/`--status`/
  `--limit` filters. This is the TeReGen-backed replacement for the removed
  `smelt_updates`/`smelt_requests` commands.
- `updates` gained assignment exposure (restoring functionality dropped with
  SMELT): `--assignee <user>` and `--mine` (the current session user) filter the
  queue to updates assigned to that user, while `--all-assignees` shows every
  update regardless of assignee (overriding the unassigned default) and
  annotates each row with its assignee. Requires the matching TeReGen
  `/updates` assignment support.
- New `checkers` command — lists the build-check (checker) result runs for the
  loaded update, fetched live from the TeReGen report API
  (`GET /reports/{id}/checkers`). This is the TeReGen-backed replacement for the
  removed `smelt_checkers` command (same data, now via TeReGen instead of SMELT).
  Report-bound and fans out across loaded templates like the other inspection
  commands.
- Multiple templates can now be loaded at once. `load_template` adds a template
  to the session instead of overwriting the currently loaded one (loading an
  already-loaded RRID reloads and replaces it). New commands manage the loaded
  set: `list_templates` shows every loaded template (RRID, host count, workflow
  mode, and which one is active), `switch <RRID>` changes the active template,
  and `unload <RRID>` drops one template, closing only its host connections.
  `quit` now disconnects every loaded template's hosts. The bottom toolbar shows
  the loaded-template count and the active RRID.
- Action commands now fan out across every loaded template by default. `run`,
  `update`, `prepare`, `install`, `uninstall`, `downgrade`, `export`, `set_repo`,
  `reboot`, `put`, `get`, `commit`, `checkout`, `approve`, `assign`, `unassign`,
  `reject`, `comment`, `show_diff`, `analyze_diff`, `reload_openqa`,
  `openqa_overview`, `openqa_jobs`, `smelt_update`, and `smelt_checkers` run once
  per loaded template, each against its own hosts/report, with the output of each
  template prefixed by an `=== <RRID> ===` banner. A new `-T/--template
  RRID` flag scopes such a command to a single loaded template, and
  `--all-templates` forces fan-out explicitly. When a fanned-out command fails on
  one template it keeps running on the others and reports an aggregate failure at
  the end.
- Report-bound inspection commands `list_metadata`, `list_bugs`,
  `list_update_commands`, `list_versions`, `list_packages`, and
  `show_update_repos` now fan out across every loaded template too (previously
  they only reported the active one), and accept the same `-T/--template` and
  `--all-templates` flags.
- Host-locking commands `lock` and `unlock` and host-listing commands
  `list_hosts`, `list_locks`, `list_timeout`, `list_sessions`, `show_log`, and
  `list_history` now fan out across every loaded template by default (previously
  they acted on the active template only), each acting on that template's own
  hosts with the output prefixed by an `=== <RRID> ===` banner. They accept the
  same `-T/--template` and `--all-templates` flags to scope a single template or
  force fan-out explicitly.
- Tab completion for the `-T/--template` and `--all-templates` flags on every
  fan-out command, completing the loaded template RRIDs as values (like `switch`
  and `unload`).
- Host arbitration for multi-template fan-out. Each loaded template draws a
  *distinct* free reference host per test-target slot (product + version + arch
  + addons) from the shared pool, arbitrated in-process so two templates never
  collide on the same host; claimed hosts carry an identifying `mtui pool <RRID>
  [<owner>]` remote-lock comment and are released on `unload`/`quit` and when an
  mtui-mcp session closes. New `lock.wait` / `lock.wait_poll` options make a busy
  host *queue* (polling) instead of failing immediately, so several fanned-out
  templates wait politely on an exhausted pool. This arbitration is always on;
  scope a command with `-T` to act on a single template.
- mtui-mcp gained multi-template parity. Fan-out action tools expose optional
  `template` (scope the call to one loaded RRID) and `all_templates` parameters;
  a call without them fans out across the session's loaded templates. A
  backgrounded fanned-out slow command now mints one job per template (visible in
  `job_list`, individually pollable and cancellable) instead of a single job for
  the whole fan-out.
- `list_locks` and `unlock` gained a `-p`/`--pool` flag to act on host pool
  claims instead of the zypper/operation lock. By default both commands operate
  on the zypper/operation lock only (pool claims are no longer shown by
  `list_locks`). `unlock --pool --force` removes a pool claim held by another
  user or template.

### Changed

- `analyze_diff` was reworked to handle complex OBS `source.diff` files. It now
  parses the diff section by section (coping with multiple spec files and
  repeating `other changes:` blocks) and produces a review-friendly summary:
  changed source archives, patches added to / removed from the spec (flagging
  any added patch missing from the changelog), version/`%global` bumps, and
  `bsc#`/`CVE-` references. The define-vs-apply cross-check now warns only on a
  genuine mismatch and is skipped for `%autosetup`/`%autopatch` packages, and
  changelog matching is literal (a `.` in a patch name is no longer a wildcard).
- `set_workflow`, `add_host`, and `remove_host` now fan out across all loaded
  templates by default (each acting on that template's own report and host
  list), matching the other action commands. Use `-T RRID`/`--template RRID` to
  scope a single command to one loaded template, or `--all-templates` to request
  fan-out explicitly.
- Over `mtui-mcp`, tools that previously acted on the "active" template now fan
  out across every loaded template when more than one is loaded and no
  `template`/`-T` is given, since MCP has no client-addressable active pointer
  (`switch` is REPL-only). Pass `template=RRID` to scope a call to a single
  template. `list_templates` no longer shows the `*` active marker over MCP
  (the marker remains in the interactive REPL).
- Over `mtui-mcp`, the testreport file tools (`testreport_read`,
  `testreport_logs`, `testreport_patch`, `testreport_write`, `testreport_fill`)
  gained an optional `template=RRID` parameter selecting which loaded template's
  checkout to act on. With more than one template loaded they now refuse an
  unscoped call (naming the loaded RRIDs) instead of silently editing the
  last-loaded one; single-template sessions are unchanged.
- Over `mtui-mcp`, `testreport_read` now accepts an optional `relpath` to read
  any file under the loaded template's checkout (traversal-guarded), defaulting
  to the report's `log` file when omitted. This absorbs the former
  `testreport_read_file` tool (see Removed); `testreport_logs` still lists the
  auxiliary `build_checks/` and `install_logs/` files, now fetched via
  `testreport_read`.
- Over `mtui-mcp`, loading a template (`load_template`, `regenerate`) no longer
  moves the session's active-template pointer. The active template is REPL-only
  navigation state; setting it on every load made it hidden, unaddressable
  state that let unscoped tools silently act on the last-loaded template.
  Clients now address a template explicitly per call (`template=`/`-T`), and an
  unscoped action fans out across all loaded templates. A single-template
  session is unchanged (the registry's active fallback is the only loaded
  report).
- Over `mtui-mcp`, tool calls now run **concurrently across different
  templates** within one session: a command (or testreport tool) scoped to one
  loaded template holds only that template's lock, so work on other templates
  (and backgrounded per-template jobs) proceeds in parallel instead of queuing
  behind a single session-wide lock. Two calls on the *same* template still
  serialise, and registry-mutating commands (`load_template`, `unload`) take an
  exclusive gate that briefly drains in-flight per-template work. Per-call
  stdout/display capture is now isolated per call so concurrent commands never
  cross-contaminate each other's output.

### Removed

- The `mtui-mcp` `testreport_read_file` tool has been removed; its function is
  now served by `testreport_read` with the new optional `relpath` parameter
  (which defaults to the report's `log` file). Replace
  `testreport_read_file(relpath=…)` calls with `testreport_read(relpath=…)`.
- The initrd / pre-post custom-check framework has been removed. The
  `PreScript`/`PostScript`/`CompareScript` hook engine that copied probe
  scripts onto reference hosts and ran them before/after an `update` (diffing
  the pre/post output to flag regressions), the shipped probes
  (`check_initrd_state`, `check_vendor_and_disturl` and their `compare_*`
  scripts, plus the standalone `doit.sh` harness), the `update --noscript`
  flag (and its `mtui-mcp` tool parameter), and the per-host `scripts:` section
  in the manual test-report export no longer exist. The `=> PASSED/FAILED`
  install verdict is unchanged — it still derives from the package-version
  check. The zypper exit-code validators that confirm an `update`/`prepare`/
  `downgrade` actually succeeded are unaffected.
- SMELT support has been removed; report data is now sourced from the TeReGen
  report API (`[teregen] api`, default `https://qam.suse.de/api/v1`). The
  `Smelt` data-source client, the `smelt_update`, `smelt_updates`,
  `smelt_requests` and `smelt_checkers` commands, and the `[smelt] url`
  configuration option no longer exist. Picking up an update now surfaces its
  priority/deadline from TeReGen (`metadata.json` priority/deadline fields)
  instead of SMELT. `smelt_checkers` is replaced by the new TeReGen-backed
  `checkers` command and `smelt_updates`/`smelt_requests` by the new `updates`
  command (see Added) — all SMELT-sourced data now flows through TeReGen.
- Location support has been removed. The `set_location` command, the
  `-l`/`--location` command-line option (both `mtui` and `mtui-mcp`), and the
  `mtui.location` configuration option no longer exist. Legacy `refhosts.yml`
  files that group hosts under top-level location keys are still read: every
  group is now merged into a single host pool. A stray `location =` line in an
  existing config is harmless and simply ignored.
- The `set_session_name` command and the `session` field in the bottom toolbar
  (and the `:name` suffix it added to the prompt string) have been removed. They
  are redundant now that multiple templates can be loaded at once and the toolbar
  shows the active template's RRID alongside the loaded-template count.
- The `-c`/`--clean-hosts` flag on `load_template` has been removed. Host
  carry-over between templates no longer happens (each template owns its own
  hosts), so the flag that toggled it is obsolete.
- The SSH password fallback has been removed. When public-key authentication to
  a host fails, mtui no longer prompts for a root password; the connection is
  reported as failed instead. This removes a rarely-used fallback whose prompt
  got overwritten by sibling hosts' output when connecting a group of hosts in
  parallel, leaving an unclear UI. Set up working SSH key authentication to the
  target (verify with `ssh root@<host>`).

### Fixed

- Colorized log formatting no longer corrupts the record for other handlers
  on the same logger: the formatter used to overwrite each record's
  `levelname` in place, so any second handler (a file handler, a test log
  capture) saw the ANSI-wrapped name instead of `WARNING`/`ERROR`. The
  substitution is now scoped to the colorized handler's own formatting.
  Debug-level caller attribution is also resolved from the calling frame's
  own globals rather than `inspect.getmodule`, which returned "unknown"
  under import hooks that report a different file path than the code object
  records.
- A zypper exit code 104 during `update` is now reported as "package not
  found" instead of the misleading "update stack locked". Zypper's 104 is
  `ZYPPER_EXIT_INF_CAP_NOT_FOUND` (the requested package/capability does not
  exist), not a ZYpp-lock condition, so the old message sent testers chasing
  a lock that was never there; the `update` check now matches the `install`
  check's mapping. Additionally, the failure logs of the update, install,
  prepare and downgrade checks now label the captured command output
  correctly as "stdout:" (it was mislabeled "stdin:"), and an "errocode"
  typo in the downgrade check's exit-106 warning is fixed.
- MANUAL `export` now writes the per-host package-version lines into templates
  that use the indented `      before:` / `      after:` state headers, and
  bounds that lookup to the current host's own block so it can no longer
  wander into a neighbouring host's section. The indented-header lookup could
  never match (a formatting bug hidden by an unindented fallback), so on such
  templates the version lines were omitted (with only a misleading
  "before/after packages section not found" error) and the PASSED/FAILED
  verdict was never filled in. Additionally, when a version line cannot be
  written because the template is malformed (a state header as the very last
  line), the export now logs a warning naming the host and package instead of
  silently skipping the line.
- The lock time shown in "locked by" messages (e.g. by `list_locks` and
  `update`) is now actually UTC, as its label has always claimed. The shared
  lock timestamp was converted to the tester's local timezone before being
  formatted with the "UTC" suffix, so anyone outside UTC saw a time wrong by
  their UTC offset.
- Tab completion of file paths now works for partial paths under `~`: typing
  e.g. `put ~/Doc<TAB>` offers `~`-expanded matches such as
  `/home/user/Documents/`. Previously the completer appended a `/` to the
  expanded text unconditionally, so a partial basename like `~/Doc` was
  treated as the non-existent directory `$HOME/Doc/` and no completions were
  offered at all; typing the exact directory name (`~/Documents`, no
  trailing slash) already worked, but input that already ended in `/` (bare
  `~/`, or `~/Documents/`) got a second slash appended on top of the one
  already typed, e.g. `$HOME//Documents`. Directory candidates (including
  bare `~`'s home-directory entries) are now suffixed with `/`, matching
  shell convention, so a following `TAB` descends into them.
- `quit` (and `exit`/Ctrl-D) no longer hides host-teardown problems: a
  disconnect that crashes, including one that only crashes after running
  past the 45-second teardown window, is now reported as a warning naming
  the affected host. The exit status is still unaffected, but quitting
  already blocked until every disconnect finished before this fix and
  still does — only the visibility of the outcome changed, not the wait.
- SLE 15 LTSS-ERICSSON products are now normalized to a dedicated
  `SLES-LTSS-ERICSSON` product with the whole `-LTSS-ERICSSON` suffix
  stripped from the version. Previously the generic LTSS check ran first, so
  a version such as `15-SP4-LTSS-ERICSSON` was misfiled as `SLES-LTSS` with
  `-ERICSSON` left dangling in the version string, breaking product matching
  for those updates. A compound `LTSS-ERICSSON` branch now runs before the
  generic ERICSSON and LTSS checks, matching the SLE 12 convention where
  every compound LTSS flavor (e.g. `SLES-LTSS-ERICSSON`) is normalized to its
  own product name with a clean version, instead of leaving any flavor
  suffix dangling.
- The local system detection now parses `/etc/os-release` files with
  unquoted values, which the os-release spec allows for values without
  spaces (e.g. `NAME=Fedora` or `VERSION_ID=15.6`). Such values
  previously never matched, so the export footer and the `commit` line
  silently reported an empty distro/version on those systems.
- Corrected user-facing typos: `uninstall --help` described its positional as
  "package to install" (copy-paste from `install`) and now says "package to
  uninstall"; `terms` printed "available terminals scripts:" / logged "Aviable
  term scripts" and now says "available terminal scripts:" / "Available term
  scripts"; a failed openQA connection logged "Cannont connect to openQA" and
  now logs "Cannot connect to openQA".
- `export` in the AUTO and KERNEL workflows no longer fails with
  "No refhosts defined" when no reference hosts are connected. These
  workflows build the template from openQA/dashboard data and never touch a
  connected host, so fanning `export` out across loaded templates now runs
  each one instead of skipping host-less templates and erroring when every
  template was skipped. MANUAL export, which reports per-host results, keeps
  requiring a connected host.
- Under the acceptance-test harness (`ACCTEST_ROWS`/`ACCTEST_COLS`, no
  controlling TTY) the terminal geometry is no longer transposed: the
  fallback returned (rows, cols) while the normal path returns
  (width, height), so the pager wrapped at the row count and printed a
  column-count of lines per page.
- `put` of a directory now preserves the directory tree on the remote
  hosts. Every nested file used to be uploaded flat under its basename, so
  two files with the same name in different subdirectories silently
  overwrote each other (only the last survived, while the per-file success
  log suggested both were transferred).
- Removing a remote directory (e.g. via `FileDelete`/host cleanup) works
  again, and a failed file removal is no longer reported as success. The
  connection layer swallowed the SFTP error internally, so the target
  layer's ENOENT tolerance and its directory `rmdir` fallback were dead
  code; the error now propagates to the layer that handles it.
- The `mtui-mcp` tool-schema slimming no longer deletes a tool parameter
  (or nested object property) literally named `title` or `description`.
  The pydantic-boilerplate strip treated every dict key as a schema
  keyword — including the keys of a `properties` map, which are parameter
  *names* — so such a parameter vanished from the schema while staying
  listed in `required`, making the tool impossible to call correctly.
- Connecting reference hosts in several rounds (e.g. template autoconnect,
  which resolves recorded hosts and testplatform hosts separately) no longer
  drops the earlier rounds from the "Hosts" metadata line: the connected-
  systems map is now merged across rounds instead of rebuilt from the latest
  one, and pruned together with inactive hosts.
- A host-setup failure right after a successful SSH connect (e.g. the
  connection dropping while the remote lock file is written) no longer leaks
  the open SSH session; the half-set-up connection is closed before the host
  is reported as failed — on Ctrl-C too, and equally for `add_host -t`,
  which had the same leak.
- `openqa_jobs --failed` no longer lists still-running or scheduled jobs as
  failures. openQA reports an unfinished job's result as `none`, which the
  filter treated as "not passing" — during active testing an in-progress
  build looked like it had a dozen failures (all red, counted as `none=12`
  in the summary), risking a wrong verdict. Unfinished jobs are now
  excluded from `--failed`, and the listing shows their actual state
  (`running`, `scheduled`, ...) in yellow instead of a red `none`. When the
  filter leaves nothing, the message now distinguishes an in-progress build
  ("No failed openQA jobs ...; 12 of 12 still pending") from a build with no
  jobs at all.
- A command whose output contains non-UTF-8 bytes (a Latin-1 locale, `rpm`
  metadata with odd bytes, `cat` of a binary) is no longer recorded as
  failed. The final decode of the captured stdout/stderr was strict, so a
  single invalid byte raised `UnicodeDecodeError` out of the SSH run; the
  command — which may have exited 0 — was then logged as "failed to run"
  with exit code -1 and its whole output lost. Undecodable bytes are now
  replaced (U+FFFD) and the rest of the output is preserved, matching the
  tolerant per-line debug logging that already existed. The interactive
  `shell` mirror had the same defect, plus a sharper edge: it decoded each
  1024-byte chunk separately, so even a valid multibyte character straddling
  a chunk boundary killed the shell; it now uses an incremental decoder.
- Loading a report whose `metadata.json` omits (or nulls) the `testplatform`
  or `products` key no longer breaks later commands. The parser stored the
  raw `None` over the report's list defaults, so a PI/SL report load died in
  repository parsing with a `TypeError`, and on other report kinds
  `list_metadata` and refhost autoconnect crashed the same way. Both keys now
  fall back to an empty list, like the sibling `jira`/`bugs`/`packages` keys
  already did. An explicitly null `repositories` key — which crashed the
  parse itself (`frozenset(None)`) — is guarded the same way.
- Re-exporting a kernel-workflow testreport no longer silently deletes the
  injected `openqa_overview` block. From the second export on, the kernel
  results stage clears the regression-section body to drop the previous
  run's results — and the overview block, injected just before it, sat
  exactly in that range: it survived the first export and vanished on every
  one after. The overview is now injected after the results stage, so both
  the refreshed kernel results and the overview block survive re-exports.
- The `mtui-mcp` http idle sweeper no longer tears down a session that a
  client re-activated while the same sweep round was running. The sweep
  snapshotted the stale keys and then evicted them one by one; while one
  eviction waited on a slow host disconnect, a client could be handed a
  later stale-listed session (its idle clock freshly reset) — and the sweep
  still closed it, cutting SSH host connections out from under the active
  client's command. Staleness is now re-checked immediately before each
  eviction, so a just-re-activated session is spared.
- Concurrent `mtui-mcp` commands no longer race on the shared `mtui` logger
  level, which could silently drop captured `INFO` log lines from a reply.
  Two overlapping calls (different templates or different client sessions)
  each lowered/restored the process-global level around their log capture;
  the first call to finish restored the stricter level while the other was
  still running, filtering its remaining INFO records before the capture
  handler saw them. The lowering is now reference-counted, so the level is
  restored only when the last concurrent capture ends.
- A blank `[mtui] ssl_verify` value (`ssl_verify =`, usually an unfinished
  edit) no longer turns TLS certificate verification off. The empty string
  used to reach `requests` as a falsy `verify` — silently disabling
  certificate checks for every HTTPS call (openQA, dashboard, TeReGen,
  Gitea). A blank value is now treated exactly like an unset option: the
  verifying default applies and a warning explains how to disable
  verification explicitly (`ssl_verify = false`) if that was really meant.
- `list_history` no longer aborts the whole listing when `/var/log/mtui.log`
  contains a malformed line. The log is appended to over sftp by independent
  mtui sessions, so concurrent writes can tear a line (and the file may be
  hand-edited or rotated); a line whose first field is not a parseable
  timestamp used to raise out of the command, dropping every remaining
  host's history. Such lines are now skipped like the too-few-fields case.
- `list_packages -p <pkg>` no longer prints the literal word `None` for a
  queried package that is not part of the loaded update. The state column
  now behaves exactly like the no-update case: blank when the package is
  installed, `not installed` when it is not — and a not-installed package's
  empty version no longer renders as a literal `None` either.
- `install` and `uninstall` events are recorded in the per-host history
  again (`/var/log/mtui.log`, shown by `list_history`). The history writer
  was handed a nested package list, the resulting `TypeError` was silently
  swallowed, and no install/uninstall entry was ever written — unlike
  `update`/`downgrade`, which recorded fine. The callers now pass flat
  strings, and a failed history write logs a warning instead of hiding
  the error (the write stays best-effort and never breaks the operation).
- Writing a testreport (and every other atomic file write, e.g. the
  refhosts.yml cache) now always encodes UTF-8 on disk, matching the UTF-8
  read path. The write previously used the process locale codec, so under a
  non-UTF-8 locale a template containing non-ASCII content (bug summaries,
  maintainer names) either failed with `UnicodeEncodeError` — losing the
  edits — or silently wrote mojibake that the next load misread. The
  refhosts.yml store now reads with explicit UTF-8 to match (it read with
  the locale codec, which would have mis-decoded the now-UTF-8 cache).
- `commit -m <message>` no longer stores literal double quotes around the
  message in the SVN log. The message was manually wrapped in `"` for a
  shell that is never involved — svn receives the argv list directly — so
  `commit -m fix the thing` recorded `"fix the thing"` verbatim.
- A command that hits the inactivity timeout no longer leaks its SSH channel.
  Both timeout paths (the non-interactive abort used under `mtui-mcp` and an
  interactive "don't wait" answer) raised without closing the channel, so it
  stayed registered on the paramiko transport and the remote command kept
  running — repeated timeouts accumulated orphaned channels and remote
  processes. The channel is now closed before the timeout error propagates,
  and likewise when the wait-prompt is abandoned with Ctrl-D or Ctrl-C.
- Connecting a reference host no longer fails outright when one of its addon
  product files under `/etc/products.d/` is a dangling symlink or otherwise
  unreadable. The base-product path already tolerated exactly this; the addon
  loop did not, so a single stale `.prod` entry made the whole host
  impossible to add. The offending addon is now skipped with a warning.
- The testreport export/commit footer no longer loses the distro and version
  on hosts whose `/etc/os-release` single-quotes its values (permitted by the
  os-release spec). The parser's character classes matched a double quote or
  a literal pipe — `["|]` — instead of double-or-single quote.
- Re-exporting a testreport no longer degrades the template. Manual and
  kernel re-exports used to stack an empty duplicate `Links for update
  logs:` header per run (the links were de-duplicated, the header was not) —
  new links now go under the existing section, and empty duplicate headers
  stacked by earlier exports are cleaned up so damaged templates converge.
  The kernel re-export also duplicated the "All installation tests done in
  openQA" notice on every run; it is now inserted only once and stacked
  copies are reduced back to one.
- An auto-workflow `export` on a template without a `Links for update logs:`
  section no longer deletes everything from the install-tests section to the
  end of the file (export footer and any appended notes included). The
  fallback boundary looked for a line exactly equal to `## export MTUI:`,
  which never matches the real footer; it now scans for the footer as a
  substring.
- Re-exporting a manual testreport now actually refreshes stale per-command
  result lines (`… : SUCCEEDED`/`FAILED`/`INTERNAL ERROR`) for hosts in the
  current session. The host-tracking pattern required two spaces after
  `reference host:` (the template emits one) and compared the whole match
  instead of the hostname, so the cleanup never removed anything; other
  hosts' sections remain untouched.
- `mtui-mcp` no longer corrupts a `commit`/`lock` call that also carries a
  `template` argument, nor a backgrounded `run` that fans out across several
  loaded templates. A `-m`/`-c` message or a `run` command line no longer
  swallows the template-scoping flag, so the message stays intact, the call
  is scoped to the intended template (instead of silently fanning out to
  every one), and no stray `-T <RRID>` leaks into the remote command.
- `qam reject` messages and `qam comment` text are no longer corrupted with
  literal quote characters. They were being run through `shlex.quote` before
  being placed into the `osc` argv list, but that list is executed without a
  shell, so the escaping was delivered verbatim — a message like
  `does not build` reached osc as `'does not build'` and `won't build` became
  garbled. The message and comment are now passed through unmodified.
- Answering a yes/no confirmation prompt (e.g. `approve`, `remove_host`,
  `regenerate` overwrite, or `q` at the pager) no longer erases the last
  command from your REPL history. The prompt used to scrub the newest
  entry from `~/.mtui_history`, which — since the answer itself is never
  written there — silently deleted the real command you had just run,
  from both the up-arrow stack and the persisted file.
- Replacing an already-loaded template in the registry (e.g. `regenerate`
  rebuilding a report without a prior `unload`) now tears the previous report
  down — releasing its host-arbitration pool claims and closing its refhost SSH
  connections — instead of silently overwriting it. Repeated `regenerate`
  cycles no longer leak file descriptors or leave stale arbiter claims that
  could block later slot re-acquisition on the same refhost pool.
- `mtui-mcp` session teardown (idle-TTL sweep or explicit eviction) is now
  genuinely time-bounded. A wedged refhost close (a dead peer that never sends
  an RST can hang paramiko's disconnect forever) no longer blocks the whole
  disconnect: the stuck close is waited on only up to a fixed budget, then
  logged and abandoned so `close()` — and the http registry's idle-sweep behind
  it — always returns. Previously the bound was defeated by the thread pool's
  context-manager exit, which re-joined every worker regardless of the timeout.
- The dashboard-backed auto-workflow openQA report no longer counts superseded
  (obsoleted) jobs. When a job is retriggered the dashboard keeps the older run;
  its stale result was still folded into the install verdict and listed as a
  phantom failure. Obsoleted runs (marked by the dashboard's `obsolete` flag or
  an `obsoleted` result) are now dropped, matching the openQA-search connector,
  so a retriggered-and-now-passing install is reported as passed and only the
  current run per scenario is shown.
- The dashboard-backed auto-workflow openQA section no longer prints
  "All jobs passed." while its own Summary lists a problem group. A group whose
  only problem is a still-running, `parallel_failed`, or otherwise non-failed
  job (i.e. it has no failed/incomplete/timeout job) is now reported with a
  "some groups need review" note instead of a contradictory all-passed verdict.
- An unscoped multi-template fan-out of a host-phase command (e.g. `lock`,
  `run`) no longer fails the whole fan-out — or, for `lock`, pretends to
  succeed — when a loaded template has no connected host. A host-less template
  is now skipped with a warning when no `-t` host was named, the command still
  runs on every template that does have hosts, and it fails with "No refhosts
  defined" if every template was skipped. Naming a disconnected host with `-t`
  is still a per-template failure, and explicitly `-T`/`--template`-scoped
  calls are unchanged and still raise.
- `Ctrl-C` during a multi-template fan-out now aborts the remaining templates
  instead of being recorded as a per-template failure while the loop keeps
  going.
- An invalid `[mtui] ssl_verify` value (e.g. the typo `false1`, a CA-bundle
  path that does not exist, or a certificate directory without `c_rehash`-ed
  entries) is now rejected when the config is read — one clear error naming
  the accepted forms, falling back to the verifying default — instead of
  flowing verbatim into `requests` and crashing the first HTTPS call (e.g.
  loading an SLFO template) with an opaque `OSError: Could not find a
  suitable TLS CA certificate bundle`. A `~` in a bundle path is expanded, a
  relative path is pinned absolute at startup (so `chdir_to_template_dir`
  cannot invalidate it), and a blank value keeps meaning "verification off"
  but warns. Validation rejections across all config options now log a single
  actionable line; unexpected parser errors keep their full traceback.
- When `ssl_verify` is unset — or set to `true`, which now behaves
  identically — TLS verification prefers the system's CA bundle (the
  interpreter's OpenSSL default cafile, honouring `SSL_CERT_FILE`; e.g.
  `/etc/ssl/ca-bundle.pem`) over `requests`' bundled `certifi` CAs, so
  system-installed CAs like the SUSE root work when running mtui from a git
  checkout — previously internal hosts failed certificate verification there
  even with `ca-certificates-suse` correctly installed, because PyPI
  `certifi` never consults the system trust store.
- Fetching openQA data from the QEM Dashboard no longer floods the log with
  "Connection pool is full, discarding connection" warnings. The shared HTTP
  session's connection pool is now sized to match mtui's default worker-thread
  count (`min(32, cpu + 4)`), so the concurrent per-setting requests reuse
  connections instead of repeatedly opening and tearing them down.
- `load_template` no longer fans out across already-loaded templates. Over
  `mtui-mcp`, where an unscoped command fans out by default, loading a new RRID
  while several templates were already loaded re-ran the load — and its host
  autoconnect — once per loaded template, needlessly grabbing reference hosts
  from the pool. `load_template` now runs exactly once (`scope = "single"`, like
  `unload`), connecting only the newly loaded template's own hosts.
- Over `mtui-mcp`, a command with a `nargs=REMAINDER` *optional* flag declared
  before another flag (the shape used by `reject --message`) could have the
  later flag swallowed into the message when re-encoding a tool call to argv.
  The REMAINDER optional is now always emitted after every other flag, so the
  trailing flag is parsed correctly regardless of argument declaration order.
- The remote refhost pool lock is now claimed with an atomic exclusive create
  (`O_EXCL`) instead of a separate "read the lock state, then write it" pair.
  This closes a TOCTOU window where two separate mtui processes/users running
  pool selection at the same time could both believe a host was free and clobber
  each other's claim; exactly one now wins the create and the other backs off to
  the next candidate.
- Transactional (read-only-root, SL Micro) hosts now install and downgrade every
  package in a SINGLE `transactional-update` snapshot. The previous per-package
  loop ran one `transactional-update pkg in` per package, each opening its own
  snapshot, so the packages never landed together in the booted snapshot — the
  prepare/downgrade reported success yet the packages were missing (or stayed at
  the test version) after reboot. `prepare` and `downgrade` now pass the whole
  package set to a single fresh-snapshot invocation, dropping the obsolete
  `--continue`/`-C` chain and the separate init-snapshot/start-command canaries.
- SLFO updates now run in AUTOMATIC mode. Their openQA install jobs
  (`qam-incidentinstall-SLFO`) were previously excluded from the auto workflow,
  forcing every SLFO update to fall back to manual; they are now recognised and
  their install logs fetched from the correct artifact
  (`SLFO_update_install-zypper.log`, distinct from the classic
  `update_install-zypper.log`) via a job-name to log-filename mapping shared by
  both auto connectors (poo#200832).
- `mtui-mcp` now disconnects every loaded template's hosts when a session is
  closed (idle-TTL eviction or shutdown), not just the active template's. A
  session can hold several templates at once, each owning its own host group;
  the teardown previously reaped only the active template's connections,
  leaking the others' SSH sessions. This now mirrors the REPL `quit` command,
  which already closed every template's hosts.
- `approve`, `reject`, and `assign` no longer refuse a Gitea PR whose review was
  re-requested after an earlier decision (e.g. the package was rebuilt following
  a `REQUEST_CHANGES`). The "already approved/rejected" guard scanned the
  append-only comment history and so honoured a stale decision forever; it now
  treats a pending review request for the group as superseding the old decision,
  letting the group review the rebuilt PR again.
- Host *pool* claims and the zypper/operation lock are now fully independent.
  Previously both shared a single lock file (`/var/lock/mtui.lock`), so a pool
  claim taken during host selection blocked subsequent zypper operations
  (`install`, `uninstall`, `update`, `prepare`, `downgrade`) with "Hosts
  locked", and `list_locks` showed pool claims mixed in with operation locks.
  Pool claims now live in their own file (`/var/lock/mtui-pool.lock`) and never
  interfere with operation locks.
- A host pool-claimed by the current template no longer prevents that template
  from (re)connecting to it, even across `mtui` restarts (pool-claim ownership
  is now identified by template RRID + user instead of process PID). A host
  claimed by a different user or template still correctly blocks connection.
- Pool claims are now reliably removed on disconnect (`remove_host`, `quit`,
  `unload`, and MCP session teardown), so hosts are not left claimed after a
  session releases them.
- Authentication failures (failed key auth, generic SSH errors) now print a
  single clear error line instead of dumping a raw paramiko traceback and a
  duplicate message. The full traceback is still available with `--debug`.
- Loading a template no longer connects to *every* matching reference host when a
  testplatform resolves to several candidates on the same architecture. The
  autoconnect that runs during `load_template` was happening before the host
  arbiter was wired, so it fell back to the legacy "connect all candidates" path;
  it now draws exactly one host per test-target slot (product + version + arch +
  addons). If that host fails to connect, mtui automatically falls back to a
  backup candidate from the same slot, and only warns once when every candidate
  for a slot is unreachable.
- `add_host` without `--target` (selecting hosts from the testplatform) now
  connects exactly one host per test-target slot. Two problems caused several
  machines on the same architecture/product to be connected: (1) in automatic
  mode the reference hosts pre-loaded from the template were still connected
  alongside the pool-selected host, and (2) the per-slot grouping keyed on every
  module a host happened to have installed, so two otherwise-interchangeable
  hosts that differed by a single installed module were treated as separate
  slots and both connected. Selection now groups by what the testplatform
  actually requests (base product + version + arch + the requested addons) and
  connects only the arbiter-chosen host per slot. The host drawn for each slot
  (and the backup-on-failure order) is chosen at random among the free
  candidates so load is spread across interchangeable refhosts.
- `load_template` no longer reconnects the previously active template's hosts
  onto the newly loaded one. Each loaded template now owns its own reference
  hosts: loading a second template (e.g. after `add_host` had switched the
  first to the manual workflow and connected its testplatform hosts) connects
  only the new template's own hosts and leaves the first template's hosts and
  pool claims untouched. This carry-over was a leftover from the pre-1.0,
  single-template world where loading *replaced* the session.
- `assign`/`approve`/`reject`/`comment` no longer hang the mtui-mcp server. The
  `osc qam` subprocess inherited the server's stdin (under mtui-mcp that is the
  MCP stdio JSON-RPC pipe), so an interactive `osc` prompt (e.g. an approve
  confirmation) blocked reading it forever and, because the server serialises
  calls through one lock, every subsequent tool call wedged behind it. `osc` now
  runs with stdin detached (`DEVNULL`) and a 180s timeout, so it either completes
  or fails cleanly instead of deadlocking the session.
- A failed `assign`/`approve`/`reject`/`comment` now reports *why*. `osc qam` is
  run with its output captured, so a non-zero exit logs osc's actual stderr
  (e.g. "request already accepted", "user not assigned") instead of a bare "the
  command returned a non-zero exit code", and the `OSC` methods now return a
  success/failure boolean to the caller. When the failure followed a
  `-G/--group` call, the error adds an actionable hint: that path triggers an
  interactive osc confirmation which cannot be answered headless (e.g. under
  mtui-mcp), so the operation should be re-run without `-G/--group` to act on the
  review assigned to you.
- Desktop notifications (the `notify` extra) work again. They imported the
  long-dead `pynotify` PyGTK binding, which is unavailable on Python 3.13+, so
  the REPL's update-finished/update-failed toasts were a silent no-op; the
  extra also pinned an unrelated, Python 2-only package literally named
  `notify`. Notifications are now backed by
  [notify-py](https://pypi.org/project/notify-py/) (pure-Python DBus on Linux,
  no system GTK/libnotify needed), fire only in an interactive desktop REPL
  session (piped/cron/CI/MCP runs are skipped), and the notification class is
  no longer mistakenly passed as the icon name.
- `reject -m <message>` no longer aborts with `TypeError: expected string or
  bytes-like object, got 'list'` before the rejection is sent. `--message` uses
  `nargs=REMAINDER`, so it arrives as a list of words; `reject` passed it
  unjoined to `osc.reject`/`gitea.reject`, which hand it to `shlex.quote` (a
  list crashes it). The message is now joined to a single string first (matching
  `commit`), so a reject reason actually reaches the maintainer. Rejecting with a
  bare reason and no message was unaffected.
- The SMELT deadline shown for a **classic Maintenance** incident (on `assign`
  pickup and in `smelt_update`) no longer prints `?` when a real deadline
  exists. mtui read the incident's `crd` (customer-required date), which is
  `null` for most incidents, instead of `prd` (the planned release date SMELT
  displays as the deadline). It now uses `crd` when set and falls back to
  `prd`, and `smelt_update` shows both raw dates. SLFO updates were already
  correct; the REST v2 API exposes a dedicated `deadline` field.
- A testplatform `base=<extension>` (e.g. `base=SLES-LTSS`, `base=sle-ha`,
  `base=SLES_SAP`, `base=SLE_RT`) now resolves to refhosts that carry that
  product as an **addon** on a SLES/SLED base, not only to hosts whose base
  product is literally that name. In the refhosts-ng schema a single host has
  one base (`SLES`) with LTSS/HA/SAP recorded as addons, so the previous
  base-only match found nothing; `add_host` reported "No refhosts to add" for
  every LTSS/HA/SAP incident even though matching hosts existed.
- The product/refhost check no longer misreports a healthy host as having a
  dangling `/etc/products.d/baseproduct` symlink when that symlink uses an
  **absolute** target (`/etc/products.d/SLES.prod`) rather than a bare
  `SLES.prod`. The target was concatenated onto `/etc/products.d/`, producing
  `/etc/products.d//etc/products.d/SLES.prod`, which failed to open and was
  reported as dangling (also mis-classifying the real base product as an
  unexpected addon). The symlink target is now reduced to its basename, so
  both relative and absolute forms resolve.
- `openqa_overview` build-checks now match `python3-<pkg>` binary packages
  against their `python-<pkg>`-named source log. The normalization regex
  previously required at least two digits after `python` (`python38-`,
  `python313-`), so single-digit flavors like `python3-tornado` were missed
  and their build-check log was silently skipped.
- The kernel-export log downloader no longer swallows download failures. It
  fanned the per-log downloads out to a thread pool without ever collecting
  the futures, so any error raised in a worker (an unexpected one such as a
  bad TLS CA path — or even the downloader's own `ResultsMissingError` under
  `errormode="full"`) was stored on a discarded future and vanished: the
  kernel export finished "successfully" with zero downloaded logs and no
  error output. Failures are now collected from every future: unexpected
  errors are logged with the affected test and host, a summary warning
  reports how many downloads failed, and under `errormode="full"` the failure
  propagates to the caller once the whole batch has finished.
- `config set` now parses values through the option's declared getter and
  fixup — the same pipeline used for the config file — instead of coercing via
  the current attribute's type. Boolean options accept the INI spellings
  (`config set use_keyring true` previously set **False**, because only the
  literal `True` compared as true), integer options reject non-numbers
  (`config set connection_timeout abc` used to store the string `abc`), and
  `ssl_verify` only accepts booleans or an existing CA bundle path (a typo like
  `false1` used to be stored verbatim and made every later HTTP call fail with
  an `OSError` about an invalid CA path — such a value is now also rejected at
  config-file parse time, falling back to the verifying default). An invalid
  value is rejected with a single actionable error and the option is left
  unchanged.
- More config options are validated when the configuration is read, each
  invalid value producing one actionable error and falling back to its default
  instead of a delayed crash: the endpoint URLs (`[openqa] openqa`/`baremetal`,
  `[qem_dashboard] api`, `[teregen] api`, `[refhosts] https_uri`) must be
  http(s) URLs with a host and a numeric port (a typo like
  `https://openqa.suse.de:44e3` used to surface as a raw `InvalidURL`
  traceback at the first query), the duration/count options
  (`connection_timeout`, `[lock] wait_poll`, `[mcp] session_cap` /
  `session_idle_timeout`, `[refhosts] https_expiration`) must be positive
  integers (a negative `connection_timeout` reached paramiko and failed every
  host with a bogus "Error reading SSH protocol banner"), and
  `[mtui] install_logs` must be a single relative directory name (a nested
  value crashed after a successful template checkout; an absolute one silently
  replaced the whole log path). A `[mtui] template_dir` that cannot be created
  (e.g. a plain file in the way) is now reported as one clear error naming the
  option instead of an `OSError` traceback, and openQA queries catch any
  remaining URL/transport error shape instead of crashing the command.
- Log lines emitted while the TTY spinner is painting (e.g. the per-host
  `Removing repo … on <host>` lines under the `set_repo remove` spinner during
  `update`/`prepare`) no longer render with phantom leading padding — or, with
  colours off, glued to the leftover frame text. The spinner and the logging
  handler now coordinate through a shared paint lock: each record first erases
  the live frame and homes the cursor to column 0, then prints, and the spinner
  repaints on its next tick. Interactive prompts raised under a live spinner
  (e.g. the SSH command-timeout question) also pause the spinner for the whole
  read instead of repainting over the prompt. Off a TTY nothing changes.

## 18.2.0 - 2026-06-23

### Added

- Under `mtui-mcp`, the slow host commands (`run`, `update`, `downgrade`,
  `prepare`, `install`, `uninstall`, `set_repo`, `reboot`) accept a
  `background=true` flag: instead of holding the request open for the minutes
  the operation takes, the call returns a job id immediately and runs the
  command in the background under the session lock. Four new tools drive the
  job: `job_list` (every job in the session and its state), `job_status`
  (one job's state and elapsed time), `job_result` (a finished job's output,
  erroring while it still runs and surfacing the command's failure envelope if
  it failed), and `job_cancel`. Jobs are scoped to the session (one process
  under stdio; the caller's isolated session under http), so a client can fire
  off a slow host op and keep issuing other calls while it runs. A cancelled
  job already executing on a host may keep running there to completion; the
  same caveat as interrupting a foreground `run`.
- New `list_refhosts` command (and `mcp__mtui__list_refhosts` tool) to query and
  search the reference-host inventory **without connecting**: no SSH, no lock,
  no loaded template. Reads the same source `add_host` resolves through
  (`RefhostsFactory`) and filters by hostname glob (`--name`), arch (`--arch`),
  base product (`--product`), version (`--version`, `15-SP6`/`15.6`/`15`), addon
  (`--addon`), or a full `--testplatform` query; `--pool` groups by test-target
  slot, `--json` emits structured output, and `--free` additionally probes each
  matched host's live mtui-lock state (the only part that goes on the wire).
  **Location is not used to scope the search** (it is being retired): every
  location is searched and results are de-duplicated by host name, with
  `--location` to restrict to one. Lets fleet maintenance and manual users find
  refhosts through mtui instead of parsing `refhosts.yml` by hand.
- The `testreport_read` MCP tool accepts optional `offset` (1-based first line)
  and `limit` (max lines) to return a line window instead of the whole file.
  Without them the behaviour is unchanged (full file). This lets a caller page a
  large report (a Product Increment `log` runs to thousands of lines after
  `export` and otherwise overflowed the reply) using the same 1-indexed line
  numbers `testreport_patch` consumes; the reply still reports the file's total
  `line_count` (plus `offset`/`returned_lines` when a window is requested).
- Connecting a reference host now verifies that its installed products match
  what `refhosts.yml` records for that host. On any drift (wrong or
  wrong-version base product, wrong architecture, addons that are missing,
  unexpected, or at a different version, or a dangling
  `/etc/products.d/baseproduct` symlink), mtui logs a `WARNING` per drift class
  and keeps the host (the check never aborts a connect). `qa` is ignored on both
  sides to match the products mtui already skips. This catches validating an
  update on a host that is not the system its metadata claims; hosts absent from
  `refhosts.yml` are skipped silently.
- Under `mtui-mcp`, a command's own `mtui` log records (`INFO` and above) emitted
  while it runs are now included in the tool reply, not just what it prints to
  stdout. The capture follows the command into the worker threads it fans out to
  (MTUI's thread pools now propagate context), so warnings logged off the main
  thread (such as the per-host product-drift report above, emitted on
  `add_host`'s connect pool) reach MCP clients directly; `add_host` no longer
  re-echoes them to stdout.

### Removed

- The `-p`/`--prerun` option and the `-n`/`--noninteractive` option are gone,
  along with the prerun command-queue machinery they drove. A prerun script was
  the only thing `--noninteractive` supported (mtui refused `-n` without `-p`),
  so both flags are retired together. Drive mtui through its interactive prompt
  (or the `mtui-mcp` server) instead; existing invocations that passed `-p` or
  `-n` must drop those flags.

### Fixed

- A failed `update` now reports the outcome of **every** host instead of only the
  first failure. The post-update check aborted on the first host that failed, so a
  parallel update that broke on several hosts surfaced just one `UpdateError` and
  the rest went unreported. mtui now checks all hosts, logs a per-host
  success/failure summary, and raises an error naming **all** failed hosts (a
  single-host failure still re-raises that host's original error unchanged). No
  reboot is attempted when any host failed.
- `prepare --installed` (`-i`) now actually installs/updates the already-installed
  packages. The `installed_only` zypper and yum command templates were missing
  the install verb (`zypper -n -y -l $pkg` / `yum -y $pkg`), so the gated command
  ran an invalid no-op and prepared nothing. They now run `zypper -n in …` /
  `yum -y install …`, matching the non-`--installed` path (just limited to
  packages already present). The slmicro template was already correct.
- `downgrade` (the rollback after a failed `update`, and the standalone command)
  no longer crashes with `KeyError(<hostname>)` when one host in a multi-host
  group reports no downgradable versions (package not installed, or the list
  command produced no output): that host is now simply skipped for the package
  instead of aborting the whole downgrade.
- `downgrade` no longer leaves every host locked when a host has no downgrader
  for its product. The `MissingDowngraderError` early-return happened after the
  group was locked but outside the `try/finally` that unlocks it; the doer lookup
  now runs before locking (matching `prepare`), so the early return cannot strand
  the locks.
- The post-`prepare` and post-`downgrade` zypper checks now report a detected
  package-manager lock correctly. The `UpdateError` raised on "ZYpp transaction
  already in progress" had its `(reason, host)` arguments swapped (so `.host`
  became the literal reason and the message read backwards), and the matching
  `downgrade` log calls were malformed: one passed **no** arguments for its four
  `%s` placeholders, the other passed one too many. The exception arguments and
  the log-call arities are now correct (matching the `update` check), so a lock is
  attributed to the right host and the diagnostic prints real values.
- `mtui-mcp` no longer corrupts the message for `commit` and the comment for
  `lock` when given more than one word. Both options are `append` + `REMAINDER`,
  and the kwargs→argv encoder emitted the flag once per token (`-m a -m b`); with
  `REMAINDER` the second flag is swallowed as a value, so the committed message
  became e.g. `"a -m b"`. The encoder now emits an `append`+remainder/multi-`nargs`
  flag once followed by all its tokens, so the message/comment round-trips intact.
- `RPMVersion` no longer raises `ValueError: too many values to unpack` when a
  version string contains more than one dash. The version/release split now uses
  the last dash (`rsplit("-", 1)`); the release field never contains a dash, but
  the version field can (e.g. a Debian-style `upstream-debrev` arriving through
  the dpkg querier).
- Hashing a `UserMessage`/`UserError` (e.g. putting one in a `set` or using it
  as a dict key) no longer raises `RecursionError`. `__hash__` returned
  `hash(self)`, which called itself forever; it now hashes `str(self)`, matching
  the existing `__eq__`.
- `HostLog.append`/`insert` now raise the intended `ValueError` ("it need 5
  args, got N") when given the wrong number of positional arguments, instead of
  a confusing `TypeError` from `len(*args)` (which unpacked the args into
  `len()`).
- `export` now deduplicates install-log links against the correct part of the
  template. The loop meant to find the `HAS_UNTRACKED` marker had its `o += 1`
  outside the loop body, so the index was always `1` and the marker search was
  dead; a URL appearing anywhere earlier in the template could wrongly suppress
  adding the real link. The dedup is now scoped to the lines after the marker.
- The kernel openQA result matrix now annotates a failed `ltp_` test's
  `result: failed` line as intended. The annotation used `text.replace(...)`
  without assigning the result back (strings are immutable), so it was a no-op.
- Acquiring an update lock no longer crashes with `ValueError` when another
  session's lockfile has a malformed or empty timestamp. `update_lock` reports a
  foreign lock via `TargetLock.time()`, which did `float(timestamp)` unguarded; a
  bad value aborted the whole lock-acquisition walk (blocking `update`/`prepare`/
  `downgrade`). `time()` now returns `"unknown"` on a bad timestamp, mirroring the
  already-hardened `age_seconds`.
- The QEM-dashboard openQA accounting no longer crashes with `TypeError` when a
  dashboard job has no `test`/`name`. `_has_passed_install_jobs` and
  `_get_logs_url` tested `"qam-incidentinstall" in job.get("test")`, which raised
  on a `None` value (a normalized job whose `name` was absent); they now use
  `job.get("test", "")`, matching the defensive pattern already used elsewhere
  in the connector, so one odd job no longer aborts the whole install-result
  summary during `export`.
- `get` now downloads only from **enabled** hosts, matching `put` and its own
  docstring. It previously contacted every connected host, including ones the
  tester had deliberately disabled (e.g. a refhost parked during a batch).
- Connecting to an unreachable host now reports the clean "connecting to <host>
  failed: <reason>" message instead of a confusing downstream crash. A network
  `OSError` on the initial connect was logged but swallowed, so `Connection`
  returned a dead transport and `Target.connect` then blew up in
  `is_locked()`/`parse_system()` with an opaque error. The initial connect now
  re-raises the `OSError` so the `ConnectingTargetFailedMessage` handler runs; the
  reconnect path (host rebooting) still swallows it and relies on its own
  `is_active()` give-up check.
- Parsing a testreport's JSON metadata no longer raises `TypeError` when the
  `jira`, `bugs`, or `packages` keys are absent or null. `JSONParser.parse`
  iterated `data.get("jira")` / `get("bugs")` / `get("packages").items()` with no
  default, so a partial/malformed metadata blob crashed the parse; these now fall
  back to empty, matching the existing handling of `repositories`.
- `atomic_write_file` no longer leaves its temporary file behind in the
  destination directory when the write or the final `move` fails. The `mkstemp`
  temp file is now unlinked on any error before the exception propagates.
- `mtui-mcp` now advertises `readOnlyHint=True` for the `openqa_jobs` tool (it
  only queries openQA) and drops a stale `"products"` entry from the read-only
  allow-list (no such command exists; it is `list_products`, already covered by
  the `list_` prefix). Corrects the advisory hint shown to MCP clients; no
  behavioural change to the commands themselves.
- A failed `update` no longer strips the test update repositories from the
  affected hosts. The repo cleanup used to run unconditionally (in a `finally`),
  so a host whose update failed was left with no issue repo; retrying or
  diagnosing it (`zypper patches`) then saw nothing and the repo had to be
  re-added by hand. The repos are now removed only on a successful update; on
  failure they are kept (with a WARNING) for retry/diagnosis and removed by the
  next successful update or an explicit `set_repo --remove`. The hosts are still
  unlocked on failure as before.
- `mtui-mcp` no longer floods its log with a repeated
  `Warning: InsecureRequestWarning: Unverified HTTPS request ...` line (one per
  openQA (or other internal-host) request) when TLS verification is disabled
  via `ssl_verify = false`. The MCP SDK records and re-emits warnings raised
  while handling each request, which defeated the per-request suppression; the
  warning is now silenced once at server start-up (only when verification is
  off), so a genuine certificate problem is still reported when verification is
  on.
- `run` now closes the write half of the SSH channel after dispatching the
  command, sending EOF to the remote command's stdin. A command that reads input
  (an interactive prompt, `read`, `cat` with no redirect) previously blocked
  forever waiting for input that never came; it now receives EOF and
  proceeds/aborts instead of hanging the session.
- A command run over a non-interactive session (`mtui-mcp`) that produces no
  output for the whole `connection_timeout` window (default 300s) now aborts
  with a command-timeout instead of looping forever. There is no human to ask
  "keep waiting?" in that context, so a silent/stuck command previously wedged
  the call until the session was killed. Interactive sessions are unchanged
  (they still prompt, or silently wait when no prompter is wired); raise
  `connection_timeout` for legitimately long, fully silent commands.
- `set_repo` no longer returns silent success when it does nothing. If none of
  the update's products match a host's installed products (e.g. the host's parsed
  products drifted from what the update targets), no repo was ever registered;
  this now logs a `WARNING` naming the host and the mismatched product sets
  instead of appearing to succeed. A failed `zypper ar` (non-zero exit) is also
  surfaced as a `WARNING` rather than ignored.

## 18.1.0 - 2026-06-19

### Added

- `smelt_updates` gains `--unassigned` and `--show-assignment` (with `--group`,
  default `qam-sle`) to surface each SLFO update's current assignee, read from the
  PR's mtui assign/unassign comments via Gitea. The lookup is lazy and
  highest-priority-first, so `smelt_updates --pending qam-sle-review --unassigned
  --limit 1` returns the top unassigned update in a few calls; without the flags
  the listing stays a single SMELT call.

### Fixed

- Running under MCP (`mtui-mcp`) no longer hangs when a refhost's SSH key
  authentication fails. Previously the connect path fell back to an interactive
  root-password prompt, which has no TTY in MCP mode (stdin is the JSON-RPC pipe)
  and blocked the session indefinitely. The connect path now fails fast with a
  single actionable WARNING naming the fix (set up working SSH key auth, verify
  with `ssh root@<host>`); the affected host is reported as unreachable instead
  of stalling the whole client.
- A failed Gitea API call caused by TLS certificate verification (common when
  the SUSE root CA is not in the system trust store) now logs a single,
  actionable message naming the two remedies (install the SUSE CA or set
  `ssl_verify = false`, or a CA-bundle path, under `[mtui]`) instead of dumping
  a multi-frame `SSLCertVerificationError` traceback. The full traceback is
  still available at debug level.
- The HTTPS refhosts resolver no longer fails silently when its on-disk cache
  directory (e.g. `~/.cache/mtui`) does not exist: the cache write now creates
  the destination directory. Previously the download succeeded but persisting it
  raised a `FileNotFoundError` that the resolver chain swallowed, making the
  fetch appear to fail regardless of the `ssl_verify` setting.
- A failing refhosts resolver now logs the real reason for the failure (e.g. the
  underlying connection, file, or TLS error) at WARNING instead of only a
  generic "resolver X failed" line, so refhosts download problems are
  diagnosable without enabling debug logging.
- A `~`-prefixed `[refhosts] path` (and `[mtui] install_logs`, `[target]
  tempdir`) is now expanded to the user's home directory instead of being used
  as a literal relative path, so a home-relative `refhosts.yml` location loads
  correctly.
- Setting `[mtui] location` no longer breaks refhosts resolution during config
  parsing with `'Config' object has no attribute 'ssl_verify'`. The `location`
  option is now parsed after the `ssl_verify` and `refhosts_*` options it
  depends on, so the validation resolve triggered by setting a location reads a
  fully-populated config.
- A failed `update` no longer crashes with `KeyError(<hostname>)` during its
  automatic rollback. The downgrade builds a command only for hosts with a
  recorded previous version, but `RunCommand` ran it against the whole group;
  it now acts only on the hosts a per-host command dict actually covers.
- A failed `update` now surfaces the original `UpdateError` (e.g. a dependency
  error) to the caller even when the rollback itself raises; previously a
  rollback error masked the real reason the update failed, and a clean rollback
  silently swallowed it.
- The SSH connection setup now honours `connection_timeout` for the TCP
  connect, SSH banner, and authentication (previously only remote command
  execution was bounded, so a dead/firewalled refhost stalled on the OS TCP
  timeout, making a bulk `add_host` appear to hang for minutes).
- `connection_timeout` is now read from the `[connection]` section (falling
  back to the legacy `[mtui]` section), matching where it is documented to live.

### Removed

- Removed the `template.smelt_threshold` config option. It was parsed and
  documented but never consumed by any command (intended to limit smelt-checkers
  output in the template, never wired up).

## [18.0.1] - 2026-06-18

### Added

- New SMELT query commands (auto-exposed over MCP as `mcp__mtui__smelt_*`):
  `smelt_update` (the loaded update's priority/deadline/status/…; SLFO via REST,
  Maintenance via GraphQL), `smelt_checkers` (checker/build-check result runs for
  the loaded SLFO update), and `smelt_updates` (enumerate the SLFO update queue
  with `--status` / `--review-group` / `--pending` filters, e.g. the testing
  updates still pending `qam-sle-review`), and `smelt_requests` (the classic
  Maintenance review-request queue, e.g. pending `qam-sle`). Require `[smelt] url`.
- `assign` now prints the update's **priority and deadline** from SMELT when
  picking it up, so the tester sees the urgency. Read-only and best-effort: it is
  silent unless the SMELT base URL is configured via `[smelt] url` and SMELT has
  data for the request. Backed by the new `mtui.data_sources.Smelt` connector
  (SMELT REST v2 for SLFO updates, GraphQL for classic Maintenance incidents).
- New `openqa_jobs` command (auto-exposed over MCP as `mcp__mtui__openqa_jobs`)
  lists the **individual** openQA jobs for the loaded update's incident build
  (scenario, arch, result and job URL), so testers can see *which* jobs failed and
  judge whether a failure relates to the package under test, rather than only the
  per-version summary `openqa_overview` gives. `obsoleted` (superseded) jobs are
  dropped by default; `--all` keeps them, `--failed` shows only non-passing jobs,
  `--arch` filters by architecture.
- `list_bugs` now resolves the bug and Jira *titles* from the checkout's
  `patchinfo.xml` instead of showing "Description not available" for
  updates whose JSON metadata carries only ids (the git/SLFO and PI
  workflows). A missing or malformed `patchinfo.xml` is ignored.
- New `mtui-mcp` console script (optional `mcp` extra) ships a
  Model Context Protocol server, built on the official `mcp` Python
  SDK, that exposes every non-interactive mtui command as an MCP
  tool, plus dedicated testreport tools: `testreport_read` /
  `testreport_patch` / `testreport_write` to edit the report, and
  `testreport_logs` / `testreport_read_file` to inspect the rest of the
  checkout (the `build_checks/` and `install_logs/` files, `source.diff`,
  `patchinfo.xml`) that the `log` file does not cover, so LLM clients
  can drive a headless mtui session over `stdio` or `http`. The `mcp` extra installs
  `mcp[cli]>=1.2`; on openSUSE the SDK is packaged as
  `python3-mcp`. See `Documentation/mcp.rst` for the deny-list,
  per-client isolation model, and a `read → patch → read` worked
  example.
- `mtui-mcp`'s `http` transport now isolates session state **per
  client**: each connected client gets its own loaded test report and
  set of SSH hosts, keyed on the MCP session, so concurrent clients no
  longer share (or clobber) one another's `load_template` / `add_host`
  state and run without cross-session serialisation. The number of
  concurrent sessions is bounded by `[mcp] session_cap` (default 32)
  and idle sessions are reaped after `[mcp] session_idle_timeout`
  seconds (default 1800), disconnecting their hosts. `stdio` is
  unchanged (one process, one session).

### Changed

- Outbound HTTP calls (Gitea PR client, QEM Dashboard client, openQA /
  QAM Dashboard search, and the openQA job client) now share a single
  source of truth for the `(connect, read)` timeout and
  TLS-certificate-verification policy in `mtui.support.http`, replacing
  several independent, inconsistent copies. A new `[mtui] ssl_verify`
  option controls verification globally and **defaults to `true`**, so
  MTUI now verifies TLS certificates on every outbound connection out
  of the box. Reaching internal hosts that present an internal-CA
  certificate therefore requires the SUSE CA in the system trust store;
  set `ssl_verify = false` to disable verification everywhere, or point
  at a CA bundle with `ssl_verify = /path/to/ca.pem`. The Gitea client
  now also applies the shared request timeout (it previously had none).
- **Security:** previously the Gitea client and the openQA / QAM
  Dashboard search disabled certificate verification unconditionally,
  the QEM Dashboard client used the bare `requests` default, and the
  install-log export silently retried unverified after a TLS error.
  Every one of these now honors `[mtui] ssl_verify` and verifies by
  default; the install-log export no longer falls back to an unverified
  connection.
- The remaining raw-`urllib` download paths now route through
  `mtui.support.http` too: the openQA install-log export, the
  result/install-log downloader, and the `refhosts.yml` fetch all use
  the shared `(connect, read)` timeout and honor `[mtui] ssl_verify`
  (verify-on by default).
- MTUI now requires Python 3.13 or newer (previously 3.11). The
  minimum supported interpreter, the packaging classifiers, and the
  `ruff`/`ty` configuration were all raised to 3.13, and the
  `typing-extensions` backport dependency was dropped now that
  `typing.override` is available in the standard library.
- `mtui-mcp` no longer accepts boot-time test-report or host flags
  (`-a`/`--auto-review-id`, `-k`/`--kernel-review-id`, `-s`/`--sut`);
  passing them now errors as unknown arguments. A single boot-time
  seed cannot belong to any one client under the new per-client
  isolation, so each client loads its own state at runtime via the
  `load_template` and `add_host` tools instead. (The REPL `mtui`
  keeps these flags.)

### Removed

- Removed the interactive `report-bug` command, which opened a
  pre-populated bugzilla form via `xdg-open`, along with its
  `[mtui] report_bug_url` configuration option. Report bugs on
  MTUI's GitHub (https://github.com/openSUSE/mtui) or via the
  project tracker at https://progress.opensuse.org. Any
  `report_bug_url` key left in a config file is now ignored.

### Fixed

- `openqa_overview` now matches flavored Python binary packages to their
  source-named `build_checks` logs. The package list carries names like
  `python313-ecdsa`, but the build_checks index names logs after the
  source package (`python-ecdsa`), so a plain substring match dropped
  them; the `pythonNNN-` flavor prefix is now normalized before matching.
- `openqa_overview` now builds the correct QAM `build_checks` URL for
  SLFO updates. It previously used the openQA Dashboard
  `effective_incident_id` (the request id) when constructing the
  `qam.suse.de/testreports/...` URL, yielding e.g.
  `SUSE:SLFO:5348:5348` and a 404 instead of `SUSE:SLFO:1.2:5348`.
  The actual `maintenance_id` is now passed to `build_checks()`.
- `mtui-mcp` no longer paints the interactive `|/-\` spinner to
  stderr during long-running parallel actions (`run`, `set_repo`,
  `sftp_*`). The spinner is a REPL-only progress channel; over MCP
  the proper signal is `notifications/progress`. The REPL keeps its
  spinner unchanged.
- `mtui-mcp` now emits MCP `notifications/progress` every 10 seconds
  while a tool call is running, so spec-compliant MCP clients
  (Inspector, Claude Desktop, opencode, …) no longer time out on
  long-running commands such as `run`, `update`, `set_repo`, `commit`,
  slow `add_host`, or `load_template`. The heartbeat is automatic and
  applies to every auto-generated tool plus the three testreport
  tools; clients that ignore progress notifications can raise their
  own per-server timeout (see `Documentation/mcp.rst`).
- `run`, `show_log`, and `show_diff` now return their output when
  invoked through `mtui-mcp`. The three commands routed results through
  the interactive pager (`page()`), which early-returned in
  non-interactive mode and left the captured stdout buffer empty;
  MCP clients received an empty response instead of the per-host
  command output, log lines, or source diff. The pager now forwards
  each line to the caller's display sink in non-interactive mode while
  the REPL pager behaviour is unchanged. A latent
  `UnboundLocalError` in `run` when target locking failed is also
  fixed.
- `lrun` now captures child stdout/stderr and propagates the real exit
  code when invoked through `mtui-mcp` (or any non-interactive prompt).
  Previously a failing local command surfaced to the MCP client as
  `exit_code=1` with no output; the child's streams went to the server's
  TTY and `CalledProcessError.returncode` was discarded. The interactive
  REPL path is unchanged; output still streams live to the terminal.
- `openqa_overview` now shows build check logs for all packages in a
  multi-package update, not just one. Previously only logs matching a
  single package name extracted from the build string were displayed,
  causing build check results for other packages in the update to be
  silently omitted.
- `mtui-mcp` now exposes the full surface of `set_repo` and
  `load_template`. The previous schema-synthesis collapsed each
  command's mutually exclusive flag pair onto a single MCP parameter,
  silently hiding the `--remove` operation of `set_repo` and the
  `--kernel-review-id` path of `load_template` from clients (visible at
  boot as `duplicate dest` warnings). `set_repo` now takes a required
  `operation` enum (`add`/`remove`); `load_template` now takes two
  optional strings `auto_review_id` / `kernel_review_id` with an
  "exactly one required" runtime check.
- `mtui-mcp` now preserves real argparse defaults for optional
  list-shaped arguments. Previously the schema layer replaced every
  optional list default with `[]`, so invoking `openqa_overview`
  without `aggregated_groups` emitted a bare `--aggregated-groups`
  flag and crashed with *"expected at least one argument"*. Non-empty
  argparse defaults (like `["core"]`) now flow through to the MCP
  schema and match the REPL behaviour.
- `mtui-mcp` single-token positional commands now accept a scalar
  argument instead of a 1-element array. Affected tools: `set_location`,
  `set_log_level`, `set_timeout`, `set_host_state`, `put`, `get`.
  Previously the schema demanded e.g. `{"site": ["prague"]}` because
  the synthesis layer treated argparse `nargs=1` like `nargs="+"`,
  causing clients that sent `{"site": "prague"}` to fail with
  `Input should be a valid list`. The MCP schema now exposes these
  arguments as plain scalars while the underlying argparse layer is
  unchanged.
- `config show` with no attributes no longer crashes with
  `TypeError: 'ConfigOption' object is not subscriptable`. The handler
  was still indexing config entries as tuples after they were
  refactored into a `ConfigOption` dataclass, so any caller (REPL or
  `mtui-mcp`'s `config_show` tool) that omitted attribute names hit
  the error instead of getting the sorted option list.
- `mtui-mcp` now shuts down cleanly on Ctrl-C. The previous handler
  caught a bare `KeyboardInterrupt`, but the MCP server runs under
  `anyio.run`, which on Python 3.11+ wraps a Ctrl-C delivered
  to an active task group inside a `BaseExceptionGroup`. The group
  slipped past the handler and the user saw a multi-frame traceback
  instead of a graceful exit. The server now also catches
  `BaseExceptionGroup` instances whose leaves are all shutdown
  sentinels (`KeyboardInterrupt`, `SystemExit`,
  `asyncio.CancelledError`), logs a single `mtui-mcp: shutting down`
  line, and exits 0. Ctrl-C pressed during pre-server preload
  (`-a` / `-k` testreport load) or autoconnect (`-s` SUT loop)
  is handled the same way. Groups containing a real error still hit
  the existing crash path (`mtui-mcp crashed`, exit 1).

### Changed

- Non-interactive calls to `prompt_user` now return the ``default``
  argument instead of always returning ``False``. Callers that pass
  ``default=True`` (``load_template``, which overwrites an already-loaded
  session; ``updateid.py``, which deletes a checked-out template) now
  auto-confirm in non-interactive mode (MCP, scripts). All other
  callers pass no ``default`` and are unaffected. The docstring for
  ``prompt_user`` was updated to document this contract.
