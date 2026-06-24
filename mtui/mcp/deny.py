"""REPL-only commands that the MCP server must not expose as tools.

Each entry cannot meaningfully run outside an interactive terminal session
and is filtered out when ``mtui.mcp.tools`` synthesises tools from
``mtui.commands.Command.registry``.

Per-entry rationale:

- ``quit``, ``exit``, ``EOF``: call ``sys.exit`` and would tear down the
  MCP server process along with the client connection.
- ``edit``: spawns ``$EDITOR`` on ``metadata.path``; replaced by the
  ``testreport_read`` / ``testreport_patch`` / ``testreport_write`` MCP
  tools that operate on the loaded testreport file directly.
- ``shell``: opens an interactive root PTY on a refhost and needs a TTY
  attached to the client, which MCP transports do not provide.
- ``help``: prints argparser help text to stdout; the MCP protocol
  already advertises tool descriptions to clients.
- ``terms``: launches local terminal-emulator scripts (``term.<name>.sh``)
  that spawn ``xterm``/``konsole``/etc. on the operator's ``$DISPLAY``.
- ``switch``, ``unload``: interactive multi-template navigation that mutates
  the session's active-template pointer / loaded set. MCP multi-template
  control is exposed instead through the per-tool ``template`` parameter
  (Phase 4); ``list_templates`` remains available as a read-only listing.
"""

REPL_ONLY: frozenset[str] = frozenset(
    {"quit", "exit", "EOF", "edit", "shell", "help", "terms", "switch", "unload"}
)
