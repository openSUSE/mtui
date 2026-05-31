# Changelog

All notable user-visible changes to MTUI are documented in this file.

The format is based on [Keep a Changelog 1.1.0](https://keepachangelog.com/en/1.1.0/),
and this project follows [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- The `approve` command now accepts `-r REVIEWER` / `--reviewer REVIEWER`.
  It records the reviewer in the testreport's `Test Plan Reviewer:` line,
  commits the testreport to SVN, and then approves the update — all in one
  step. If the testreport has no `Test Plan Reviewer:` line or the SVN
  commit fails, the approval is aborted and nothing is approved. Running
  `approve` without `-r` behaves exactly as before.
- Reference hosts are now automatically locked while testing a Product
  Increment (PI). Running `assign` on a PI locks every connected
  reference host with the comment `testing of SUSE:PI:<id>:<review>`
  (an exclusive lock that blocks other sessions); hosts added via
  `add_host` while the assignment is active are locked too. The locks
  are released at the end of testing — `unassign`, `approve`, or
  `reject` — and only this session's locks are removed (a host locked by
  someone else is left alone). Controlled by a new `[lock] pi_autolock`
  option (default `true`); set it to `false` to disable.
- Stale reference-host locks are now reaped automatically on connect.
  When `add_host` (or any connect) finds a pre-existing `/var/lock/mtui.lock`
  older than a configurable threshold, mtui force-removes it regardless of
  owner — including exclusive (commented) locks — since such a lock is
  almost always left over from a crashed or abandoned session. The removal
  is logged at `WARNING` (`<host>: removing stale lock held by <user>
  (<N> h old)`). Controlled by two new `[lock]` config options:
  `reap_stale` (default `true`) and `stale_age` in seconds (default
  `86400`, i.e. one day); set `reap_stale = false` or `stale_age = 0` to
  restore the previous behaviour of only warning. Fresh locks are still
  only reported, never removed.

### Tests

- Phase 6 test backfill: total suite grew from 697 to 952 tests
  (+255, +37%); total branch coverage rose from 71% to 82%
  (+11 pp). New `tests/test_cmd_<name>.py` files cover one happy
  and one error path for every interactive command in
  `mtui/commands/*` that previously lacked focused tests.
  Coverage was also brought up across `mtui/actions/*`,
  `mtui/checks/*` (all four modules now at 100%),
  `mtui/template/*` (testreport from 34% to 72%; the four
  subclass reports plus the products normalisers all sit at
  100%), `mtui/export/{downloader,kernel}.py`, and the
  previously-uncovered SFTP, reconnect, host-key and channel
  paths of `mtui/connection.py` (85% → 96%).
- The seven `test_target_init_*` cases were consolidated into a
  single `@pytest.mark.parametrize`-driven `test_target_init`;
  the seven test IDs are preserved so `pytest -k init` still
  surfaces them individually (Phase 6 / D3).
- The 0-byte `tests/fixtures/mtuirc` was replaced with a
  realistic six-section INI fixture and a dedicated
  `tests/test_config.py` test round-trips it through
  `mtui.config.Config` across str/int/bool value types
  (Phase 6 / D5).

### Removed

- Three unreferenced JSON fixtures (`tests/fixtures/inc_12358*.json`,
  ~1.1 MB total) were deleted after a repo-wide reference sweep
  confirmed no code, test, or doc loaded them (Phase 6 / D4).

### CI

- Codecov project floor ratcheted from 66% to 81% to reflect the
  Phase 6 coverage gains; the `coverage.range` was widened to
  `81..95` to match. Patch target is unchanged at 80%.

### Fixed

- `set_location` now resets the working reference-host selection. Hosts
  inherited from the testreport template and from the previously
  configured location are dropped, so a subsequent `add_host` connects
  only hosts from the newly selected location (the per-testplatform
  fallback to the `default` location is unchanged). Previously the
  template/old-location hosts lingered in the accumulated host set and
  `add_host` connected them alongside the new location's hosts.
- Parallel target operations no longer race for `stdin` when an SSH
  command times out on multiple hosts simultaneously. The timeout
  prompt (`command "..." timed out on <host>. wait? (Y/n)`) is now
  serialised through a single `mtui.prompter.Prompter` constructed in
  `main()` and threaded down through `CommandPrompt` / `TestReport` /
  `Target` to `Connection`. Previously the prompt was raised from the
  SSH worker thread via a bare `input()` call; with several hosts in
  flight two workers could attempt to read the same line of input and
  prompt text was interleaved with sibling stdout writes. Library
  callers that build `Connection` directly without wiring a prompter
  now silently wait on timeout (matching the historical Enter / Y
  default) and emit one `WARNING` log line so the silence is
  observable (Phase 5b / C6).
- `Config` now logs an error and falls back to the documented default when
  an INI value fails to parse. Previously `connection_timeout = abc`
  crashed startup with an uncaught `ValueError`, and malformed typed
  options like `refhosts.https_expiration = xyz` or
  `mtui.use_keyring = perhaps` were silently replaced with the default
  because the intended diagnostic log call was itself broken
  (Phase 5b / C10).
- Malformed host rows in `refhosts.yml` are now logged at ERROR level and
  dropped; previously they were silently skipped at search time with no
  operator-visible signal (Phase 5b / C11).
- Loading an SLFO or PI update with no `GITEA_TOKEN` configured no longer
  crashes with an uncaught traceback. Missing tokens are now reported with
  a clear configuration hint and exit with a non-zero status. Transient
  Gitea API failures and hash mismatches raised during the post-checkout
  template retry now reach the same handlers (`TestReportNotLoadedError`
  and the force-continue prompt) as failures from the initial read.
- `GiteaError` now derives from `Exception` instead of `BaseException`,
  so the interactive command loop's catch-all handler traps Gitea errors
  cleanly instead of letting them tear down the prompt.

### Changed

- Internal refactor: `Target` was decomposed into four focused
  collaborators. Out-of-tree consumers that imported
  `mtui.target.Target` lose the following methods, all moved to
  collaborator properties on the same instance:
  - `target.get_installer()` / `get_uninstaller()` / `get_downgrader()`
    / `get_updater()` / `get_preparer()` and the five matching
    `..._check()` variants are now reached as
    `target.doer("installer")` / `target.check("installer")` etc.
    The preparer arm keeps its `(force, testing)` kwargs:
    `target.doer("preparer", force=True, testing=True)`.
  - `target.report_self()` / `report_history()` / `report_locks()` /
    `report_timeout()` / `report_sessions()` / `report_log()` /
    `report_products()` move to `target.reporter.self_()`,
    `target.reporter.history()`, etc.
  - `target.set_repo()` and `target.run_zypper()` move to
    `target.repo_manager.set()` and `target.repo_manager.run_zypper()`.
  - `target.query_package_versions()` is kept as a thin delegate but
    the rpm-vs-dpkg logic now lives in
    `target.package_querier`. All in-tree callers were migrated.
- Internal refactor: `mtui.refhost` was rewritten around a typed schema
  and a `Resolver` registry. Behaviour is unchanged for in-tree
  callers; out-of-tree consumers may need to migrate (Phase 5b / C11):
  - `Attributes` is now a `@dataclass` with typed fields (`arch: str`,
    `product: Product | None`, `addons: list[Addon]`). The historical
    free-form `setattr` shape and the undocumented `tags=(name)` /
    `<other>=name(...)` segments in `testplatform` strings are no
    longer parsed; unknown segments log an ERROR and are skipped. SMELT
    never emitted those forms, and no field in `refhosts-ng.yml`
    matched them, so this prunes dead grammar.
  - `Refhosts.data` is now `dict[str, list[Host]]` (was
    `dict[str, list[dict]]`). The `Host` dataclass and the matcher work
    against typed fields; `is_candidate_match` no longer iterates
    `vars(attribute)`.
  - `_RefhostsFactory` now takes a `dict[str, Resolver]` registry
    instead of five positional collaborators. Use
    `_RefhostsFactory({"https": HttpsResolver(...), "path": PathResolver()})`.
    The `getattr(self, f"resolve_{name}")` reflection is gone; the
    `resolve_https` / `resolve_path` methods and the cache-refresh
    helpers (`_is_https_cache_refresh_needed`,
    `refresh_https_cache_if_needed`, `refresh_https_cache`) move to
    `HttpsResolver` as private methods.
- Internal refactor: the grab-bag `mtui/utils.py` is split into five
  topical modules and the file itself is deleted. Out-of-tree consumers
  that imported from `mtui.utils` must update each import per the map
  below (Phase 5b / C8):
  - `green`, `red`, `yellow`, `blue` → `mtui.colors`.
  - `complete_choices`, `complete_choices_filelist` → `mtui.completion`.
  - `chdir`, `ensure_dir_exists`, `atomic_write_file`, `timestamp` →
    `mtui.fileops`.
  - `termsize`, `filter_ansi`, `prompt_user`, `page` → `mtui.term`.
  - `DictWithInjections`, `SUTParse`, `requires_update` → `mtui.misc`.

