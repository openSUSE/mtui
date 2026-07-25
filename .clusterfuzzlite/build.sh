#!/bin/bash -eu
# oss-fuzz/ClusterFuzzLite build contract: build every cargo-fuzz target and
# place the resulting binaries in $OUT. cargo-fuzz comes preinstalled in the
# base-builder-rust image and picks up fuzz/ automatically.
cargo fuzz build
find fuzz/target/x86_64-unknown-linux-gnu/release -maxdepth 1 -type f -executable ! -name '*.d' -exec cp -t "$OUT" {} +
