#!/usr/bin/env bash
# Verify every invoke('<cmd>') in the frontend is registered in
# tauri::generate_handler! and that the capabilities file exists.
set -u
cd "$(dirname "$0")/../.." || exit 2

fail=0

registered=$(awk '/tauri::generate_handler!\[/{f=1;next} f&&/\]/{f=0} f' src-tauri/src/main.rs \
  | sed -E 's/^[[:space:]]*//; s/,.*//' \
  | awk -F'::' '{print $NF}' | sed 's/[[:space:]]*$//' | sort -u)

called=$(grep -rhoE "invoke\(['\"][a-zA-Z0-9_]+" src --include='*.html' --include='*.js' \
  | sed -E "s/invoke\(['\"]//" | sort -u)

for cmd in $called; do
  if ! grep -qx "$cmd" <<<"$registered"; then
    echo "MISSING: invoke('$cmd') is not registered in generate_handler!"
    fail=1
  fi
done

# Capabilities: windows exist and core defaults are present.
cap=src-tauri/capabilities/default.json
if [ ! -f "$cap" ]; then
  echo "MISSING: $cap"
  fail=1
else
  python3 - "$cap" <<'PY' || fail=1
import json, sys
cap = json.load(open(sys.argv[1]))
wins = set(cap.get("windows", []))
for w in ("main", "settings", "onboarding"):
    if w not in wins:
        print(f"MISSING: capability window '{w}'")
        sys.exit(1)
perms = cap.get("permissions", [])
if "core:default" not in perms:
    print("MISSING: core:default permission")
    sys.exit(1)
PY
fi

if [ "$fail" -eq 0 ]; then
  echo "OK: $(grep -c . <<<"$called") invoke commands registered; capabilities valid"
fi
exit "$fail"