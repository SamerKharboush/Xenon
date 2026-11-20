#!/usr/bin/env bash
# scripts/sign-app.sh <path-to-Xenon.app>
#
# Re-sign an ad-hoc (--sign -) Xenon.app with a *stable*, identifier-based
# designated requirement:
#
#     designated => identifier "com.xenon.app"
#
# Why: macOS (TCC) stores Microphone / Screen Recording / Accessibility grants
# against the app's code-signing designated requirement. A plain ad-hoc bundle
# gets "cdhash H…" — a per-build hash — so EVERY rebuild or update looks like a
# different app and silently loses every granted permission. Signing with an
# identifier requirement keeps the same requirement across builds, so grants
# survive rebuilds and in-app updates. (Users upgrading from v0.2.0, which was
# cdhash-signed, must re-grant permissions once.)
#
# The entitlements plist is re-applied so audio-input + network-client stay.
# No Developer ID / notarization (ADR-0007); this is still ad-hoc.
set -euo pipefail

usage() {
    echo "usage: $0 <path-to-Xenon.app>   e.g. $0 src-tauri/target/release/bundle/macos/Xenon.app" >&2
    exit 1
}

[ $# -eq 1 ] || usage

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
APP="$1"
# Accept a relative path from the repo root as well as an absolute one.
[[ "$APP" = /* ]] || APP="$ROOT/$APP"
ENTITLEMENTS="$ROOT/src-tauri/Entitlements.plist"

if [ ! -d "$APP" ]; then
    echo "error: no .app at $APP" >&2
    exit 1
fi
if [ ! -f "$ENTITLEMENTS" ]; then
    echo "error: missing entitlements at $ENTITLEMENTS" >&2
    exit 1
fi

echo "Signing $APP (ad-hoc, identifier com.xenon.app)…"

# --deep re-signs nested helpers (WebKit/GPU XPC bundles) with the same ad-hoc
# identity; --entitlements re-applies the audio-input/network-client grants;
# -r pins the designated requirement to the bundle identifier so TCC grants
# survive future rebuilds.
codesign --force --deep --sign - \
    --entitlements "$ENTITLEMENTS" \
    -r='designated => identifier "com.xenon.app"' \
    "$APP"

echo "Verifying…"
codesign --verify --deep --strict --verbose=2 "$APP"

echo "Designated requirement:"
codesign -dr - "$APP" 2>&1 | sed 's/^/  /'

# Fail loudly if the requirement is not the stable identifier one.
if codesign -dr - "$APP" 2>&1 | grep -q 'designated => cdhash'; then
    echo "error: designated requirement is still a cdhash — grants would not survive rebuilds" >&2
    exit 1
fi

echo "OK: $APP is identifier-signed and verifies."