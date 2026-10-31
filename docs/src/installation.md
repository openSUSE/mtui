# Installation

mtui ships as two static binaries — `mtui` (the REPL) and `mtui-mcp` (the MCP
server) — with no runtime interpreter or virtualenv. On openSUSE install the
package from the openSUSE Build Service (OBS); elsewhere take the
[prebuilt x86_64 packages](#prebuilt-packages-github-releases) from the release
page, or build from source.

## Requirements

- A Rust toolchain, **edition 2024, MSRV 1.96**. The MSRV is pinned via
  `rust-version` in `Cargo.toml`. There is no `rust-toolchain.toml`.
- Optional runtime tools (see [Runtime dependencies](#runtime-dependencies)).

## Build from source

```sh
# Both binaries, optimized.
cargo build --release
```

The root `mtui` facade package exposes a `cli` and an `mcp` feature, both
**enabled by default**, so a plain `cargo build --release` produces both
binaries. `--no-default-features --features cli` (or `mcp`) builds only the
REPL (or only the MCP server), if you want to skip pulling in the MCP SDK
(`rmcp`/`axum`) or the REPL's dependencies.

The binaries land in `target/release/`:

```sh
install -Dm755 target/release/mtui     /usr/local/bin/mtui
install -Dm755 target/release/mtui-mcp /usr/local/bin/mtui-mcp
```

Verify:

```sh
mtui --help
mtui --version        # prints version + build provenance (sha, profile, target)
mtui-mcp --help
```

## Shell completions

Completions for bash, zsh, and fish are pre-generated (from the two `clap`
parsers) and checked into `dist/completions/`. They are regenerable with
`cargo xtask gen`. Install the ones your shell uses:

```sh
# bash
install -Dm644 dist/completions/bash/mtui.bash     /usr/share/bash-completion/completions/mtui
install -Dm644 dist/completions/bash/mtui-mcp.bash /usr/share/bash-completion/completions/mtui-mcp

# zsh
install -Dm644 dist/completions/zsh/_mtui     /usr/share/zsh/site-functions/_mtui
install -Dm644 dist/completions/zsh/_mtui-mcp /usr/share/zsh/site-functions/_mtui-mcp

# fish
install -Dm644 dist/completions/fish/mtui.fish     /usr/share/fish/vendor_completions.d/mtui.fish
install -Dm644 dist/completions/fish/mtui-mcp.fish /usr/share/fish/vendor_completions.d/mtui-mcp.fish
```

## Man pages

Man pages for both binaries are pre-generated into `dist/man/` (regenerable with
`cargo xtask gen`, byte-stable — they carry the plain crate version, not the
build-provenance string):

```sh
install -Dm644 dist/man/mtui.1     /usr/share/man/man1/mtui.1
install -Dm644 dist/man/mtui-mcp.1 /usr/share/man/man1/mtui-mcp.1
```

## Runtime dependencies

mtui needs no external programs beyond its own binaries.

The QAM review workflow (`assign`/`unassign`/`approve`/`reject`/`comment`) talks
to the OBS/IBS API **natively** — no `osc` subprocess. It reads credentials from
your `oscrc`, located exactly like `osc` itself: `$OSC_CONFIG`, then
`$XDG_CONFIG_HOME/osc/oscrc`, then `~/.oscrc`. See the `[obs]` table in
[Configuration](configuration.md).

## Prebuilt packages (GitHub releases)

Every release carries `.deb` and `.rpm` packages next to the tarballs, built by
the `package` job in `.github/workflows/release.yml`. They split the same way the
spec does — `mtui` recommends `mtui-mcp`, so installing `mtui` alone still gets
both — with three caveats the OBS build does not have:

- **x86_64 only**, built for `x86_64-unknown-linux-musl`. No aarch64, no macOS.
- **Same `Name:` as the OBS RPM.** In a repository carrying both, they compete by
  version comparison alone. On openSUSE prefer the OBS package.
- **The `.rpm`s own no directories.** cargo-generate-rpm has no `%dir`, so
  erasing them leaves the doc- and licensedirs behind.

Because the binaries are static the packages declare no *mandatory* runtime or
library dependencies — no libc, and no shell either, since nothing they ship is
a script. Both are asserted by the release job. `mtui` declares `mtui-mcp` as a
recommends.

Verify with `sha256sum --ignore-missing -c mtui-packages.sha256` — one file
covers all four packages, so a partial download needs `--ignore-missing`.

## Packaged install (openSUSE)

On openSUSE, prefer the `mtui.spec` package build, which installs the binaries,
completions, and man pages into the standard system paths. It builds two packages:

- **`mtui`** — the REPL, its completions and man page. Recommends `mtui-mcp`, so
  a plain `zypper in mtui` still gets both binaries.
- **`mtui-mcp`** — the MCP server, its completions and man page. Installable on
  its own; it reads the same config.

## Cutting a release (maintainers)

The **supported** openSUSE RPM is built and published from the Build Service, not
from CI. The package sources at the repo root — `_service` and `mtui.spec` —
build it fully **offline from vendored crates** via the OBS source services, so
nothing is fetched during the network-isolated build. CI's `.deb`/`.rpm` are a
separate, x86_64-only convenience (see
[above](#prebuilt-packages-github-releases)) and do not replace this recipe.

### One-time maintainer setup

Install the source-service packages and confirm your `ibs` alias (the same `oscrc`
the native review backend reads):

```sh
zypper install obs-service-cargo osc obs-service-tar obs-service-obs_scm \
  obs-service-recompress obs-service-set_version obs-service-format_spec_file \
  cargo cargo-packaging
osc -A ibs whoami        # confirms the `ibs` alias resolves
```

For the release automation itself, the repository needs two secrets (both
one-time, both in the GitHub web UI):

- **`RELEASE_PLZ_TOKEN`**: a Personal Access Token (classic `repo` scope,
  or fine-grained with Contents + Pull requests read/write) on an account
  with write access. Both jobs authenticate as this token: pushes made with
  the default `GITHUB_TOKEN` trigger no further workflow runs, so with it
  the tag push would never drive `release.yml` and CI checks would never
  run on the Release PR. If the Release PR and release commits should not
  carry a personal account's authorship, create a machine user for it.
- **`CARGO_REGISTRY_TOKEN` secret**: a crates.io token with the
  `publish-new`/`publish-update` scopes, otherwise the `release-plz-release`
  job cannot publish.

### Release flow (main line)

Releases on `main` are driven by release-plz (config: `release-plz.toml`,
workflow: `.github/workflows/release-plz.yml`):

1. Every push to `main` refreshes the **Release PR**: mechanical version
   bumps plus a draft CHANGELOG entry.
2. The maintainer **sets the release by editing that PR** — the version
   number and the changelog prose. release-plz's inference from commit
   prefixes is only a proposal; the edited PR is the decision. The draft
   entry is rewritten into the hand-written Keep-a-Changelog style before
   merging (the generated text is never merged as-is). Edit, then merge
   promptly: if another `main` push refreshes the PR first, release-plz
   **closes a hand-touched PR and opens a fresh one without your edits**
   (any non-bot commit on its branch counts as a contributor edit, and the
   fresh PR is recomputed from `main`). Re-apply the edits there — or put a
   version bump on `main` itself, which the refresh picks up as an
   already-bumped version instead of clobbering. Never "Update branch" the
   Release PR: that merge commit reads as an edit and triggers the same
   close-and-recreate.
3. Merging the PR makes release-plz **tag the release and publish to
   crates.io — all 10 workspace crates**: the `mtui` facade plus the 9
   `mtui-*` libraries, which become public surface on first publish even
   though only the facade is supported (`xtask` has `publish = false` and
   is skipped). The tag is the bare version (`26.4.1`); see the tag-shape
   note in the recipe below.
4. The tag push drives `.github/workflows/release.yml`, which builds the
   artifacts and creates the GitHub release. release-plz never creates
   GitHub releases itself.
5. The OBS recipe below then ships the openSUSE RPM from the tag.

Direct pushes to `main` never publish (`release_always = false`): only a
merged Release PR does.

### Maintenance branches (`26.N.x`)

release-plz assumes a single release branch, so it never runs on the
per-minor branches (`26.0.x`, `26.1.x`, … — one per released line, forked
from `main` and never merged back). Fixes for a released line are
cherry-picked by hand:

```sh
git checkout 26.3.x
git cherry-pick <fix-commit>…
# Bump the workspace version, e.g. 26.3.5 → 26.3.6, as its own commit
# (`chore(release): cut 26.3.6`), so the tag matches the package version.
git tag 26.3.6
git push origin 26.3.x 26.3.6
```

Pushing the tag drives `release.yml` exactly like a main-line release, and
its notes-base walk picks the predecessor by version order across the
sibling branches — no ancestry needed. The same unprefixed tag shape
applies; a `v`-prefixed tag is invalid there too.

Note: tags `26.3.2`–`26.3.5` predate the automation and have no CHANGELOG
section (#401, not fixed here). A maintenance release cut from them still
resolves its notes base by version order, not from the CHANGELOG file, so
the missing sections do not shift the generated notes' start point — but
expect the hand-written history for that line to stay sparse until it is
backfilled.

### Release recipe (build.suse.de / IBS, project `QA:Maintenance:Test`)

1. **Tag the release commit** so `tar_scm`'s `revision=@PARENT_TAG@` resolves and
   `git describe --tags` stamps the version into the binaries. On `main` the
   tag comes from merging the Release PR (flow above) — reach for `git tag`
   only for a maintenance-branch cherry-pick. The `_service`
   `versionrewrite` pattern (`v?([0-9].*)`) tolerates a leading `v` but does not
   require one, and mtui's tags carry none: `26.4.1` becomes `Version: 26.4.1`.
   The format is `XX.Y.Z` — year-based line, major, patch. A `v`-prefixed tag is
   invalid and does not trigger the release workflow.

   ```sh
   git tag 26.4.1
   git push origin 26.4.1
   ```

2. **Check out the IBS package and drop in the sources:**

   ```sh
   osc -A ibs checkout QA:Maintenance:Test mtui
   cd QA:Maintenance:Test/mtui
   cp /path/to/mtui/_service /path/to/mtui/mtui.spec .
   ```

3. **Run all source services.** This fetches the tagged source, compresses it,
   fills the spec `Version:`, and vendors + audits every crate dependency:

   ```sh
   osc service ra
   # emits: mtui-<ver>.tar.zst, vendor.tar.zst (with .cargo/config +
   #        Cargo.lock + vendor/), cargo_config, _servicedata
   ```

4. **Commit and build:**

   ```sh
   osc add _service _servicedata cargo_config \
     mtui-<ver>.tar.zst mtui.spec vendor.tar.zst
   osc ci
   osc build          # local build against the service-generated tarballs
   osc results        # watch the OBS build
   ```

`cargo_vendor` audits the vendored crates against RustSec and can fail on an
advisory; triage it — `i-accept-the-risk=<RUSTSEC-ID>` is the security-reviewed
escape hatch, not a default. `update=false` in `_service` pins the checked-in
`Cargo.lock`; flip it to `true` only if a crate-conflict build error appears.

### Local distributable tarball (optional)

To build a plain binary tarball locally (e.g. to test the install layout) without
OBS, use the `xtask package` helper — it assembles the documented tree (both
binaries, completions, man pages, `LICENSE`, `README`) into a
`mtui-<version>-<target>.tar.gz` plus a `.sha256`:

```sh
cargo build --release
cargo xtask package --version 26.4.1 --target "$(rustc -vV | sed -n 's/host: //p')"
# → dist/release/mtui-26.4.1-<target>.tar.gz (+ .sha256)
```

This tarball is a local convenience only; the OBS build uses the git-tag source
from `tar_scm`, not this artifact.
