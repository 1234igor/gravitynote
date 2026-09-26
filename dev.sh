#!/usr/bin/env bash
# Build and launch the development copy of GravityNote.
#
#   ./dev.sh          build dist/GravityNoteDev.app and launch it
#   ./dev.sh --wipe   ...after throwing the dev note away and re-seeding it
#
# This is the copy to use while working on the app. It carries a DEV ribbon on
# its icon, and it reads and writes only
# ~/Library/Application Support/gravitynote-gpui-dev — so editing, deleting,
# find-and-replacing or corrupting its note cannot touch the real one. It also
# leaves the global show/hide chord to the real copy.
#
# The real app is built and installed with ./package.sh --install. Do that when
# shipping a finished change, not while trying one out.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")" && pwd)"
DATA="$HOME/Library/Application Support/gravitynote-gpui-dev"
NOTE="$DATA/note.md"
APP="$ROOT/dist/GravityNoteDev.app"

wipe=0
args=()
for arg in "$@"; do
  case "$arg" in
    --wipe) wipe=1 ;;
    *) args+=("$arg") ;;
  esac
done

"$ROOT/package.sh" --dev

if [[ "$wipe" -eq 1 ]]; then
  echo "gravitynote-dev: wiping $DATA"
  rm -rf "$DATA"
fi

# Seed a note worth scrolling. An empty dev app shows the welcome text, which
# is one screen and no separators — not enough to see wrapping, headings, code
# fences, rules or the scroll rail behave.
if [[ ! -f "$NOTE" ]]; then
  echo "gravitynote-dev: seeding $NOTE"
  mkdir -p "$DATA"
  cat > "$NOTE" <<'NOTE'
### dev note

This is the development copy. Nothing here is real — the app writes only to
`gravitynote-gpui-dev`, so edit, delete and break it freely.

- [ ] a task
- [x] a finished one
    - a nested line, long enough to soft-wrap somewhere in the middle of the reading measure so wrap behaviour is visible

`inline code`, **bold**, *italic*, and a [link](https://example.com).

```rust
fn main() {
    println!("a fenced block, for the code background and the fence map");
}
```

---

## second note

The rule above is a note boundary. The one below it too — three notes in all,
which is what the ⌘⇧↑ promote chip and block motion need to have something to
act on.

> a quote line
> and its continuation

1. numbered
2. list
3. items

---

# third note

Enough lines follow to make the window scroll, so the top and bottom edges,
the scroll rail and the caret's reveal margin can all be seen doing their job.

line one
line two
line three
line four
line five
line six
line seven
line eight
line nine
line ten
line eleven
line twelve
line thirteen
line fourteen
line fifteen
line sixteen
line seventeen
line eighteen
line nineteen
line twenty
NOTE
fi

# `open -na` starts a fresh copy every time, so without this the Dock fills up
# with dev instances over a working session. Match the bundle path: the
# executable is `gravitynote-gpui-dev`, not `GravityNoteDev`.
pkill -f "GravityNoteDev.app" 2>/dev/null || true
sleep 1

echo "gravitynote-dev: launching…"
exec open -na "$APP" --args "${args[@]+"${args[@]}"}"
