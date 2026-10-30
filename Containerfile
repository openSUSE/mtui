# Container image for mtui: the `mtui` REPL and the `mtui-mcp` MCP server.
#
# Multi-stage. The `builder` stage compiles both release binaries with the BCI
# Rust toolchain; the `runtime` stage ships them on an openSUSE base with
# nothing else but an SSH client (debugging) and the Mozilla CA bundle.
# Multi-arch (`linux/amd64`, `linux/arm64`) is delivered by building natively
# per arch under buildx (see .github/workflows/container.yml): the Rust build
# needs no cross toolchain, so no stage branches on TARGETARCH.
#
# Base choice: `opensuse/bci/bci-base` is not published on
# registry.opensuse.org (tags/list returns NAME_UNKNOWN; only `bci-base-fips`
# is), and `opensuse/bci/bci-minimal` ships neither zypper nor useradd, so
# `openssh-clients` cannot be installed there. The plain `opensuse/tumbleweed`
# base is the working equivalent — the same userspace with zypper and shadow —
# and pairs with the Tumbleweed-based BCI Rust builder.
#
# Base images are pinned by tag + index digest (multi-arch manifest list) and
# written literally, not via ARG indirection: Dependabot's docker parser does
# not resolve ARG-fed FROM lines (dependabot-core #4597, #2057), so
# indirection would silently opt out of base-image bumps. Refresh a pin with
# e.g. `curl -sID -H "Accept: application/vnd.docker.distribution.manifest.list.v2+json"
# https://registry.opensuse.org/v2/<repo>/manifests/<tag>` (Docker-Content-Digest
# header) and keep the tag in step; Dependabot's `docker` ecosystem (see
# .github/dependabot.yml) proposes tag bumps, digest included.

FROM registry.opensuse.org/opensuse/bci/rust:1.99@sha256:9c07acbf8a1fb0379b3520e7f6bbb29f2f4563eeed4a159c5011df80b9ccb227 AS builder
WORKDIR /build
# Manifests first: the fetch layer below survives source-only edits (no cache
# mounts: `RUN --mount=type=cache` is a BuildKit extension podman/buildah
# reject, and this file must build under both). A new workspace member needs
# its manifest line above, or the COPY fails loudly.
COPY Cargo.toml Cargo.lock ./
COPY xtask/Cargo.toml xtask/
COPY crates/mtui-cli/Cargo.toml crates/mtui-cli/
COPY crates/mtui-config/Cargo.toml crates/mtui-config/
COPY crates/mtui-core/Cargo.toml crates/mtui-core/
COPY crates/mtui-datasources/Cargo.toml crates/mtui-datasources/
COPY crates/mtui-hosts/Cargo.toml crates/mtui-hosts/
COPY crates/mtui-mcp/Cargo.toml crates/mtui-mcp/
COPY crates/mtui-testreport/Cargo.toml crates/mtui-testreport/
COPY crates/mtui-tui/Cargo.toml crates/mtui-tui/
COPY crates/mtui-types/Cargo.toml crates/mtui-types/
# Target stubs so the fetch can resolve the workspace (target autodiscovery
# needs a source file per member; every crate's lib lives at `src/lib.rs`).
# The real sources overlay these below.
# `--locked`: image bytes come from the committed Cargo.lock, as in release.yml.
RUN for d in crates/*/ xtask/; do mkdir -p "$d/src"; touch "$d/src/lib.rs"; done && cargo fetch --locked
COPY crates/ crates/
COPY src/ src/
COPY xtask/ xtask/
RUN cargo build --release --locked --package mtui

FROM registry.opensuse.org/opensuse/tumbleweed:latest@sha256:b56730060917ba7411c7720bb262b5512a048a302c78dc7a0149a985d7b85a8a AS runtime
# Labels carry the release version/revision passed by the workflow; local
# builds keep the `unreleased`/`unknown` defaults.
ARG BUILD_VERSION=unreleased
ARG BUILD_REVISION=unknown
# openssh-clients is for debugging only: mtui itself speaks SSH through its
# russh stack, never through this binary. ca-certificates-mozilla already
# ships in the base; naming it keeps the TLS trust explicit across refreshes.
RUN zypper --non-interactive install --no-recommends openssh-clients ca-certificates-mozilla && zypper clean --all
# Fixed UID so a numeric `run --user` and volume ownership stay predictable.
RUN useradd --create-home --uid 10001 --user-group mtui
# Root-owned and mode-pinned: a process running as mtui must not be able to
# rewrite its own binaries in place (`/usr/local/bin` is root-owned, but the
# file mode alone decides writability).
COPY --from=builder --chown=root:root --chmod=755 /build/target/release/mtui /build/target/release/mtui-mcp /usr/local/bin/
# Startup smoke: the builder and runtime snapshots float independently, so a
# glibc skew would surface here at build time instead of on first run.
RUN /usr/local/bin/mtui --version && /usr/local/bin/mtui-mcp --version
USER mtui
WORKDIR /home/mtui
ENV HOME=/home/mtui
EXPOSE 8000
LABEL org.opencontainers.image.title="mtui" \
      org.opencontainers.image.description="Maintenance Test Update Installer for SUSE QE update validation" \
      org.opencontainers.image.version="${BUILD_VERSION}" \
      org.opencontainers.image.revision="${BUILD_REVISION}" \
      org.opencontainers.image.source="https://github.com/openSUSE/mtui" \
      org.opencontainers.image.licenses="GPL-3.0-or-later"
# Both binaries are on PATH; the run command selects the mode, e.g.
# `run … mtui` (REPL, needs `-it`) or `run … mtui-mcp --transport http …`.
CMD ["/usr/local/bin/mtui", "--help"]
