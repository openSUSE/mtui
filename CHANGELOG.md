# Changelog

All notable user-visible changes to MTUI are documented in this file.

The format is based on [Keep a Changelog 1.1.0](https://keepachangelog.com/en/1.1.0/),
and this project follows [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Fixed

- Transactional hosts no longer fail to reconnect after the post-update
  reboot. The reboot command was run through the normal command path,
  which on the (expected) dropped connection tried its own single,
  immediate reconnect and gave up with `Failed to reconnect … after 1
  retries` before the real retry loop ran. The reboot is now dispatched
  fire-and-forget (the disconnect is expected), then mtui reconnects with
  retries and backoff while the host comes back up.
- Reconnect backoff now works. `Target.reconnect` passed `backoff`
  positionally into `Connection.reconnect`, whose second positional
  parameter is `timeout` — so the backoff flag was silently dropped and
  the per-attempt wait collapsed to ~1s. It is now passed by keyword.

- `downgrade` now passes `--oldpackage` to zypper (and the
  `transactional-update pkg in` variant on transactional systems), so
  installing the previously released, lower version actually proceeds.
  Without it zypper refused to replace an installed package with an older
  one, leaving the host on the too-recent version.
- `prepare` no longer dumps a Python traceback when a host hits an
  expected error such as an unresolved dependency conflict. The preparer
  check already logs the actionable detail (the zypper resolutions to
  pick); the `UpdateError` it raises is now reported as a single concise
  `error: Prepare failed: <host>: <reason>` line instead of a stack
  trace. Genuinely unexpected errors still log a full traceback.

### Changed

- `commit` without `-m` no longer opens an editor to ask for a message.
  It now commits non-interactively with a generated message that reuses
  the testreport export footer, e.g. `committed from MTUI:<version>,
  paramiko <version> on <distro>-<verid> (kernel: <kernel>) by <user>`.
  Passing `-m` still uses the given message.
