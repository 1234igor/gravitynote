#!/usr/bin/env bash
# Stamp a DEV ribbon onto the app icon and write AppIconDev.icns.
#
# The development build is a second copy of the app on the Dock, one window
# away from the one holding real notes. The ribbon is the only thing that says
# which is which at a glance, so it is loud on purpose: a black band across the
# bottom-right corner, outlined in white so it survives both a dark icon and a
# pale one.
#
# Run this after changing AppIcon-1024.png. Like AppIcon.icns, the result is
# committed — `package.sh --dev` only rebuilds it when the master is newer, so
# the dev build never depends on Pillow being installed.
set -euo pipefail
DIR="$(cd "$(dirname "$0")" && pwd)"
MASTER="$DIR/AppIcon-1024.png"
ICONSET="$DIR/AppIconDev.iconset"
[[ -f "$MASTER" ]] || { echo "missing $MASTER"; exit 1; }
rm -rf "$ICONSET"
mkdir -p "$ICONSET"
python3 - << PY
from PIL import Image, ImageDraw, ImageFont
from pathlib import Path

S = 1024
im = Image.open("$MASTER").convert("RGBA").resize((S, S), Image.Resampling.LANCZOS)

# The band, as the strip of the square between two anti-diagonals x + y = k.
# Inside a square, x + y = k (k > S) is the segment (k-S, S) → (S, k-S), so a
# pair of them is a quadrilateral across the corner and no rotation is needed.
INNER, OUTER = 1.40, 1.72
def diagonal(k):
    return [(k * S - S, S), (S, k * S - S)]

band = Image.new("RGBA", (S, S), (0, 0, 0, 0))
d = ImageDraw.Draw(band)
d.polygon(diagonal(INNER) + list(reversed(diagonal(OUTER))), fill=(17, 17, 17, 255))
# Hairlines along both edges: on a dark icon the black band would otherwise
# dissolve into the artwork it is meant to interrupt.
for k in (INNER, OUTER):
    d.line(diagonal(k), fill=(255, 255, 255, 235), width=int(S * 0.012))

# "DEV" along the band, which runs up to the right — so the text is rotated a
# counter-clockwise 45°, drawn oversized and scaled down for a clean edge.
FONTS = [
    "/System/Library/Fonts/Supplemental/Arial Black.ttf",
    "/System/Library/Fonts/Supplemental/Arial Bold.ttf",
    "/System/Library/Fonts/HelveticaNeue.ttc",
]
size = int(S * 0.135)
font = None
for path in FONTS:
    try:
        font = ImageFont.truetype(path, size)
        break
    except OSError:
        continue
if font is None:
    raise SystemExit("no bold system font found for the DEV ribbon")

label = Image.new("RGBA", (S, S), (0, 0, 0, 0))
ld = ImageDraw.Draw(label)
ld.text((S / 2, S / 2), "DEV", font=font, fill=(255, 255, 255, 255), anchor="mm")
label = label.rotate(45, resample=Image.Resampling.BICUBIC)
# Centred on the middle of the band: the point where x = y on x + y = k.
mid = (INNER + OUTER) / 4 * S
band.alpha_composite(label, (int(mid - S / 2), int(mid - S / 2)))

im.alpha_composite(band)

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
im.save("$DIR/AppIconDev-1024.png", "PNG")
print("dev iconset written")
PY
iconutil -c icns "$ICONSET" -o "$DIR/AppIconDev.icns"
echo "wrote $DIR/AppIconDev.icns"
