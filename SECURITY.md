# Security Policy

## Supported versions

Only the current `main` branch receives security fixes. There are no
maintained release branches.

## Reporting a vulnerability

Please report suspected security issues privately, either via GitHub's
[**Report a vulnerability**](https://github.com/openSUSE/mtui/security/advisories/new)
(Security → Advisories) or by mail to **qa-maintenance@suse.de**.

Do not open a public GitHub issue or pull request for a suspected
vulnerability before it has been triaged.

When reporting, include as much of the following as you can:

- A description of the issue and its potential impact.
- Steps to reproduce or a proof of concept.
- The mtui version (`mtui -V`).
- Any relevant configuration (with secrets redacted).

## Response expectations

MTUI is internal QA tooling and has no dedicated security responder.
Reports are handled on a best-effort basis by the SUSE QA Maintenance
team; there is no committed response time or fix-time SLA.

## Scope

In scope: defects in mtui itself. The security-relevant areas are:

- **SSH.** Host-key verification against `~/.ssh/known_hosts` (including
  hashed entries) and remote command construction (shell quoting). mtui is
  **pubkey-only by design** — password authentication is deliberately
  absent and should not be added.
- **Credential handling.** Service credentials (Gitea token, `oscrc` for
  OBS, openQA API secrets, and any chat integration token) must never reach
  terminal output, logs, or MCP responses. Secret config fields are
  classified by `is_secret_attr` and masked as `<set>` by both
  `config show` and `config set`; a change adding a credential field must
  add it there in the same commit.
- **URLs in logs.** URLs may carry credentials in userinfo or query
  parameters, so they are passed through `mtui_datasources::sanitize_url`
  before being logged.
- **The MCP server.** `mtui-mcp --transport http` binds loopback
  (`127.0.0.1`) by default and relies on rmcp's DNS-rebinding guard. It
  exposes mtui commands — including ones that mutate remote hosts — to
  whatever can reach that port.
- The configuration loader and the OBS / openQA / Gitea connectors.

Out of scope: vulnerabilities in upstream crates (`russh`, `rmcp`,
`reqwest`, `tokio`, …). Please report those to the respective upstream
projects; we will bump the dependency once an upstream fix is available.

## Dependency and code scanning

Pull requests are checked by `dependency-review`, which blocks a change
introducing a dependency with a known high-severity advisory. The code and
the workflows themselves are scanned by CodeQL, the repository is assessed
by OpenSSF Scorecard, and Dependabot keeps Cargo and Actions dependencies
current and opens security updates for known-vulnerable crates.
