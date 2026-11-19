#!/usr/bin/env bash
# Measure Xenon's idle physical footprint: the app process plus the WebKit
# XPC processes (WebContent / GPU / Networking) that belong to it.
#
# Usage: scripts/measure-ram.sh [path-to-Xenon.app] [settle-seconds]
# Defaults to the universal release bundle and 15 s.
#
# Identification: `ps -axo pid,comm` is snapshotted before launch and after
# the settle window. WebKit XPC services are reparented to launchd, so
# ancestry does not work, and the responsible-pid check in `launchctl
# procinfo` needs root. Instead we take the new processes whose executable
# lives inside /WebKit.framework/ -- exactly the per-view WebContent / GPU /
# Networking helpers the app just caused launchd to spawn.
#
# `footprint` reports the physical footprint, the metric that matches the
# PRD budget; RSS counts shared clean framework pages several times over.
set -euo pipefail

APP="${1:-src-tauri/target/universal-apple-darwin/release/bundle/macos/Xenon.app}"
SETTLE="${2:-15}"
BIN="$APP/Contents/MacOS/Xenon"

[[ -x "$BIN" ]] || { echo "no binary at $BIN" >&2; exit 1; }

# Physical footprint in MB. `footprint` prints a "Footprint: N MB" header;
# vmmap --summary is the fallback on older systems.
fp() {
  local pid="$1" out val unit
  out=$(footprint "$pid" 2>/dev/null | awk -F'Footprint: ' '/Footprint:/ {print $2; exit}')
  if [[ -n "$out" ]]; then
    val=$(echo "$out" | awk '{print $1}')
    unit=$(echo "$out" | awk '{print $2}')
    case "$unit" in
      KB) val=$(awk -v v="$val" 'BEGIN{printf "%.0f", v/1024}') ;;
      GB) val=$(awk -v v="$val" 'BEGIN{printf "%.0f", v*1024}') ;;
      B|bytes|"") val=$(awk -v v="$val" 'BEGIN{printf "%.0f", v/1048576}') ;;
    esac
    echo "$val"
    return
  fi
  vmmap --summary "$pid" 2>/dev/null | awk -F: '/Physical footprint/ {gsub(/[()M]/,"",$2); print $2; exit}'
}

psnap() { ps -axo pid=,comm=; }

before=$(psnap)
"$BIN" >/tmp/xenon-measure.log 2>&1 &
APP_PID=$!
cleanup() { kill "$APP_PID" 2>/dev/null; wait "$APP_PID" 2>/dev/null; }
trap cleanup EXIT

sleep "$SETTLE"

APP_PID=$(pgrep -n -f "MacOS/Xenon" | head -1)
[[ -n "$APP_PID" ]] || { echo "app exited early; see /tmp/xenon-measure.log" >&2; exit 1; }

rows=()
total=0
row() {
  local mb
  mb=$(fp "$2")
  [[ -n "$mb" ]] || return 0
  rows+=("$(printf '%-13s %-8s %s MB' "$1" "$2" "$mb")")
  total=$(awk -v a="$total" -v b="$mb" 'BEGIN{printf "%.0f", a+b}')
}

row xenon "$APP_PID"
while read -r pid comm; do
  case "$comm" in
    */WebKit.framework/*)
      name=$(basename "$comm")
      name=${name%.xpc*}
      name=${name#com.apple.WebKit.}
      row "$name" "$pid"
      ;;
  esac
done < <(comm -13 <(echo "$before" | sort) <(psnap | sort))

printf '%s\n' "${rows[@]}"
printf '%-13s %-8s %s MB\n' TOTAL "" "$total"