# MCP server

`mtui-mcp` is a [Model Context Protocol](https://modelcontextprotocol.io) server
that exposes mtui's functionality as tools an LLM client can call. Its tool
surface is *synthesised from the same command registry* the `mtui` REPL uses:
each non-denied command becomes one tool, with a JSON input schema derived from
the command's `clap` argument spec, dispatched through the same engine as the
REPL. Adding, renaming, or removing a command changes the MCP tools
automatically. The underlying semantics of each tool are the command semantics in
the [Command reference](cli.md).

## Building and running

The server lives behind the `mcp` feature so the default build and the `mtui`
REPL never pull in the MCP SDK:

```sh
cargo run -p mtui-mcp --features mcp -- --help
```

Two transports are served:

- **stdio** (default) — one process == one client. Use this for a local LLM
  client that spawns the server as a subprocess.

  ```sh
  mtui-mcp                       # stdio
  ```

- **streamable HTTP** — many clients, with per-client session isolation.

  ```sh
  mtui-mcp --transport http --host 127.0.0.1 --port 8000
  ```

  `--host`/`--port` bind the HTTP listener and are ignored under stdio. The bind
  address is **loopback only** — rmcp's DNS-rebinding guard rejects non-loopback
  binds.

Logs go to **stderr**; **stdout** is the transport, so never write to it.

## Tool profiles

The advertised tool surface can be narrowed with `[mcp] profile`:

- **`full`** (default) — every synthesised tool.
- **`core`** — a curated everyday subset; the rest are filtered out before they
  reach the wire.

Fine-tune with `[mcp] tools_allow` (add specific tools on top of the profile) and
`[mcp] tools_deny` (remove specific tools; deny wins last). See the `[mcp]` table
in [Configuration](configuration.md) for these and the resource caps
(`max_output_bytes`, `max_active_jobs`, session budget, …).

## Security boundary

**Profiles are not authentication or authorization.** They reduce the advertised
tool surface; they do not gate callers. HTTP session isolation is likewise not
caller authentication. Keep the HTTP transport on its default loopback interface,
or place it behind an authenticated boundary you trust to operate the remaining
maintenance tools.

### Permanent deny-list

Some commands are **never** synthesised into MCP tools, under every profile — this
cannot be re-enabled with `[mcp] tools_allow`:

- **`lrun`** — executes commands as the local `mtui-mcp` process user. It stays
  available through direct `mtui` use and trusted callers of the core engine, but
  is never exposed over stdio or HTTP.
- The interactive/REPL-only commands, which require a controlling terminal or
  have no headless meaning: `quit`, `exit`, `EOF`, `switch`, `shell`, `help`,
  `edit`, `terms`.

The deny-list is intersected with the live registry and consistency-tested; a
renamed or removed command that leaves a stale deny-list entry is warned about at
boot, so the boundary cannot drift silently.

## Background jobs

Long-running commands (e.g. a fan-out `update` or `export`) run as background jobs
so a tool call returns promptly with job ids you can poll. The per-session job
budget (`max_active_jobs`, `max_completed_jobs`) and the single-result size cap
(`max_output_bytes`) bound resource use so one client cannot exhaust the server or
dwarf the client's context.
