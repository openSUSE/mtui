# Changelog

All notable user-visible changes to MTUI are documented in this file.

The format is based on [Keep a Changelog 1.1.0](https://keepachangelog.com/en/1.1.0/),
and this project follows [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- Connecting a reference host now verifies that its installed products match
  what `refhosts.yml` records for that host. On any drift — wrong or
  wrong-version base product, wrong architecture, addons that are missing,
  unexpected, or at a different version, or a dangling
  `/etc/products.d/baseproduct` symlink — mtui logs a `WARNING` per drift class
  and keeps the host (the check never aborts a connect). `qa` is ignored on both
  sides to match the products mtui already skips. This catches validating an
  update on a host that is not the system its metadata claims; hosts absent from
  `refhosts.yml` are skipped silently.
- Under `mtui-mcp`, a command's own `mtui` log records (`INFO` and above) emitted
  while it runs are now included in the tool reply, not just what it prints to
  stdout. The capture follows the command into the worker threads it fans out to
  (MTUI's thread pools now propagate context), so warnings logged off the main
  thread — such as the per-host product-drift report above, emitted on
  `add_host`'s connect pool — reach MCP clients directly; `add_host` no longer
  re-echoes them to stdout.

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
  `getpass` root-password prompt, which has no TTY in MCP mode (stdin is the
  JSON-RPC pipe) and blocked the session indefinitely. Non-interactive sessions
  now skip the prompt and fail fast with a single actionable WARNING naming the
  fix (set up working SSH key auth, verify with `ssh root@<host>`); the affected
  host is reported as unreachable instead of stalling the whole client.
- A failed Gitea API call caused by TLS certificate verification (common when
  the SUSE root CA is not in the system trust store) now logs a single,
  actionable message naming the two remedies — install the SUSE CA or set
  `ssl_verify = false` (or a CA-bundle path) under `[mtui]` — instead of dumping
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
  error) to the caller even when the rollback itself raises — previously a
  rollback error masked the real reason the update failed, and a clean rollback
  silently swallowed it.
- The SSH connection setup now honours `connection_timeout` for the TCP
  connect, SSH banner, and authentication (previously only remote command
  execution was bounded, so a dead/firewalled refhost stalled on the OS TCP
  timeout — making a bulk `add_host` appear to hang for minutes).
- `connection_timeout` is now read from the `[connection]` section (falling
  back to the legacy `[mtui]` section), matching where it is documented to live.

### Removed

- Removed the `template.smelt_threshold` config option. It was parsed and
  documented but never consumed by any command (intended to limit smelt-checkers
  output in the template, never wired up).

## [18.0.1] - 2026-06-18

### Added

- New SMELT query commands (auto-exposed over MCP as `mcp__mtui__smelt_*`):
  `smelt_update` (the loaded update's priority/deadline/status/… — SLFO via REST,
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
  lists the **individual** openQA jobs for the loaded update's incident build —
  scenario, arch, result and job URL — so testers can see *which* jobs failed and
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
  tool, plus dedicated testreport tools — `testreport_read` /
  `testreport_patch` / `testreport_write` to edit the report, and
  `testreport_logs` / `testreport_read_file` to inspect the rest of the
  checkout (the `build_checks/` and `install_logs/` files, `source.diff`,
  `patchinfo.xml`) that the `log` file does not cover — so LLM clients
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
  non-interactive mode and left the captured stdout buffer empty —
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
  REPL path is unchanged — output still streams live to the terminal.
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
  ``default=True`` (``load_template`` — overwrites an already-loaded
  session; ``updateid.py`` — deletes a checked-out template) now
  auto-confirm in non-interactive mode (MCP, scripts). All other
  callers pass no ``default`` and are unaffected. The docstring for
  ``prompt_user`` was updated to document this contract.
