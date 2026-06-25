[![CI](https://github.com/openSUSE/mtui/actions/workflows/ci.yml/badge.svg)](https://github.com/openSUSE/mtui/actions/workflows/ci.yml)
[![Codecov](https://codecov.io/github/openSUSE/mtui/branch/main/graph/badge.svg?token=60D3XUROAF)](https://codecov.io/github/openSUSE/mtui)
[![Ask DeepWiki](https://deepwiki.com/badge.svg)](https://deepwiki.com/openSUSE/mtui)

# MTUI

The Maintenance Test Update Installer (MTUI) allows you to run shell commands on multiple hosts in parallel.

In addition, MTUI provides convenience commands to help with maintenance update testing and integrating with other systems like Bugzilla and test report templates.

## Features

- Parallel SSH command execution across SUSE reference hosts (`run`, `prepare`, `update`, `install`, `downgrade`, …) with per-host `enabled` / `disabled` / `dryrun` states and `parallel` / `serial` execution modes.
- OBS / IBS maintenance-request workflow: `assign`, `unassign`, `approve`, `reject`, `comment`, each dispatched to either `osc` or Gitea depending on the request kind. `approve -r REVIEWER` records the reviewer in the testreport and commits to SVN in one step.
- openQA integration: `reload_openqa`, `set_workflow {auto,manual,kernel}`, and `openqa_overview` (port of `oqa-search`) which prints PASSED/FAILED/RUNNING per SLE version, aggregated-update builds, and parsed build-check summaries, with `--export` to inject the block into the testreport's `regression tests:` section.
- Reference-host lock management: cooperative `/var/lock/mtui.lock` files, automatic locking of every connected host while a Product Increment is under test (`[lock] pi_autolock`), and automatic reaping of stale locks left over from crashed sessions (`[lock] reap_stale`, `[lock] stale_age`).
- Test-report lifecycle: `load_template`, `checkout`, `commit`, `edit`, `export`, with SVN and Gitea checkout backends.
- Reference-host discovery: HTTPS- or filesystem-resolved `refhosts.yml` with location-aware fallback and configurable cache expiry, plus offline inventory search (`list_refhosts`) that filters the fleet by hostname glob, arch, product, version, addon, or testplatform query (and optionally probes live lock state) without connecting, locking, or loading a template.
- File transfer: `put` (glob upload to all hosts) and `get` (download with per-host filename suffix or recursive folder mode).
- Interactive `prompt_toolkit`-based shell: tab completion over the live command registry, persistent history with reverse-search (Ctrl-R), autosuggest-from-history (right-arrow to accept), lexer-highlighted command tokens, a bottom toolbar showing the loaded RRID, per-command `--help`, configurable log level, optional desktop notifications (`notify` extra), and OS-keyring credential storage (`keyring` extra).
- Shell-completion script via the `completion` extra (`register-python-argcomplete mtui`).
- MCP server (`mtui-mcp`, optional `mcp` extra): exposes every non-interactive mtui command as a [Model Context Protocol](https://modelcontextprotocol.io) tool, plus dedicated `testreport_read` / `testreport_patch` / `testreport_write` tools, so LLM clients can drive a headless mtui session over `stdio` or `http`.

## License

This project is licensed under the GPLv2 license, see the COPYING file for details.

## Contents

- [Installation](Documentation/installation.rst)
- [User Guide](Documentation/user.rst)
- [Developer Guide](Documentation/developer.rst)
- [Support](Documentation/support.rst)
- [FAQ](Documentation/faq.rst)

## Authors

MTUI was originally written by:

- Christian Kornacker
- Heiko Rommel <rommel@suse.de>
- Jan Matějka
- Roman Neuhauser
- David Santiago

The project is currently maintained mainly by:

- Ondřej Súkup <osukup@suse.cz>
- Jan Baier <jbaier@suse.cz>

Besides that, numerous other contributors have committed to MTUI. Thanks everyone for their contributions!
