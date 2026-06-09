#!/bin/sh
# Fail if the committed bin/ccq is stale vs the current source. Rebuilds the
# universal binary (same toolchain + source → identical bytes, so this is a no-op
# when bin/ccq is already current) and compares it to what's committed at HEAD.
# Used by the pre-push hook — the freshness gate lives here because same-machine
# builds are deterministic (CI can't byte-compare: SDK/linker differ across hosts).
set -eu
cd "$(git rev-parse --show-toplevel)"

sh scripts/build-universal.sh >&2

if git rev-parse --verify -q HEAD >/dev/null && ! git diff --quiet HEAD -- bin/ccq; then
  echo >&2
  echo "ccq: bin/ccq was stale — it has been rebuilt from the current source." >&2
  echo "     Stage and amend (or add a new commit), then push again:" >&2
  echo "       git add bin/ccq && git commit --amend --no-edit" >&2
  exit 1
fi
echo "ccq: bin/ccq is current." >&2
