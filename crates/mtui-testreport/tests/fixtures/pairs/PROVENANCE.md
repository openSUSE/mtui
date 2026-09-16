# Paired fixtures — provenance

Captured 2026-09-15 from production (`https://qam.suse.de/api/v2/reports/<RRID>`
and `svn+ssh://svn@qam.suse.de/testreports/<RRID>`), for the document-vs-SVN
equivalence test in `crates/mtui-testreport/tests/ingest.rs`.

Each `<RRID>/` directory holds exactly the three files the equivalence test
needs: `document.json` (the live `GET /api/v2/reports/<RRID>` body, pretty
printed and key-sorted), `metadata.json` and `log` (the matching SVN checkout's
files, byte-for-byte apart from the redaction below). `project.xml` and
`patchinfo.xml` are deliberately **not** committed:

- Omitting `patchinfo.xml` is what makes divergence #2 (bug/jira titles) live
  in these fixtures — the SVN side falls back to the `NO_DESCRIPTION`
  placeholder exactly as it would on a real checkout that never had one
  fetched, while the document side always carries the real title.
- Omitting `project.xml` leaves `update_repos_parser()` degrading to an empty
  map on both sides identically (that function is unchanged by the ingest
  path and reads from the checkout dir regardless of which path populated the
  rest of the model), so the comparison never depends on it.

**Redacted:** `update.packager` / `metadata.json`'s `packager` to
`someone@suse.com`, and the log's `Test Plan Reviewer:` line to `someone`, in
every fixture (real names/emails of SUSE engineers). No other field carries
personal data — `people.testers`/`people.reviewer.name` were `null`/empty on
every captured document at capture time.

## Shapes covered

| RRID | kind | workflow | targets | notes |
|---|---|---|---|---|
| `SUSE:Maintenance:46456:424163` | maintenance | obs | 4 | addon test_platforms (PackageHub, sle-we) |
| `SUSE:Maintenance:46572:423943` | maintenance | obs | 3 | 5 test_platforms incl. SLE_HPC/SLE_RT bases |
| `SUSE:SLFO:1.2:7787` | slfo | gitea | 10 | 3 products (SLE-BCI, SLES, SLES-SAP) |
| `SUSE:SLFO:1.2:7810` | slfo | gitea | 5 | 3 products (SLE-BCI, SLES, SLES-SAP) |

**Not represented in this set** — neither shape was in-flight in the
`testing` queue at capture time:

- **SLFO OBS-served** (#433's dual-serving case: an SLFO RRID built via the
  classic OBS pipeline rather than Gitea). Of the 83 SLFO ids in the queue, 55
  served a v2 document and every one of them was `workflow: gitea`.
- **PI** (Product Increment). The queue carried zero `SUSE:PI:*` ids.

Both are worth adding once a live example exists; until then the PI and
SLFO/OBS mapping rules (the `kind: pi` arm, `DocWorkflow::Obs` origin gating)
are covered only by `crates/mtui-testreport/src/ingest.rs`'s unit tests
against the synthetic fixtures in `crates/mtui-types/tests/fixtures/document/`
(`pi.json`), not by this equivalence corpus.

## Known gap not exercised here

An earlier candidate for the second SLFO/gitea slot, `SUSE:SLFO:1.2:7925`
(same `generated_at` on both artifacts — not a timing skew), surfaced a real
discrepancy the mapping table does not name: metadata.json's `binaries` block
carried non-package build artifacts (`aws-cli-image`, `aws-sdk-image`,
`az-cli-image`, `az-sdk-image`, `google-sdk-image`) for `SLES 16.0`/`x86_64`
that the *same document's* `install.targets[].binaries` did not — while
`metadata.json`'s own `packages` block agreed with the document exactly (both
carried only `openssl-3-livepatches`). The document's `target.binaries`
appears scoped to `packages`, not to metadata's broader `binaries` manifest;
`SUSE:SLFO:1.2:7810` was substituted because it has no such artifacts. This is
a genuine v1/v2 content gap worth a closer look before anything leans on
`target.binaries` for more than what `packages_map`/`composed_index` already
read — not an `ingest.rs` mapping bug (the code correctly transcribes
whatever the document says).
