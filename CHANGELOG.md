# Changelog

All notable user-visible changes to MTUI are documented in this file.

The format is based on [Keep a Changelog 1.1.0](https://keepachangelog.com/en/1.1.0/),
and this project follows [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- New `mtui-mcp` console script (optional `mcp` extra) ships a
  FastMCP server that exposes every non-interactive mtui command as
  a Model Context Protocol tool, plus dedicated `testreport_read` /
  `testreport_patch` / `testreport_write` tools, so LLM clients can
  drive a headless mtui session over `stdio` or `http`. See
  `Documentation/mcp.rst` for the deny-list, single-session caveat,
  and a `read → patch → read` worked example.

### Fixed

- `openqa_overview` now shows build check logs for all packages in a
  multi-package update, not just one. Previously only logs matching a
  single package name extracted from the build string were displayed,
  causing build check results for other packages in the update to be
  silently omitted.
