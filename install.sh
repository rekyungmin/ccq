#!/bin/sh
# ccq installer — works two ways:
#
#   1. From a cloned repo:   sh install.sh
#      → installs the repo's bin/ccq (delegates to `ccq install`, one source of truth).
#
#   2. As a one-liner:       curl -fsSL https://raw.githubusercontent.com/rekyungmin/ccq/main/install.sh | sh
#      → downloads the latest release binary into ~/.local/bin (override with CCQ_BINDIR).
#
# In a Claude Code session the plugin already puts `ccq` on PATH, so you can just run
# `ccq install` directly — no clone, no curl.
set -eu

REPO="rekyungmin/ccq"
BINDIR="${CCQ_BINDIR:-$HOME/.local/bin}"

# Mode 1 — running inside a clone (bin/ccq sits next to this script).
DIR="$(cd "$(dirname "$0")" 2>/dev/null && pwd || true)"
if [ -n "${DIR:-}" ] && [ -x "$DIR/bin/ccq" ]; then
  exec "$DIR/bin/ccq" install "$@"
fi

# Mode 2 — standalone download of the latest release.
URL="https://github.com/$REPO/releases/latest/download/ccq"
mkdir -p "$BINDIR"
tmp="$BINDIR/ccq.tmp.$$"
trap 'rm -f "$tmp"' EXIT
echo "downloading ccq → $BINDIR/ccq"
curl -fsSL "$URL" -o "$tmp"
chmod +x "$tmp"
mv -f "$tmp" "$BINDIR/ccq"
echo "installed: $BINDIR/ccq ($("$BINDIR/ccq" version 2>/dev/null || echo '?'))"

case ":$PATH:" in
  *":$BINDIR:"*) ;;
  *) printf 'note: %s is not on PATH — add this to your shell rc:\n  export PATH="%s:$PATH"\n' "$BINDIR" "$BINDIR" >&2 ;;
esac
