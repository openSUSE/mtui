# Configuration

mtui-rs is configured with a **sectioned TOML** file. (This is an intentional
deviation from upstream mtui's INI; every option's default matches upstream
exactly.) All configuration is optional: with no file present, mtui-rs runs on
built-in defaults.

## File resolution

The config file is resolved in this order:

1. **`--config <file>`** — an explicit path. Short-circuits the search.
2. **`$MTUI_CONF`** — an explicit path in the environment. Short-circuits the
   search.
3. Otherwise the default pair is merged, **lowest precedence first**:
   - `/etc/mtui.toml` (system-wide)
   - `~/.mtui.toml` (home dotfile)
   - `$XDG_CONFIG_HOME/mtui/mtui.toml` (per-user XDG; falls back to
     `~/.config/mtui/mtui.toml`)

   When several of these exist they are merged: an option set in a
   higher-precedence file overrides the same option in a lower one. The per-user
   XDG file therefore wins over `/etc` on shared keys.

Loading is **lenient**: a missing or malformed file — or a single bad value — is
logged at `ERROR` and skipped, falling back to the default. A bad value never
invalidates the rest of the file, and loading never hard-fails.

`~` is expanded to your home directory in path-valued options.

## Inspecting and changing config at runtime

The `config` command shows the resolved configuration and can set individual
options for the running session:

```
config show                 # print all resolved options
config show connection_timeout
config set max_parallel 8
```

Secret options are never echoed: `config show`/`config set` print `<set>` for a
configured secret (currently `gitea_token`) instead of its value.

## Options

Defaults below are the built-in (upstream-matching) values.

### `[mtui]`

| Key | Type | Default | Meaning |
|-----|------|---------|---------|
| `template_dir` | path | `$TEMPLATE_DIR` or `.` | Directory holding checked-out testreport templates. |
| `tempdir` | path | `$TMPDIR` or `/tmp` | Local scratch directory. |
| `user` | string | current login user | User attributed to this session (locks, logs). |
| `install_logs` | relative dir name | `install_logs` | Sub-directory (single relative name, no separators) where install logs are written per update. |
| `chdir_to_template_dir` | bool | `false` | `chdir` into the template dir on load. |
| `ssl_verify` | bool / string | `true` | TLS verification for outbound HTTP: a boolean, a boolean spelling (`yes`/`no`/`on`/`off`/…), or a path to a custom CA bundle. |

### `[connection]`

| Key | Type | Default | Meaning |
|-----|------|---------|---------|
| `connection_timeout` | seconds (>0) | `300` | SSH connect + command timeout. |
| `max_parallel` | int (>0) | `50` | Max hosts to fan out to concurrently (SSH/SFTP/lock/connect batches). |
| `max_oqa_parallel` | int (>0) | `8` | Max concurrent openQA/QAM HTTP requests in the overview search (kept low to be polite to shared hosts). |
| `ssh_strict_host_key_checking` | string | `auto_add` | SSH host-key checking policy (`auto_add`, `strict`, `warn`, …). |

> **SSH is pubkey-only by design.** mtui-rs authenticates from the SSH agent or
> `~/.ssh/id_*`; there is no password auth.

### `[refhosts]`

| Key | Type | Default | Meaning |
|-----|------|---------|---------|
| `resolvers` | string | `https,path` | Comma-separated ordered list of refhosts resolvers. |
| `https_uri` | URL | `https://qam.suse.de/refhosts/refhosts.yml` | HTTPS URI of the refhosts database. |
| `https_expiration` | seconds (>0) | `43200` (12h) | Age before a cached HTTPS fetch is considered stale. |
| `path` | path | `/usr/share/qam-metadata/refhosts.yml` | Local filesystem refhosts database. |

### `[url]`

| Key | Type | Default | Meaning |
|-----|------|---------|---------|
| `bugzilla` | URL | `https://bugzilla.suse.com` | Bugzilla base URL. |
| `testreports` | URL | `https://qam.suse.de/testreports` | Testreports base URL. |
| `fancy_reports` | URL | `https://qam.suse.de/reports` | "Fancy" reports base URL. |

### `[svn]`

| Key | Type | Default | Meaning |
|-----|------|---------|---------|
| `path` | string | `svn+ssh://svn@qam.suse.de/testreports` | SVN base path for testreport checkout. |

### `[target]`

| Key | Type | Default | Meaning |
|-----|------|---------|---------|
| `tempdir` | path | `/tmp` | Remote scratch directory on target hosts. |

### `[qem_dashboard]`

| Key | Type | Default | Meaning |
|-----|------|---------|---------|
| `api` | URL | `http://dashboard.qam.suse.de/api` | QEM Dashboard API base URL. |

### `[teregen]`

| Key | Type | Default | Meaning |
|-----|------|---------|---------|
| `api` | URL | `https://qam.suse.de/api/v1` | TeReGen report/queue API base URL. |

### `[openqa]`

| Key | Type | Default | Meaning |
|-----|------|---------|---------|
| `openqa` | URL | `https://openqa.suse.de` | openQA instance URL. |
| `baremetal` | URL | `http://openqa.qam.suse.cz` | Baremetal openQA instance URL. |
| `distri` | string | `sle` | openQA install `distri` parameter. |

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

Remote-lock behaviour on target hosts (interoperable with Python mtui on a shared
fleet).

| Key | Type | Default | Meaning |
|-----|------|---------|---------|
| `reap_stale` | bool | `true` | On connect, force-remove a pre-existing lock older than `stale_age`, regardless of owner. |
| `stale_age` | seconds | `86400` | Age beyond which a lock is stale and reapable. `0` disables reaping. |
| `pi_autolock` | bool | `true` | When testing a Product Increment (PI), auto-lock all refhosts on `assign` and unlock at end of testing. |
| `wait` | seconds | `0` | Pool-claim queueing budget. `0` fails fast on a busy host. |
| `wait_poll` | seconds (>0) | `15` | Poll interval while waiting for a busy pool lock to free. |

### `[mcp]`

`mtui-mcp` server behaviour. See [MCP server](mcp.md).

| Key | Type | Default | Meaning |
|-----|------|---------|---------|
| `max_output_bytes` | bytes | `100000` | Upper bound on a single tool result (truncated at the tail with a notice). `0` disables the cap. |
| `max_request_bytes` | bytes | `10000000` | Upper bound on an inbound HTTP request body under `--transport http` (oversized requests are rejected with `413` before rmcp buffers them). `0` disables mtui's limit entirely, dropping even axum's implicit 2 MB floor. |
| `max_active_jobs` | int | `16` | Ceiling on concurrent background jobs per session. `0` disables. |
| `max_completed_jobs` | int | `128` | Ceiling on retained terminal job records per session (FIFO eviction). `0` disables. |
| `session_cap` | int (>0) | `32` | Ceiling on concurrent per-client sessions under `--transport http`. |
| `session_idle_timeout` | seconds (>0) | `1800` | Inactivity before an idle http session is swept. |
| `sweep_parallel` | int (>0) | `4` | Max stale sessions the idle sweeper tears down concurrently per cycle. |
| `profile` | string | `full` | Tool-surface profile: `full` (every synthesised tool) or `core` (curated everyday subset). Unknown → `full` with a warning. |
| `tools_allow` | array of strings | *(empty)* | Extra tool names to keep on top of the profile. |
| `tools_deny` | array of strings | *(empty)* | Tool names to remove regardless of profile/allow (deny wins last). |

> Upstream names the profile key `tool_profile`; here it is `profile` under the
> already tool-scoped `[mcp]` table. `tools_allow`/`tools_deny` are native TOML
> arrays rather than upstream's comma-separated strings.

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

- The **`gitea_token`** is masked as `<set>` by both `config show` and
  `config set`, and is never written to logs or display output.
- Configured datasource URLs may embed credentials
  (`scheme://user:pass@host`). mtui-rs sanitises the userinfo to `***` before any
  URL reaches a log or an error message, keeping the host visible for diagnosis.

## Example

```toml
[connection]
connection_timeout = 300
max_parallel = 8

[refhosts]
resolvers = "https,path"

[obs]
api_url = "https://api.suse.de"

[mcp]
profile = "core"
tools_deny = ["put", "get"]
```
