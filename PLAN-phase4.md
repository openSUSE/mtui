# Phase 4 — `mtui-testreport` (test-report lifecycle + update workflow) (detailed)

Goal: port the **test-report model + lifecycle** (`load` → `checkout` → `edit` →
`commit` → `export`), its metadata parsers, the SVN/Gitea checkout backends, and
the **update-workflow** engine (install/update/prepare/downgrade/uninstall
action + check registries, and the auto/manual/kernel exporters).

**Size:** large. **Convergence point:** this is the first phase that depends on
**both** Phase 2 (`HostsGroup`/`Target`) **and** Phase 3 (Gitea, openQA,
refhost). **Prereqs:** Phase 0, 1, 2, 3.

Source grounding (upstream `main`):

**`mtui/test_reports/`:**

| Python file                | Size  | Rust target                                        |
| -------------------------- | ----- | -------------------------------------------------- |
| `testreport.py`            | 41.5K | `testreport.rs` — TestReport trait + core (the hub)|
| `metadata_parsers.py`      | 8.4K  | `metadata/{reduced,json,repoparse}.rs`             |
| `sl_report.py`             | 3.7K  | `reports/sl.rs`                                     |
| `svn_io.py`                | 3.7K  | `checkout/svn.rs`                                   |
| `pi_report.py`             | 2.9K  | `reports/pi.rs`                                     |
| `obs_report.py`            | 2.8K  | `reports/obs.rs`                                    |
| `null_report.py`           | 1.5K  | `reports/null.rs` — null-object                     |
| `products/{sle11,12,15,misc}.py` | 6.1K | `products/*.rs` — per-SLE product config       |

**`mtui/update_workflow/`:**

| Python file                     | Size  | Rust target                              |
| ------------------------------- | ----- | ---------------------------------------- |
| `export/manual.py`              | 11.5K | `export/manual.rs`                       |
| `export/base.py`                | 7.5K  | `export/base.rs` — BaseExport trait      |
| `export/overview_inject.py`     | 6.0K  | `export/overview_inject.rs`              |
| `export/auto.py`                | 5.7K  | `export/auto.rs`                         |
| `export/downloader.py`          | 4.7K  | `export/downloader.rs`                   |
| `export/kernel.py`              | 3.1K  | `export/kernel.rs`                       |
| `actions/{install,update,prepare,downgrade,uninstall}.py` | 8.6K | `actions/*.rs` — doer registries |
| `checks/{install,update,prepare,downgrade}.py` | 9.7K | `checks/*.rs` — check registries |

---

## 4.1 Objectives / definition of done

- [ ] `TestReport` trait + concrete impls (SL/PI/OBS/Null) load metadata from a
      source and expose the loaded state (products, packages, hostgroup, RRID).
- [ ] Metadata parsers reproduce upstream output byte-for-byte on the fixtures
      (`tests/fixtures/metadata/log`): reduced line parser, JSON envelope parser,
      and the four `*repoparse` variants (OBS/SL/git/repo).
- [ ] Lifecycle: `checkout` (SVN + Gitea backends), `commit`, `edit`, `export`.
- [ ] Update workflow: action registries (installer/updater/preparer/downgrader/
      uninstaller) + check registries, keyed `(release, transactional)`, wired to
      Phase 2's `Target.doer/check` seam.
- [ ] Exporters (auto/manual/kernel) + `overview_inject` produce the same
      testreport text (snapshot-verified), incl. idempotent BEGIN/END replacement.
- [ ] Ported tests green.

## 4.2 The hub: `TestReport` (testreport.py, 41.5K)

Confirmed: an abstract base (`ABC`) that pulls together HostsGroup (Phase 2),
Gitea (Phase 3), metadata parsers, products, and uses `concurrent.futures` +
`threading` internally. It is the single largest file and the integration point.

### Rust plan
- `TestReport` **trait** with the lifecycle surface + a `TestReportBase` struct
  holding shared state (RRID, products, packages, `HostsGroup`, paths). Concrete
  reports (`SlReport`, `PiReport`, `ObsReport`, `NullReport`) implement the trait;
  `NullReport` is the null-object used when nothing is loaded.
- **Factory/dispatch**: `updateid.py` notes "only one type of testreport now" but
  the hierarchy + factory still exist — port a `TestReport::for_kind(...)`
  constructor selecting the concrete type. Confirm which types are live vs vestigial
  before implementing all four.
- **Concurrency**: internal `concurrent.futures`/`threading` → `tokio` tasks +
  `join_all` (consistent with Phase 2). Any shared mutable state → `tokio::Mutex`.
- Because this file touches every other crate, **build it incrementally**: stub
  the trait first, land parsers + a minimal `SlReport`, then layer checkout/export.

## 4.3 Metadata parsers (metadata_parsers.py, 8.4K)

Confirmed three consolidated parsers:
- `ReducedMetadataParser` — line-based log/text metadata (ex `parsemeta`).
- `JSONParser` — JSON envelope from the newer build pipeline (ex `parsemetajson`).
- `*repoparse` helpers — derive `Product → repo-URL` map from OBS / SUSE Linux /
  git / plain-repository sources (ex `repoparse`).

### Rust plan
```
crates/mtui-testreport/src/metadata/
├── mod.rs
├── reduced.rs   # line-based key/value parser
├── json.rs      # serde_json envelope -> metadata
└── repoparse.rs # obsrepoparse / slrepoparse / gitrepoparse / reporepoparse
```
- Reduced parser: careful regex/line handling; **golden-test against
  `tests/fixtures/metadata/log`** (6.7K real fixture).
- JSON parser: `serde_json` into typed structs.
- repoparse: four functions returning `HashMap<Product, Url>`; each variant is a
  distinct source format — snapshot each.

## 4.4 Checkout backends (svn_io.py + Gitea)

Confirmed SVN backend is **subprocess** (`svn add --force`, `svn up`, `svn ci`),
committing testreport artifacts (install_logs, `results/`, `checkers.log`).
Gitea is the alternate checkout backend (via Phase 3's `Gitea` client).

### Rust plan
```
crates/mtui-testreport/src/checkout/
├── mod.rs   # Checkout trait
├── svn.rs   # tokio::process svn add/up/ci; SvnCheckoutFailed/Interrupted errors
└── gitea.rs # uses mtui-datasources::Gitea
```
- `svn.rs`: `tokio::process::Command`; preserve the exact svn subcommand
  sequence + error mapping (`SvnCheckoutFailed`, `SvnCheckoutInterruptedError`).
  Requires `svn` at runtime (document as optional runtime dep).
- **Note the `updateid` seam**: Phase 1 deferred `testreport_svn_checkout`
  (the `Callable` passed into `UpdateID`). It lands here; wire it back to the
  UpdateID value type from Phase 1.

## 4.5 Update workflow: actions + checks registries

Confirmed dispatch tables (from `target.py`, consumed by Phase 2):
```
_DOERS = { installer, uninstaller, downgrader, updater }      # (release, transactional) -> dict-of-templates
_CHECKS = { installer:install_checks, uninstaller:install_checks,
            downgrader:downgrade_checks, updater:update_checks,
            preparer:prepare_checks }                          # (release, transactional) -> callable
```
Notable quirks to preserve:
- `uninstaller` deliberately reuses `install_checks` (no dedicated uninstall
  checks table).
- `preparer` yields a **callable**, not a dict, and is dispatched inline.

### Rust plan
```
crates/mtui-testreport/src/actions/  # installer/updater/preparer/downgrader/uninstaller
crates/mtui-testreport/src/checks/   # install/update/prepare/downgrade check fns
```
- Registries keyed by `(release: String, transactional: bool)`. Templates are
  shell-command templates (Python `string.Template`) → use a small
  `${var}` substitution helper.
- **Cross-crate seam:** Phase 2's `Target.doer/check` needs these registries.
  Options: (a) `mtui-hosts` defines `Doer`/`Check` traits, `mtui-testreport`
  provides impls injected at runtime (avoids a hosts→testreport dep cycle);
  (b) move the registries to a lower crate. **Decision:** trait-based injection —
  `mtui-hosts` stays free of `mtui-testreport`, `mtui-core` (Phase 5) wires them.

## 4.6 Exporters (update_workflow/export/)

Confirmed: `BaseExport` ABC; concrete `auto`, `manual`, `kernel`; a `downloader`;
and `overview_inject` which inserts/replaces an `openqa_overview` block under the
`regression tests:` section using BEGIN/END markers (**idempotent** re-export).

### Rust plan
```
crates/mtui-testreport/src/export/
├── base.rs           # Export trait
├── auto.rs, manual.rs, kernel.rs
├── downloader.rs     # pulls logs (uses mtui-datasources HTTP)
└── overview_inject.rs
```
- `Export` trait with the shared template + a per-variant body (mirrors ABC).
- ExportKind enum matches `enums.py` `AUTO/MANUAL/KERNEL`.
- `overview_inject`: detect existing BEGIN/END markers → replace in place, else
  append; **golden-test idempotency** (export twice → identical file).
- `downloader` uses Phase 3's `HttpClient` (openQA/result-log download).

## 4.7 Products config (test_reports/products/)

Per-SLE product definitions (sle11/12/15 + misc). Port as static tables/consts;
tested by `test_pi_report.py`, `test_sl_report.py`.

## 4.8 Test strategy

| Upstream test                     | Ports to                                       |
| --------------------------------- | ---------------------------------------------- |
| `test_testreport.py` (35.3K)      | TestReport core lifecycle                       |
| `test_testreport_set_reviewer.py` | reviewer mutation                               |
| `test_sl_report.py`, `test_pi_report.py`, `test_obs_report.py`, `test_null_report.py` | per-report impls |
| `test_reporter.py`                | reporter output                                 |
| `test_svn_io.py`                  | svn subprocess sequence (mock `svn`)            |
| `test_cmd_checkout/commit/export/loadtemplate/templates.py` | lifecycle (feeds Phase 5) |
| `test_export_auto_download/downloader/kernel/manual/openqa.py` | exporters + downloader |
| `test_overview_inject.py` (8.9K)  | idempotent overview injection                   |
| `test_template_completion/registry.py` | template registry                          |
| fixture `metadata/log`            | metadata parser golden output                   |

### Infrastructure
- **`insta` snapshots** are the primary tool here — testreport rendering, export
  output, and metadata parsing are all text-shape contracts.
- **Mock `svn`**: stub binary on `PATH` or a command-runner trait; assert argv.
- Reuse Phase 2 `MockConnection` + Phase 3 `wiremock` for report methods that
  touch hosts/HTTP.
- Copy `tests/fixtures/metadata/log` into `crates/mtui-testreport/tests/fixtures/`.

## 4.9 Task breakdown

1. `TestReport` trait skeleton + `TestReportBase` shared state + `NullReport`.
2. `metadata/` parsers (reduced, json, repoparse) → golden fixture tests.
3. `products/` static tables.
4. Concrete `SlReport` (then PI/OBS as needed) implementing the trait.
5. `checkout/svn.rs` + `gitea.rs`; wire the `updateid` checkout callable seam.
6. `actions/` + `checks/` registries + `${}` template helper; define the
   Doer/Check traits consumed by `mtui-hosts`.
7. `export/` base + auto/manual/kernel + downloader.
8. `overview_inject` with idempotent BEGIN/END replacement.
9. Full test port; snapshot review.

## 4.10 Deliverables (files)

- **Create:** `crates/mtui-testreport/src/{lib.rs, testreport.rs, metadata/**,`
  `reports/**, products/**, checkout/**, actions/**, checks/**, export/**}` +
  `tests/**` + `tests/fixtures/metadata/log`.
- **Modify:** `crates/mtui-testreport/Cargo.toml` — deps `mtui-types`,
  `mtui-config`, `mtui-hosts`, `mtui-datasources`, `serde_json`, `regex`,
  `tokio`, `tracing`, `thiserror`; dev `insta`.
- **Possibly modify:** `crates/mtui-hosts` — add `Doer`/`Check` traits for the
  action/check injection seam (if not already stubbed in Phase 2).

## 4.11 Risks / decisions to confirm

- **Convergence dependency load.** This crate pulls in hosts + datasources; keep
  the `TestReport` trait thin and inject collaborators to avoid tight coupling.
- **hosts↔testreport cycle.** The action/check registries are needed by
  `Target.doer/check` (hosts) but live conceptually in the workflow (testreport).
  Resolved via trait injection wired in `mtui-core`; confirm the trait boundary
  before coding step 6.
- **Report-type liveness.** `updateid` hints only one testreport type is used now.
  Confirm which of SL/PI/OBS are live to avoid porting dead code (Null is needed).
- **Text-format contracts.** Testreport/export/metadata output must match upstream
  for interop and for humans editing templates — snapshot everything, do not
  reformat.
- **Subprocess deps** (`svn`) — runtime dependency; Gitea backend is the
  HTTP-native alternative. Document both.
- **Preserve behavioral quirks** — `uninstaller`→`install_checks`, `preparer`
  callable dispatch. Port exactly; add tests pinning them.

## 4.12 Out of scope for Phase 4

No command definitions (`load_template`/`checkout`/`export`/`update` commands →
Phase 5), no REPL, no MCP. Phase 4 delivers the report model, parsers, checkout
backends, workflow registries, and exporters as a tested library.
