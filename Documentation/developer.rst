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
      __init__.py          # __version__
      __main__.py          # `python -m mtui` entry
      main.py              # console-script entry point
      cli/                 # command-line surface
        args.py            #   argparse setup
        argparse.py        #   ArgsParseFailureError + custom parser
        completion.py      #   tab-completion helpers (legacy cmd.Cmd-style
                           #     signature, adapted by _completer.py)
        display.py         #   tabular CLI output helpers
        notification.py    #   desktop notification helpers
        prompt.py          #   back-compat shim re-exporting CommandPrompt
                           #     from repl.py
        repl.py            #   interactive REPL (prompt_toolkit PromptSession)
        _completer.py      #   prompt_toolkit Completer adapter for the
                           #     legacy complete() signature
        _history.py        #   shared FileHistory (~/.mtui_history)
        _lexer.py          #   prompt_toolkit Lexer for command-token highlighting
        prompter.py        #   confirm/choice prompt helpers
        term.py            #   terminal size / paging helpers
        colors/            #   ANSI constants, log formatter, runtime switch
      hosts/               # SSH + Target + reference-host metadata
        connection/        #   paramiko-based SSH Connection (split)
        target/            #   Target / HostsGroup / lock files / parsers
        refhost/           #   reference-host schema (models, resolvers, store)
      data_sources/        # external HTTP services
        gitea.py           #   Gitea API client
        oscqam.py          #   osc-qam wrapper
        openqa/            #   openQA clients (standard, kernel)
        qem_dashboard/     #   QEM dashboard client (split)
        oqa_search/        #   openQA build/version search (split)
      test_reports/        # test-report templates + metadata parsers
        testreport.py      #   TestReport ABC
        obs_report.py      #   OBS / kernel / PI / SL report subclasses
        pi_report.py       #     (pi_report.py, sl_report.py, null_report.py)
        sl_report.py
        null_report.py
        metadata_parsers.py#   parsemeta + parsemetajson + repoparse (merged)
        svn_io.py          #   SVN checkout/commit + template errors
        products/          #   per-codestream product normalisers
      update_workflow/     # update execution (the verbs)
        actions/           #   background SSH worker actions
        checks/            #   post-update verification probes
        export/            #   testreport export + artefact download
        hooks.py           #   pre/post hook runner
      support/             # cross-cutting utilities (the only layer-named dir)
        config.py          #   INI config loader
        exceptions.py      #   typed exception hierarchy
        messages.py        #   user-facing message strings
        misc.py            #   requires_update etc.
        fileops.py         #   atomic write, ensure_dir
        systemcheck.py     #   `ssh`/`osc`/`svn` availability probes
        paths.py           #   scripts_path / terms_path / XDG cache (merged)
      commands/            # one module per `do_<command>` (auto-discovered)
      types/               # typed value objects (UpdateID, RpmVer, …)
      helper/              # shell/perl helper scripts (data)
      scripts/             # pre/post/compare hook scripts (data)
      terms/               # terminal-launcher snippets (data)

    tests/                 # pytest test suite (flat, mirrors mtui/ paths)
    Documentation/         # Sphinx sources (this directory)
    .github/               # CI workflows, dependabot, templates
    pyproject.toml         # PEP 621 metadata, ruff/ty/pytest config
    uv.lock                # pinned dependency graph


Development environment
#######################

MTUI uses `uv <https://docs.astral.sh/uv/>`_ for environment and
dependency management.

.. code-block:: sh

    uv sync --extra norpm --extra mcp --group dev

This creates ``.venv/`` with mtui in editable mode plus the test and
lint tooling. The ``norpm`` extra pulls in ``version_utils`` so the
test suite runs without the system ``rpm`` Python bindings. The ``mcp``
extra pulls in ``mcp``/``pydantic``, without which the ``test_mcp_*``
suites fail to import.

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
- ``complete(state, text, line, begidx, endidx)``: optional tab completer
  (legacy ``cmd.Cmd``-style signature, still used by every command; the
  ``prompt_toolkit`` adapter in ``cli/_completer.py`` translates a
  ``Document`` into this tuple). The base class returns ``[]``.

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

       from ..cli.argparse import ArgumentParser
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

  - ``slow``: noticeably slower than the rest of the suite.
  - ``integration``: exercises multiple components (e.g. spawning
    ``python -m mtui``).
  - ``network``: performs real network I/O.

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
``mtui/types/**`` and ``mtui/data_sources/**`` to ``error all``. New
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


MCP server
##########

The ``mtui-mcp`` console script (see :doc:`mcp` for the user-facing
reference) auto-synthesises one MCP tool per non-denied entry in
``Command.registry`` and adds three hand-written testreport tools.
Tool registration goes through the official SDK's
:class:`mcp.server.fastmcp.FastMCP` server.
This section covers smoke-testing and debugging the server itself;
operator-facing client configuration lives in :doc:`mcp`.

Smoke-testing with MCP Inspector
================================

The upstream `MCP Inspector`_ is a browser UI that speaks the MCP
protocol directly, so the tool surface can be exercised without an
LLM in the loop. Useful when iterating on schema generation, the
deny-list, or a new ``testreport_*`` tool.

Start ``mtui-mcp`` under the HTTP transport on a known port:

.. code-block:: sh

    mtui-mcp --transport http --port 8765 --debug

Then launch the Inspector:

.. code-block:: sh

    npx @modelcontextprotocol/inspector

In the UI: select transport ``Streamable HTTP``, set the URL to
``http://127.0.0.1:8765/mcp``, connect, and use the *Tools* tab to
list every registered tool and call it with hand-crafted arguments.
``--debug`` makes both the mtui logger and the
``mcp.server.fastmcp`` logger emit ``DEBUG``-level frames on stderr
so JSON-RPC traffic and the MCP server's routing decisions are
visible alongside the Inspector interaction.

.. _MCP Inspector: https://github.com/modelcontextprotocol/inspector

For a stdio-transport smoke-test, point the Inspector at the
binary instead of an HTTP URL; it spawns the subprocess itself and
attaches to its stdio streams. The first ``tools/list`` round-trip
should return one entry per command in ``Command.registry`` minus the
REPL-only deny-list (``mtui/mcp/deny.py``) plus
``testreport_read``, ``testreport_patch``, ``testreport_write``.

Verifying dispatch
==================

``whoami`` is the cheapest end-to-end check; it requires no loaded
test report and no connected hosts, so a successful call confirms the
schema-synthesis path, the dispatch wrapper, the session lock, and
the response envelope. From a fresh server:

.. code-block:: text

    > whoami()
    User: <username>, Session PID: <pid>

Once a test report has been loaded for the session (via the
``load_template`` tool), ``testreport_read`` returns its current
contents. Before that the editing tools refuse cleanly with
``no testreport loaded; run `load_template` first``, which is the
correct refusal path and exercises
:class:`~mtui.support.messages.UserError` handling.

Troubleshooting
===============

* **``mtui-mcp: command not found`` from a stdio client.** The MCP
  host launched the subprocess outside the environment where the
  ``mcp`` extra was installed. Use the absolute path
  (``which mtui-mcp`` inside the right venv) or wrap the command in
  ``uv --directory <repo> run mtui-mcp …``.
* **HTTP client gets ``404`` at the root.** The streamable-HTTP
  endpoint is mounted at ``/mcp``, not ``/``. Append ``/mcp`` to the
  URL the client is configured with.
* **Tool calls time out under HTTP.** mtui's network-bound commands
  (``update``, ``prepare``, ``checkout``) can run for minutes against
  real refhosts. Raise the client's per-tool timeout. Each HTTP client
  has its own isolated session and lock, so a long tool call blocks
  only that client's own subsequent calls; other clients run
  concurrently against their own sessions.
* **Need to see the wire protocol.** Pass ``--debug`` to ``mtui-mcp``;
  both the server logger and the ``mcp.server.fastmcp`` logger drop
  to ``DEBUG``, surfacing JSON-RPC frames and MCP-server routing
  decisions on stderr. Combine with the Inspector to see request and
  response on both sides of the wire.
* **A new command is missing from the MCP tool surface.** Check
  ``mtui/mcp/deny.py``; only commands on the REPL-only deny-list are
  filtered. If the command is not denied and still not exposed, the
  registry probably failed to import the module (a syntax error or a
  missing ``command = "..."`` class attribute); ``mtui-mcp --debug``
  logs the synthesis loop and names the commands it registered.


Branching and commits
#####################

- One PR per logical change, based on ``main``.
- `Conventional Commits <https://www.conventionalcommits.org/>`_ for
  every commit; rebase rather than merge.
- Reference SUSE Bugzilla / Bugzilla.opensuse.org issues with
  ``bsc#NNNN`` / ``boo#NNNN`` in the commit body.

CI matrix: Python 3.11, 3.12, 3.13 and 3.14 (see
``.github/workflows/ci.yml``).
