# Agent Notes

## Overview
- MTUI (Maintenance Test Update Installer) is the SUSE QE tool for validating maintenance updates: load a request by RRID, install/test it on reference hosts over SSH, and approve/reject. It drives `osc`/`svn`/Gitea under the hood.
- Two driving surfaces: the interactive REPL (`uv run mtui --help`) and the `mtui-mcp` MCP server. Keep both working when touching command or entrypoint code.
- Primary issue tracker is https://progress.opensuse.org (GitHub blank issues are disabled); security reports follow `SECURITY.md`.

## Setup & Commands
- Setup (matches CI): `uv sync --extra norpm --extra mcp --group dev`. The `norpm` extra swaps the system `rpm` bindings for `version_utils`; the `mcp` extra provides `mcp`/`pydantic`, without which the `test_mcp_*` suites fail to import.
- Run it: `uv run mtui --help` or `uv run python -m mtui --help` (entrypoint is `mtui.main:main`).
- Auto-fix style: `uv run ruff format .` then `uv run ruff check --fix .`.
- Coverage run: `uv run pytest -v --cov=./mtui --cov-report=xml --cov-report=term`.
- Docs build (warnings are errors): `uv run --group doc sphinx-build -W -b html Documentation Documentation/.build/html`.

## Testing
- Pytest only collects `tests/`; strict markers are `slow`, `integration`, `network`.
- Focus a test with `uv run pytest tests/test_file.py::test_name`; skip costly/external tests with `uv run pytest -m 'not slow and not network'`.
- HTTP is mocked with `responses`; shared fixtures live in `tests/conftest.py`.

## Architecture (non-obvious bits)
- `mtui/commands/` is a package; every `Command` subclass **auto-registers** via `Command.__init_subclass__` into `Command.registry`. One module per interactive command; modules starting with `_` are skipped.
- A command subclasses `mtui.commands._command.Command`, sets `command`, implements `_add_arguments()` when needed, and implements `__call__()`.
- Config is INI from `MTUI_CONF`, explicit `--config`, or `/etc/mtui.cfg` plus `~/.mtuirc`; `TEMPLATE_DIR`, `TMPDIR`, and `GITEA_TOKEN` are also read.

## Type & Style Quirks
- Supported Python is 3.13 and 3.14; `ty` is pinned to 3.13 (lowest supported) and treats warnings as errors. CI runs the pytest matrix on both versions but only one `ty` job.
- `mtui/types/**` and `mtui/data_sources/**` carry stricter `ty` rules (`all = "error"`).
- Suppress a type error with a specific `# ty: ignore[<code>]` (bare ignores are rejected by Ruff `PGH003`).
- Tests have relaxed Ruff ignores for asserts/private access; do not copy those into package code.

## Change Workflow
- User-visible changes belong in `CHANGELOG.md` under `[Unreleased]`; internal-only refactors usually do not.
- Conventional Commits are expected; reference SUSE Bugzilla as `bsc#NNNN` / `boo#NNNN` when applicable.
- When adding an interactive command, also add focused tests and document it in `Documentation/iui.rst`.

## Definition of Done (hard rules)
- Run the full gate set on the **whole repo** (`.`), never a subset, before pushing or claiming done: `uv run ruff format --check .` **and** `uv run ruff check .` **and** `uv run ty check` **and** `uv run pytest`. CI lints `tests/` too, so formatting only `mtui/` is the most common way to red the pipeline.
- "Done" means CI is observed green, not predicted green. Report status from the actual run.
- New/changed lines need >=80% patch coverage (`codecov.yml` `patch.target: 80`). If a branch is genuinely un-coverable (e.g. best-effort network/error paths), add a focused test or a justified `# pragma: no cover` -- never leave `codecov/patch` silently red.
- Rebase on `upstream/main` and squash to a coherent commit set before requesting review (see `CONTRIBUTING.md`).

## External Runtime Dependencies
- Source installs are not from PyPI. `uv` is the expected dev path; `pip install -e '.[norpm]'` is a documented alternative.
- MTUI shells out to `osc` for OBS/IBS actions and `svn` for test report checkouts/commits; SSH connections go through `paramiko`.

## Further Reading
- `CONTRIBUTING.md` covers branching/PR/commit conventions; `Documentation/developer.rst` is the deeper architectural reference; `Documentation/iui.rst` documents interactive commands and must be updated when adding one.
