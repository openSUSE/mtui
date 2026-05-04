# Security Policy

## Supported versions

Only the current `main` branch receives security fixes. There are no
maintained release branches.

## Reporting a vulnerability

Please report suspected security issues privately to
**qa-maintenance@suse.de**.

Do not open a public GitHub issue or pull request for a suspected
vulnerability before it has been triaged.

When reporting, include as much of the following as you can:

- A description of the issue and its potential impact.
- Steps to reproduce or a proof of concept.
- The mtui version (`mtui -V`) and the Python / paramiko / openqa-client
  versions.
- Any relevant configuration (with secrets redacted).

## Response expectations

MTUI is internal QA tooling and has no dedicated security responder.
Reports are handled on a best-effort basis by the SUSE QA Maintenance
team; there is no committed response time or fix-time SLA.

## Scope

In scope: defects in mtui itself (the CLI, the SSH connection layer,
the configuration loader, the OBS / openQA / Gitea connectors).

Out of scope: vulnerabilities in upstream dependencies (`paramiko`,
`openqa-client`, `requests`, `ruamel.yaml`, `pyxdg`, `osc`, …). Please
report those directly to the respective upstream projects; we will
update the dependency once an upstream fix is available.
