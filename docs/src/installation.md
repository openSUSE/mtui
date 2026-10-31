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

## Container image

Every push to `main` and every release tag also ships a multi-arch
(`linux/amd64`, `linux/arm64`) image with both binaries, built by
`.github/workflows/container.yml`:

- `ghcr.io/opensuse/mtui:latest` (GA releases), `:<X.Y.Z>`, `:<X.Y>`,
  `:edge` (main branch). Pre-release tags (`-rcN`) publish only their exact
  version, never `latest`.
- The same tags are mirrored to `registry.opensuse.org/opensuse/mtui` once the
  mirror credentials are wired up (see the workflow).

The image runs as a non-root user (`mtui`, uid 10001) on the openSUSE base with
an SSH client for debugging; mtui itself speaks SSH through its own stack, never
through that binary. Unlike the static musl release binaries, the image binaries
are dynamically linked against the base image's glibc. No credentials are baked
in: mount yours read-only from the host. Stage copies owned by the container
user (uid 10001) — a host-only `~/.ssh` at mode 700 is invisible inside, so copy
first. Never mount the live `~/.ssh` with `:Z`: the private relabel can lock a
confined host `sshd` out of `authorized_keys`. The copies below use the shared
`:z` label instead. `~/.cache/mtui-container` holds secret material (plaintext
OBS password, private keys): the recipe scopes it to uid 10001, never
group/other-readable.

```sh
# Stage copies owned by the container user (once; re-run after rotation).
mkdir -p ~/.cache/mtui-container
cp -p ~/.oscrc ~/.cache/mtui-container/oscrc
cp -rp ~/.ssh ~/.cache/mtui-container/ssh
podman unshare chown -R 10001:10001 ~/.cache/mtui-container
podman unshare chmod -R u+rwX,go-rwx ~/.cache/mtui-container
```

To refresh after rotation, remove the container-owned tree from inside the
user namespace first, then re-run the recipe: `podman unshare rm -rf
~/.cache/mtui-container`.

A passphrase-protected key needs the agent — mtui tries `SSH_AUTH_SOCK` first,
then identity files, and never prompts for a passphrase. Forward yours with two
extra flags on any example below:

```sh
-v "$SSH_AUTH_SOCK:/ssh-agent.sock:ro" -e SSH_AUTH_SOCK=/ssh-agent.sock
```

Note `:ro` does not restrict what the container can do with the agent: anything
reaching the socket can use every loaded key, so forward an agent holding only
the keys mtui needs.

`$HOME` is `/home/mtui`, and the default `template_dir` (`.`) and the history
file resolve under it, so back it with a writable volume — that same volume
makes the container read-only-rootfs friendly:

```sh
# REPL (needs a TTY for `shell`/`edit`).
podman run -it --rm \
  -v mtui-home:/home/mtui \
  -v "$HOME/.cache/mtui-container/oscrc:/home/mtui/.oscrc:ro,z" \
  -v "$HOME/.cache/mtui-container/ssh:/home/mtui/.ssh:ro,z" \
  -v "$HOME/.config/mtui/mtui.toml:/home/mtui/.config/mtui/mtui.toml:ro,z" \
  ghcr.io/opensuse/mtui:latest mtui
```

With `--read-only`, add only the `mtui-home` volume (writable) and keep the
credential mounts as above:

```sh
podman run -it --rm --read-only \
  -v mtui-home:/home/mtui \
  -v "$HOME/.cache/mtui-container/oscrc:/home/mtui/.oscrc:ro,z" \
  -v "$HOME/.cache/mtui-container/ssh:/home/mtui/.ssh:ro,z" \
  ghcr.io/opensuse/mtui:latest mtui
```

Serve the MCP tools over HTTP (one process, many isolated clients):

```sh
podman run -d --name mtui-mcp -p 127.0.0.1:8000:8000 \
  -v mtui-home:/home/mtui \
  -v "$HOME/.cache/mtui-container/oscrc:/home/mtui/.oscrc:ro,z" \
  -v "$HOME/.cache/mtui-container/ssh:/home/mtui/.ssh:ro,z" \
  ghcr.io/opensuse/mtui:latest \
  mtui-mcp --transport http --host 0.0.0.0 --port 8000
```

`--host 0.0.0.0` binds all container interfaces so the published port reaches
the server; `-p 127.0.0.1:` keeps it off the LAN. Reach it through localhost:
rmcp's DNS-rebinding guard compares the request's `Host` header against its
allowlist (`localhost`, `127.0.0.1`, `::1` by default), not the client's
address, so any LAN client sending `Host: localhost` would pass it. Commands on
the [MCP deny-list](mcp.md#permanent-deny-list) are not MCP tools.

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

### Release recipe (build.suse.de / IBS, project `QA:Maintenance:Test`)

1. **Tag the release commit** so `tar_scm`'s `revision=@PARENT_TAG@` resolves and
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
binaries, completions, man pages, `LICENSE`, `README`) into a
`mtui-<version>-<target>.tar.gz` plus a `.sha256`:

```sh
cargo build --release
cargo xtask package --version 26.4.1 --target "$(rustc -vV | sed -n 's/host: //p')"
# → dist/release/mtui-26.4.1-<target>.tar.gz (+ .sha256)
```

This tarball is a local convenience only; the OBS build uses the git-tag source
from `tar_scm`, not this artifact.
