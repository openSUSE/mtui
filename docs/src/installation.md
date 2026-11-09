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

## Vim syntax highlighting

A Vim plugin for editing QAM test reports ships in `dist/vim-plugin/`: filetype
detection plus syntax highlighting for the testreport/export text format
(status keywords, section labels, and unfilled `YES/NO`/`PASSED/FAILED`
placeholders shown as errors so you don't forget to fill them in). Filetype
detection triggers on any file named `log` whose first line starts with
`SUMMARY:` (i.e. an exported testreport).

On openSUSE it is packaged as the `mtui-vim-plugin` subpackage, installed into
the system Vim runtime addon dir. To install manually:

```sh
install -Dm644 dist/vim-plugin/ftdetect/testreport.vim /usr/share/vim/site/ftdetect/testreport.vim
install -Dm644 dist/vim-plugin/syntax/testreport.vim   /usr/share/vim/site/syntax/testreport.vim
```

For a per-user install without root, drop the two files under your Vim runtime
directory instead (`~/.vim/{ftdetect,syntax}/testreport.vim`, or
`~/.config/nvim/{ftdetect,syntax}/testreport.vim` for Neovim).

## Runtime dependencies

One backend shells out to an external tool. It is optional — mtui degrades
gracefully when it is absent:

- **`svn`** — testreport checkout/commit (the SVN backend).

The QAM review workflow (`assign`/`unassign`/`approve`/`reject`/`comment`) talks
to the OBS/IBS API **natively** — no `osc` subprocess. It reads credentials from
your `oscrc`, located exactly like `osc` itself: `$OSC_CONFIG`, then
`$XDG_CONFIG_HOME/osc/oscrc`, then `~/.oscrc`. See the `[obs]` table in
[Configuration](configuration.md).

## Prebuilt packages (GitHub releases)

Every release carries `.deb` and `.rpm` packages next to the tarballs, built by
the `package` job in `.github/workflows/release.yml`. They split the same way the
spec does — `mtui` recommends `mtui-mcp`, so installing `mtui` alone still gets
both — with four caveats the OBS build does not have:

- **x86_64 only**, built for `x86_64-unknown-linux-musl`. No aarch64, no macOS.
- **Same `Name:` as the OBS RPM.** In a repository carrying both, they compete by
  version comparison alone. On openSUSE prefer the OBS package.
- **No `mtui-vim-plugin`.**
- **The `.rpm`s own no directories.** cargo-generate-rpm has no `%dir`, so
  erasing them leaves the doc- and licensedirs behind.

Because the binaries are static the packages declare no *mandatory* runtime or
library dependencies — no libc, and no shell either, since nothing they ship is
a script. Both are asserted by the release job. They do declare recommends:
`subversion` on both, plus `mtui-mcp` from `mtui`.

Verify with `sha256sum --ignore-missing -c mtui-packages.sha256` — one file
covers all four packages, so a partial download needs `--ignore-missing`.

## Packaged install (openSUSE)

On openSUSE, prefer the `mtui.spec` package build, which installs the binaries,
completions, and man pages into the standard system paths and declares `svn` as a
recommends. It builds three packages:

- **`mtui`** — the REPL, its completions and man page. Recommends `mtui-mcp`, so
  a plain `zypper in mtui` still gets both binaries as it did before they were
  split.
- **`mtui-mcp`** — the MCP server, its completions and man page. Installable on
  its own; it reads the same config.
- **`mtui-vim-plugin`** — see [above](#vim-syntax-highlighting).

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

The crates.io publish uses **Trusted Publishing**: no registry token is stored.
One-time setup:

- the **`crates-io`** environment on `openSUSE/mtui` (GitHub web UI, needs repo
  admin; optionally with required reviewers, so a publish waits for a human
  click);
- on crates.io, for each of the nine crates (`suse-mtui` and the eight `mtui-*`
  crates; Settings → Trusted Publishing, as an owner): repository owner
  `openSUSE`, repository `mtui`, workflow `release.yml`, environment
  `crates-io`.

The `crates-io` job requests `id-token: write`, and
`rust-lang/crates-io-auth-action` swaps the job's OIDC token for a short-lived
crates.io token that is revoked when the job ends. A crate that does not exist
yet cannot be configured, so the very first publish of a new crate name needs a
scoped API token (`publish-new`, with an expiry) exported as
`CARGO_REGISTRY_TOKEN` and run locally.

### Release model

`main` is development. Its version is `X.Y.0` and it never tags or publishes a
release. A release is always cut on a per-minor branch (`26.3.x`, `26.4.x`, …,
forked from `main` and never merged back):

- The **stable branch** is the highest-numbered `X.Y.x` branch (compared as
  numbers, so `26.10.x` is higher than `26.9.x`). Its versions are `X.Y.Z` with
  `Z >= 1`.
- Only a plain `X.Y.Z` tag (`Z >= 1`, no pre-release suffix) whose commit is on
  the stable branch **publishes to crates.io**. The `gate` job in
  `release.yml` decides this with `.github/scripts/crates-io-gate.sh` (tested
  by `xtask/tests/crates_io_gate.rs`) plus a `git merge-base --is-ancestor`
  check of the tagged commit.
- Every tag still gets a GitHub release. A tag on an older branch, an `X.Y.0`
  tag or a pre-release tag is not published to crates.io.
- After `26.5.x` is forked and before `26.5.1` is tagged, a `26.4.z` tag does
  not publish. That is by design.

All nine publishable crates move together: the `suse-mtui` facade (binaries
stay `mtui`/`mtui-mcp`) and the eight `mtui-*` libraries (`xtask` has
`publish = false` and is skipped). Cargo publishes them in dependency order.

### Release flow

1. Cherry-pick the fixes onto the stable branch (for a new line, fork
   `X.Y.x` from `main` and set the first patch version `X.Y.1`).
2. Bump the version, as its own commit (`chore(release): bump version to
   X.Y.Z`): `[workspace.package] version` plus the `=…` pins in
   `[workspace.dependencies]`, then refresh `Cargo.lock` and run `cargo xtask
   gen`. The pins cannot derive the workspace version with plain cargo, so the
   bump is manual, and `cargo test -p xtask --test packaging` fails until all
   match.
3. Cut the changelog (`chore(release): cut X.Y.Z`): rename `## [Unreleased]`
   to `## [X.Y.Z] - <date>`.
4. Check the publish locally: `cargo publish --workspace --locked --dry-run`.
5. Tag the commit (`git tag X.Y.Z`) and push the branch and the tag
   (`git push origin X.Y.x X.Y.Z`). The tag is the bare version; a
   `v`-prefixed tag is invalid.
6. The tag push drives `.github/workflows/release.yml`: build and package, the
   GitHub release, then, when the gate says so, `cargo publish --workspace
   --locked` in the `crates-io` job.
7. The OBS recipe below then ships the openSUSE RPM from the tag.

`main` takes no release commit. Do not tag on `main`: an `X.Y.0` tag would get
a GitHub release but never publishes.

### Recovering from a partial publish

`cargo publish --workspace` is not atomic, and crates.io limits new crate names
to a burst of five, then about one every ten minutes, so the first publish of a
line with more than five new crates stops with HTTP 429 after some crates are
out. Existing crate names are not limited, so later releases do not hit this.

A published version cannot be re-published, and **re-running the failed job
cannot work**: `cargo publish --workspace` does not skip crates that already
exist and fails on the first one. Publish the rest by hand, in dependency order,
from a checkout of the tag, retrying on a 429:

```sh
cargo publish -p mtui-core --locked   # then mtui-cli, mtui-mcp, suse-mtui
```

If a published crate has a defect, `cargo yank` hides it, but the fix is a new
patch release.

### Maintenance branches (`X.Y.x`)

The release flow above applies to every branch. Pushing the tag drives
`release.yml` exactly like a stable-branch release, and its notes-base walk
picks the predecessor by version order across the sibling branches — no
ancestry needed.

### Release recipe (build.suse.de / IBS, project `QA:Maintenance:Test`)

1. **Tag the release commit** (step 5 of the [release flow](#release-flow)) so
   `tar_scm`'s `revision=@PARENT_TAG@` resolves and
   `git describe --tags` stamps the version into the binaries. The `_service`
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
binaries, completions, man pages, the Vim plugin, `LICENSE`, `README`) into a
`mtui-<version>-<target>.tar.gz` plus a `.sha256`:

```sh
cargo build --release
cargo xtask package --version 26.4.1 --target "$(rustc -vV | sed -n 's/host: //p')"
# → dist/release/mtui-26.4.1-<target>.tar.gz (+ .sha256)
```

This tarball is a local convenience only; the OBS build uses the git-tag source
from `tar_scm`, not this artifact.
