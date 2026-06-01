# Changelog

All notable user-visible changes to MTUI are documented in this file.

The format is based on [Keep a Changelog 1.1.0](https://keepachangelog.com/en/1.1.0/),
and this project follows [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Changed

- `commit` without `-m` no longer opens an editor to ask for a message.
  It now commits non-interactively with a generated message that reuses
  the testreport export footer, e.g. `committed from MTUI:<version>,
  paramiko <version> on <distro>-<verid> (kernel: <kernel>) by <user>`.
  Passing `-m` still uses the given message.
