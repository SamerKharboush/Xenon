#!/usr/bin/env bash
# Regenerate the lightweight sprite set from the full-size originals.
#
# src/sprites/*.png       448x448 originals (pet-mochi artwork, bundled)
# src/sprites/light/*.png 320x320 copies used when light_sprites = true
#
# 320 px is the largest size the pet is ever drawn at (240 CSS px) with
# headroom for HiDPI screens; the 448 px originals decode to ~800 KB each in
# the WebContent process. Settings -> Performance picks between the sets.
set -euo pipefail

SRC="${1:-src/sprites}"
DST="${2:-src/sprites/light}"
SIZE="${3:-320}"

mkdir -p "$DST"
for f in "$SRC"/*.png; do
  sips --resampleHeightWidth "$SIZE" "$SIZE" "$f" --out "$DST/$(basename "$f")" >/dev/null
done
echo "resized $(ls "$SRC"/*.png | wc -l | tr -d ' ') sprites to ${SIZE}x${SIZE} in $DST"