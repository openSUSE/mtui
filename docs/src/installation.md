# Installation

mtui-rs ships as two static binaries — `mtui` (the REPL) and `mtui-mcp` (the MCP
server) — with no runtime interpreter or virtualenv. On openSUSE install the
package from the openSUSE Build Service (OBS); everywhere else, build from source.

## Requirements

- A Rust toolchain, **edition 2024, MSRV 1.96**. The MSRV is pinned via
  `rust-version` in `Cargo.toml`. There is no `rust-toolchain.toml`.
- Optional runtime tools (see [Runtime dependencies](#runtime-dependencies)).

## Build from source

```sh
# Both binaries, optimized.
cargo build --release -p mtui-cli -p mtui-mcp --features mtui-mcp/mcp
```

`mtui-mcp`'s server is behind the `mcp` feature so the default build and the
`mtui` REPL never pull in the MCP SDK (`rmcp`/`axum`). Build it with that feature
enabled as shown above.

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

## Terminal-launcher scripts

The `terms`/`switch` REPL commands open reference-host sessions in a terminal
emulator using the `term.*.sh` launcher scripts shipped in `dist/terms/`
(gnome-terminal, konsole/kde, sakura, screen, tmux, urxvtc, xterm). Install them
into the datadir:

```sh
install -Dm755 dist/terms/*.sh -t /usr/share/mtui/terms/
```

`mtui` looks for the scripts under `$MTUI_TERMS_DIR` if that is set (this is how a
system install points at its shared datadir, e.g.
`MTUI_TERMS_DIR=/usr/share/mtui/terms`); otherwise it uses
`$XDG_DATA_HOME/mtui/terms`.

## Runtime dependencies

Some backends shell out to external tools. They are optional — mtui-rs degrades
gracefully when they are absent:

- **`svn`** — testreport checkout/commit (the SVN backend).
- **a terminal emulator** — for `terms`/`switch` (see above).

The QAM review workflow (`assign`/`unassign`/`approve`/`reject`/`comment`) talks
to the OBS/IBS API **natively** — no `osc` subprocess. It reads credentials from
your `oscrc`, located exactly like `osc` itself: `$OSC_CONFIG`, then
`$XDG_CONFIG_HOME/osc/oscrc`, then `~/.oscrc`. See the `[obs]` table in
[Configuration](configuration.md).

## Packaged install (openSUSE)

On openSUSE, prefer the `mtui-rs.spec` package build, which installs the binaries,
completions, man pages, and `term.*.sh` scripts into the standard system paths
and declares the optional runtime tools as recommends.

## Cutting a release (maintainers)

Releases are built and published through the **openSUSE Build Service (OBS)** from
the `mtui-rs.spec`, not from CI. Tag the release commit so `git describe --tags`
stamps the version into the binaries, then update the OBS package sources:

```sh
git tag v1.2.0
git push origin v1.2.0
```

To build a distributable tarball locally (e.g. to feed OBS or to test the layout),
use the `xtask package` helper — it assembles the documented tree (both binaries,
completions, man pages, `term.*.sh`, `LICENSE`, `README`) into a
`mtui-rs-<version>-<target>.tar.gz` plus a `.sha256`:

```sh
cargo build --release -p mtui-cli -p mtui-mcp --features mtui-mcp/mcp
cargo xtask package --version v1.2.0 --target "$(rustc -vV | sed -n 's/host: //p')"
# → dist/release/mtui-rs-v1.2.0-<target>.tar.gz (+ .sha256)
```
