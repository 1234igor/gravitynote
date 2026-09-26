#!/usr/bin/env bash
# Build GravityNote.app: a self-contained bundle that can be dragged to /Applications.
#
#   ./package.sh              build dist/GravityNote.app
#   ./package.sh --install    ...and move it to /Applications
#   ./package.sh --zip        ...and write dist/GravityNote-<version>.zip
#   ./package.sh --dev        build dist/GravityNoteDev.app instead
#
# --dev builds the same binary under a different name, identity and icon. The
# executable ends in `-dev`, which is how the app knows to keep its notes,
# settings and backups in `gravitynote-gpui-dev` and to leave the global hotkey
# to the real copy (see src/dev.rs). It is never installed: the whole point is
# that the copy holding real notes is not the copy being restarted every few
# minutes. Use ./dev.sh to build and launch one.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")" && pwd)"
cd "$ROOT"

BIN_SRC="$ROOT/target/release/gravitynote-gpui"
ICONSET="$ROOT/assets/icon/AppIcon.iconset"
ICNS="$ROOT/assets/icon/AppIcon.icns"
DEV_MASTER="$ROOT/assets/icon/AppIcon-1024.png"
DEV_ICNS="$ROOT/assets/icon/AppIconDev.icns"
PLIST_TMPL="$ROOT/assets/icon/Info.plist.template"

install=0
zip=0
dev=0
for arg in "$@"; do
  case "$arg" in
    --install) install=1 ;;
    --zip) zip=1 ;;
    --dev) dev=1 ;;
    *) echo "package.sh: unknown option $arg" >&2; exit 2 ;;
  esac
done

# The bundle's identity. Everything that differs between the real app and the
# development one is these five lines; the rest of the script builds whichever
# it was handed.
if [[ "$dev" -eq 1 ]]; then
  label="gravitynote-dev"
  app_name="GravityNoteDev"
  bundle_name="GravityNote Dev"
  bundle_id="com.gravitynote.gpui.dev"
  exe="gravitynote-gpui-dev"
  icon_name="AppIconDev"
  icon_src="$DEV_ICNS"
else
  label="gravitynote"
  app_name="GravityNote"
  bundle_name="GravityNote"
  bundle_id="com.gravitynote.gpui"
  exe="gravitynote-gpui"
  icon_name="AppIcon"
  icon_src="$ICNS"
fi

APP="$ROOT/dist/$app_name.app"
CONTENTS="$APP/Contents"

# Installing a dev build into /Applications would put it exactly where the app
# holding real notes lives, under a name one letter away from it. Refuse.
if [[ "$dev" -eq 1 && ( "$install" -eq 1 || "$zip" -eq 1 ) ]]; then
  echo "package.sh: --dev is a local build; it is never installed or shipped" >&2
  exit 2
fi

version="$(awk -F'"' '/^version = /{print $2; exit}' Cargo.toml)"

echo "$label: building release…"
cargo build --release

# The icon is committed as a master PNG plus an iconset; .icns is derived.
if [[ ! -f "$ICNS" || "$ICONSET" -nt "$ICNS" ]]; then
  echo "$label: rebuilding AppIcon.icns…"
  iconutil -c icns "$ICONSET" -o "$ICNS"
fi
# The dev icon is the same master with a DEV ribbon stamped on it. Also
# committed, so a dev build does not need Pillow on the machine.
if [[ "$dev" -eq 1 && ( ! -f "$DEV_ICNS" || "$DEV_MASTER" -nt "$DEV_ICNS" ) ]]; then
  echo "$label: rebuilding AppIconDev.icns…"
  "$ROOT/assets/icon/build-dev-icns.sh" >/dev/null
fi

echo "$label: assembling $app_name.app…"
rm -rf "$APP"
mkdir -p "$CONTENTS/MacOS" "$CONTENTS/Resources"
install -m 755 "$BIN_SRC" "$CONTENTS/MacOS/$exe"
cp "$icon_src" "$CONTENTS/Resources/$icon_name.icns"
sed -e "s/__VERSION__/$version/g" \
    -e "s/__EXECUTABLE__/$exe/g" \
    -e "s/__IDENTIFIER__/$bundle_id/g" \
    -e "s/__NAME__/$bundle_name/g" \
    -e "s/__ICON__/$icon_name/g" \
    "$PLIST_TMPL" > "$CONTENTS/Info.plist"

# Ad-hoc signature. Without one the bundle reports as "damaged" once it has been
# copied or zipped; there is no Developer ID, so the identity is "-".
echo "$label: signing…"
xattr -cr "$APP"
codesign --force --sign - --timestamp=none "$APP" 2>/dev/null

# A bundle that is not self-contained fails only on someone else's machine, so
# check here rather than trusting it: no dylibs outside the OS, valid signature.
foreign="$(otool -L "$CONTENTS/MacOS/$exe" | tail -n +2 |
  grep -v -e '/usr/lib/' -e '/System/Library/' || true)"
if [[ -n "$foreign" ]]; then
  echo "package.sh: bundle depends on libraries outside the OS:" >&2
  echo "$foreign" >&2
  exit 1
fi
codesign --verify --strict "$APP"

if [[ "$install" -eq 1 ]]; then
  dest="/Applications/$app_name.app"
  if [[ -d "$dest" ]]; then
    echo "$label: replacing ${dest}…"
    rm -rf "$dest"
  fi
  ditto "$APP" "$dest"
  /System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister -f "$dest" 2>/dev/null || true
  echo "$label: installed $dest"
fi

if [[ "$zip" -eq 1 ]]; then
  archive="$ROOT/dist/GravityNote-$version.zip"
  rm -f "$archive"
  ditto -c -k --keepParent "$APP" "$archive"
  echo "$label: wrote $archive"
fi

# Comparing the installed binary against target/release will always differ —
# the copy in the bundle carries the ad-hoc signature. Compare bundles.
if [[ "$install" -eq 1 ]]; then
  if ! cmp -s "$CONTENTS/MacOS/$exe" \
    "/Applications/$app_name.app/Contents/MacOS/$exe"; then
    echo "package.sh: /Applications does not match what was just built" >&2
    exit 1
  fi
fi

if [[ "$dev" -eq 1 ]]; then
  echo "$label: $APP ($version) — data in ~/Library/Application Support/gravitynote-gpui-dev"
else
  echo "$label: $APP ($version) — drag it to /Applications"
fi
