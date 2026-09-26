#!/usr/bin/env bash
# Build and launch GravityNote from dist/. Packaging lives in package.sh.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")" && pwd)"
"$ROOT/package.sh"

echo "gravitynote: launching…"
exec open -na "$ROOT/dist/GravityNote.app" --args "$@"
