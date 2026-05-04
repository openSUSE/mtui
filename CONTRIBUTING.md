# Contributing to MTUI

Thanks for your interest in improving MTUI. This document covers the
day-to-day workflow; see `Documentation/developer.rst` for a deeper
walk-through of the codebase.

## Branching and pull requests

- Base all work on `main`; one logical change per PR.
- Larger refactors should be split into reviewable commits or, if they
  cross subsystems, into a series of PRs.
- Rebase rather than merge when bringing your branch up to date.
- CI must be green before review (ruff, ty, pytest on Python 3.11–3.14).

## Commit messages

We use [Conventional Commits](https://www.conventionalcommits.org/).
Common prefixes used in this repository:

- `feat:` — user-visible new behaviour
- `fix:` — bug fix
- `docs:` — documentation only
- `test:` — test-only changes
- `refactor:` — internal restructuring with no behaviour change
- `chore:` / `build:` / `ci:` — tooling, packaging, CI

Reference SUSE Bugzilla / Bugzilla.opensuse.org issues with the standard
keywords (`bsc#NNNN`, `boo#NNNN`) in the commit body when applicable.

## Development setup

The project uses [`uv`](https://docs.astral.sh/uv/) for environment and
dependency management.

```sh
uv sync --extra norpm --group dev
```

This creates a virtualenv under `.venv/` containing mtui in editable
mode plus the test and lint tooling. The `norpm` extra pulls in
`version_utils` so the test suite runs without the system `rpm` Python
bindings.

To run mtui from the checkout:

```sh
uv run python -m mtui --help
```

## Quality gates

Run these locally before opening a PR (CI runs the same commands):

```sh
uv run ruff format --check .
uv run ruff check .
uv run ty check
uv run pytest
```

Auto-fix formatting:

```sh
uv run ruff format .
uv run ruff check --fix .
```

### pre-commit (optional)

```sh
uv run pre-commit install
```

Installs hooks for ruff format, ruff check and `ty check`. CI runs the
same gates, so pre-commit is purely a convenience.

## Tests

- Place new tests under `tests/`, mirroring the module path of the code
  under test (`mtui/foo/bar.py` → `tests/test_bar.py`).
- Mark slow / network / integration tests with the registered pytest
  markers (`slow`, `network`, `integration`).
- Prefer `pytest.mark.parametrize` over copy-pasted test bodies.

## Changelog

User-visible changes belong in the `[Unreleased]` section of
`CHANGELOG.md`. Internal refactors do not need an entry.

## Code of Conduct

By participating you agree to abide by the
[Code of Conduct](CODE_OF_CONDUCT.md).
