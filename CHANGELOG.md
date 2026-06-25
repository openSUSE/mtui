# Changelog

All notable user-visible changes to MTUI are documented in this file.

The format is based on [Keep a Changelog 1.1.0](https://keepachangelog.com/en/1.1.0/),
and this project follows [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]:

### Added

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

### Removed

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
