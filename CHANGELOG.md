# Changelog

All notable user-visible changes to MTUI are documented in this file.

The format is based on [Keep a Changelog 1.1.0](https://keepachangelog.com/en/1.1.0/),
and this project follows [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- New `reboot` command: reboots all connected reference hosts (or only
  those given with `-t`/`--target`) and reconnects automatically, with
  retries and backoff, once each host is back up. Works for both
  transactional and non-transactional hosts. After reconnecting, the host's
  boot id (`/proc/sys/kernel/random/boot_id`) is compared with the value
  taken before the reboot; if it is unchanged an error is logged because the
  host did not actually reboot. While testing a Product Increment, the
  per-host testing lock is re-applied after the reboot (a reboot clears
  `/var/lock`), so it is not lost.

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
- Transactional-update systems are detected more reliably. Detection
  previously only checked `/usr/etc/transactional-update.conf`, so older
  transactional hosts (SLE Micro 5.x, openSUSE MicroOS) that keep the
  config in `/etc/transactional-update.conf` were misdetected as
  non-transactional. Both locations are now probed.

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
- `openqa_overview --no-aggregated` now correctly omits the aggregated
  updates section from exported logs. Previously the section appeared with
  a "No aggregated updates builds available" message even when explicitly
  skipped, making it look like data was missing rather than intentionally
  excluded.
- Test result summaries in `openqa_overview` now correctly parse the
  `# PASS: N` / `# FAIL: N` format used in OBS build logs. Previously only
  the `N passed` / `N failed` format was recognized, causing OBS test
  counts to be reported as zero.

### Changed

- `commit` without `-m` no longer opens an editor to ask for a message.
  It now commits non-interactively with a generated message that reuses
  the testreport export footer, e.g. `committed from MTUI:<version>,
  paramiko <version> on <distro>-<verid> (kernel: <kernel>) by <user>`.
  Passing `-m` still uses the given message.
