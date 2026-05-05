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
