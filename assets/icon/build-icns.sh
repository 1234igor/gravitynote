#!/usr/bin/env bash
set -euo pipefail
DIR="$(cd "$(dirname "$0")" && pwd)"
MASTER="$DIR/AppIcon-1024.png"
ICONSET="$DIR/AppIcon.iconset"
[[ -f "$MASTER" ]] || { echo "missing $MASTER"; exit 1; }
rm -rf "$ICONSET"
mkdir -p "$ICONSET"
python3 - << PY
from PIL import Image
from pathlib import Path
im = Image.open("$MASTER").convert("RGBA")
iconset = Path("$ICONSET")
sizes = {
    "icon_16x16.png": 16,
    "icon_16x16@2x.png": 32,
    "icon_32x32.png": 32,
    "icon_32x32@2x.png": 64,
    "icon_128x128.png": 128,
    "icon_128x128@2x.png": 256,
    "icon_256x256.png": 256,
    "icon_256x256@2x.png": 512,
    "icon_512x512.png": 512,
    "icon_512x512@2x.png": 1024,
}
for name, px in sizes.items():
    im.resize((px, px), Image.Resampling.LANCZOS).save(iconset / name, "PNG")
print("iconset written")
PY
iconutil -c icns "$ICONSET" -o "$DIR/AppIcon.icns"
echo "wrote $DIR/AppIcon.icns"
