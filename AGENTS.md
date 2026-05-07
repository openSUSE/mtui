# Agent Notes

## Scope
- Primary issue tracker for project work is https://progress.opensuse.org. GitHub issue forms disable blank issues; security reports follow `SECURITY.md`.

## Project Shape
- Python package entrypoint is `mtui.main:main`; run it from the checkout with `uv run python -m mtui --help` or `uv run mtui --help`.
- `mtui/commands/` is dynamically imported at startup. Add one command module per interactive command; modules starting with `_` are skipped.
- Commands subclass `mtui.commands._command.Command`, set `command`, implement `_add_arguments()` when needed, and implement `__call__()`.
- Main runtime wiring is `mtui/main.py` -> `mtui/prompt.py` -> dynamic `mtui.commands` registry.
- Config is INI from `MTUI_CONF`, explicit `--config`, or `/etc/mtui.cfg` plus `~/.mtuirc`; `TEMPLATE_DIR`, `TMPDIR`, and `GITEA_TOKEN` are also read.

## Development Commands
- Setup: `uv sync --extra norpm --group dev`. The `norpm` extra avoids requiring system `rpm` Python bindings by using `version_utils`.
- Full local gates matching CI: `uv run ruff format --check .`, `uv run ruff check .`, `uv run ty check`, `uv run pytest`.
- Auto-fix style: `uv run ruff format .` then `uv run ruff check --fix .`.
- Coverage run: `uv run pytest -v --cov=./mtui --cov-report=xml --cov-report=term`.
- Docs build with warnings as errors: `uv run --group doc sphinx-build -W -b html Documentation Documentation/.build/html`.

## Testing
- Pytest only collects `tests/`; registered strict markers are `slow`, `integration`, and `network`.
- Focus a test with `uv run pytest tests/test_file.py::test_name`; skip costly/external tests with `uv run pytest -m 'not slow and not network'`.
- HTTP tests use `responses`; shared mock fixtures live in `tests/conftest.py`.
- CLI smoke tests invoke `python -m mtui`; keep that path working when touching entrypoint or argument parsing code.

## Type And Style Quirks
- Supported Python is 3.11 through 3.14; `ty` is pinned to 3.11 (lowest supported) and treats warnings as errors. CI runs the pytest matrix across all four versions but only one ty job.
- `mtui/types/**` and `mtui/connector/**` have stricter `ty` rules (`all = "error"`).
- Ruff enforces import sorting, pyupgrade for py311+, no `print`, and specific `# ty: ignore[...]` codes via `PGH003`.
- Tests have relaxed Ruff ignores for asserts/private access; do not copy those assumptions into package code.

## Change Workflow
- User-visible changes belong in `CHANGELOG.md` under `[Unreleased]`; internal-only refactors usually do not.
- Conventional Commits are expected. Existing docs mention `bsc#NNNN` / `boo#NNNN` for SUSE Bugzilla references when applicable.
- When adding an interactive command, also add focused tests and document the command in `Documentation/iui.rst`.

## External Runtime Dependencies
- Source installs are not from PyPI. `uv` is the expected dev path; `pip install -e '.[norpm]'` is documented as an alternative.
- MTUI shells out to `osc` for OBS/IBS actions and expects `svn` for test report checkouts/commits; SSH connections are handled through `paramiko`.

## Further Reading
- `CONTRIBUTING.md` covers branching/PR/commit conventions; `Documentation/developer.rst` is the deeper architectural reference. `Documentation/iui.rst` documents interactive commands and must be updated when adding one.
