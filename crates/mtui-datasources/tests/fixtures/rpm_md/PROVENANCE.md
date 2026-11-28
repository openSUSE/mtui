# rpm-md fixtures — provenance

**Hand-written, not captures.** Written to the documented rpm-md layout that
`createrepo_c` produces: `repodata/repomd.xml` lists one `<data type="...">`
entry per metadata file with a `<location href="..."/>` relative to the
repository root, and the `primary` file is a `<metadata packages="N">` root
holding one `<package type="rpm">` per RPM with `<name>`, `<arch>` and
`<version epoch= ver= rel=/>` children. Checksums and timestamps are
placeholders; nothing here verifies them.

The package names (`foo`, `foo-devel`, `foo-doc`) are placeholders, not SUSE
packages, and no product or version id appears.

- `repomd.xml` — a primary and a filelists entry; the primary at
  `repodata/00-primary.xml.gz`.
- `primary.xml.zst` — `primary.xml` compressed by the reference `zstd`
  CLI (v1.5.7, `zstd -19`), so the decoder is tested against the reference
  encoder's output and not only its own.
- `primary.xml` — three packages, two `x86_64` and one `noarch`, each with an
  `rpm:provides` entry whose attribute names (`name`, `ver`, `rel`) collide
  with the package's own fields.

The tests in `tests/repo_check.rs` serve these through `wiremock`, gzip- or
zstd-compressing `primary.xml` themselves, and derive the empty, truncated and
mismatched shapes from them. The fuzz seeds in `fuzz/seed_corpus/repo_metadata/`
are copies.
