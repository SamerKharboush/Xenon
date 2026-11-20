#!/usr/bin/env bash
# scripts/make-dmg.sh <signed-Xenon.app> <out.dmg>
#
# Build a compressed, drag-to-install DMG from an ALREADY-SIGNED .app, matching
# the layout in tauri.conf.json (window 660x400, dmg-bg.png background, app icon
# at (180,170), Applications link at (480,170)).
#
# Why separate from `cargo tauri build --bundles dmg`: the DMG bundler rebuilds
# the app, which would throw away the identifier signature from sign-app.sh and
# reintroduce the TCC cdhash problem. Build the app once, sign it, then package
# the signed app into the DMG here.
set -euo pipefail

usage() { echo "usage: $0 <signed-Xenon.app> <out.dmg>" >&2; exit 1; }
[ $# -eq 2 ] || usage

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
APP="$1"
OUT="$2"
[[ "$APP" = /* ]] || APP="$ROOT/$APP"
BG="$ROOT/src-tauri/icons/dmg-bg.png"
VOLNAME="Xenon"

[ -d "$APP" ] || { echo "error: no .app at $APP" >&2; exit 1; }
[ -f "$BG" ] || { echo "error: no background at $BG" >&2; exit 1; }

APPNAME="$(basename "$APP")"
TMP_RW="$(mktemp -d)/rw.dmg"
STAGE="$(mktemp -d)"

# --- stage the DMG contents --------------------------------------------------
cp -R "$APP" "$STAGE/$APPNAME"
ln -s /Applications "$STAGE/Applications"
mkdir "$STAGE/.background"
cp "$BG" "$STAGE/.background/dmg-bg.png"

# --- create a writable image, then convert to compressed read-only -----------
rm -f "$TMP_RW" "$OUT"
hdiutil create -fs HFS+ -srcfolder "$STAGE" -volname "$VOLNAME" -format UDRW "$TMP_RW" >/dev/null
MOUNT="$(hdiutil attach "$TMP_RW" -readwrite -noverify -noBrowse | grep -oE '/Volumes/[^[:space:]]+' | head -1)"

cleanup() {
    hdiutil detach "$MOUNT" >/dev/null 2>&1 || true
    rm -rf "$STAGE"
}
trap cleanup EXIT

# --- icon positions + background via Finder AppleScript ----------------------
osascript <<OSA
tell application "Finder"
    tell disk "$VOLNAME"
        open
        set current view of container window to icon view
        set toolbar visible of container window to false
        set statusbar visible of container window to false
        set the bounds of container window to {0, 0, 660, 400}
        set theViewOptions to the icon view options of container window
        set arrangement of theViewOptions to not arranged
        set icon size of theViewOptions to 80
        set background picture of theViewOptions to file ".background:dmg-bg.png"
        set position of item "$APPNAME" of container window to {180, 170}
        set position of item "Applications" of container window to {480, 170}
        update without registering applications
        close
    end tell
end tell
OSA

# Bless the layout so Finder re-opens it the same way on the user's Mac.
touch "$MOUNT/.DS_Store" 2>/dev/null || true
hdiutil detach "$MOUNT" >/dev/null

hdiutil create -srcfolder "$STAGE" -volname "$VOLNAME" -fs HFS+ \
    -format UDZO -imagekey zlib-level=9 -ov "$OUT" >/dev/null

echo "Wrote $OUT"
ls -la "$OUT"