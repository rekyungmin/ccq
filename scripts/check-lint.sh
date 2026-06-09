#!/bin/sh
# Fast local gate mirroring CI's fmt + clippy steps, so a commit never lands code
# CI would reject. fmt first (instant) for a fast fail; then clippy (warnings = errors).
# Run by the pre-commit hook and by `make lint`. Bypass a commit with --no-verify.
set -eu
cd "$(git rev-parse --show-toplevel)"

cargo fmt --check || {
  echo "ccq: code is not formatted — run \`make fmt\` (cargo fmt) and re-stage." >&2
  exit 1
}
cargo clippy --all-targets -- -D warnings
