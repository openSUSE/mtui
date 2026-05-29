# MTUI Improvement Plan

This plan was distilled from a read-only review of the MTUI codebase, tests,
CI, and documentation. It is organised as a sequence of shippable phases, one
PR per phase, with deeper architectural refactors gated behind a
characterization-test safety net.

- **Strategy**: tests-first for refactors.
- **Branching**: one PR per phase (Phase 5 split into clusters for
  reviewability).
- **Conventions**: Conventional Commits; CI must stay green; coverage ratchet
  enforced from Phase 3 onwards.
- **Decisions already locked**:
  - README.md is canonical; `README.rst` will be removed and Sphinx will
    consume the MD via `myst-parser`.
  - SSH host-key policy gains a config knob; default behaviour stays
    backward compatible (auto-add).
  - Codecov floor = current % − 1, ratchet upward over time.
  - Track C deep refactors in scope (`C2`, `C4`, `C10`, `C11`) plus extras
    `C1`, `C3`, `C5`, `C6`, `C7`, `C8`, `C9`.

---

## Phases 1–4 Summary — ✅ DONE

Foundational work shipped across four PRs. Test count grew from baseline
to 351; coverage instrumentation and CI hardening landed before any
refactors began.

### Phase 1 — Quick wins (PR #1)
Mechanical low-blast-radius fixes: switched to `contextlib.chdir`,
`shlex.split` for argument parsing, narrowed `BaseException` catches,
fixed call-stack walking in `colorlog`, stripped literal `\n` from
argparse help strings, repaired `.gitignore`, removed legacy
`Documentation/README`, replaced the `Documentation/index.rst` symlink
with a proper include, and synced docs with the actual command set
(`analyze_diff`, `show_diff`, `comment`, `EOF`).

### Phase 2 — Latent bugs + test foundation (PR #2)
Configured pytest (`addopts`, `testpaths`, registered markers), fixed
`Target.__hash__`/`__eq__` contract, replaced silent
`except Exception: pass` with debug logs, narrowed `BaseException`
handlers in `actions.py`/`target.py`/`refhost.py`/`config.py`/
`commands/__init__.py`, added warning when SSH port falls back to 22,
introduced the `ssh_strict_host_key_checking` config knob
(`auto_add`/`warn`/`reject`), and added CLI smoke tests with a new
`mtui/__main__.py` entrypoint. Also unblocked `mtui/target/` from a
mis-anchored gitignore rule.

### Phase 3 — CI / tooling (PR #3)
Committed `uv.lock`, cleaned up Phase 1/2 ruff and ty carry-overs,
replaced `requirements_*.txt` with `uv sync`, dropped `osc` from the
Python install set (it's a CLI not an import), extended the CI matrix
to 3.11/3.12/3.13/3.14, widened ruff scope to the whole repo, reset
Codecov to a 56% floor (current 57% − 1) with 80% patch target,
aligned mergify on `main`, added `[tool.ty]` config with strict
overrides for `mtui/types/**` and `mtui/connector/**`
(`error-on-warning = true`), added `.pre-commit-config.yaml`, added
dependabot for pip and github-actions, and fixed `MANIFEST.in` to
recursively include Documentation.

### Phase 4 — UX & documentation hygiene (PR #4)
Added `CHANGELOG.md` (Keep-a-Changelog 1.1.0) with backfilled Phase
1–3 sections, `CONTRIBUTING.md`, `CODE_OF_CONDUCT.md` (Contributor
Covenant 2.1), `SECURITY.md`, GitHub issue forms (blank issues
disabled), and a PR template. Fixed `Documentation/conf.py` (dropped
non-existent templates path, switched theme to `alabaster`, refreshed
copyright), deleted the empty static dir, deleted `README.rst` in
favour of `README.md` via `myst-parser`, rewrote
`Documentation/installation.rst` with full source-build coverage,
documented `ssh_strict_host_key_checking` in `cfg.rst`, rewrote
`developer.rst` with project layout / dev setup / CI gates / how-to-
add-a-command walkthrough. UX: `--color {auto,always,never}` flag
honouring `NO_COLOR` and `isatty()` (default behaviour change: colours
now off when stderr isn't a TTY); `-V/--version` prints versions of
`mtui`, Python, paramiko, and openqa-client; optional `[completion]`
extra providing `argcomplete` integration.

---

## Phase 5a — Characterization tests (PR #5) — ✅ DONE

Tests-first safety net before any internal restructuring. Each module
got public-API tests that capture current behaviour so refactors can be
verified.

Status: shipped on `main` in 6 `test(...)` commits
(`349b972`..`38e8d4f`) plus a `chore: refresh uv.lock` (`35d0811`),
a coverage-floor bump to 66 % (`48a32a1`), and a follow-up changelog
note documenting the surfaced config bugs (`599c518`). One unrelated
bug fix (`6894757`, recover missing install logs in dashboard) and an
agent-guidelines doc (`48cddbf`) also landed on the same branch.

Test count rose from 351 (end of Phase 4) to 532. Coverage rose from
57 % to 67 %; Codecov project floor bumped from 56 % to 66 %.

| Module | Reference | Commit |
| --- | --- | --- |
| `mtui/config.py` — env var, malformed INI, location setter, typed-getter parse failures | `mtui/config.py` | ✅ `349b972` |
| `mtui/prompt.py` — notify, history, dispatch failures, `cmdloop` error paths | `mtui/prompt.py` | ✅ `1c9e945` |
| `mtui/refhost.py` — `Attributes`, `Refhosts`, resolver factory | `mtui/refhost.py` | ✅ `43c0334` |
| `mtui/target/target.py` — connect, queries, `run_zypper`, sftp helpers, `report_*` sinks | `mtui/target/target.py` | ✅ `b22488b` |
| `mtui/target/hostgroup.py` — four `perform_*` flows + group helpers | `mtui/target/hostgroup.py` | ✅ `ce0272f` |
| `mtui/connection.py` — reconnect, sftp lifecycle, remainder of `run()` | `mtui/connection.py` | ✅ `38e8d4f` |

**Surfaced bugs** (documented under `[Unreleased] / Known issues` in
`CHANGELOG.md` rather than fixed in the same PR per the
characterization-tests-first discipline):

- `Config.__init__` crashes with `ValueError` on a non-numeric
  `connection_timeout` instead of falling back to the documented 300 s
  default.
- The error path for the `configparser.getint`/`getboolean` config
  options has a malformed format string, so a bad value is silently
  replaced with the default and no log line is emitted.

**Carry-overs surfaced during Phase 5a (handled in later phases):**

- The two config bugs above are deferred to Phase 5b / C10
  (`Config` → `@dataclass ConfigOption`; raise on parse failure).
- The new coverage % (67 %) is the new ratchet baseline; the floor sits
  at current − 1 = 66 % and ratchets upward as Phase 5b/6 land.

---

## Phase 5b — Track C refactors (PR #6 … PR #N)

Each cluster is an independent PR within the Phase 5 track and is
driven by the characterization tests added in Phase 5a.

| Order | ID  | Change | Status |
| --- | --- | --- | --- |
| 1   | C9  | Introduce `Enum`s for target state (`enabled/dryrun/disabled`, `serial/parallel`) and request kind (`S/M/P/SLFO/Maintenance`); use `match/case` where it clarifies. | ✅ DONE — see below |
| 2   | C5  | Replace filesystem-glob + `globals()` plugin loading with `Command.__init_subclass__` registry (or `@register` decorator). | ✅ DONE — see below |
| 3   | C1  | Scope `actions.queue` to `HostsGroup` (or pass per call); kill the module-level mutable global. Modernised in flight: replaced the hand-rolled `Queue`+`ThreadedMethod` machinery with `concurrent.futures.ThreadPoolExecutor` since killing the global meant rewriting all the call sites anyway, and the executor surfaces worker exceptions instead of swallowing them. | ✅ DONE — see below |
| 4   | C7  | Add `@contextmanager _sftp(self)` and reuse the SFTP client across multi-step operations. Wider scope (per plan-mode review): also expose a public `Connection.sftp_session()` so external callers can batch their own multi-step flows; migrate `parsers/system.parse_system` as the first such caller. | ✅ DONE — see below |
| 5   | C3  | `class Operation` template method consolidating install/uninstall in `HostsGroup`. Scope narrowed during plan-mode review: `perform_prepare`, `perform_downgrade`, and `perform_update` have unique extras (per-package loops, fanout-set-repo, nested two-phase try/finally for guaranteed repo cleanup) that would force the template to grow several optional hooks until it became harder to read than the originals. | ✅ DONE — see below |
| 6   | C2  | Decompose `Target` into `PackageQuerier` (rpm/dpkg), `RepoManager` (zypper), `Reporter` (the seven `report_*` sinks) and a `Doer` registry replacing the seven `get_*er` methods. | ✅ DONE — see below |
| 7   | C4  | `CommandPrompt`: register `do_/help_/complete_` methods at construction; remove the per-attribute `__getattr__` synthesis. | ✅ DONE — see below |
| 8   | C10 | `Config` → `@dataclass ConfigOption`; raise on parse failure (or log explicitly). Address the existing `# FIXME` at `mtui/config.py:73`. Closes the two Phase 5a config-bug carry-overs. | ✅ DONE — see below |
| 9   | C11 | Split `Refhosts.Attributes` into a `TypedDict`/`dataclass` schema; split resolvers into separate classes. | ✅ DONE — see below |
| 10  | C6  | Move user prompting out of the SSH worker thread; surface via callback/event to the main thread. | ✅ DONE — see below |
| 11  | C8  | Split `mtui/utils.py` into `term.py`, `completion.py`, `fileops.py`, `colors.py`. | ✅ DONE — see below |

### Phase 5b / C9 — Domain enums (PR #6 / openSUSE/mtui#111) — ✅ DONE

Status: shipped on branch `phase_five_b1` in 8 commits
(`1977f67`..`05b055f`); PR https://github.com/openSUSE/mtui/pull/111.
All gates green: `uv run ruff format --check .` (163 files), `uv run
ruff check .` clean, `uv run ty check` clean (no warnings;
`error-on-warning` is on with the strict `mtui/types/**` and
`mtui/connector/**` overrides), `uv run pytest` 549 passed
(Phase 5a baseline 532 + 17 new in `tests/test_enums.py`).

| ID  | Change | Reference | Status |
| --- | --- | --- | --- |
| C9.1 | Add three enums under `mtui/types/enums.py`: `TargetState` (StrEnum, enabled/dryrun/disabled), `ExecutionMode` (plain Enum, parallel/serial), `RequestKind` (plain Enum, SLFO/Maintenance/PI with a `from_token()` classmethod accepting both long and `S`/`M`/`P` short forms). Re-exported from `mtui.types`. | `mtui/types/enums.py`, `mtui/types/__init__.py` | ✅ `1977f67` |
| C9.2a | Drop invalid `serial`/`parallel` values from `tests/test_target.py::test_target_init_with_state` (preparatory test-bug fix; the values were never valid `state`s, just hidden by the wrong Literal annotation). Standalone commit so the next refactor commit stays green and a future bisect blames the right change. | `tests/test_target.py` | ✅ `419ec77` |
| C9.2b | Wire `TargetState` into `Target` (`Literal[...]` → `TargetState | str`, runtime coercion via `TargetState(state)`); convert the three-branch `if/elif self.state == ...` chains in `Target.run` and `Target.query_versions` to `match/case`. Two-branch sites (`sftp_put`, `sftp_get`, `add_history`) keep `==` (works under StrEnum). `mtui/display.py` learns the enum members for readability. Drops two stale `# ty: ignore[invalid-assignment]` workarounds in `tests/test_target.py`. | `mtui/target/target.py`, `mtui/display.py`, `tests/test_target.py` | ✅ `f9f7198` |
| C9.3 | Replace `Target.exclusive: bool` → `Target.mode: ExecutionMode` (default `PARALLEL`). Updates the four call sites: `mtui/target/actions.py` (`is ExecutionMode.SERIAL` instead of truthy bool), `mtui/commands/hoststate.py` (the `if state in [...]` ladder becomes `match/case`; the `serial`/`parallel` arm constructs the enum via `ExecutionMode(state)`), `mtui/display.py::list_host` (accepts the enum, renders via `.value`; drops the inline ternary), `mtui/target/target.py::report_self` (Callable signature now advertises `ExecutionMode` instead of `bool`). | `mtui/target/target.py`, `mtui/target/actions.py`, `mtui/commands/hoststate.py`, `mtui/display.py`, `tests/test_target.py`, `tests/test_display.py` | ✅ `04b10bb` |
| C9.4 | Wire `RequestKind` into `RequestReviewID` (`if/elif self.kind == "M"...` chain → `RequestKind.from_token(raw_kind)`; `__str__`/`__repr__` render via `.value` so the `SUSE:SLFO:1.2:3` wire form is byte-identical). Migrate the seven production comparison sites: `mtui/connector/qem_dashboard.py:63`, `mtui/connector/oscqam.py:69` (the `in (...)` tuple), `mtui/connector/openqa/base.py:39`, `mtui/commands/apicall.py:49`, `mtui/types/updateid.py:136,138`. Test migration: `tests/test_types.py:43` (`!= "SLFO"` → `is not RequestKind.SLFO`) and `tests/test_oscqam.py:91` (the valid `"PI"` mock assignment → `RequestKind.PI`). | `mtui/types/rrid.py`, `mtui/connector/qem_dashboard.py`, `mtui/connector/oscqam.py`, `mtui/connector/openqa/base.py`, `mtui/commands/apicall.py`, `mtui/types/updateid.py`, `tests/test_types.py`, `tests/test_oscqam.py` | ✅ `60c7014` |
| C9.5 | New `tests/test_enums.py` with 17 focused tests: `TargetState` StrEnum equality / match-case-against-raw-string / unknown-raises; `ExecutionMode` two-member surface, CLI-vocabulary round-trip, no-string-equality (typo-catching); `RequestKind` canonical values, `from_token()` parametrised matrix (six long/short combos), `"SLE"` rejection (locks in the C9.6 fixture fix), no-string-equality. | `tests/test_enums.py` (new) | ✅ `56bc147` |
| C9.6 | Fix the two `rrid.kind = "SLE"` fixture typos surfaced by C9.4 (the `MagicMock` assignments bypass `RequestKind.from_token` so the bug had been latent for years). The conftest fixture's own `__str__` already advertised `"SUSE:Maintenance:..."`, confirming the intent was `MAINTENANCE`. `tests/test_oscqam.py::test_no_skip_template_for_sle` docstring updated to reflect the actual invariant ("not PI and not SLFO"). | `tests/conftest.py:136`, `tests/test_oscqam.py:101` | ✅ `552437d` |
| C9.7 | `CHANGELOG.md` `[Unreleased] / Fixed` entry documenting the one user-visible side-effect: `Target(state=...)` now raises `ValueError` on invalid values (previously silently accepted `serial`/`parallel`). Per AGENTS.md, the rest of C9 is internal refactor and does not warrant a changelog line. | `CHANGELOG.md` | ✅ `05b055f` |

**Verification**: `uv run ruff format --check .`, `uv run ruff check .`,
`uv run ty check`, `uv run pytest`, plus manual `uv run python -m mtui
--help` and `uv run python -m mtui -V`. Also confirmed
`rg "Literal\[.*enabled.*disabled.*serial.*parallel.*\]" mtui/`
returns zero hits (the stale Literal is gone).

**Carry-overs surfaced during Phase 5b / C9 (handled in later
clusters or PRs):**

- None. The two test-fixture bugs surfaced (C9.2a invalid
  `serial`/`parallel` values and C9.6 `"SLE"` typos) were both fixed
  in dedicated `fix(test):` follow-up commits inside this PR per the
  agreed policy.
- The pre-existing LSP-only complaints in `tests/test_target.py`
  (`MagicMock` arguments to typed `lock` / `connection` parameters) and
  `mtui/target/actions.py:252` (`thread possibly unbound`) remain;
  they predate C9 and are out of scope.

### Phase 5b / C1 — ThreadPoolExecutor for parallel target ops (PR ?) — ✅ DONE

Status: shipped on branch `phase_five_b3` in 4 Conventional Commits
(`ce8f738`..`fe2c03a`) plus the doc updates here. All gates green:
`uv run ruff format --check .` (167 files), `uv run ruff check .`
clean, `uv run ty check` clean, `uv run pytest` 590 passed
(C9 baseline 549 + 36 commands tests merged since C9 + 5 new in
`tests/test_actions.py`).

The plan literally said "scope `actions.queue` to `HostsGroup` (or
pass per call); kill the module-level mutable global". Killing the
global meant rewriting every call site anyway, so the in-flight
modernisation went one step further: replace the bespoke
`Queue`+`ThreadedMethod`+busy-poll-spinner machinery with
`concurrent.futures.ThreadPoolExecutor`. The bespoke implementation
also silently swallowed any exception raised by a worker callable
(no `.result()` ever called) — pytest had been emitting
`PytestUnhandledThreadExceptionWarning` for years; the executor's
`Future.result()` surfaces them.

| ID  | Change | Reference | Status |
| --- | --- | --- | --- |
| C1.1 | Pin the desired exception-propagation contract with two `xfail(strict=True)` regression tests in `tests/test_actions.py` (one against `ThreadedTargetGroup` via `FileDelete`, one against `RunCommand`). They flip to passing the moment the executor lands; the strict marker means a forgotten flip would also break CI. | `tests/test_actions.py` (new) | ✅ `ce8f738` |
| C1.2 | Extract `HostsGroup._fanout_set_repo(operation, testreport)` and replace the four open-coded `for t: queue.put(...) ; while queue.unfinished_tasks: spinner() ; queue.join()` blocks in `perform_prepare`, `perform_downgrade`, and the two sites inside `perform_update` with calls to it. Pure inlining: the helper still drives the module-level `queue` + `ThreadedMethod` + `spinner` for now, so the existing test patches against `mtui.target.hostgroup.queue` etc. stay valid and the next commit can swap the mechanism without touching the perform_* call sites again. | `mtui/target/hostgroup.py` | ✅ `55d8370` |
| C1.3 | Move the worker-pool bootstrap inside `_fanout_set_repo`. `update_lock` used to spawn one `ThreadedMethod` per locked host as a side effect; the perform_* flows that came afterwards relied on those workers picking up the `set_repo` tasks within the 10s `Queue.get` timeout. Coupling-by-side-effect plus a latent timing bug (slow lock IO would have killed the workers before the puts arrived). Make the helper own its workers, drop the spawn from `update_lock`. | `mtui/target/hostgroup.py` | ✅ `534bcbe` |
| C1.4 | Rewrite `mtui/target/actions.py` around `ThreadPoolExecutor`. New module-level `run_parallel(work)` helper builds one pool sized to the work, submits each `(callable, args)` pair, iterates `as_completed`, and re-raises the first worker exception via `Future.result()`. `ThreadedTargetGroup.run` collapses to a 1-line call into `run_parallel`; `RunCommand.run` does the same for parallel hosts and a plain for-loop with `prompt_user` between iterations for serial hosts (one worker is not a pool). `KeyboardInterrupt` still prints the original "stopping command queue" line and closes target sessions before re-raising. `_fanout_set_repo` becomes a one-liner against `run_parallel`. Drops `queue`, `ThreadedMethod`, `spinner`, `mk_thread`, `mk_threads`, `setup_queue`. Net −163 LOC in production. The hostgroup test patches against `queue`/`ThreadedMethod`/`spinner` are dropped (the symbols are gone); the two tests that inspected `mock_queue.put.call_args_list` to confirm add-then-remove repo lifecycle now inspect `mock_fanout.call_args_list` against the helper itself — same invariant, observed at a more honest level. `tests/test_actions.py` grows to five tests covering exception propagation through both `ThreadedTargetGroup` and `RunCommand`, the empty-work-list edge, the `(callable, args)` dispatch contract, and the serial-mode prompt path. Two stale `# ty: ignore[invalid-argument-type]` directives in `tests/test_hostgroup.py` (now redundant after the patch reduction tightened the inferred type) are dropped. | `mtui/target/actions.py`, `mtui/target/hostgroup.py`, `tests/test_actions.py`, `tests/test_hostgroup.py` | ✅ `fe2c03a` |
| C1.5 | `CHANGELOG.md` `[Unreleased] / Fixed` entry documenting the one user-visible side-effect: exceptions raised by parallel target operations now surface to the caller and abort the enclosing flow instead of being silently swallowed. | `CHANGELOG.md` | ✅ this commit |

**Verification**: `uv run ruff format --check .`, `uv run ruff check .`,
`uv run ty check`, `uv run pytest`, plus manual `uv run python -m mtui
--help` and `uv run python -m mtui -V`. Also confirmed
`rg "queue\.put|queue\.unfinished_tasks|queue\.join\(\)|ThreadedMethod\(" mtui/`
returns zero hits (the hand-rolled global is gone).

**Carry-overs surfaced during Phase 5b / C1 (handled in later
clusters or PRs):**

- The visual `processing... [|/-\\]` spinner is gone — it was bound to
  the busy-poll `while queue.unfinished_tasks` loop that the executor
  replaced. Most users won't notice (per-host worker output dominates),
  but if asked-for it's a 10-line follow-up using
  `concurrent.futures.wait(futures, timeout=0.1)`.
- `Phase 5b / C9` carry-over still stands: pre-existing LSP-only
  complaints in `tests/test_target.py` (`MagicMock` for typed
  `lock` / `connection` parameters). The `mtui/target/actions.py:252`
  carry-over from C9 is now obsolete (the file was rewritten and the
  `thread possibly unbound` warning is gone).

### Phase 5b / C7 — SFTP session context manager (PR ?) — ✅ DONE

Status: shipped on branch `phase_five_b4` in 5 Conventional Commits
(`f6aa040`..`8a4ce05`). All gates green: `uv run ruff format --check .`
(167 files), `uv run ruff check .` clean, `uv run ty check` clean
(no warnings; `error-on-warning` is on), `uv run pytest` 597 passed
(C1 baseline 593 + 4 new in `tests/test_connection.py`).

The plan literally said "Add `@contextmanager _sftp(self)` and reuse
the SFTP client across multi-step operations." Plan-mode review
locked the wider interpretation: also expose a public
`Connection.sftp_session()` so external callers can batch their own
multi-step flows; migrate `parsers/system.parse_system` as the first
such caller. Reconnect-on-failure semantics intentionally do **not**
extend into the session block (mid-session paramiko errors propagate;
each public `sftp_*` method still does its own one-shot reconnect at
the CM entry, preserving today's per-call resilience).

| ID  | Change | Reference | Status |
| --- | --- | --- | --- |
| C7.1 | Add private `@contextmanager _sftp(self) -> Iterator[SFTPClient]` and a thin public re-export `sftp_session()` in `mtui/connection.py`. Both call `__sftp_reconnect()` at entry, yield the client, and unconditionally `client.close()` in a `finally`. Rewrite the seven simple public methods (`sftp_put`, `sftp_get`, `sftp_get_folder`, `sftp_listdir`, `sftp_remove`, `sftp_rmdir`, `sftp_readlink`) to `with self._sftp() as sftp: ...`, dropping their manual reconnect + close pairs. `sftp_open` keeps its manual lifetime: the returned `SFTPFile` holds a strong reference to the SFTP channel, so routing through `_sftp()` would close the client on exit and break the file handle (documented with an inline comment). Behaviour byte-identical: each public call still opens one client, does one op, closes. | `mtui/connection.py` | ✅ `f6aa040` |
| C7.2 | Internal multi-step reuse: rewrite `sftp_get_folder` to call `sftp.listdir(str(remote))` directly inside its `_sftp()` block instead of going through `self.sftp_listdir` (which would open a second client). Same treatment for `sftp_rmdir`: one block doing `sftp.listdir` + per-file `sftp.remove` + final `sftp.rmdir`. Per-file `OSError` handling preserved by wrapping the inlined `sftp.remove` in `try/except OSError: logger.exception(...)` (matches the prior `Connection.sftp_remove` behaviour). Net handshake count drops from N+2 to 1 per call. | `mtui/connection.py` | ✅ `b5d2a98` |
| C7.3 | Migrate `mtui/target/parsers/system.py::parse_system` to wrap its entire body in `with connection.sftp_session() as sftp:`, calling `sftp.listdir` / `sftp.open` / `sftp.readlink` directly. Exception types preserved (paramiko raises `OSError` for missing listdir and `FileNotFoundError` for missing open — same as the prior `Connection.sftp_*` wrappers because `OSError(ENOENT)` auto-promotes to `FileNotFoundError`). The `Path()` wrapping is dropped (sftp methods take `str`), which also drops the now-unused `from pathlib import Path`. SUSE-path handshake count drops from 6+ to 1; non-SUSE path drops from 2 to 1. Tests in `tests/test_target_parsers.py` rewired to mock `conn.sftp_session().__enter__()` returning an SFTP mock with `.listdir/.open/.readlink`, plus an assertion that `conn.sftp_session` is called exactly once per `parse_system` invocation. | `mtui/target/parsers/system.py`, `tests/test_target_parsers.py` | ✅ `17aa42b` |
| C7.4 | Four new tests in `tests/test_connection.py`: `test_sftp_session_reuses_single_client` (one `open_sftp` call across a multi-op `sftp_session` block, one `close` on exit); `test_sftp_session_propagates_paramiko_errors` (mid-block `paramiko.SSHException` exits the CM, client still closed, no retry); `test_sftp_get_folder_uses_one_session` and `test_sftp_rmdir_uses_one_session` (pin the C7.2 handshake-count drop). | `tests/test_connection.py` | ✅ `5755999` |
| C7.5 | `CHANGELOG.md` `[Unreleased] / Changed`: one entry documenting the multi-step SFTP batching (parse_system, sftp_get_folder, sftp_rmdir). `[Unreleased] / Known issues`: surface the latent dead-code branch in `Connection.__sftp_open`'s except-clause (`if "sftp" in locals()` is unreachable because the named paramiko exceptions are raised by the very assignment that would bind `sftp`). Per Phase 5a discipline, surfaced bugs are documented rather than fixed in the same refactor PR. | `CHANGELOG.md` | ✅ `8a4ce05` |

**Verification**: `uv run ruff format --check .`, `uv run ruff check .`,
`uv run ty check`, `uv run pytest`, plus manual `uv run python -m mtui
--help` and `uv run python -m mtui -V`. Also confirmed
`rg "self\.__sftp_reconnect\(\)" mtui/connection.py` returns 2 hits
(one inside `_sftp`, one in the unchanged `sftp_open`); was 8 before.
`rg "sftp\.close\(\)" mtui/connection.py` returns 2 hits (`_sftp`
finally + `sftp_open` cleanup); was 9 before.

**Carry-overs surfaced during Phase 5b / C7 (handled in later
clusters or PRs):**

- The `Connection.__sftp_open` dead-code defensive branch is logged
  under `[Unreleased] / Known issues`. A one-line cleanup (drop the
  `if "sftp" in locals()` guard, unconditionally `return None`) is
  deferred to a follow-up fix PR per the Phase 5a "surface, don't
  also fix" discipline.
- The `sftp_put` reconnect-on-`mkdir`-failure loop reassigns `sftp`
  inside the `with self._sftp()` block when the original client errors
  mid-mkdir. The old client is shadowed but never explicitly closed
  (the CM's `finally` only sees the latest binding) — but the legacy
  code had the same leak, so this is a behaviour-preserving carry-over
  rather than a new bug. Worth fixing alongside the wider rework of
  `sftp_put` whenever it's revisited.

### Phase 5b / C3 — Operation ABC for install/uninstall (PR ?) — ✅ DONE

Status: shipped on `main` in 2 Conventional Commits — `205c22d`
(`refactor(hostgroup): extract Operation ABC for install/uninstall`)
and a follow-up bug fix `94a5267`
(`fix(hostgroup): catch MissingPreparerError instead of Wrong error
class`). All gates green at landing: `uv run ruff format --check .`,
`uv run ruff check .` clean, `uv run ty check` clean (no warnings;
`error-on-warning` is on), `uv run pytest` 666 passed (C7 baseline
660 + 6 new in `tests/test_operation.py`).

Plan-mode review narrowed the scope: only the two near-twin flows
(`perform_install`, `perform_uninstall`) — which shared the same
lock → run → check → reboot → unlock skeleton and differed only in
which `Target.get_*er` they consulted and which `MissingXerError`
they caught — get the template. The other three (`perform_prepare`,
`perform_downgrade`, `perform_update`) each have unique extras
(per-package loops, fanout-set-repo, nested two-phase try/finally
for guaranteed repo cleanup) and were left alone; folding them into
the same template would have required several optional hooks until
the abstraction was harder to read than the originals.

Net: ~62 LOC of structural duplication removed; install/uninstall
collapse to one-line delegations; the ABC fails fast (via
`@abstractmethod`) if a future subclass forgets either hook.

| ID  | Change | Reference | Status |
| --- | --- | --- | --- |
| C3.1 | New `mtui/target/operation.py` with `Operation(ABC)` holding the shared `collect()` / `run()` skeleton and two `@abstractmethod` hooks (`get_doer`, `get_check`). Concrete `InstallOperation` and `UninstallOperation` subclasses pair each hook with the corresponding `Target.get_*er` / `Target.get_*er_check` and the matching `MissingXerError` class. `HostsGroup` forward reference declared under `TYPE_CHECKING` to break the `hostgroup` ↔ `operation` import cycle. `HostsGroup.perform_install` and `perform_uninstall` collapse to one-line delegations; unused `MissingUninstallerError` import dropped from `hostgroup.py`. | `mtui/target/operation.py` (new), `mtui/target/hostgroup.py` | ✅ `205c22d` |
| C3.2 | Six focused unit tests in `tests/test_operation.py`: the `collect()` shape, the missing-error early-return contract, the `finally`-unlock invariant on a raising `run`, the per-target `check` call signature, the install-vs-uninstall doer routing, and ABC enforcement (`TypeError` on bare construction plus `__isabstractmethod__` flags). | `tests/test_operation.py` (new) | ✅ `205c22d` |
| C3.3 | Follow-up bug fix surfaced after C3.1 landed: the `except` clause in one of the perform_* flows was catching `MissingUninstallerError` on a code path that actually involves a *preparer*, so `MissingPreparerError` would never be caught and was silently re-raised through the generic `Exception` handler. One-line change to swap the exception class. | `mtui/target/hostgroup.py` | ✅ `94a5267` |

Strict isomorphic refactor: zero user-visible behaviour change for
the templated flows. All 45 pre-existing `tests/test_hostgroup.py`
tests pass byte-for-byte unchanged. Public API preserved
(`perform_install` / `perform_uninstall` keep their signatures).
Per AGENTS.md and the Phase 5b/C9 precedent, no `CHANGELOG` entry
for the refactor itself: internal-only.

**Verification**: `uv run ruff format --check .`, `uv run ruff check .`,
`uv run ty check`, `uv run pytest`, plus manual `uv run python -m mtui
--help` and `uv run python -m mtui -V`.

**Carry-overs surfaced during Phase 5b / C3 (handled in later
clusters or PRs):**

- `perform_prepare`, `perform_downgrade`, and `perform_update` were
  intentionally left out of the template (see scope note above). If
  a future flow turns out to share the prepare/downgrade/update
  shape, that is the moment to revisit — not now.
- The `MissingPreparerError` miscatch fixed in `94a5267` had been
  latent for a long time; no test covered it before. A targeted
  regression test for the preparer-missing path is queued under
  Phase 6 / D1 (per-command happy-path + error-path tests).

### Phase 5b / C2 — Target collaborator decomposition (PR ?) — ✅ DONE

Status: shipped on `main` in 12 Conventional Commits
(`941211b`..`ce04896`). All gates green: `uv run ruff format --check .`,
`uv run ruff check .` clean, `uv run ty check` clean (no warnings;
`error-on-warning` is on with the strict `mtui/types/**` and
`mtui/connector/**` overrides), `uv run pytest` green.

The plan literally said "Decompose `Target` into `PackageQuerier`
(rpm/dpkg), `RepoManager` (zypper), `Reporter` (the seven `report_*`
sinks) and a `Doer` registry replacing the seven `get_*er` methods."
Shipped as four narrowly-scoped extractions, each landed as a
refactor + tests pair, ordered so every intermediate commit kept the
test suite green:

1. **Doer registry first** — collapses the seven near-identical
   `get_<role>er()` / `get_<role>er_check()` accessors into
   `Target.doer(role)` / `Target.check(role)` dispatch. Call sites
   (`Operation` hooks, `HostsGroup.perform_*`, `Template`) rewired in
   the same series so the old accessors could be deleted in one go.
2. **Reporter** — moves the seven `report_*` sinks out of `Target`
   into a dedicated `Reporter` collaborator, accessed via
   `target.reporter.<name>(sink)`.
3. **PackageQuerier** — splits the rpm-vs-dpkg branching that lived
   inline in `Target.query_versions` into its own collaborator
   covering both code paths plus the parsing.
4. **RepoManager** — extracts `set_repo` and `run_zypper` (and the
   bespoke zypper-output handling) into the third collaborator.

| ID  | Change | Reference | Status |
| --- | --- | --- | --- |
| C2.1 | Collapse seven `get_<role>er` / `get_<role>er_check` methods into `Target.doer(role)` / `Target.check(role)` dispatch. | `mtui/target/target.py` | ✅ `941211b` |
| C2.2 | Route `Operation` hooks through the new `Target.doer/check` dispatch. | `mtui/target/operation.py` | ✅ `58b948a` |
| C2.3 | Route `HostsGroup.perform_*` and the `run_zypper`-adjacent template paths through `Target.doer/check`. | `mtui/target/hostgroup.py`, `mtui/template/*` | ✅ `7dafb39` |
| C2.4 | Rewire existing `tests/test_target.py` / `tests/test_hostgroup.py` / `tests/test_operation.py` mocks from `get_*er` to `doer/check`. | `tests/test_target.py`, `tests/test_hostgroup.py`, `tests/test_operation.py` | ✅ `685bb41` |
| C2.5 | New focused tests for the `Target.doer(role)` / `check(role)` dispatch surface. | `tests/test_doers.py` (new) | ✅ `88b7698` |
| C2.6 | Extract `Reporter` collaborator for the seven `report_*` sinks; expose via `target.reporter`. | `mtui/target/reporter.py` (new), `mtui/target/target.py` | ✅ `a3a448b` |
| C2.7 | Move/rewire the `report_*` tests to a dedicated `tests/test_reporter.py`. | `tests/test_reporter.py` (new), `tests/test_target.py` | ✅ `34701f1` |
| C2.8 | Extract `PackageQuerier` (rpm-vs-dpkg branch + parsing) out of `Target.query_versions`. | `mtui/target/package_querier.py` (new), `mtui/target/target.py` | ✅ `f1c614a` |
| C2.9 | Focused tests for the rpm / dpkg branch and the parser. | `tests/test_package_querier.py` (new) | ✅ `befd990` |
| C2.10 | Extract `RepoManager` (`set_repo` + `run_zypper`) collaborator. | `mtui/target/repo_manager.py` (new), `mtui/target/target.py` | ✅ `9e5a61e` |
| C2.11 | Move the `set_repo` / `run_zypper` tests into `tests/test_repo_manager.py`. | `tests/test_repo_manager.py` (new), `tests/test_target.py` | ✅ `90efac7` |
| C2.12 | `CHANGELOG.md` `[Unreleased] / Changed`: document the migration map for out-of-tree consumers that imported `mtui.target.Target` directly (ten public methods moved to collaborator properties on the same instance). Per AGENTS.md the rest is internal-only, but this one external-API change warrants a line. | `CHANGELOG.md` | ✅ `ce04896` |

Migration map (from the `ce04896` changelog entry):

    get_<role>er() / _check()      -> doer(role) / check(role)
    report_<name>(sink)             -> reporter.<name>(sink)
    set_repo(op, tr)                -> repo_manager.set(op, tr)
    run_zypper(cmd, ...)            -> repo_manager.run_zypper(cmd, ...)

**Verification**: `uv run ruff format --check .`, `uv run ruff check .`,
`uv run ty check`, `uv run pytest`, plus manual `uv run python -m mtui
--help` and `uv run python -m mtui -V`.

**Carry-overs surfaced during Phase 5b / C2 (handled in later
clusters or PRs):**

- None new. `Target` is now thin enough that the remaining `CommandPrompt`
  cleanup (C4) and `Refhosts` split (C11) are unblocked.

### Phase 5b / C4 — CommandPrompt method pre-binding (PR ?) — ✅ DONE

Status: shipped on branch `phase_five_b7` in 1 Conventional Commit
(this commit). All gates green: `uv run ruff format --check .`
(182 files), `uv run ruff check .` clean, `uv run ty check` clean
(no warnings; `error-on-warning` is on with the strict
`mtui/types/**` and `mtui/connector/**` overrides), `uv run pytest`
685 passed (C2 baseline 682 + 3 new in `tests/test_prompt.py`).

The plan literally said "register `do_/help_/complete_` methods at
construction; remove the per-attribute `__getattr__` synthesis."
Plan-mode review locked the strict reading: pre-bind the closures
via `setattr` in `_add_subcommand`, drop the runtime `__getattr__`
entirely, let unknown attributes fall through to Python's default
`AttributeError`. The four existing `tests/test_prompt.py` cases
(`test_dispatching`, `test_get_names_includes_registered_commands`,
`test_do_handles_argsparse_failure`, `test_complete_logs_and_reraises`,
plus the two `test_getattr_unknown_*`) already pin all four dispatch
paths and continue to pass byte-for-byte unchanged. The 3 new tests
lock in the new shape.

One typing-only escape hatch kept under `if TYPE_CHECKING:` — a
no-runtime-effect `__getattr__` stub returning `Callable[..., Any]`.
The previous runtime `__getattr__` annotation made `ty` believe any
attribute access on `CommandPrompt` returned `Callable`; deleting it
outright would have flagged 8 attribute-access sites (1 production
in `mtui/main.py:95`, 7 in tests). The stub preserves the typing
contract without reintroducing lazy synthesis; runtime lookups never
reach it. This is a documented deviation from the strict success
criterion "`rg \"__getattr__\" mtui/prompt.py` returns zero hits"
agreed during plan mode — the spirit (no runtime synthesis) holds.

| ID  | Change | Reference | Status |
| --- | --- | --- | --- |
| C4.1 | Rewrite `_add_subcommand` to construct three closures (`do`, `help`, `complete` — verbatim from the old `__getattr__` branches) at registration time and bind them via `setattr(self, f"do_{name}", do)` etc. `setattr` accepts attribute names containing `-`, so the lone dash-named production command (`report-bug`) round-trips unchanged. | `mtui/prompt.py` | ✅ this commit |
| C4.2 | Delete the runtime `__getattr__` (lines 240–288 of the pre-refactor file). Drop the `from collections.abc import Callable` runtime import. Add a `TYPE_CHECKING`-guarded stub re-exporting the prior typing contract so `ty` (and IDEs) continue to accept `prompt.do_<name>` accesses without per-call-site `# ty: ignore`. | `mtui/prompt.py` | ✅ this commit |
| C4.3 | Three new tests in `tests/test_prompt.py`: `test_add_subcommand_binds_methods_to_instance` (asserts `do_/help_/complete_alpha` land in `p.__dict__` after `_add_subcommand` — proves pre-binding); `test_command_prompt_has_no_getattr` (asserts `"__getattr__" not in vars(prompt.CommandPrompt)` — locks in the runtime deletion, the `TYPE_CHECKING` stub does not appear in `vars()` because the class block skips the guard at class-creation time when `TYPE_CHECKING` is `False`); `test_dash_in_command_name_dispatches` (registers a mock with `command = "dash-cmd"`, calls `getattr(p, "do_dash-cmd")("args")`, asserts the mock was driven — proves dash-named commands still round-trip). | `tests/test_prompt.py` | ✅ this commit |

`get_names` is intentionally left unchanged. With `do_X`/`help_X` now
real instance attributes, `cmd.Cmd.get_names()` (which iterates the
class via `dir(self.__class__)`) does NOT pick them up — instance
attributes are invisible there. The existing override augmenting the
list with the registered command names is therefore still required.

Per AGENTS.md and the C3 precedent ("internal-only. no `CHANGELOG`
entry for the refactor itself"), no changelog line: the `AttributeError`
message format for unknown attrs may shift from `"x"` (custom) to
Python's default `"'CommandPrompt' object has no attribute 'x'"`, but
that surface is only visible inside interactive REPL introspection;
both existing `pytest.raises(AttributeError, match=...)` checks
continue to pass because the matcher is a substring search.

**Verification**: `uv run ruff format --check .`, `uv run ruff check .`,
`uv run ty check`, `uv run pytest`, plus manual `uv run python -m mtui
--help` and `uv run python -m mtui -V`. Also confirmed
`rg "def __getattr__" mtui/prompt.py` returns exactly one hit (the
`TYPE_CHECKING`-guarded typing-only stub); was 1 before and the
runtime body has been deleted.

**Carry-overs surfaced during Phase 5b / C4 (handled in later
clusters or PRs):**

- None. The `TYPE_CHECKING` typing stub is the only deviation from
  the strict "delete `__getattr__` entirely" success criterion and is
  documented in the section header above; it is not a carry-over
  because no follow-up action is required.

### Phase 5b / C10 — Config dataclass + parse-failure logging (PR ?) — ✅ DONE

Status: shipped on branch `phase_five_b10` in 2 Conventional Commits
(`8a4327b` `refactor(config): @dataclass ConfigOption replaces 5-tuple shape`,
`76f62b4` `fix(config): log and fall back to default on parse failure`).
All gates green: `uv run ruff format --check .` (182 files), `uv run
ruff check .` clean, `uv run ty check` clean (no warnings;
`error-on-warning` is on with the strict `mtui/types/**` and
`mtui/connector/**` overrides), `uv run pytest` 691 passed (C11
baseline 691; C10 renamed two pinned characterization tests rather than
adding new ones — net count unchanged).

The plan literally said "`Config` → `@dataclass ConfigOption`; raise on
parse failure (or log explicitly). Address the existing `# FIXME` at
`mtui/config.py:73`. Closes the two Phase 5a config-bug carry-overs."
Shipped in two sequenced commits: the first is a pure structural
refactor (5-tuple → named dataclass, no behaviour change, pinned bugs
still reproduce), the second closes the two bugs by fixing the parse
path.

| ID  | Change | Reference | Status |
| --- | --- | --- | --- |
| C10.1 | New `@dataclass(frozen=True, slots=True) ConfigOption` with five named fields (`attr`, `ini_path`, `default`, `fixup`, `getter`). Replaces the historical `list[tuple[str, tuple[str, ...], Any, Callable, Callable]]` shape that relied on positional indexing and two `add_normalizer`/`add_getter` expansion helpers. `_define_config_options` rewritten to build `list[ConfigOption]` directly. `_parse_config` and `_get_option` updated to read from the dataclass fields. Stale FIXME comment block at `__init__` (claiming config override should come from env vars — `MTUI_CONF` already covered that since Phase 2) deleted. Behaviour preserved byte-for-byte; the two pinned Phase 5a bugs still reproduce in this commit and are closed in the next. | `mtui/config.py` | ✅ `8a4327b` |
| C10.2 | Fix the two Phase 5a config-bug carry-overs. (1) `_parse_config` now wraps both `_get_option` AND `fixup(val)` in the same try/except so a malformed `connection_timeout = abc` logs a clear ERROR naming the option, the offending value, and the default being applied, then continues rather than crashing with an uncaught `ValueError`. (2) `_get_option`'s broken `logger.error` arm (where `msg.format((*secopt, self.configfiles))` passed a single tuple to a three-positional-arg template, causing the log call itself to raise and the outer except to swallow everything) is removed; the caller `_parse_config` is now the single diagnostic-emission point. Two pinned characterization tests in `tests/test_config.py` flipped: `test_fixup_failure_propagates_and_aborts_construction` → `test_fixup_failure_logs_and_falls_back_to_default` (drops `pytest.raises(ValueError)`; asserts default applied + ERROR log names option and value); `test_typed_getter_failure_falls_back_to_default` → `test_typed_getter_failure_logs_and_falls_back_to_default` (adds ERROR-log assertion that previously could not pass because the logger call was broken). `CHANGELOG.md` `[Unreleased] / Fixed` entry documents the user-visible behaviour change. | `mtui/config.py`, `tests/test_config.py`, `CHANGELOG.md` | ✅ `76f62b4` |

**Verification**: `uv run ruff format --check .`, `uv run ruff check .`,
`uv run ty check`, `uv run pytest`, plus manual `uv run python -m mtui
--help` and `uv run python -m mtui -V`. Also confirmed: a bad
`MTUI_CONF` (e.g. `connection_timeout = abc`) now prints
`ERROR:mtui.config:Config option connection_timeout
(mtui.connection_timeout) failed to parse value 'abc'; falling back to
default 300` and exits 0 instead of a Python traceback.

**Carry-overs surfaced during Phase 5b / C10 (handled in later
clusters or PRs):**

- None. Both Phase 5a config-bug carry-overs are closed. The `FIXME`
  comment block deleted in C10.1 was the only outstanding item scoped
  to C10.

### Phase 5b / C11 — Refhost typed schema + Resolver registry (PR ?) — ✅ DONE

Status: shipped on branch `phase_five_b9` in 2 Conventional Commits
(`25f7328` `refactor(refhost): typed Host/Attributes dataclasses +
Resolver registry`, `706963a` `docs(changelog): note refhost typed
schema and resolver split`). All gates green: `uv run ruff format
--check .` (182 files), `uv run ruff check .` clean, `uv run ty
check` clean (no warnings; `error-on-warning` is on with the strict
`mtui/types/**` and `mtui/connector/**` overrides), `uv run pytest`
691 passed (C4 baseline 688 + 56 in the rewritten `test_refhost.py`
− 53 retired old `test_refhost.py` cases = net +3 collected).

The plan literally said "Split `Refhosts.Attributes` into a
`TypedDict`/`dataclass` schema; split resolvers into separate
classes." Plan-mode review locked five decisions:

- **Scope**: both refactors in one PR (single file, one commit).
- **Schema shape**: nested `@dataclass` for `Version`/`Product`/`Addon`/`Host`,
  not `TypedDict`, because the call-site ergonomics
  (`attribute.product.version.major`) are visibly cleaner than dict
  subscript.
- **Tags/unknown-segment prune**: when asked how to handle the
  historical `tags=(name)` and `<other>=name(...)` paths in
  `Attributes.from_testplatform` (which used `setattr(attribute, …)`
  to inject arbitrary attributes that were later iterated via
  `vars(attribute)` in `is_candidate_match`), the user pointed at the
  live `https://qam.suse.de/refhosts/refhosts-ng.yml`: the schema is
  only `name/arch/product{name,version{major,minor}}/addons[{name,
  version{...}}]`. The dynamic-attribute paths were dead grammar
  (SMELT's `parsemetajson.py` only forwards `base=…;arch=…;addon=…`
  triples per the `tests/test_metadataparsers.py` fixtures, and no
  field in the live YAML matched the injected attribute names). Both
  paths were pruned; unknown segments now log ERROR and are skipped.
- **Resolver split shape**: `Resolver` ABC + concrete
  `HttpsResolver`/`PathResolver`; factory holds a `dict[str, Resolver]`
  registry. Closes the existing `# FIXME: split resolvers into
  separate classes` at the head of `_RefhostsFactory`.
- **YAML schema failures**: when asked whether `_host_from_dict`
  should raise or log+drop on missing required fields, the user
  asked for "log.error and drop host from candidate" to preserve the
  best-effort load semantics with the only added wrinkle being an
  operator-visible signal.

| ID  | Change | Reference | Status |
| --- | --- | --- | --- |
| C11.1 | New typed schema: `Version(major: int|str, minor: int|str|None = None)`, `Product(name, version)`, `Addon(name, version)` — all frozen. `Host(name, arch, product, addons=())` frozen — the YAML row shape. `Attributes(arch="", product=None, addons=[])` mutable — the search-query shape, built incrementally by `from_testplatform`. The `minor == ""` sentinel for "candidate must NOT have a minor" is preserved in `Attributes`/`Version`; `minor is None` means "ignore minor". | `mtui/refhost.py` | ✅ `25f7328` |
| C11.2 | Rewrite `Attributes.from_testplatform` to a strict three-token grammar (`base=`, `arch=[...]`, `addon=`). Drop the `tags=(name)` and `<other>=name(...)` `setattr`-into-Attributes branches. Unknown segments log ERROR via the same `logger.error('unknown testplatform segment …')` shape as the existing malformed-line log. Helpers `_parse_named_version` and `_format_named_version` extract the repeated `name(major=X,minor=Y)` parse/render logic out of the per-call inline code. `copy.deepcopy` (not `copy.copy`) in the arch fan-out because the typed addons list now holds frozen Addon dataclasses and the safer default is documented in the commit. | `mtui/refhost.py` | ✅ `25f7328` |
| C11.3 | Replace `Refhosts._parse_refhosts`'s raw `dict[str, list[dict]]` shape with `dict[str, list[Host]]` via `_host_from_dict`. On `KeyError`/`TypeError` from a malformed row, log `"refhosts: dropping malformed host row %r: %s"` at ERROR and return `None`; the loader filters `None`s. YAML parse failures themselves still propagate (unchanged). New test `test_parse_refhosts_drops_malformed_host_and_logs` pins this. | `mtui/refhost.py` | ✅ `25f7328` |
| C11.4 | Rewrite `Refhosts.is_candidate_match` and friends to walk the three typed fields (`arch`, `product`, `addons`) directly. Three helper statics replace the dict-walking `_includes_*` cluster: `_product_matches`, `_version_matches`, `_addons_match`. The empty-field-skips-constraint semantics are preserved: `attribute.arch == ""` → no arch filter; `attribute.product is None` → no product filter; `attribute.addons == []` → no addon filter. The dead `_includes_simple_attributes` dict-key-walking branch (only exercised by the deleted `test_includes_simple_attributes_missing_key_returns_false` test, since no field in `refhosts.yml` ever had arbitrary nested keys) goes away. | `mtui/refhost.py` | ✅ `25f7328` |
| C11.5 | New `Resolver(ABC)` with one `resolve(config) -> Refhosts` method. `PathResolver(refhosts_factory=Refhosts)` — minimal, two-line `resolve`. `HttpsResolver(time_now_getter, statter, urlopener, file_writer, cache_path, refhosts_factory=Refhosts)` — owns the cache-refresh logic (`_refresh_if_needed`, `_is_refresh_needed`, `_refresh` are now private methods). | `mtui/refhost.py` | ✅ `25f7328` |
| C11.6 | Slim `_RefhostsFactory` to a dispatcher: `__init__(self, resolvers: dict[str, Resolver])`, `__call__` iterates `config.refhosts_resolvers.split(",")`, looks up each name in the registry (warning + skip on unknown), runs `resolver.resolve(config)` and returns on first success, raises `RefhostsResolveFailedError` if all fail. The `getattr(self, f"resolve_{name}")` reflection is gone. Module-level `RefhostsFactory` singleton now constructs `{"https": HttpsResolver(...), "path": PathResolver()}` explicitly. | `mtui/refhost.py` | ✅ `25f7328` |
| C11.7 | Rewrite `tests/test_refhost.py`: `TestAttributes` swaps `attr.arch = ...` for kwarg construction (`Attributes(arch="x86_64")`); `TestFromTestplatform` drops `test_tags_sets_named_attribute` and `test_unknown_property_setattr_branch` (those paths are gone), adds `test_unknown_segment_logged_and_skipped`; `TestIsCandidateMatch` swaps dict-candidate fixtures for `Host(...)` construction via a private `_host(**kwargs)` builder, drops `test_includes_simple_attributes_missing_key_returns_false` (dead path), adds three new addon-matching cases (`test_addon_missing_on_candidate_returns_false`, `test_addon_version_mismatch_returns_false`, `test_addon_name_only_matches_any_version`); new `TestPathResolver` (1 test) and `TestHttpsResolver` (7 tests) carve the cache-refresh and resolve-path tests out of the old `TestRefhostsFactory`; `TestRefhostsFactory` is rewritten around the registry shape (5 tests covering first-success / fallback-on-failure / all-fail / unknown-skip / all-unknown). Two `# ty: ignore` directives added on test helpers (MagicMock-into-typed-`refhosts_factory` parameter; same pattern as the prior `_make_factory` helper had). | `tests/test_refhost.py` | ✅ `25f7328` |
| C11.8 | `CHANGELOG.md` `[Unreleased] / Fixed`: one entry documenting the malformed-row ERROR log (previously silent). `[Unreleased] / Changed`: one entry documenting the migration map for out-of-tree consumers — `Attributes` is now a typed dataclass with the dynamic-segment grammar pruned, `Refhosts.data` is now `dict[str, list[Host]]`, `_RefhostsFactory` now takes a `{name: Resolver}` registry instead of five positional collaborators. Per AGENTS.md, internal-only mechanics aren't called out; only the surfaces that an external caller could observe. | `CHANGELOG.md` | ✅ `706963a` |

Net diff: `mtui/refhost.py` 483→545 lines (+62; the typed schema and
the four `_*_matches` helpers cost more lines than the `vars()`
walker but the `_RefhostsFactory` shrinks dramatically). The
historical `# FIXME: split resolvers into separate classes` at the
old line 342 is gone. `tests/test_refhost.py` net +3 tests
(56 vs 53 prior); coverage of the matcher's branches (addon
fallback, version-only product, name-only addon) went up.

**Verification**: `uv run ruff format --check .`, `uv run ruff check .`,
`uv run ty check`, `uv run pytest`, plus manual `uv run python -m
mtui --help` and `uv run python -m mtui -V`. Also confirmed:

- `rg "vars\(attribute\)|setattr\(attribute" mtui/refhost.py` → 0 hits.
- `rg "getattr\(self, f\"resolve_" mtui/refhost.py` → 0 hits.
- `rg "FIXME" mtui/refhost.py` → 0 hits.
- `rg "def __getattr__" mtui/refhost.py` → 0 hits (the historical
  `vars()`-iteration over dynamic attributes used the implicit
  `getattr` lookup, now gone).

**Carry-overs surfaced during Phase 5b / C11 (handled in later
clusters or PRs):**

- None new. The `# ty: ignore[invalid-argument-type]` /
  `[unresolved-attribute]` directives on the resolver test helpers
  match the prior pattern from the deleted `_make_factory` helper
  (MagicMock standing in for the typed `refhosts_factory` parameter);
  this is a known LSP-vs-test ergonomics gap rather than a fresh
  issue.
- All four Track C / Phase 5b refactors that touched `Refhosts` /
  `Attributes` (`C9` for the request enums, `C2` for the `Target`
  decomposition, `C7` for the SFTP session, and now `C11`) are now
  done; the remaining Phase 5b items (`C6` user-prompting out of the
  SSH worker thread, `C8` `mtui/utils.py` split) are independent of
  the refhost subsystem.

### Phase 5b / C8 — utils.py topical split (PR ?) — ✅ DONE

Status: shipped on branch `phase_five_b_11` in 8 Conventional Commits
(`bc33693`..`a45f2cd`). All gates green: `uv run ruff format --check .`
(187 files), `uv run ruff check .` clean, `uv run ty check` clean
(no warnings; `error-on-warning` is on with the strict
`mtui/types/**` and `mtui/connector/**` overrides), `uv run pytest`
690 passed (C11 baseline 691 − 1 strictly-redundant `test_colors`
case in `tests/test_utils.py` that duplicated
`tests/test_color.py::test_green_emits_ansi_when_enabled`).

The plan literally said "Split `mtui/utils.py` into `term.py`,
`completion.py`, `fileops.py`, `colors.py`." Plan-mode review locked
three decisions: (1) add a fifth `misc.py` to hold the three
leftovers (`DictWithInjections`, `SUTParse`, `requires_update`) that
don't fit any of the named topical modules — overloading `term.py`
with unrelated grab-bag content was the alternative considered and
rejected; (2) keep the existing `mtui/colorctl.py` policy module
separate from the new `mtui/colors.py` helpers module (one arrow:
`colors.py` → `colorctl.py`); (3) hard cut, no shim — `mtui/utils.py`
is deleted in the same PR and all in-tree imports are rewritten.
Out-of-tree consumers get an `ImportError` on upgrade with the
documented migration map in the changelog.

Ship sequencing: each extraction is one commit, the file builds and
the test suite passes at every intermediate state thanks to the
re-export shim that `mtui/utils.py` keeps until the final rewrite
commit. The shim/rewrite/delete is a single commit so a bisect lands
on the meaningful boundary.

| ID  | Change | Reference | Status |
| --- | --- | --- | --- |
| C8.1 | New `mtui/colors.py` with the four ANSI helpers (`green`, `red`, `yellow`, `blue`) moved verbatim, importing `colors_enabled` from `.colorctl`. `mtui/utils.py` rewrites those four function bodies as a single `from .colors import blue, green, red, yellow` re-export so callers stay byte-identical. | `mtui/colors.py` (new), `mtui/utils.py` | ✅ `bc33693` |
| C8.2 | New `mtui/completion.py` with `complete_choices` and `complete_choices_filelist` moved verbatim. `mtui/utils.py` re-exports both. | `mtui/completion.py` (new), `mtui/utils.py` | ✅ `8710311` |
| C8.3 | New `mtui/fileops.py` with the `chdir` re-export, `ensure_dir_exists`, `atomic_write_file`, and `timestamp` moved verbatim. `mtui/utils.py` re-exports all four. | `mtui/fileops.py` (new), `mtui/utils.py` | ✅ `bb7fdf8` |
| C8.4 | New `mtui/term.py` with `termsize`, `filter_ansi`, `prompt_user`, and `page` moved verbatim. The four stay co-located because `page()` drives `prompt_user()` / `termsize()` / `filter_ansi()` and following intra-module call sites is easier than crossing a module boundary. `mtui/utils.py` re-exports all four. | `mtui/term.py` (new), `mtui/utils.py` | ✅ `b1db13d` |
| C8.5 | New `mtui/misc.py` with the three leftovers (`requires_update` decorator, `DictWithInjections` container, `SUTParse` argv-format helper) moved verbatim. `mtui/utils.py` shrinks to a pure re-export shim. | `mtui/misc.py` (new), `mtui/utils.py` | ✅ `19c5ffc` |
| C8.6 | Mechanical rewrite of every in-tree `from mtui.utils import X` / `from ..utils import X` / `from .utils import X` line to point at the new homes. 50 production and test files touched. Mixed-symbol imports split across multiple `from` lines (one per destination module); `ruff check --fix` resorts the import blocks. `mtui/utils.py` is deleted in the same commit. The deterministic mapping table: `green/red/yellow/blue` → `mtui.colors`; `complete_choices(_filelist)` → `mtui.completion`; `chdir/ensure_dir_exists/atomic_write_file/timestamp` → `mtui.fileops`; `termsize/filter_ansi/prompt_user/page` → `mtui.term`; `DictWithInjections/SUTParse/requires_update` → `mtui.misc`. `tests/test_color.py` also touched: a multi-import line `from mtui import colorctl, colorlog, utils` swapped to `colors` and the eight `utils.green/red/yellow/blue` references rewritten — `rg` had missed this file because the regex looked only for `from … utils import` shape. | `mtui/utils.py` (deleted), 49 other production/test files, `tests/test_color.py` | ✅ `7d6d36b` |
| C8.7 | Split `tests/test_utils.py` across the new module test files: `tests/test_fileops.py` (new) gets the 4-test `TestEnsureDirExists` class plus `test_chdir`, `test_atomic_write`, `test_timestamp`; `tests/test_term.py` (new) gets `test_filter_ansi`; `tests/test_misc.py` (existing — pre-dates this PR and hosts `mtui.export.base` tests) gets `test_sutparse` appended. The redundant `test_colors` body is dropped because `tests/test_color.py::test_green_emits_ansi_when_enabled` / `test_green_returns_plain_when_disabled` already make the same four assertions across both colour modes. Net pytest count: 691 → 690 (-1). `tests/test_utils.py` deleted (tracked as a rename of test_utils.py → test_fileops.py by git). | `tests/test_fileops.py` (new), `tests/test_term.py` (new), `tests/test_misc.py`, `tests/test_utils.py` (deleted) | ✅ `b60ce99` |
| C8.8 | `CHANGELOG.md` `[Unreleased] / Changed` entry documenting the migration map for out-of-tree consumers. All 13 moved symbols and their new homes are listed; the entry explicitly notes that `mtui/utils.py` is gone (no shim) and external imports break on upgrade. Per AGENTS.md, internal mechanics are not called out — only the surface that an external caller could observe. | `CHANGELOG.md` | ✅ `a45f2cd` |

**Verification**: `uv run ruff format --check .`, `uv run ruff check .`,
`uv run ty check`, `uv run pytest`, plus manual `uv run python -m mtui
--help` and `uv run python -m mtui -V`. Also confirmed:

- `rg "from (\.|\.\.|mtui\.)utils|from mtui import utils" mtui/ tests/`
  → 0 hits.
- `ls mtui/utils.py` → file not found (deleted).
- `ls mtui/colors.py mtui/completion.py mtui/fileops.py mtui/term.py
  mtui/misc.py` → all five exist.

**Carry-overs surfaced during Phase 5b / C8 (handled in later
clusters or PRs):**

- The pre-existing `tests/test_misc.py` is a poorly-named file: it
  hosts tests for `mtui.export.base._writer` (added before C8 even
  started). Now that `mtui/misc.py` exists, the file name has
  ambiguous coverage (`test_sutparse` belongs to `mtui.misc`; the
  other two tests belong to `mtui.export.base`). Cleanly resolving
  this would move the two `_writer` tests to `tests/test_export_base.py`
  — a one-commit follow-up that was intentionally not folded into
  C8 to keep the diff focused on the utils split. Queued under
  Phase 6 / D1.
- One Phase 5b item remains: `C6` (move user prompting out of the
  SSH worker thread). Independent of the utils subsystem.

---

## Phase 6 — Test backfill (final PR)

- **D1** — Per-command happy-path + error-path tests for the ~29 untested
  commands; fill in tests for `mtui/actions/*`, `mtui/checks/*`,
  `mtui/template/*`, and the deeper paths of `mtui/connection.py` (sftp,
  reconnect, host keys, channel/stderr).
- **D3** — Consolidate near-duplicate tests using `parametrize` (e.g. the
  ten `test_target_init_*` cases).
- **D4** — Audit `tests/fixtures/*.json` (~1.1 MB) and delete what is unused.
- **D5** — Populate or remove the zero-byte `tests/fixtures/mtuirc`.
- Final ratchet of the Codecov floor.

---

## Per-PR conventions

- Each PR limited to a single phase; rebased on `main`.
- Conventional Commits messages.
- Refactor PRs in Phase 5b ship with (or reference) their characterization
  tests.
- CI green is mandatory; coverage ratchet enforced from Phase 3 onward.
- No behaviour changes outside the explicitly listed scope of each PR.

---

## Open questions / future work (out of scope for this plan)

- Async migration of `Connection` to `asyncssh` would simplify a lot of
  the threading model but is a substantial rewrite; defer until after the
  Track C refactors stabilise.
- Machine-readable output (`--output {text,json}`, item F2) deferred until
  CLI display layer is consolidated.
- Bandit / pip-audit security scanning (item E9) deferred until pre-commit
  + dependabot are in place.
