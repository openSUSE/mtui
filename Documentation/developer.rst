#######################
Developer Documentation
#######################

This guide covers the day-to-day MTUI development workflow. For the
high-level contribution process see ``CONTRIBUTING.md`` at the repo
root; for the user-facing CLI see ``user.rst`` and ``iui.rst``.


Project layout
##############

.. code-block:: text

    mtui/                  # main package
      args.py              # argparse setup
      argparse.py          # ArgsParseFailureError + custom parser
      colorlog.py          # coloured logging formatter
      config.py            # INI config loader
      connection.py        # paramiko-based SSH connection
      display.py           # tabular CLI output helpers
      main.py              # console-script entry point
      prompt.py            # interactive Cmd subclass (CommandPrompt)
      refhost.py           # reference-host attribute schema
      colors.py            # ANSI colour helpers
      completion.py        # readline completion helpers
      fileops.py           # filesystem helpers (atomic write, ensure_dir)
      term.py              # terminal size / prompting helpers
      misc.py              # remaining small helpers (e.g. requires_update)
      actions/             # background SSH worker actions
      checks/              # post-update verification probes
      commands/            # one module per `do_<command>` (see below)
      connector/           # OBS, openQA, Gitea, … HTTP backends
      target/              # Target / HostsGroup / lock files
      template/            # test-report template loaders
      types/               # typed value objects (UpdateID, RpmVer, …)

    tests/                 # pytest test suite (mirrors mtui/ paths)
    Documentation/         # Sphinx sources (this directory)
    .github/               # CI workflows, dependabot, templates
    pyproject.toml         # PEP 621 metadata, ruff/ty/pytest config
    uv.lock                # pinned dependency graph


Development environment
#######################

MTUI uses `uv <https://docs.astral.sh/uv/>`_ for environment and
dependency management.

.. code-block:: sh

    uv sync --extra norpm --group dev

This creates ``.venv/`` with mtui in editable mode plus the test and
lint tooling. The ``norpm`` extra pulls in ``version_utils`` so the
test suite runs without the system ``rpm`` Python bindings.

To run mtui from the checkout:

.. code-block:: sh

    uv run python -m mtui --help


Quality gates
#############

CI runs the following commands on Python 3.11, 3.12, 3.13 and 3.14;
all must pass before a PR can merge.

.. code-block:: sh

    uv run ruff format --check .
    uv run ruff check .
    uv run ty check
    uv run pytest

Auto-fix locally:

.. code-block:: sh

    uv run ruff format .
    uv run ruff check --fix .


pre-commit
==========

Optional but recommended. Hooks live in ``.pre-commit-config.yaml`` and
mirror the CI gates.

.. code-block:: sh

    uv run pre-commit install
    uv run pre-commit run --all-files       # one-off full sweep


Coverage
========

``pytest --cov`` runs in CI on the Python 3.13 leg and reports to
Codecov. The project floor is enforced (``codecov.yml``) and ratchets
upward as coverage grows.


Command architecture
####################

Every interactive ``mtui>`` command lives in its own module under
``mtui/commands/``. A command is a subclass of
``mtui.commands._command.Command`` exposing:

- ``command``: the textual command name (class attribute, required).
- ``_add_arguments(cls, parser)``: classmethod that registers
  argparse arguments on the supplied parser (optional; default no-op).
- ``__call__(self)``: the implementation, reads ``self.args`` and
  ``self.targets`` (required; ``Command`` is abstract on ``__call__``).
- ``complete(state, text, line, begidx, endidx)``: optional readline
  completer; the base class returns ``[]``.

Importing ``mtui.commands`` walks the package with
``pkgutil.iter_modules`` and imports every submodule whose name does
not start with ``_``. Each ``Command`` subclass that assigns ``command``
in its own body auto-registers via ``Command.__init_subclass__`` into
``Command.registry`` (re-exported as ``mtui.commands.registry``); two
subclasses claiming the same ``command`` string raise
``CommandAlreadyBoundError`` at class-creation time. ``CommandPrompt``
iterates the registry and binds ``do_<command>``, ``help_<command>``
and ``complete_<command>`` shims for each entry.


How to add a new command
========================

1. Create ``mtui/commands/foo.py`` with a single ``Foo`` class:

   .. code-block:: python

       from ..argparse import ArgumentParser
       from ._command import Command


       class Foo(Command):
           command = "foo"

           @classmethod
           def _add_arguments(cls, parser: ArgumentParser) -> None:
               parser.add_argument("name")

           def __call__(self) -> None:
               self.println(f"hello, {self.args.name}")

2. Add tests under ``tests/test_foo.py``: at minimum a happy-path
   invocation and an arg-parse error case. Use the existing
   ``CommandTestBuilder`` / fixtures patterns from ``tests/test_*.py``.

3. Add a section to ``Documentation/iui.rst`` describing the new
   command, in the appropriate cluster.

4. Add a ``[Unreleased]`` bullet to ``CHANGELOG.md``.


Testing patterns
################

- Place tests under ``tests/`` mirroring the module path
  (``mtui/foo/bar.py`` → ``tests/test_bar.py``).
- Markers (registered in ``pyproject.toml``):

  - ``slow`` — noticeably slower than the rest of the suite.
  - ``integration`` — exercises multiple components (e.g. spawning
    ``python -m mtui``).
  - ``network`` — performs real network I/O.

  Run a subset with ``uv run pytest -m 'not slow and not network'``.

- HTTP mocks use `responses <https://github.com/getsentry/responses>`_
  (see ``tests/test_gitea.py``).
- Prefer ``pytest.mark.parametrize`` over copy-paste for table-driven
  tests; ``tests/test_connection.py`` shows the pattern.
- The ``conftest.py`` autouses ``capsys`` patches; check it before
  reaching for new fixtures.


Type checking
=============

``ty check`` is a hard CI gate. ``[tool.ty]`` in ``pyproject.toml``
enables ``error-on-warning`` and promotes everything in
``mtui/types/**`` and ``mtui/connector/**`` to ``error all``. New
``# ty: ignore`` directives need a specific rule code (the
``PGH003`` ruff rule enforces this).


Documentation
#############

The Sphinx sources live under ``Documentation/``. ``index.rst``
includes ``../README.md`` via ``myst-parser``; the rest of the pages
are RST.

Build locally:

.. code-block:: sh

    uv run --group doc sphinx-build -W -b html Documentation Documentation/.build/html

The ``-W`` flag promotes warnings to errors, matching what we expect
in CI.


Branching and commits
#####################

- One PR per logical change, based on ``main``.
- `Conventional Commits <https://www.conventionalcommits.org/>`_ for
  every commit; rebase rather than merge.
- Reference SUSE Bugzilla / Bugzilla.opensuse.org issues with
  ``bsc#NNNN`` / ``boo#NNNN`` in the commit body.

CI matrix: Python 3.11, 3.12, 3.13 and 3.14 (see
``.github/workflows/ci.yml``).
