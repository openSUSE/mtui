# Phase 3 — `mtui-datasources` (refhosts + data sources) (detailed)

Goal: port every **outbound integration** — the shared HTTP policy layer, the
`refhosts.yml` resolver chain + search engine + verifier, and the external
service clients (openQA, QEM Dashboard, Gitea, osc-qam, oqa-search, teregen). All
network I/O lives here; consumers (commands, MCP) get typed clients.

**Size:** large. **Blocks:** Phases 5–7. **Prereqs:** Phase 0, Phase 1
(`mtui-types`, `mtui-config`). Independent of Phase 2 (SSH) except where refhost
*verify* consumes a `System` parsed by Phase 2 — that seam is a trait boundary.

Source grounding (upstream `main`):

**Refhost resolution — `mtui/hosts/refhost/` (moved here from Phase 2):**

| Python file            | Size  | Rust target                                       |
| ---------------------- | ----- | ------------------------------------------------- |
| `refhost/store.py`     | 13.9K | `refhost/store.rs` — loader + search engine       |
| `refhost/models.py`    | 7.9K  | `refhost/models.rs` — Version/Product/Addon/Host/Attributes |
| `refhost/verify.py`    | 7.9K  | `refhost/verify.rs` — System vs Host → ProductDiff |
| `refhost/resolvers.py` | 3.4K  | `refhost/resolvers.rs` — Path/Https resolver chain |
| `refhost/__init__.py`  | 1.7K  | `refhost/mod.rs` — RefhostsFactory                 |

**Shared HTTP — `mtui/support/http.py`:**

| Python file        | Size | Rust target                                          |
| ------------------ | ---- | ---------------------------------------------------- |
| `support/http.py`  | 7.8K | `http.rs` — timeout/TLS policy, session builder       |

**Data sources — `mtui/data_sources/`:**

| Python file                          | Size  | Kind        | Rust target                    |
| ------------------------------------ | ----- | ----------- | ------------------------------ |
| `oqa_search/search.py`               | 24.4K | HTTP+parse  | `oqa_search/search.rs`         |
| `qem_dashboard/dashboard_openqa.py`  | 16.7K | HTTP        | `qem_dashboard/dashboard.rs`   |
| `gitea.py`                           | 16.1K | HTTP        | `gitea.rs`                     |
| `teregen.py`                         | 10.9K | HTTP/gen    | `teregen.rs`                   |
| `oscqam.py`                          | 8.1K  | **subprocess** | `oscqam.rs` (wraps `osc qam`) |
| `openqa/standard.py`,`kernel.py`,`base.py` | 11.5K | HTTP  | `openqa/{base,standard,kernel}.rs` |
| `qem_dashboard/client.py`,`incident.py` | 4.5K | HTTP     | `qem_dashboard/{client,incident}.rs` |
| `oqa_search/{http,heuristics,results}.py`,`__init__.py` | 10.1K | parse | `oqa_search/*.rs`   |
| `openqa_install.py`                  | 1.9K  | HTTP        | `openqa_install.rs`            |

---

## 3.1 Objectives / definition of done

- [ ] One shared async HTTP layer (`reqwest`) with unified timeout + TLS-verify
      policy, mirroring `support/http.py` (`HTTP_TIMEOUT`, `VerifyPolicy`,
      `build_session`, `resolve_verify`, `disable_insecure_warnings`).
- [ ] `refhosts.yml` resolves via an ordered resolver chain (local path + cached
      HTTPS) and parses into the typed `Host`/`Attributes` model.
- [ ] Refhost **search** filters by hostname/arch/product/version/addon/
      testplatform, including the extension-as-addon rule (`base=SLES-LTSS` →
      SLES host with that extension).
- [ ] Refhost **verify** compares a `System` vs a `Host` row → `ProductDiff`.
- [ ] openQA / QEM Dashboard / Gitea / osc-qam / oqa-search clients return typed
      results; failures degrade gracefully (log + `None`/`Err`, as upstream).
- [ ] All HTTP clients tested against `wiremock`; oqa-search parsers tested
      against committed log fixtures. Ported tests green.

## 3.2 Shared HTTP layer (`http.rs`) — do this first

Confirmed: `support/http.py` is the "single source of truth for outbound HTTP
timeout and TLS policy." Every client imports `HTTP_TIMEOUT`, `VerifyPolicy`,
`build_session`, `resolve_verify`. Port it as the crate foundation:

- `HttpClient` wrapping `reqwest::Client` with:
  - a shared **connect+read timeout** constant (`HTTP_TIMEOUT` analog);
  - a `VerifyPolicy` enum → TLS verification on/off (`resolve_verify(secure,
    config.ssl_verify)` semantics: default verify, config can disable);
  - `disable_insecure_warnings` analog (suppress rustls/reqwest insecure warning
    logging when verification is intentionally off).
- Build once, share via `Arc<HttpClient>` handed to each service client.
- **All async** (`reqwest` + tokio) — replaces the blocking `requests.Session`.

> `openqa_client` (Python lib) has no Rust equivalent → reimplement a thin
> `OpenQaClient` over `HttpClient` (see 3.4).

## 3.3 Refhost resolution (`refhost/`)

Confirmed behavior:
- **Resolver chain**: `PathResolver` (local file) and `HttpsResolver` (cached
  HTTPS download); `RefhostsFactory` tries them in `config.refhosts_resolvers`
  order; first success wins.
- **Model**: dataclasses `Version`, `Product`, `Addon`, `Host`, `Attributes` +
  `Attributes.from_testplatform` parsing. (Some overlap with Phase 1's refhost
  serde struct — **reconcile**: Phase 1 gave the raw YAML schema; Phase 3 adds
  the query/search `Attributes` type + factory. Keep the schema in `mtui-types`
  and the resolution/search logic here.)
- **Search** (`Refhosts` in store.py): match host rows against an `Attributes`
  query by arch/product/version/addon; testplatform `base=<name>` is satisfied by
  base product **or** an installed addon (extension products like SLES-LTSS,
  sle-ha, SLES_SAP ship as addons on a SLES/SLED base).

### Rust plan
```
crates/mtui-datasources/src/refhost/
├── mod.rs        # RefhostsFactory (resolver-chain dispatch)
├── resolvers.rs  # Resolver trait; PathResolver, HttpsResolver (+ on-disk cache)
├── models.rs     # Attributes query type + from_testplatform parser
├── store.rs      # Refhosts: parse rows -> search(query) -> Vec<Host>
└── verify.rs     # compare(System, Host) -> ProductDiff
```
- **HttpsResolver cache**: on-disk cached download keyed by URL, honoring a
  configurable expiry (`config.refhosts_expiration` analog). Use the `directories`
  cache dir (from `mtui-config`). Stale → re-fetch; offline → serve cache.
- **verify.rs** consumes a `System` (produced by Phase 2's host parsers). Define
  a small trait/DTO so `mtui-datasources` does not depend on `mtui-hosts`
  (avoid a cycle) — `System` already lives in `mtui-types`, so import from there.

## 3.4 openQA client (`openqa/`)

Confirmed: `openqa/base.py` wraps the Python `openqa_client` lib
(`OpenQA_Client.openqa_request("GET","jobs",params)`), with base/standard/kernel
variants and graceful `None` on `RequestError`/`ConnectionError`.

### Rust plan
- Reimplement the minimal openQA REST surface used by mtui as `OpenQaClient` over
  `HttpClient`: `jobs(params)`, result-log download, plus the standard/kernel
  result mapping (`openqa_install.py`, `types::oqaresults`).
- Preserve the graceful-degradation contract (log + `None`/`Err`, never panic).
- Port `test_openqa_connector.py`, `test_openqa_kernel.py`,
  `test_openqa_install_map.py`, `test_oqaresults.py`.

## 3.5 QEM Dashboard, Gitea, osc-qam, oqa-search, teregen

- **QEM Dashboard** (`qem_dashboard/`): `client.rs` (low-level GET/POST) +
  `incident.rs` + `dashboard.rs` (16.7K — the big one: incident/job status
  aggregation). Tests: `test_qem_dashboard_connector.py` (26.4K).
- **Gitea** (`gitea.rs`): comment-based PR workflow — assign/unassign/approve/
  reject via specially-formatted comments; error types `GiteaNoReviewError`,
  `GiteaAssignInvalidError` → Rust error enum. Tests: `test_gitea.py` (20.3K).
- **osc-qam** (`oscqam.rs`): **NOT HTTP** — wraps the `osc qam` CLI via
  subprocess (`shlex` quoting, `subprocess.run` with timeout). Rust:
  `tokio::process::Command`; preserve arg quoting + timeout + exit-code handling.
- **oqa-search** (`oqa_search/`): HTTP fetch (`http.rs`, `lru_cache`) + log
  **parsing/heuristics** (`search.rs`, `heuristics.rs`, `results.rs`). Rich
  parser logic + committed **log fixtures** (`tests/fixtures/oqa_search/**` — real
  `.log` files with `.matches` golden outputs). Tests: `test_oqa_search_connector.py`
  (28.8K). This is the most parser-heavy client.
- **teregen** (`teregen.rs`): test-report generation data source (10.9K).

## 3.6 Test strategy

| Upstream test                         | Ports to                                        |
| ------------------------------------- | ----------------------------------------------- |
| `test_support_http.py` (8.2K)         | HTTP timeout/verify policy                       |
| `test_refhost.py` (27.5K)             | refhost load + search + factory                  |
| `test_refhost_verify.py` (8.6K)       | verify → ProductDiff                             |
| `test_list_refhosts.py` (10.7K)       | offline filtering (feeds the `list_refhosts` cmd)|
| `test_openqa_connector.py` +kernel+install_map | openQA client + result mapping         |
| `test_qem_dashboard_connector.py` (26.4K) | QEM dashboard client                        |
| `test_gitea.py` (20.3K)               | Gitea PR workflow + error cases                  |
| `test_oqa_search_connector.py` (28.8K)| oqa-search parser vs log fixtures                |
| `test_oqaresults.py`, `test_reloadoqa.py` | result types, reload path                    |

### Infrastructure
- **`wiremock`** for every HTTP client (stub openQA/QEM/Gitea/refhosts.yml/
  oqa-search responses); assert request shape + typed response parsing.
- **Committed fixtures**: copy `tests/fixtures/oqa_search/**` (`.log` + `.matches`)
  and `tests/fixtures/refhosts.yml` into `crates/mtui-datasources/tests/fixtures/`;
  golden-compare parser output with `insta`.
- **osc-qam**: mock the subprocess via a stub `osc` on `PATH` or inject a command
  runner trait; assert argv + parse of canned stdout.

## 3.7 Task breakdown

1. `http.rs`: `HttpClient`, timeout const, `VerifyPolicy`, `resolve_verify`,
   insecure-warning suppression → port `test_support_http`.
2. `refhost/models.rs` + `store.rs` search (reuse Phase 1 schema) → `test_refhost`.
3. `refhost/resolvers.rs` (Path + Https + cache) + factory → resolver-order test.
4. `refhost/verify.rs` compare → `ProductDiff` → `test_refhost_verify`.
5. `openqa/` client + result mapping → openQA tests.
6. `qem_dashboard/` client + incident + dashboard → dashboard tests.
7. `gitea.rs` PR workflow + errors → `test_gitea`.
8. `oscqam.rs` subprocess wrapper → subprocess-stub test.
9. `oqa_search/` fetch + heuristics + parser → fixture golden tests.
10. `teregen.rs`, `openqa_install.rs` remainders.

## 3.8 Deliverables (files)

- **Create:** `crates/mtui-datasources/src/{http.rs, refhost/**, openqa/**,`
  `qem_dashboard/**, oqa_search/**, gitea.rs, oscqam.rs, teregen.rs,`
  `openqa_install.rs, lib.rs}` + `tests/**` + `tests/fixtures/**` (refhosts.yml,
  oqa_search logs/matches).
- **Modify:** `crates/mtui-datasources/Cargo.toml` — add `reqwest`
  (rustls-tls, json), `tokio`, `serde`/`serde_yaml`/`serde_json`, `directories`,
  `tracing`, `thiserror`; dev-deps `wiremock`, `insta`.

## 3.9 Risks / decisions to confirm

- **No `openqa_client` crate** → reimplement the minimal REST surface. Risk:
  missing an endpoint/param mtui relies on. Mitigation: drive the reimpl from the
  ported connector tests (they encode the exact requests).
- **oqa-search parser fidelity** — the heuristics/log parsing is intricate and
  fixture-driven; treat `.matches` files as golden contracts and port carefully.
- **Refhost model duplication** — reconcile Phase 1 YAML schema vs Phase 3
  `Attributes`/`Host` query model; single source of truth for the schema in
  `mtui-types`, resolution/search logic here. Confirm the seam before coding.
- **TLS verify default** — `resolve_verify(True, config.ssl_verify)`: verify by
  default, config can disable. Preserve exactly (security-relevant).
- **osc-qam is a subprocess dependency** — requires `osc` + `osc-plugin-qam`
  installed at runtime; document as an optional/runtime dependency, not a crate.
- **HTTPS cache format/location** — align the refhosts cache dir with `directories`
  cache path; decide whether to stay compatible with Python's cache location.

## 3.10 Out of scope for Phase 3

No command wiring (`assign`/`approve`/`list_refhosts`/`openqa_overview` land in
Phase 5), no test-report lifecycle (Phase 4), no SSH. Phase 3 delivers typed,
tested clients + refhost resolution/search/verify, all behind async APIs.
