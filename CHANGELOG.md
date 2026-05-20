# Changelog

All notable user-visible changes to MTUI are documented in this file.

The format is based on [Keep a Changelog 1.1.0](https://keepachangelog.com/en/1.1.0/),
and this project follows [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- `--color {auto,always,never}` flag and `NO_COLOR` environment variable
  support for both ANSI log output and colorised display helpers; `auto`
  (the new default) disables colour when stderr is not a TTY.
- `-V` / `--version` now also reports the resolved Python, paramiko and
  openqa-client versions in addition to the mtui version.
- Optional `completion` extra (`pip install 'mtui[completion]'`) enabling
  `argcomplete`-based shell completion. Activate per shell with
  `eval "$(register-python-argcomplete mtui)"`.
- `CHANGELOG.md`, `CONTRIBUTING.md`, `CODE_OF_CONDUCT.md`, `SECURITY.md`,
  GitHub issue templates and a pull-request template.

### Changed
- `export` now reports `UNKNOWN` (previously `FAILED`) on the
  "Installation tests done in openQA with following results:" line when
  the openQA install results have not been fetched or have not run yet,
  so a missing fetch is no longer indistinguishable from a real failure.
- `export` further condenses the openQA results section for incidents
  loaded from the QEM Dashboard: zero counters are dropped from Summary
  rows, the shared aggregate `BUILD` is hoisted into one header line,
  all-passed groups are folded across architectures into a single row,
  problem groups are sorted to the top, and the `Failed jobs:` subsection
  is now nested under each group with the redundant
  product/build/arch prefix removed and openQA URLs aligned. Cuts the
  openQA section roughly to a third of its previous size.
- `export` now condenses the openQA results section for incidents loaded
  from the QEM Dashboard: passed jobs are collapsed into per-group counts
  (version/flavor/arch for incident jobs, product/build/arch for aggregate
  jobs), and only jobs whose result is `failed`, `incomplete` or
  `timeout_exceeded` are listed individually under a `Failed jobs:`
  subsection with a direct openQA URL. This shrinks the exported log from
  hundreds of lines to a reviewable summary while keeping focus on
  failures.
- Documentation source switched from `README.rst` to `README.md` via
  `myst-parser`; `README.rst` has been removed.
- Sphinx HTML theme switched to the built-in `alabaster`; copyright
  refreshed; obsolete `.templates` / `.static` paths cleaned up.
- `Documentation/installation.rst` now covers source builds with `uv` /
  `pip`, optional extras, and the system-package requirement for the
  `osc` CLI.
- `Documentation/developer.rst` rewritten to cover the modern dev
  workflow (`uv`, `ruff`, `ty`, `pytest`, `pre-commit`) and a walk-through
  for adding a new command.
- `Documentation/cfg.rst` documents the `ssh_strict_host_key_checking`
  option introduced in the Phase 2 release.
- Project test coverage rose from 58 % to 67 % after a public-API safety
  net was added for `mtui/config.py`, `mtui/prompt.py`, `mtui/refhost.py`,
  `mtui/target/target.py`, `mtui/target/hostgroup.py` and
  `mtui/connection.py`. The Codecov project floor is bumped from 56 % to
  66 % (current − 1, ratchets upward); patch target stays at 80 %.
- `mtui.commands` now exposes a `registry: dict[str, type[Command]]`
  populated automatically via `Command.__init_subclass__`. The legacy
  `cmd_list` attribute and the `globals()`-based per-class re-export have
  been removed; the only documented consumer was `CommandPrompt`, which
  now iterates `commands.registry.values()` directly.
- Multi-step SFTP operations now share a single SFTP session instead of
  opening a fresh channel for each step. `Connection.sftp_get_folder` and
  `Connection.sftp_rmdir` (1 listdir + N gets/removes + optional rmdir)
  drop from N+2 channel handshakes to 1, and target-system probing
  (`parsers/system.parse_system`, called once per target on `add_host`)
  drops from 6+ handshakes to 1. A new `Connection.sftp_session()`
  context manager exposes the same batching primitive to other callers
  that want to fan multiple reads/writes through one session.

### Fixed
- `Connection.sftp_put` no longer leaks SFTP clients when a transient
  paramiko transport error interrupts the `mkdir` walk. The old
  hand-rolled retry rebound the local `sftp` to a fresh client returned
  from `__sftp_reconnect`, but the surrounding `_sftp()` context manager
  only closes the client it originally captured; every reconnect (and
  the final working client used for the actual `put` / `chmod`) leaked.
  `sftp_put` now lets paramiko errors propagate per the documented
  `_sftp()` contract, so the single client opened for the call is
  always closed exactly once.
- Exceptions raised inside parallel target operations
  (`Target.set_repo`, `Target.run`, the SFTP helpers used by `put` /
  `get` / `remove`) now surface to the caller and abort the enclosing
  `update` / `prepare` / `downgrade` / `install` / `uninstall` flow,
  instead of being silently lost in a worker thread while the flow
  pretended to succeed.
- `update` now removes the test update repositories from every SUT after
  the update finishes (and even when the update or its post/compare
  scripts raise), instead of leaving them configured behind.
- QEM Dashboard HTTP requests now carry a (5 s connect, 30 s read)
  timeout, and the parallel per-setting jobs fan-out is bounded by a 60 s
  per-future wall-clock cap. A stuck connection or unresponsive endpoint
  no longer hangs `mtui` startup or `reloadoqa` indefinitely; a single
  timed-out setting is logged and skipped while the rest of the batch
  still completes.
- Pressing `Ctrl+C` during a parallel `run` now drops queued hosts that
  have not started yet and unblocks in-flight workers by closing their
  SSH sessions, instead of waiting for the whole parallel batch to drain
  before the interrupt was honoured. Workers already executing a remote
  command will still finish their current command (Python cannot
  interrupt a foreign blocking call), but session shutdown unblocks them
  promptly.
- Long-running parallel operations (`run`, file uploads / downloads /
  deletes, `set_repo` fan-out used by `update`/`prepare`/`downgrade`)
  now show user-visible progress again via a `|/-\` spinner on stderr
  while work is in flight. The spinner is suppressed when stderr is
  not a TTY (log files and redirected output stay clean). Restores the
  "mtui is alive" feedback that was lost when the queue/spinner thread
  was retired.
- `put` command log message no longer contains an extra `i` character;
  fixed "uploaded /X to i/tmp/Y" → "uploaded /X to /tmp/Y".
- `Attributes.__str__` (refhost string representation) now formats addon
  versions consistently with product versions: only adds a dot separator
  when the minor version exists and is numeric. Previously addons always
  included a trailing dot ("ha 15."), while products omitted it ("sles 12").

### Known issues
- A non-numeric value for `connection_timeout` (e.g.
  `connection_timeout = abc` under `[mtui]`) crashes `Config.__init__`
  with `ValueError` instead of falling back to the documented default of
  300 seconds. Workaround: ensure the option is unset or numeric.
- A bad value for the typed config options handled by
  `configparser.getint` / `getboolean` (`refhosts.https_expiration`,
  `template.smelt_threshold`, `mtui.chdir_to_template_dir`,
  `mtui.use_keyring`) is silently replaced by the default with no log
  message, due to a malformed format string in the error path. The
  default behaviour is correct but the diagnostic is missing.
- `Connection.__sftp_open` has a dead defensive branch
  (`if "sftp" in locals() and isinstance(sftp, SFTPClient)`) in its
  exception handler: the named exceptions are raised by the very
  `client.open_sftp()` assignment that would bind `sftp`, so `sftp` is
  never in `locals()` when that branch runs. Harmless (the function
  correctly returns `None` immediately afterwards) but the cleanup it
  attempts can never fire.

### Fixed
- `Target.state` is now validated at construction; previously the
  Literal type annotation listed `serial`/`parallel` (which are not
  valid states, only execution modes) and omitted `dryrun` (which is).
  The C9 refactor introduces a `TargetState` enum that rejects invalid
  values with a clear `ValueError` instead of letting them sit in the
  field and silently misroute downstream branches.

## [Phase 3] — CI / tooling

### Added
- `uv.lock` committed; CI now installs via `uv sync --extra norpm --group dev`.
- Test matrix extended to Python 3.11, 3.12, 3.13 and 3.14 (all gating).
- Whole-repo lint scope: `ruff format --check .` and `ruff check .` now
  cover `mtui/`, `tests/` and `Documentation/`.
- Strict `ty` configuration with `error-on-warning` and per-package
  overrides promoting `mtui/types/**` and `mtui/connector/**` to
  `error all`.
- Opt-in `.pre-commit-config.yaml` running ruff format, ruff check and
  `ty check`.
- Dependabot configuration for `pip` and `github-actions`.

### Changed
- Codecov targets reset: project floor 56 % (current − 1, ratchets
  upward), patch coverage target 80 %.
- Mergify rules now reference the `main` branch instead of the obsolete
  `master`.
- `MANIFEST.in` correctly bundles `Documentation/` recursively in the
  sdist.

### Removed
- `requirements_ci.txt` and `requirements_dev.txt` (superseded by `uv`).
- Stale `osc` Python install requirement (it is invoked exclusively as a
  CLI subprocess).

## [Phase 2] — Latent bugs and test foundation

### Added
- `ssh_strict_host_key_checking` configuration option with three policies
  (`auto_add` default, `warn`, `reject`); unknown values warn and fall
  back to the default. Default behaviour is byte-identical to before.
- `python -m mtui` entry point (mirrors the `mtui` console script) and a
  CLI smoke test invoking `--help` and `-V`.
- Pytest configuration with registered `slow`, `integration` and
  `network` markers and `--strict-markers`.
- Warning log when an SSH port fails to parse and falls back to 22.

### Fixed
- `Target.__hash__` / `__eq__` contract: equality now matches the hash
  identity (`hostname`); regression tests cover set deduplication and
  `NotImplemented` delegation.
- Silent `except Exception: pass` blocks in `mtui/connection.py` and
  `mtui/target/locks.py` now log at debug level.
- Broad `except BaseException` clauses narrowed to the actual exception
  types they were hiding (`Empty`, `IndexError`, `Exception`); import
  failures in `mtui/commands/__init__.py` now log full tracebacks.

## [Phase 1] — Quick wins

### Added
- `comment` and `EOF` commands documented in `Documentation/iui.rst`.

### Changed
- `Command.parse_args` uses `shlex.split`, so arguments containing
  spaces are handled correctly.
- Custom `chdir` context manager replaced by `contextlib.chdir`.
- Stack-walking colour-log formatter no longer relies on a fixed frame
  depth, so module/function attribution remains correct across Python
  versions.
- Repaired `.gitignore`; added editor- and tooling-specific patterns
  (`.DS_Store`, `.idea/`, `.vscode/`, `*.profraw`, `.ruff_cache/`,
  `.ty_cache/`).

### Fixed
- Argparse help strings no longer contain literal `\n` escapes.
- `do_add_host` failures surface the real `ArgsParseFailureError`
  instead of being swallowed by a `BaseException` suppress block.

### Removed
- Stray `default.profraw`, legacy `Documentation/README` plain-text
  manual, and the dangling cross-references for the long-removed
  `testsuite_run` / `testsuite_submit` / `testsuite_list` commands.

[Unreleased]: https://github.com/openSUSE/mtui/compare/main...HEAD
