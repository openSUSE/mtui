.. vim: tw=72 sts=2 sw=2 et

##########
MCP server
##########

.. contents::
  :depth: 3

Synopsis
========

``mtui-mcp`` is a `Model Context Protocol`_ server that exposes a
headless mtui session to LLM clients. Every non-interactive mtui
command is auto-exposed as an MCP tool, and three dedicated tools
(``testreport_read``, ``testreport_patch``, ``testreport_write``)
replace the REPL's ``$EDITOR``-based ``edit`` flow.

The server is **single-session** and **single-tenant** by contract:
one process holds one ``Config``, one loaded test report, and one set
of connected hosts. A process-wide ``asyncio.Lock`` serialises every
tool invocation so the HTTP transport can accept concurrent clients
without interleaving mutations of ``metadata`` / ``targets``.

.. _Model Context Protocol: https://modelcontextprotocol.io


Installation
============

``mtui-mcp`` ships in the optional ``mcp`` extra. Pick the install
method that matches the rest of your mtui setup (see
:doc:`installation`):

With uv
-------

.. code-block:: sh

    uv sync --extra mcp
    uv run mtui-mcp --help

With pip
--------

.. code-block:: sh

    pip install -e '.[mcp]'
    mtui-mcp --help

The extra pulls in `fastmcp <https://pypi.org/project/fastmcp/>`_
(``>=3``) and its transitive tree (pydantic, uvicorn, starlette,
httpx, sse-starlette). The rest of mtui keeps working without it; the
``mtui.mcp`` package is import-gated so a missing extra produces a
friendly error from ``mtui-mcp`` rather than breaking ``mtui``.


Invocation
==========

.. code-block:: sh

    mtui-mcp --transport {stdio,http} [options]

Flags mirror :doc:`cli` for the configuration surface that
``Config.merge_args`` consumes, plus three MCP-server flags:

``--transport {stdio,http}``
    Transport to serve on. ``stdio`` (default) speaks the MCP
    framing on stdin/stdout — the standard way an LLM client spawns
    an MCP server as a subprocess. ``http`` binds a streamable-HTTP
    endpoint suitable for long-lived sessions.

``--host HOST``
    Bind address for ``--transport http``. Default ``127.0.0.1``;
    deliberately loopback-only — HTTP exposure beyond loopback is
    out of scope for v1 and is left to the operator's reverse-proxy
    of choice.

``--port PORT``
    Bind port for ``--transport http``. Default ``8000``. Pass ``0``
    to let the kernel choose a free port (useful for tests).

``-c, --config PATH``
    Override the default config path; same semantics as ``mtui -c``.

``--debug``
    Raise both the ``mtui-mcp`` logger and the ``fastmcp`` logger
    to ``DEBUG`` so protocol-level frames become visible.

``--color {auto,always,never}``
    Control coloured log output; default ``auto``.

``-l, --location``, ``-t, --template_dir``, ``-g, --gitea_token``, ``-w, --connection_timeout``
    Configuration overrides identical to ``mtui``.

``-s, --sut HOST[,HOST...]``
    Cumulative list of refhosts to autoconnect on boot. Failures are
    logged and the server keeps running (a long-lived MCP session
    has its own recovery paths via the ``add_host`` tool).

``-a, --auto-review-id ID`` / ``-k, --kernel-review-id ID``
    Preload an auto- or kernel-update test report on boot.
    Mutually exclusive. Failures are logged and the server keeps
    running with a :class:`NullTestReport`; the LLM can recover by
    calling the ``load_template`` tool.

``-V, --version``
    Print mtui, Python, paramiko and openqa-client versions, then
    exit.


Tool coverage
=============

Auto-generated tools
--------------------

Every :class:`mtui.commands.Command` subclass in
``Command.registry`` that is **not** on the REPL-only deny-list is
synthesised into an MCP tool at boot. The tool name is the command's
``command`` attribute; its description is the command's docstring;
its JSON schema is derived from the command's :mod:`argparse` parser.

See :doc:`iui` for the per-command semantics — the MCP surface and
the REPL surface share the same command catalogue and the same
implementations.

.. note::

   The auto-generation includes **destructive** commands such as
   ``approve``, ``commit``, ``update``, ``downgrade``, ``reboot``,
   ``remove_host``, and ``set_repo``. The ``readOnlyHint`` annotation
   is set conservatively (``list_``/``show_``/``whoami``/``products``/
   ``openqa_overview`` are marked read-only) but the server does not
   refuse destructive calls. Operators driving an autonomous LLM
   client should reason about blast radius accordingly.

REPL-only deny-list
-------------------

The following commands stay in the REPL but are filtered out of the
MCP tool surface. ``mtui/mcp/deny.py`` is the source of truth.

.. list-table::
  :header-rows: 1
  :widths: 20 80

  * - Command
    - Reason it cannot run over MCP
  * - ``quit``, ``exit``, ``EOF``
    - Call ``sys.exit``; would tear down the MCP server process.
  * - ``edit``
    - Spawns ``$EDITOR`` on ``metadata.path``. Replaced by
      ``testreport_read`` / ``testreport_patch`` /
      ``testreport_write``.
  * - ``shell``
    - Opens an interactive root PTY on a refhost; needs a TTY on
      the client.
  * - ``help``
    - Prints argparser help to stdout; the MCP protocol already
      advertises tool descriptions.
  * - ``terms``
    - Launches local terminal-emulator scripts (``term.<name>.sh``)
      that spawn ``xterm``/``konsole``/etc. on the operator's
      ``$DISPLAY``.

Interactive-prompt defaults
---------------------------

mtui commands that would normally call ``prompt_user(default, …)``
at a REPL prompt receive ``interactive=False`` from the MCP session
and silently return the documented *default*. The two defaults worth
knowing:

* ``load_template`` defaults to **"no"** when asked to overwrite an
  existing checkout.
* ``approve`` defaults to **"yes"** when asked for confirmation.

Where the REPL would otherwise prompt, pass the matching flag
explicitly from the MCP client so the behaviour is unambiguous.


Testreport editing tools
========================

Three hand-written tools operate on the path tracked by
``session.metadata.path``. All three refuse cleanly with
``no testreport loaded; run `load_template` first`` when no test
report is loaded.

``testreport_read() -> dict``
    Returns ``{"path": str, "line_count": int, "content": str}``.
    Reads the file as UTF-8 with ``errors="replace"``. Marked
    ``readOnlyHint=True`` and ``idempotentHint=True``.

``testreport_patch(start_line: int, end_line: int, replacement: str) -> dict``
    Splices an **inclusive, 1-indexed** line range. ``end_line ==
    start_line - 1`` means a pure insertion before ``start_line``.
    The replacement is normalised to end with exactly one ``\n``
    (an empty replacement is a pure delete). The write is atomic:
    new contents land in a sibling ``NamedTemporaryFile``, are
    flushed and ``fsync``-ed, then swapped into place with
    ``os.replace``. Returns ``{"path", "new_line_count",
    "replaced_lines", "inserted_lines"}``.

``testreport_write(content: str) -> dict``
    Full-file overwrite, same atomic-rename routine. Returns
    ``{"path", "bytes_written", "line_count"}``. Use this when line
    drift makes patching unreliable.

.. warning::

   **Always call** ``testreport_read`` **immediately before**
   ``testreport_patch`` **to get current line numbers; line numbers
   shift after every patch.** Two patches issued against the same
   ``read`` will land the second patch at the wrong offset.

Worked example
--------------

Read the loaded report, replace lines 2–3 with three new lines, and
re-read to confirm the new line count:

.. code-block:: text

    > testreport_read()
    {"path": "/srv/qa/templates/SUSE:Maintenance:12345/log",
     "line_count": 5,
     "content": "header\nfoo\nbar\nfooter\ntrailer\n"}

    > testreport_patch(start_line=2, end_line=3, replacement="X\nY\nZ\n")
    {"path": "/srv/qa/templates/SUSE:Maintenance:12345/log",
     "new_line_count": 6,
     "replaced_lines": 2,
     "inserted_lines": 3}

    > testreport_read()
    {"path": "/srv/qa/templates/SUSE:Maintenance:12345/log",
     "line_count": 6,
     "content": "header\nX\nY\nZ\nfooter\ntrailer\n"}


Concurrency and safety
======================

* **Single session per process.** The HTTP transport accepts
  concurrent clients, but they all share the same ``McpSession`` and
  therefore the same ``Config``, ``metadata``, and ``targets``.
* **Process-wide ``asyncio.Lock``.** Every tool invocation —
  auto-generated and testreport-editing alike — acquires the same
  lock, so mutations of ``metadata`` / ``targets`` cannot interleave.
* **Atomic file writes.** ``testreport_patch`` and
  ``testreport_write`` swap via ``os.replace`` after ``fsync``; the
  on-disk file is always either fully old or fully new, never torn.
* **Concurrent writers are not detected.** The lock prevents
  interleaving within one process, but two MCP clients reading the
  file, computing patches against the same line numbers, then
  applying them sequentially will still cause the second one to
  land at stale offsets. A future ``expected_sha256`` round-trip
  field on ``patch``/``write`` is the planned mitigation.
* **HTTP is single-tenant by contract.** No auth, no TLS. Bind to
  loopback (the default) and front with the operator's preferred
  reverse-proxy if external access is required.
