# ClusterFuzzLite build image for the cargo-fuzz targets in fuzz/.
# Digest-pinned like every other external dependency; v1 currently aliases
# latest.
FROM gcr.io/oss-fuzz-base/base-builder-rust:v1@sha256:8a5b2b370c7778ce0190986e487dde952aa1d80496721f5647a3f11838a00a1c
# The pinned builder ships nightly-2025-09-05 (rustc 1.91), which is older
# than the workspace MSRV (rust-version = "1.96"). Install a newer dated
# nightly for the fuzz build; -Zsanitizer needs nightly, so a stable
# toolchain is not an option here. The base image selects its toolchain
# through the RUSTUP_TOOLCHAIN env var, which shadows `rustup default`, so
# override the env var.
RUN rustup toolchain install nightly-2026-07-01 --profile minimal
ENV RUSTUP_TOOLCHAIN=nightly-2026-07-01-x86_64-unknown-linux-gnu
COPY . $SRC/mtui
COPY .clusterfuzzlite/build.sh $SRC/build.sh
WORKDIR $SRC/mtui
