#!/bin/sh
# Build the committed universal (arm64 + x86_64) macOS binary at bin/ccq.
# The Claude Code plugin ships bin/ccq with no build step, so it must be a current
# 2-arch binary. Run via `make build-universal` (or directly). Same toolchain +
# source → deterministic output, so re-running on unchanged source is a no-op diff.
set -eu
cd "$(git rev-parse --show-toplevel)"

rustup target add aarch64-apple-darwin x86_64-apple-darwin >/dev/null 2>&1 || true
cargo build --release --target aarch64-apple-darwin
cargo build --release --target x86_64-apple-darwin
lipo -create -output bin/ccq \
  target/aarch64-apple-darwin/release/ccq \
  target/x86_64-apple-darwin/release/ccq

echo "built universal bin/ccq ($(./bin/ccq version)) — $(file -b bin/ccq)"
