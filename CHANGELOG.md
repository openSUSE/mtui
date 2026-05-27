# Changelog

All notable user-visible changes to MTUI are documented in this file.

The format is based on [Keep a Changelog 1.1.0](https://keepachangelog.com/en/1.1.0/),
and this project follows [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Fixed

- Loading an SLFO or PI update with no `GITEA_TOKEN` configured no longer
  crashes with an uncaught traceback. Missing tokens are now reported with
  a clear configuration hint and exit with a non-zero status. Transient
  Gitea API failures and hash mismatches raised during the post-checkout
  template retry now reach the same handlers (`TestReportNotLoadedError`
  and the force-continue prompt) as failures from the initial read.
- `GiteaError` now derives from `Exception` instead of `BaseException`,
  so the interactive command loop's catch-all handler traps Gitea errors
  cleanly instead of letting them tear down the prompt.

### Changed

- Internal refactor: `Target` was decomposed into four focused
  collaborators. Out-of-tree consumers that imported
  `mtui.target.Target` lose the following methods, all moved to
  collaborator properties on the same instance:
  - `target.get_installer()` / `get_uninstaller()` / `get_downgrader()`
    / `get_updater()` / `get_preparer()` and the five matching
    `..._check()` variants are now reached as
    `target.doer("installer")` / `target.check("installer")` etc.
    The preparer arm keeps its `(force, testing)` kwargs:
    `target.doer("preparer", force=True, testing=True)`.
  - `target.report_self()` / `report_history()` / `report_locks()` /
    `report_timeout()` / `report_sessions()` / `report_log()` /
    `report_products()` move to `target.reporter.self_()`,
    `target.reporter.history()`, etc.
  - `target.set_repo()` and `target.run_zypper()` move to
    `target.repo_manager.set()` and `target.repo_manager.run_zypper()`.
  - `target.query_package_versions()` is kept as a thin delegate but
    the rpm-vs-dpkg logic now lives in
    `target.package_querier`. All in-tree callers were migrated.

