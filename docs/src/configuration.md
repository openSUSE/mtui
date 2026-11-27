# Configuration

mtui is configured with a **sectioned TOML** file. All configuration is optional:
with no file present, mtui runs on built-in defaults.

## File resolution

The config file is resolved in this order:

1. **`--config <file>`** — an explicit path. Short-circuits the search.
2. **`$MTUI_CONF`** — an explicit path in the environment. Short-circuits the
   search.
3. Otherwise the default pair is merged, **lowest precedence first**:
   - `/etc/mtui.toml` (system-wide)
   - `~/.mtui.toml` (home dotfile)
   - `$XDG_CONFIG_HOME/mtui/mtui.toml` (per-user XDG; falls back to
     `~/.config/mtui/mtui.toml`). On macOS the per-user file is
     `~/Library/Application Support/mtui/mtui.toml` and `$XDG_CONFIG_HOME` is
     not consulted.

   When several of these exist they are merged: an option set in a
   higher-precedence file overrides the same option in a lower one. The per-user
   XDG file therefore wins over `/etc` on shared keys.

**Command-line flags** are the highest-precedence layer, applied on top of every
file: `-t/--template-dir`, `-w/--connection-timeout`, `--reboot-timeout`
and `--reboot-retries` (both `mtui` only), `-g/--gitea-token` and `--ssl-verify`.

Loading is **lenient** and never hard-fails:

- A missing file is skipped silently, including a mistyped `--config` path or
  `$MTUI_CONF`.
- A file that fails to parse as TOML, or holds a value of the wrong type, is
  logged at `ERROR` and skipped **whole**; its options take their defaults.
- A value check — a zero where a positive number is required, an invalid
  `install_logs` — rejects only that key, logs it at `ERROR`, and keeps the rest
  of the file. A relative `put_dirs` entry is dropped on its own, with an error.
- Unknown keys are ignored without a warning.

`~` is expanded to your home directory in path-valued options, except in the
CA-bundle path of `ssl_verify`.

The data directory (`$XDG_DATA_HOME/mtui`, or `~/Library/Application
Support/mtui` on macOS) holds the REPL `history` and the cached
`teregen-token.json`.

## Inspecting and changing config at runtime

The `config` command shows the resolved configuration and can set individual
options for the running session:

```
config show                 # print all resolved options
config show connection_timeout
config set max_parallel 8
```

`config` addresses options by flattened attribute names — `session_user`,
`refhosts_*`, `openqa_instance*`, `target_tempdir`, `lock_*`, `slack_*`, and so
on — rather than by `[section]` key. `config show` lists 39 attributes and
omits `max_oqa_parallel`, the `[mcp]` table and the `[obs]` table.
`template_dir`, `install_logs`, `refhosts_path`, `target_tempdir` and
`target_protected_packages` can be shown but not `set`.

Secret options are never echoed: `config show`/`config set` print `<set>` for a
configured secret (`gitea_token` and `slack_token`) instead of its value. Over MCP,
`config show` additionally refuses the bulk dump and operator-local values;
see [MCP server](mcp.md#config_show-filtering).

## Options

Defaults below are the built-in values.

### `[mtui]`

| Key | Type | Default | Meaning |
|-----|------|---------|---------|
| `template_dir` | path | `$TEMPLATE_DIR` or `.` | Directory holding each loaded report's working directory (install logs, result files). |
| `user` | string | current login user | User attributed to this session (locks, logs). |
| `install_logs` | relative dir name | `install_logs` | Sub-directory (single relative name, no separators) where install logs are written per update. |
| `ssl_verify` | bool / string | `true` | TLS verification for outbound HTTP: a boolean, a boolean spelling (`yes`/`no`/`on`/`off`/…), or a path to a custom CA bundle. |

### `[connection]`

| Key | Type | Default | Meaning |
|-----|------|---------|---------|
| `connection_timeout` | seconds (>0) | `300` | SSH per-command no-output timeout. The SSH link is kept alive, so this window, not a transport idle limit, decides when a quiet command is aborted. Does not bound the connect handshake — see `connect_timeout`. |
| `connect_timeout` | seconds (>0) | `60` | SSH connect handshake budget (TCP connect, banner, and auth). Also bounds each SFTP request (lock/history file operations), which otherwise defaults to a fixed 10s. Raise it for a host with a genuinely slow banner or link (e.g. a loaded s390x LPAR, a jump-path host, or a WAN/VPN refhost); a host slower than this to respond is dropped from a batch with a warning instead of eventually connecting. |
| `reboot_timeout` | seconds (>0) | `10` | Backoff base for reconnecting after a reboot (`prepare`/`reboot`/`update`). Sleeps grow as `2*(reboot_timeout + 5*count)` across attempts. |
| `reboot_retries` | int (>0) | `10` | Number of post-reboot reconnect attempts beyond the first. With the defaults this gives a rebooting host up to ~12.7 minutes to come back. |
| `max_parallel` | int (>0) | `50` | Max hosts to fan out to concurrently (SSH/SFTP/lock/connect batches). |
| `max_oqa_parallel` | int (>0) | `8` | Max concurrent openQA/QAM HTTP requests in the overview search (kept low to be polite to shared hosts). |
| `ssh_strict_host_key_checking` | string | `auto_add` | SSH host-key checking policy: `auto_add`, `warn` or `reject`. An unknown value falls back to `auto_add` with a warning. |

> **SSH is pubkey-only by design.** mtui authenticates from the SSH agent or
> `~/.ssh/id_*`; there is no password auth.

> `reboot_timeout`/`reboot_retries` only apply to the post-reboot reconnect
> budget. A dead SSH link mid-`run`/`shell`, or the `sftp` pre-op check, still
> fails on the first probe — those are not recovering from a reboot.

### `[refhosts]`

| Key | Type | Default | Meaning |
|-----|------|---------|---------|
| `resolvers` | string | `https,path` | Comma-separated ordered list of refhosts resolvers. |
| `https_uri` | URL | `https://qam.suse.de/refhosts/refhosts.yml` | HTTPS URI of the refhosts database. |
| `https_expiration` | seconds (>0) | `43200` (12h) | Age before a cached HTTPS fetch is considered stale. |
| `path` | path | `~/.local/share/refdb/refhosts.yml` | Local filesystem refhosts database (also used as the HTTPS resolver's on-disk mirror; shared with the refdb tooling). |

### `[url]`

| Key | Type | Default | Meaning |
|-----|------|---------|---------|
| `bugzilla` | URL | `https://bugzilla.suse.com` | Bugzilla base URL. |
| `testreports` | URL | `https://qam.suse.de/testreports` | Testreports base URL. |
| `fancy_reports` | URL | `https://qam.suse.de/reports` | "Fancy" reports base URL. |

### `[target]`

| Key | Type | Default | Meaning |
|-----|------|---------|---------|
| `tempdir` | path | `/tmp` | Remote scratch directory on target hosts, and where `put` publishes uploads (see [Remote uploads](#remote-uploads)). |
| `protected_packages` | array of strings | `[]` | Package names the [removal guard](concepts.md#removal-guard) protects on top of its built-in list. Matched by exact name, with no glob or `grub2-*`-style family matching: list each kernel flavour or service package by its full name. Each entry is trimmed; an empty or non-string entry, or one with a character rpm does not allow in a package name, is dropped with an error and the rest are kept. A value that is not an array, or an array with no valid name, is ignored with an error and the list from a lower-precedence file stays; an explicit `[]` clears it. It only adds names: the built-in list always applies. Like every list option, a later file's list replaces an earlier file's rather than adding to it. `config show` prints it as `target_protected_packages`. |

### `[qem_dashboard]`

| Key | Type | Default | Meaning |
|-----|------|---------|---------|
| `api` | URL | `http://dashboard.qam.suse.de/api` | QEM Dashboard API base URL. |

### `[teregen]`

| Key | Type | Default | Meaning |
|-----|------|---------|---------|
| `api` | URL | `https://qam.suse.de/api/v1` | TeReGen report/queue API base URL. |
| `api_v2` | URL | `https://qam.suse.de/api/v2` | TeReGen v2 API base URL: reading **and writing** report documents, `teregen_login`/`whoami`, `checkout`, `regenerate` and the MCP document tools. |

### `[openqa]`

| Key | Type | Default | Meaning |
|-----|------|---------|---------|
| `openqa` | URL | `https://openqa.suse.de` | openQA instance URL. |
| `baremetal` | URL | `http://openqa.qam.suse.cz` | Baremetal openQA instance URL. |

openQA API credentials (`key`/`secret`, used to sign requests with
`X-API-Hash`) are **not** part of `mtui.toml`. mtui delegates their discovery
to the `ruoqa` client, which resolves them in this order:

1. **`$OPENQA_API_KEY`/`$OPENQA_API_SECRET`**, honoured as a pair — setting
   only one is a configuration error. When both are set they are used for
   every instance, ahead of `client.conf`.
2. The openQA project's own `client.conf` — an INI file with one
   `[host[:port]]` section per instance — found via a **tiered**, non-merging
   search: `$OPENQA_CONFIG` (when set), else the user config directory
   (`$XDG_CONFIG_HOME/openqa`, or `~/.config/openqa` when `$XDG_CONFIG_HOME`
   is unset), else `/etc/openqa` and `/usr/etc/openqa`. **The first tier that
   yields any file wins outright; later tiers are not read at all** — a
   `~/.config/openqa/client.conf` does not merge with sections that exist
   only in `/etc/openqa/client.conf`; it replaces it. Within the winning
   tier, each directory contributes its `client.conf` (if present) followed
   by its sorted `client.conf.d/*.conf` drop-ins, later files overriding
   earlier keys.

An instance with no matching section (or no `client.conf` at all, and no
`$OPENQA_API_KEY`/`$OPENQA_API_SECRET` pair) is queried unauthenticated, which
openQA permits for GET requests.

### `[gitea]`

| Key | Type | Default | Meaning |
|-----|------|---------|---------|
| `token` | string (**secret**) | *(empty)* | API token for the Gitea PR review workflow. The Gitea connector refuses to build without it. Masked as `<set>` in `config` output; sent only in an `Authorization` header, never logged. |
| `url` | URL | `https://src.suse.de` | Trusted Gitea origin the `token` may be sent to. A PR API URL comes from checked-out metadata (which is not fully trusted), so the token is attached **only** to requests whose origin (scheme/host/port) matches this value, over `https`, with no embedded userinfo. Point it at another instance (e.g. `https://src.opensuse.org`) to use that one. A metadata URL on any other origin is refused before the token is sent. |

### `[slack]`

The Slack review-request integration behind the `request_review` command.

**Off by default.** Posting into a chat workspace is an outward-facing side
effect, so it never happens implicitly: `enabled` must be `true` *and* both
`token` and `channel` must be set. An mtui with no `[slack]` section never
contacts Slack, and `request_review` refuses with the reason.

**Enabling it also gates `approve`.** Once `enabled = true`, an update can only
be approved after its Slack review request has been acknowledged. `approve`
refuses unless all three hold:

1. `request_review` recorded a marker for the update (and committed it, so a
   reviewer approving from a different checkout sees it);
2. the marked Slack message still names that RRID — a marker copied between
   templates cannot launder an approval;
3. somebody other than the bot left an approving reaction (👍, ✅; skin tone
   ignored).

If Slack cannot be read, `approve` fails closed — an unreadable review request
is not an approved one. There is deliberately **no per-command bypass flag**: a
gate with one is not a gate. Turning it off is a config change
(`config set slack_enabled false`), which is explicit and auditable. `reject` is
never gated — blocking it would strand an update a reviewer wants stopped.

| Key | Type | Default | Meaning |
|-----|------|---------|---------|
| `enabled` | bool | `false` | Whether the integration is available at all. Set `true` to opt in; leaving it unset (or `false`) makes `request_review` refuse up front, which is how a site that does not use Slack — or wants the feature switched off despite credentials being present — runs mtui. |
| `token` | string (**secret**) | *(empty)* | Slack bot token. Required scopes: `chat:write`, `reactions:read`, `channels:history`. Masked as `<set>` in `config` output; sent only in an `Authorization` header, never logged. |
| `channel` | string | *(empty)* | Channel review requests are posted to — an ID (`C0123456789`) or a `#name`. Overridable per call with `request_review --channel`. |
| `api_url` | URL | `https://slack.com/api` | Slack API base the `token` may be sent to. Refused unless it is `https` (or `http` to loopback, which is what makes the test suite's mock server possible) and carries no userinfo. |
| `poll_interval` | int (seconds) | `120` | How often `request_review --watch` polls for reactions. Jittered by ±15% so several mtui instances watching the same channel do not synchronise. Slack's tier-3 methods allow roughly 50 requests/minute. |
| `watch_timeout` | int (seconds) | `3600` | How long `request_review --watch` runs before giving up. |

Example:

```toml
[slack]
enabled = true
token = "xoxb-…"
channel = "#qam-review"
```

### `[lock]`

Remote-lock behaviour on target hosts, so concurrent testers can share a fleet.

| Key | Type | Default | Meaning |
|-----|------|---------|---------|
| `reap_stale` | bool | `true` | On connect, force-remove a pre-existing operation lock older than `stale_age`, regardless of owner. |
| `stale_age` | seconds | `86400` | Age beyond which an operation lock is stale and reapable. `0` disables reaping. |
| `pool_reap_stale` | bool | `true` | On a pool claim attempt, force-remove a pre-existing pool claim older than `pool_stale_age`, regardless of owner. The only automatic recovery for a claim orphaned by an uncatchable exit (SIGKILL/panic/power loss). |
| `pool_stale_age` | seconds | `86400` | Age beyond which a pool claim is stale and reapable. `0` disables pool-claim reaping. |
| `pi_autolock` | bool | `true` | When testing a Product Increment (PI), lock each refhost as it connects while the report is loaded, released on unload/quit. |
| `wait` | seconds | `0` | Queueing budget for pool arbitration: `0` fails fast on a busy host. It also applies when an operation-lock create loses a race to another tester; a host already held by someone else when `install`/`update` probe it is refused at once. |
| `wait_poll` | seconds (>0) | `15` | Poll interval while waiting for a busy lock to free. |

### `[mcp]`

`mtui-mcp` server behaviour. See [MCP server](mcp.md).

| Key | Type | Default | Meaning |
|-----|------|---------|---------|
| `max_output_bytes` | bytes | `100000` | Upper bound on a single tool result (truncated at the tail with a notice). `0` disables the cap. |
| `max_input_bytes` | bytes | `10000000` | Per-host source cap on data a tool reads before returning it (testreport reads, `get` downloads; `put` payloads above it are refused). `0` disables. |
| `max_request_bytes` | bytes | `10000000` | Upper bound on an inbound HTTP request body under `--transport http` (oversized requests are rejected with `413` before rmcp buffers them). Also sets rmcp's own request-body ceiling (default 4 MB), so a single knob governs both gates. `0` disables mtui's limit entirely, dropping even axum's implicit 2 MB floor, and raises rmcp's gate to unlimited. |
| `max_active_jobs` | int | `16` | Ceiling on concurrent background jobs per session. `0` disables. |
| `max_completed_jobs` | int | `128` | Ceiling on retained terminal job records per session (FIFO eviction). `0` disables. |
| `session_cap` | int (>0) | `32` | Ceiling on concurrent per-client sessions under `--transport http`. |
| `session_idle_timeout` | seconds (>0) | `14400` | Inactivity before an idle http session is swept. Also pins rmcp's streamable-HTTP keep-alive (rmcp's own default is 300 s), so the transport never tears a quiet session down before this sweeper would. |
| `sweep_parallel` | int (>0) | `4` | Max stale sessions the idle sweeper tears down concurrently per cycle. |
| `profile` | string | `full` | Tool-surface profile: `full` (every synthesised tool) or `core` (curated everyday subset). Unknown → `full` with a warning. Agent clients should prefer `core` (43 vs 71 tools on every request); see [MCP server](mcp.md). |
| `tools_allow` | array of strings | *(empty)* | Extra tool names to keep on top of the profile. |
| `tools_deny` | array of strings | *(empty)* | Tool names to remove regardless of profile/allow (deny wins last). E.g. `["updates", "openqa_overview"]` for a workflow that never lists the queue or polls openQA. |
| `audit_log` | path | *(unset)* | Durable audit sink for tool calls (versioned JSONL, `0600`): one or two records per call — an `intent` before and an outcome after for a tool without `readOnlyHint`, an outcome alone for a read-only one — plus a terminal record per background job. Unset disables auditing. A path that cannot be opened refuses to start the server; afterwards a failed pre-dispatch `intent` write refuses the call (nothing ran), while a failed outcome write returns the result with an in-band notice. See [MCP server](mcp.md#audit-log). |
| `audit_log_max_bytes` | bytes | `268435456` | Size at which `audit_log` rotates to `<path>.1`, keeping five generations (`.1`…`.5`); the oldest is discarded, so archiving beyond it is yours to arrange. Rotation never blocks the dispatch worker and never refuses a call: it takes the file's advisory lock without waiting, and a lock it cannot take or a rename it cannot perform warns once and keeps appending past the cap. `0` disables rotation entirely; any other value below `4096` is raised to it with a warning. |
| `put_dirs` | array of paths | *(empty)* | Opt-in allowlist for the MCP `put` tool's `source` argument: absolute directories on the `mtui-mcp` host whose files `put` may upload without the bytes passing through the call (a leading `~` is expanded; an empty or relative entry is dropped with an error). Empty disables `source`. List only directories that nobody but the operator can write. A later config file replaces the list wholesale. |

> OTLP export of the same record needs no TOML key by design: it is env-only
> (`OTEL_EXPORTER_OTLP_ENDPOINT` / `..._LOGS_ENDPOINT`, `..._HEADERS` /
> `..._LOGS_HEADERS`, `..._PROTOCOL` / `..._LOGS_PROTOCOL`, `OTEL_SERVICE_NAME`).
> Endpoint set with `audit_log` unset is OTLP-only; neither is auditing off.
> See [MCP server](mcp.md#otlp-log-export).

> The profile key is `profile`, not `tool_profile` — it already sits under the
> tool-scoped `[mcp]` table. `tools_allow`/`tools_deny` are native TOML arrays,
> not comma-separated strings. (Both matter if you are carrying over a config
> from a pre-26.0 release.)

### `[obs]`

The native OBS/IBS QAM review backend.

| Key | Type | Default | Meaning |
|-----|------|---------|---------|
| `api_url` | URL | `https://api.suse.de` | OBS/IBS API the review backend acts against. Must equal a section header in your `oscrc`. |
| `request_timeout` | seconds (>0) | `180` | Coarse wall-clock budget for a whole native OBS operation, checked *between* HTTP calls (not a mid-call hard kill). |

**Credentials** for OBS live only in your `oscrc`, not here. The oscrc is located
like `osc` itself: `$OSC_CONFIG` → `$XDG_CONFIG_HOME/osc/oscrc` → `~/.oscrc`.
There is no mtui-side path option — set `$OSC_CONFIG` to point at a non-default
oscrc.

## Secrets and credentials

- The **`gitea_token`** and **`slack_token`** are masked as `<set>` by both
  `config show` and `config set`, and are never written to logs or display
  output. `-g/--gitea-token` puts the Gitea token on the command line, where
  other local users can see it; prefer the config file.
- Configured datasource URLs may embed credentials
  (`scheme://user:pass@host`). mtui sanitises the userinfo to `***` before any
  URL reaches a log or an error message, keeping the host visible for diagnosis.
- A repository URL from the report metadata may carry userinfo
  (`https://user:token@host/repo/`) and is sent to `zypper` unchanged, so
  authenticated repositories keep working. Everything mtui records about the
  command — log lines, the timeout prompt, errors, and the command and output
  kept in the per-host log that `showlog` and `export` print — masks the userinfo
  to `***`, including a URL a remote tool echoes back in its output. A password
  that appears without a `scheme://…@` URL around it is not recognised.

## Example

```toml
[connection]
connection_timeout = 300
connect_timeout = 60
reboot_timeout = 10
reboot_retries = 10
max_parallel = 8

[refhosts]
resolvers = "https,path"

[obs]
api_url = "https://api.suse.de"

[mcp]
profile = "core"
tools_deny = ["put", "get"]
```

## Remote uploads

`put` (REPL and MCP) never opens its destination for writing. The bytes go to a
private `.mtui-upload-<uuid>` directory (mode `0700`) beside the destination,
the file is created there exclusively, given mode `0770` and verified, and then
`posix-rename@openssh.com` swaps it in under the destination name. The
destination path is exactly what it was before; only the mechanism changed.

Consequences to expect:

- An existing file, symlink or hardlink at the destination is **replaced**, not
  written through: the old inode and anything it was linked to stay as they
  were, and the new file is owned by the login user with mode `0770`.
- Every directory on the way must be a real directory owned by `root` or the
  login user and not writable by anyone else (a sticky world-writable directory
  such as `/tmp` is fine). Missing directories are created `0755`. Otherwise
  the host is refused, naming the directory; the destination is not relocated.
  `..` in a path is refused.
- The server must offer `posix-rename@openssh.com` (OpenSSH does). There is no
  slower fallback: a server without it fails the upload for that host.
- The login's numeric uid is read with a fixed `id -u` over the existing SSH
  connection, so a `put` runs one extra short command per host.
- Replacement is atomic but not durable: nothing is `fsync`ed.
- A `put` that is cancelled or loses its connection mid-transfer can leave an
  empty-or-partial private `.mtui-upload-*` directory behind; it is never
  visible under the destination name and is safe to delete. If the link dies
  after the final rename was sent, the host reports that the upload *may or may
  not* have been published, and mtui does not retry it.
- A sticky directory restricts replacing another user's file; as a non-root
  login, uploading over a file owned by someone else there fails with an
  explicit error.
