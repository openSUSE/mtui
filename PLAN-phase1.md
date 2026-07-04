# Phase 1 — `mtui-types` + `mtui-config` (detailed)

Goal: port the **domain foundation** — the pure data types (`mtui/types/`), the
error hierarchy (`support/exceptions.py`), and the configuration loader
(`support/config.py` + `support/paths.py`) — into two I/O-light crates every
later phase depends on. This phase locks the data contracts (refhosts.yml,
config, RRID/UpdateID grammar) that everything else is validated against.

**Size:** medium. **Blocks:** Phases 2–7. **Prereqs:** Phase 0 (workspace).

Source grounding (upstream, branch `main`):

| Python file                | Size | Purpose (Rust target)                                    |
| -------------------------- | ---- | -------------------------------------------------------- |
| `types/updateid.py`        | 13.8K | UpdateID hierarchy (⚠ has I/O — see 1.4)                 |
| `types/rrid.py`            | 5.6K | OBS Request Review ID parse/format                       |
| `types/systems.py`         | 4.0K | `System` (base product + addons, pretty print)          |
| `types/rpmver.py`          | 3.9K | RPM version comparison (⚠ wraps librpm — see 1.4)        |
| `types/oqaresults.py`      | 3.9K | openQA result enums/structs                              |
| `types/enums.py`           | 3.3K | TargetState, ExecutionMode, RequestKind, HTTP methods    |
| `types/hostlog.py`         | 3.0K | per-host command log entry                               |
| `types/package.py`         | 2.8K | Package (name + before/after/current/required versions)  |
| `types/filelist.py`        | 1.8K | file list wrapper                                        |
| `types/targetmeta.py`      | 0.6K | target metadata                                          |
| `types/commandlog.py`      | 0.5K | command log record                                       |
| `types/test.py`            | 0.5K | test descriptor                                          |
| `types/urls.py`            | 0.4K | URL holder                                               |
| `types/product.py`         | 0.4K | `Product` NamedTuple (name, version, arch)               |
| `support/exceptions.py`    | 5.1K | error hierarchy                                          |
| `support/config.py`        | 18.9K | INI config loader + defaults                             |
| `support/paths.py`         | 1.7K | path resolution (terms, etc.)                            |

---

## 1.1 Objectives / definition of done

- [ ] `mtui-types` compiles with zero network/filesystem deps in its core types.
- [ ] All domain types round-trip via `serde` where they map to YAML/JSON.
- [ ] RRID and UpdateID parsers accept/reject the exact strings the Python
      parsers do (ported test vectors from `test_updateid.py`, `test_types*`).
- [ ] `refhosts.yml` fixture parses into typed structs (all groups merged + deduped).
- [ ] `mtui-config` loads INI from the correct search order and applies defaults
      with graceful fallback (matching `test_config.py`).
- [ ] Error hierarchy (`thiserror`) mirrors `support/exceptions.py` semantics.
- [ ] Ported unit tests pass; `cargo clippy -D warnings` clean.

## 1.2 `mtui-types` design

### Module layout
```
crates/mtui-types/src/
├── lib.rs            # re-exports; crate-level docs
├── error.rs          # port of support/exceptions.py  (thiserror)
├── enums.rs          # TargetState, ExecutionMode, RequestKind, RequestMethod, Assignment
├── product.rs        # Product { name, version, arch }
├── version.rs        # ProductVersion { major: u32, minor: Minor }  (minor = "5" | "sp4")
├── rpmver.rs         # RpmVersion + Ord impl  (see 1.4 re: librpm)
├── package.rs        # Package { name, before, after, current, required }
├── system.rs         # System { base: Product, addons: Vec<Product> } + Display/pretty
├── rrid.rs           # RequestReviewID { project, kind, maintenance_id, review_id }
├── updateid.rs       # UpdateId enum + trait  (see 1.4 — split pure vs I/O)
├── refhost.rs        # Refhost / refhosts.yml schema (serde)
├── hostlog.rs        # HostLog / CommandLog / TargetMeta
├── oqaresults.rs     # openQA result types
└── urls.rs, filelist.rs, test.rs
```

### Key type decisions
- **`Product`** — plain struct `#[derive(Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]`
  (Python `NamedTuple`).
- **Version.minor is polymorphic** — the fixture shows `minor: 5` and
  `minor: sp4`. Model as an enum `Minor { Num(u32), Sp(String) }` with custom
  `Deserialize`, or normalize to `String`. Decide via `test_products_sle.py`.
- **Enums → Rust `enum`** with `#[serde(rename_all = ...)]`. Preserve string
  values used in wire formats (RRID `kind.value`, TargetState names).
- **RRID** — `Display` must emit `<RRID - {project}:{kind}:{maintenance_id}:{review_id}>`
  repr and the canonical `project:kind:mid:rid` parse form; port
  `RequestReviewIDParseError` subtypes as error enum variants.
- **`Refhost`** — serde struct matching refhosts.yml; loader flattens all
  top-level location groups into one `Vec<Refhost>` and de-dupes (fixture comment
  is explicit that location support is retired).
- **`RpmVersion`** — implement `Ord`/`PartialOrd` for comparison; native Rust
  epoch:version-release parse (see 1.4).

### Error strategy
- One `thiserror`-derived enum per logical group (e.g. `RridError`,
  `ConfigError`, `ParseError`) rather than one god-enum; re-export a top-level
  `Error`/`Result` alias. Python multiple-inheritance
  (`ValueError, ArgumentTypeError`) collapses to a single Rust enum variant with
  context fields (`old`/`new`/`id`).

## 1.3 `mtui-config` design

Upstream behavior (confirmed from `config.py`):
- Config source order: explicit `--path` → `$MTUI_CONF` env →
  `[/etc/mtui.cfg, ~/.mtuirc]`.
- INI format via `configparser` (inline comments `#`/`;`).
- Declarative `ConfigOption { ini_path, getter, fixup, default }`; on read/fixup
  failure, log ERROR and fall back to default (never hard-fail on a bad option).

### Rust mapping
```
crates/mtui-config/src/
├── lib.rs        # Config struct + load()
├── option.rs     # ConfigOption analog: {section, key, parse fn, default}
├── paths.rs      # search order + XDG (directories crate) + terms_path
└── ini.rs        # thin INI reader (rust-ini) or serde-based
```
- **INI reader:** `rust-ini` (closest to `configparser`, supports inline
  comments) — or hand-rolled if fidelity issues arise.
- **`Config`** = a plain struct of typed fields; `load()` reads INI, then for
  each option: parse → on error `tracing::error!` + default. Mirror the
  graceful-fallback contract exactly (tested by `test_config.py`).
- **Paths** via `directories`/`etcetera` for `~` and env expansion; keep the
  hard-coded `/etc/mtui.cfg` + `~/.mtuirc` fallbacks for parity.
- **Overrides:** Python overlays argparse `Namespace` onto config. Model as a
  `Config::with_overrides(cli_opts)` builder consumed later by `mtui-cli`.

## 1.4 Tricky ports (call out early)

- **`rpmver.py` wraps librpm** (`import rpm` / fallback `version_utils`). Rust
  options: (a) `rpm` crate / FFI to librpm — heavy, platform-bound; (b) pure-Rust
  reimplementation of RPM version compare (epoch:ver-rel, `~`/`^` semantics).
  **Decision:** pure-Rust compare, validated against `test_rpm_version.py`
  vectors. Keep an optional `librpm` feature behind a flag if exactness demands it.
- **`updateid.py` is NOT pure** — imports `shutil`, `Path`, `cli.prompter`,
  `data_sources`. Split it: put the **parsing/identity** (the ID grammar +
  equality) in `mtui-types::updateid`; leave the **downloading / template
  checkout / prompting** behavior for `mtui-datasources`/`mtui-core`
  (Phases 3/5). Phase 1 ships only the value type + parser.
- **Version.minor polymorphism** (`5` vs `sp4`) — needs a custom serde impl;
  cover both in tests.
- **`configparser` semantics** — case-insensitivity of keys, inline comments,
  interpolation (Python default is BasicInterpolation). Verify `rust-ini`
  matches or disable interpolation to avoid surprises.

## 1.5 Test strategy

Port these upstream tests as Rust `#[test]` / `insta` snapshots:

| Upstream test              | Ports to                                             |
| -------------------------- | --------------------------------------------------- |
| `test_updateid.py` (12.4K) | `mtui-types` updateid parse accept/reject vectors    |
| `test_types.py` / `_expanded` (10K) | product/system/package/rrid behavior       |
| `test_enums.py` (3.8K)     | enum value round-trips                               |
| `test_rpm_version.py`      | RpmVersion ordering vectors                          |
| `test_products_sle.py` (7.4K) | SLE product/version parsing (minor polymorphism) |
| `test_config.py` (8.9K)    | `mtui-config` load order + default fallback          |
| `test_cmd_config.py`       | config override semantics                            |
| `tests/fixtures/refhosts.yml` | refhosts parse + flatten + dedup golden test     |

Testing tools: `#[test]` for vectors, `insta` for pretty/Display snapshots,
committed fixture files under `crates/*/tests/fixtures/`.

## 1.6 Task breakdown

1. Scaffold `error.rs` (thiserror hierarchy) — everything else imports it.
2. `enums.rs` + `product.rs` + `version.rs` (with serde) — smallest, unblock refhost.
3. `refhost.rs` + refhosts.yml fixture + flatten/dedup loader → golden test.
4. `rrid.rs` + parser + error variants → port `test_updateid`/`test_types` RRID cases.
5. `rpmver.rs` pure-Rust compare → port `test_rpm_version`.
6. `package.rs`, `system.rs`, `hostlog.rs`, remaining small types.
7. `updateid.rs` value type + parser only (defer I/O).
8. `mtui-config`: paths → ini reader → Config/ConfigOption + defaults → tests.
9. Wire re-exports in both `lib.rs`; run full `clippy`/`fmt`/`test`.

## 1.7 Deliverables (files)

- **Create:** `crates/mtui-types/src/{lib,error,enums,product,version,rpmver,`
  `package,system,rrid,updateid,refhost,hostlog,oqaresults,urls,filelist,test}.rs`
  + `crates/mtui-types/tests/*` + fixtures.
- **Create:** `crates/mtui-config/src/{lib,option,paths,ini}.rs` +
  `crates/mtui-config/tests/*` + sample `.cfg` fixtures.
- **Modify:** member `Cargo.toml`s to add `serde`, `serde_yaml`, `rust-ini`,
  `directories`, `thiserror`, `tracing` (dev: `insta`).

## 1.8 Risks / decisions to confirm

- **librpm vs pure-Rust version compare** — default pure-Rust; confirm no exotic
  RPM version edge case in scope needs librpm exactness.
- **YAML crate** — `serde_yaml` (maintenance mode) vs `serde_yml` vs `saphyr`.
  Pick during 1.6 step 3 based on which parses the fixture cleanly incl. the
  `minor: sp4` scalar.
- **INI fidelity** — if `rust-ini` diverges from `configparser` (interpolation,
  duplicate keys), fall back to a small hand-rolled parser.
- **`updateid` split boundary** — confirm the parser/identity surface needed by
  Phase 1 consumers vs the I/O behavior deferred to Phase 3/5.

## 1.9 Out of scope for Phase 1

No SSH, no HTTP, no refhosts *resolution* (network fetch/cache — that's
Phase 3), no UpdateID download/checkout behavior, no command wiring. Phase 1 is
pure types + config loading only.
