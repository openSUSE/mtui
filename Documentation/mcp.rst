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
command is auto-exposed as an MCP tool, and dedicated testreport tools
(``testreport_read``, ``testreport_patch``, ``testreport_write``)
replace the REPL's ``$EDITOR``-based ``edit`` flow, with
``testreport_logs`` and ``testreport_read_file`` exposing the rest of
the checkout (build-check and install logs, ``source.diff``, …).

Session state is isolated per client. Under ``stdio`` one process
serves one client; under ``http`` each connected client gets its
**own** isolated session — its own ``metadata`` and ``targets`` — so
concurrent clients never see each other's loaded template or hosts
(see :ref:`mcp-concurrency`). Either way a single client can hold
several **named workspaces** (the ``workspace`` argument on every tool),
each an independent loaded template + set of hosts, to drive more than
one update at once (see :ref:`mcp-parallel-updates`). Clients load their
own state at runtime via the ``load_template`` and ``add_host`` tools.

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

The extra pulls in the official `mcp
<https://pypi.org/project/mcp/>`_ Python SDK (``mcp[cli]>=1.2``) and
its transitive tree (pydantic, uvicorn, starlette, httpx,
sse-starlette). On openSUSE the SDK is packaged as ``python3-mcp``
and can be installed with ``zypper in python3-mcp``. The rest of
mtui keeps working without it; the ``mtui.mcp`` package is
import-gated so a missing extra produces a friendly error from
``mtui-mcp`` rather than breaking ``mtui``.


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
    Raise both the ``mtui-mcp`` logger and the
    ``mcp.server.fastmcp`` logger to ``DEBUG`` so protocol-level
    frames become visible.

``--color {auto,always,never}``
    Control coloured log output; default ``auto``.

``-l, --location``, ``-t, --template_dir``, ``-g, --gitea_token``, ``-w, --connection_timeout``
    Configuration overrides identical to ``mtui``.

``-V, --version``
    Print mtui, Python, paramiko and openqa-client versions, then
    exit.

.. note::

   ``mtui-mcp`` takes **no** boot-time test-report or host flags.
   Earlier versions accepted ``-a``/``-k`` (preload an RRID) and
   ``-s``/``--sut`` (autoconnect hosts); these were removed when the
   HTTP transport gained per-client isolation, because a single
   boot-time seed cannot belong to any one client. Each client loads
   its own state at runtime with the ``load_template`` and
   ``add_host`` tools.


Connecting an LLM client
========================

``mtui-mcp`` speaks the standard Model Context Protocol framing, so
any MCP-aware client wires up the same way it would for any other
server: ``stdio`` clients spawn the binary as a subprocess; ``http``
clients connect to a streamable-HTTP endpoint at ``/mcp`` on the
configured host and port. The two examples below — Claude Desktop
(stdio) and opencode (remote HTTP) — cover the common shapes; other
clients (Cursor, Zed, Codex CLI, Continue, etc.) follow the same
pattern with their own config file names.

For Inspector-based smoke-testing and operational troubleshooting,
see the *MCP server* section of :doc:`developer`.

Claude Desktop (stdio)
----------------------

stdio is the default transport: the client spawns ``mtui-mcp`` as a
child process and exchanges MCP frames over its stdin/stdout. Add
``mtui`` to ``mcpServers`` in
``~/.config/Claude/claude_desktop_config.json`` (Linux/macOS) or the
equivalent on other platforms:

.. code-block:: json

    {
      "mcpServers": {
        "mtui": {
          "command": "mtui-mcp",
          "args": ["--transport", "stdio"]
        }
      }
    }

When mtui is installed inside a ``uv`` project or a virtualenv that
is not on the client's ``PATH``, give the absolute path
(``which mtui-mcp`` from the right environment) or wrap it in ``uv``:

.. code-block:: json

    {
      "mcpServers": {
        "mtui": {
          "command": "uv",
          "args": [
            "--directory", "/path/to/mtui",
            "run", "mtui-mcp", "--transport", "stdio"
          ]
        }
      }
    }

Point the server at a custom config with ``-c`` if needed; the test
report and hosts are **not** seeded from CLI flags — the client loads
them at runtime by calling the ``load_template`` and ``add_host``
tools:

.. code-block:: json

    {
      "mcpServers": {
        "mtui": {
          "command": "mtui-mcp",
          "args": [
            "--transport", "stdio",
            "-c", "/etc/mtui/qam.cfg"
          ]
        }
      }
    }

opencode (remote HTTP)
----------------------

The HTTP transport binds a streamable-HTTP server on
``--host`` / ``--port`` (default ``127.0.0.1:8000``); the MCP endpoint
is mounted at ``/mcp``, so the URL the client connects to is
``http://HOST:PORT/mcp``.

Start the server:

.. code-block:: sh

    mtui-mcp --transport http --port 8765

Add the endpoint to ``opencode.json`` as a ``remote`` MCP server:

.. code-block:: json

    {
      "$schema": "https://opencode.ai/config.json",
      "mcp": {
        "mtui": {
          "type": "remote",
          "url": "http://127.0.0.1:8765/mcp",
          "enabled": true
        }
      }
    }

opencode also accepts ``type: "local"`` for stdio-style spawning,
which avoids the long-lived server process entirely:

.. code-block:: json

    {
      "$schema": "https://opencode.ai/config.json",
      "mcp": {
        "mtui": {
          "type": "local",
          "command": ["mtui-mcp", "--transport", "stdio"],
          "enabled": true
        }
      }
    }

The HTTP transport isolates state per client but does **not**
authenticate them (see :ref:`mcp-concurrency`). Bind to loopback (the
default) and front with the operator's reverse-proxy of choice if
remote access is required — ``mtui-mcp`` itself does not terminate TLS
or authenticate callers.


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

``testreport_read(offset: int = None, limit: int = None) -> dict``
    Returns ``{"path": str, "line_count": int, "content": str}``.
    Reads the file as UTF-8 with ``errors="replace"``. Marked
    ``readOnlyHint=True`` and ``idempotentHint=True``.

    ``offset`` (1-based first line) and ``limit`` (max lines) return a
    line window instead of the whole file, using the same 1-indexed
    line numbers ``testreport_patch`` consumes — useful for paging a
    large report whose ``log`` runs to thousands of lines after
    ``export``. Without them the full file is returned. The reply
    always reports the file's total ``line_count``, and adds
    ``offset`` and ``returned_lines`` when a window is requested.

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

``testreport_logs() -> dict``
    Lists the auxiliary log files in the loaded testreport's checkout
    that the ``log`` file does not cover: the per-package/arch
    build-check logs (``build_checks/``) and the per-refhost install
    logs (``install_logs/``). Returns ``{"path": str, "build_checks":
    [{"name", "size"}], "install_logs": [{"name", "size"}]}``. Marked
    ``readOnlyHint=True``.

``testreport_read_file(relpath: str) -> dict``
    Reads any file in the checkout by path relative to the checkout
    directory — e.g. ``build_checks/<pkg>.<arch>.log``,
    ``install_logs/<host>.log``, ``source.diff`` or ``patchinfo.xml``.
    The path is resolved under the checkout and may **not** escape it
    (``..`` traversal and absolute paths are refused). Returns
    ``{"path", "line_count", "content"}``. Marked ``readOnlyHint=True``.

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


.. _mcp-concurrency:

Concurrency and safety
======================

* **One isolated session per client (HTTP).** Under ``--transport
  http`` each connected client gets its own :class:`McpSession` — its
  own ``Config`` view, ``metadata``, and ``targets``. One client's
  ``load_template`` / ``add_host`` is invisible to every other client,
  so concurrent reviewers never collide. Under ``stdio`` there is one
  client and therefore one session.
* **Sessions are keyed on the MCP session.** The registry keys each
  session on the identity of the request's ``ServerSession`` object
  (``id(ctx.session)``), which the SDK keeps 1:1 with the MCP session
  for the connection's lifetime. The ``Mcp-Session-Id`` header is used
  for log lines only, never to route state.
* **Per-session ``asyncio.Lock``.** Within a single session every tool
  invocation — auto-generated and testreport-editing alike — acquires
  *that session's* lock, so one client's calls cannot interleave
  mutations of its own ``metadata`` / ``targets``. Calls from different
  clients hold different locks and run concurrently.
* **Bounded session count.** The HTTP registry refuses to create more
  than ``[mcp] session_cap`` concurrent sessions (default ``32``),
  failing the offending tool call with a clear error rather than
  spawning unbounded SSH connections and worker threads. Raise the cap
  in config if you genuinely need more simultaneous clients.
* **Idle sessions are reaped.** A session that receives no tool calls
  for ``[mcp] session_idle_timeout`` seconds (default ``1800``) is
  evicted and its hosts disconnected. Because the MCP SDK gives the
  application no per-session teardown callback, this sweep is what
  releases the SSH connections of a client that simply disconnected;
  set the timeout to ``0`` to disable it. (A client that reconnects
  after eviction simply re-loads its template and hosts.)
* **Atomic file writes.** ``testreport_patch`` and ``testreport_write``
  swap via ``os.replace`` after ``fsync``; the on-disk file is always
  either fully old or fully new, never torn.
* **Concurrent writers within one session are not detected.** The
  per-session lock prevents interleaving, but if one client reads the
  file, computes two patches against the same line numbers, then
  applies them sequentially, the second still lands at stale offsets.
  A future ``expected_sha256`` round-trip field on ``patch`` /
  ``write`` is the planned mitigation.
* **No auth, no TLS.** Isolation is not authentication: ``mtui-mcp``
  does not identify callers. Bind to loopback (the default) and front
  with the operator's preferred reverse-proxy if external access is
  required.

These knobs live in the ``[mcp]`` section of the mtui config file:

.. code-block:: ini

    [mcp]
    session_cap = 32
    session_idle_timeout = 1800


.. _mcp-parallel-updates:

Working on several updates in parallel
======================================

Validating a queue of updates is faster when the independent parts overlap.
Three composable features make that possible; together they let one client
(or several agents) keep more than one update moving at once.

* **Named workspaces (one client, several updates).** Every tool takes an
  optional ``workspace`` argument (default ``"default"``). Each distinct
  name resolves to its own isolated :class:`McpSession` — own loaded
  template, own ``targets``, own lock — so a single ``stdio`` client can
  ``load_template`` update A in workspace ``a`` and update B in workspace
  ``b`` and drive both without one's ``load_template`` tearing the other's
  hosts down. Because the lock is per-session, calls in *different*
  workspaces run concurrently. ``list_workspaces`` shows the caller's
  workspaces (their loaded template and hosts); ``close_workspace``
  disconnects one and drops it. (Under ``stdio`` the idle sweeper is
  disabled, so a workspace left quiet while you work another keeps its
  hosts.)

* **Background jobs (don't block on a slow host op).** The slow host
  commands — ``run``, ``update``, ``downgrade``, ``prepare``, ``install``,
  ``uninstall``, ``set_repo``, ``reboot`` — take ``background=true``: the
  call returns a job id immediately instead of holding the request open for
  the minutes the operation takes, while the command runs in the
  background (still under the workspace's lock, so it serialises against
  that workspace's other mutating calls). Poll with ``job_status`` and
  fetch output with ``job_result`` (``job_list`` / ``job_cancel`` round it
  out); pass the same ``workspace``. So one workspace's ``update`` can run
  on the hosts while you advance another workspace's review.

* **Waiting for a busy refhost (sharing a pool across agents).** Several
  *agents* are separate processes, so a refhost lock genuinely excludes
  them and one would otherwise fail with "locked by …". Set ``[lock] wait``
  to a number of seconds to make ``lock`` **queue** on a busy host —
  polling every ``[lock] wait_poll`` seconds until it is released (or
  reaped as stale, or becomes yours) — rather than erroring. A warning is
  still logged when the wait starts (and on timeout), so a REPL user sees
  the host is busy. The default ``0`` keeps the immediate-failure
  behaviour.

.. code-block:: ini

    [lock]
    wait = 0          ; seconds to wait for a busy refhost (0 = fail fast)
    wait_poll = 15    ; poll interval while waiting

* **Refhost pool selection (different workspaces/agents, different hosts).**
  When ``refhosts.yml`` lists several interchangeable hosts for the same
  **test target** and ``[refhosts] pool_select`` is on, ``add_host`` connects
  just **one free** host per target instead of the whole matching matrix: it
  tries candidates in turn, skips any already in use, and **claims** the one
  it takes, so parallel workspaces/agents drawing from the same pool land on
  different hosts. The "target" is the full ``product + version + arch +
  addons`` the update asks for, **not** just the arch — so an update spanning,
  say, all arches of SLE15-SP5 *and* SP7 still gets a host for every
  (service-pack, arch) pair; only genuine duplicates collapse to one. The pool
  is searched across **all** locations. Off by default, so a one-host-per-target
  ``refhosts.yml`` behaves exactly as before.

  "In use" is judged on two levels so the pool is safe both within and across
  processes:

  - **Within one ``mtui-mcp`` process** an in-process *host arbiter* tracks
    which workspace owns which host — necessary because all workspaces share
    the OS pid, so the remote lock alone cannot tell them apart. A candidate
    owned by another workspace is skipped; if every candidate is owned, the
    claim **queues** up to ``[lock] wait`` seconds for one to be released
    (multiple workspaces queueing on a shared pool).
  - **Across processes / manual users** the claim also takes the **remote**
    ``/var/lock/mtui.lock`` with an identifying comment
    (``mtui-mcp pool <RRID> [<owner>]``), so other ``mtui-mcp`` servers and
    people running ``mtui`` see the host busy and by what. Closing the
    workspace removes that remote lock (so the host shows free again), and a
    crashed server's pool locks are recovered by the normal stale-lock reaping
    (``[lock] reap_stale``; the identifying comment does not exempt them).

.. code-block:: ini

    [refhosts]
    pool_select = false   ; true: pick one free host per arch from the pool

Together these compose: a pool (``pool_select``) gives several agents
distinct hosts; ``lock wait`` makes them queue politely when the pool is
exhausted; workspaces and background jobs let each agent (or a single
client) keep several updates moving.

.. note::

   ``location`` is optional. With no ``[mtui] location`` configured and none
   set interactively or over MCP it defaults to ``default`` — the
   ``default`` bucket of ``refhosts.yml`` — so a plain setup needs no
   location at all. Pool selection ignores location entirely regardless.

.. note::

   Workspaces inside *one* ``mtui-mcp`` process share the OS pid, so the remote
   refhost lock (keyed on user+pid) cannot tell them apart. The in-process host
   arbiter (above) is what keeps two same-process workspaces off the same pool
   host; ``[lock] wait`` then governs queueing both within the process (arbiter)
   and across separate processes/agents (remote lock). Without ``pool_select``
   (a single host per arch), still serialise the host phase per host — only the
   pool makes concurrent host-lanes safe.


Long-running tool calls
=======================

Many mtui commands legitimately take minutes rather than seconds —
``run`` against a slow refhost, ``update``, ``set_repo``, ``commit``,
``add_host`` against an SSH endpoint that takes its time to come up,
``load_template`` against a fresh SVN checkout, anything that drives
``osc``/``svn`` over the network. The MCP client's default JSON-RPC
read timeout (often 30 s, matching the SDK's
``MCP_DEFAULT_TIMEOUT``) used to cancel those calls before the
server's worker thread returned.

To keep clients patient, every ``mtui-mcp`` tool emits
``notifications/progress`` every 10 seconds while its underlying
command is running. MCP-spec-compliant clients (the official
Inspector, Claude Desktop, opencode, Cursor, …) reset their read
deadline on each frame, so a command that takes ten minutes still
returns its captured stdout cleanly.

The heartbeat is automatic and applies to every auto-generated tool
plus the three testreport tools; no client configuration is required
to benefit from it.

Clients that ignore progress notifications
------------------------------------------

A small number of older / non-spec-compliant clients ignore
``notifications/progress`` and enforce their own short read timeout.
For those, raise the timeout in the client's own configuration:

* **Claude Desktop** — per-server ``timeout`` field (milliseconds) in
  ``mcpServers.<name>``:

  .. code-block:: json

      {
        "mcpServers": {
          "mtui": {
            "command": "mtui-mcp",
            "args": ["--transport", "stdio"],
            "timeout": 600000
          }
        }
      }

* **opencode** — per-server ``timeout`` field on the MCP entry.

* **Custom Python clients built on the SDK** — pass
  ``read_timeout_seconds`` to ``ClientSession`` or
  ``request_read_timeout_seconds`` to ``send_request``; the SDK's
  ``MCP_DEFAULT_TIMEOUT = 30.0`` in ``mcp.shared._httpx_utils`` is
  the lower-bound httpx default if you do not override it.

The heartbeat itself never *caps* execution time; if your client
honours progress it will simply wait as long as the server takes.

Background jobs (don't block on a slow host op)
-----------------------------------------------

The heartbeat keeps a *synchronous* call alive, but the client is
still parked on that one request until the command finishes. When you
would rather fire off a slow host operation and keep working, the eight
slow host commands —
``run``, ``update``, ``downgrade``, ``prepare``, ``install``,
``uninstall``, ``set_repo``, ``reboot`` — accept a ``background=true``
flag. Instead of holding the request open for the minutes the op takes,
the call returns **immediately** with a job id::

    run(command=["zypper", "-n", "patch"], background=true)
    -> "started background job 'run-1' for `run`; it runs on the hosts
        while you work elsewhere. Poll job_status(job_id='run-1') and
        fetch output with job_result(job_id='run-1')."

The command still runs under the session lock for its whole duration —
so it serialises against the session's other mutating calls exactly
like a foreground call — but you are free to issue other (read-only)
tool calls meanwhile and poll the job:

* ``job_list`` — every job in the session and its state;
* ``job_status(job_id=…)`` — one job's state
  (``running`` / ``done`` / ``failed`` / ``cancelled``) and elapsed
  time;
* ``job_result(job_id=…)`` — a finished job's captured stdout. It
  *errors* while the job is still running (poll ``job_status`` first)
  and surfaces the command's failure envelope (stdout, error, exit
  code) if it failed — exactly as a foreground failure would have;
* ``job_cancel(job_id=…)`` — cancel a running job.

Jobs are scoped to the session: under stdio that is the single process;
under http it is the caller's isolated session, so one client never
sees another's jobs. The job table lives in memory and persists for the
session's lifetime — finished records are not evicted — but under http
the registry's idle-TTL sweep drops the whole session (and its jobs)
once it goes quiet.

.. note::

   Cancellation detaches the awaiter, but a job already executing on a
   host (an SSH command or subprocess) may keep running to completion
   on that host even after ``job_cancel`` returns — the same caveat as
   interrupting a foreground ``run`` with Ctrl-C.


Command output and logging
===========================

A tool call returns the text the command wrote to **stdout**. In
addition, any log record a command emits through the ``mtui`` logger
tree (``mtui.commands.*``, ``mtui.template.*``, ``mtui.checks.*``, …) at
``INFO`` level or above *while it runs* is captured and appended to that
same reply, prefixed with its level (``WARNING: …``, ``ERROR: …``). This
means conditions a command reports by logging rather than printing — for
example the product-drift warnings emitted when ``add_host`` connects a
reference host whose installed products disagree with ``refhosts.yml`` —
reach the client even though they never touched stdout.

The capture is deliberately scoped:

* **``INFO`` and above only.** ``DEBUG`` records stay out of the reply
  (use ``--debug`` to see them on the server's stderr).
* **Per call, by capture token.** A unique token is set for the duration
  of one command and the capturing handler admits only records tagged
  with it. The token rides along into any worker threads the command
  fans out to — MTUI's thread pools are
  :class:`~mtui.support.concurrency.ContextExecutor` instances that copy
  the caller's :mod:`contextvars` context into each task — so a
  command's own background work is captured too (for example the
  product-drift warnings logged on ``add_host``'s connect pool). Records
  produced under a *different* token, including a concurrent
  ``--transport http`` client's call, never bleed into this reply.
* **The server's own bookkeeping is excluded.** Lines the session logs
  about a call (e.g. "command … wrote to stderr") go through the
  ``mtui-mcp`` logger, which is outside the captured ``mtui`` subtree and
  therefore never echoed back into the reply.

.. note::

   When TLS verification is disabled (``ssl_verify = false``), the
   server silences urllib3's ``InsecureRequestWarning`` **once at
   start-up**. The MCP SDK records and re-emits every Python warning
   raised while it handles a request, so without this the warning would
   otherwise reappear on the server log for *every* request to an
   internal host reached without verification (openQA, the QAM
   Dashboard, …). Suppression happens only when verification is off, so
   a real certificate problem is still surfaced when it is on.

