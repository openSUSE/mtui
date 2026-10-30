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
# Base images float on tags, with no digest pin: registry.opensuse.org rebuilds
# these tags daily and deletes the old digests within days, so a pin goes 404
# before Dependabot's weekly docker bump can refresh it. Dependabot (see
# .github/dependabot.yml) still proposes bumps for versioned tags (e.g.
# bci/rust:1.99); the rolling `tumbleweed:latest` has no successor tag and
# floats without PRs. FROM lines stay literal, not ARG-fed, as its docker
# parser does not resolve ARG-fed FROM lines.
# Build-time truth is the SLSA provenance, not the recorded digest: the CI
# build sets `provenance: true`, whose materials list the base digests each
# leg actually pulled, and the merge job attests the final manifest digest
# (see .github/workflows/container.yml). The `Record base digests` step only
# re-resolves the tags at merge time for the run summary — informational, and
# it can differ from what was built when a tag rolls mid-run. Both native legs
# pull their bases at build time, so a tag roll mid-run can leave the arches
# on different base snapshots; the provenance materials identify the actual
# snapshot per build.

FROM registry.opensuse.org/opensuse/bci/rust:1.99 AS builder
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
# Binaries, not the package name: `--bin` survives a root-package rename.
RUN cargo build --release --locked --bin mtui --bin mtui-mcp

FROM registry.opensuse.org/opensuse/tumbleweed:latest AS runtime
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
