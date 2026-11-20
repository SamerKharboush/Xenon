#!/usr/bin/env bash
# Harness 7: 20-minute wake_word soak on an already-running Xenon.
# Samples the existing process every 2 minutes; never starts or kills Xenon.
set -euo pipefail
cd "$(dirname "$0")/../.." || exit 2
mkdir -p scripts/stress/out

PID=$(pgrep -f 'Xenon.app/Contents/MacOS/Xenon' | head -n1 || true)
if [ -z "${PID:-}" ]; then
  echo "soak: no running Xenon found; start wake_word mode and rerun" \
    | tee scripts/stress/out/soak.txt
  exit 0
fi

: >scripts/stress/out/soak.txt
for i in $(seq 1 10); do
  ts=$(date '+%Y-%m-%d %H:%M:%S')
  cpu=$(ps -o %cpu= -p "$PID" | tr -d ' ')
  rss_kb=$(ps -o rss= -p "$PID" | tr -d ' ')
  echo "$ts cpu=${cpu}% rss_mb=$((rss_kb / 1024))" | tee -a scripts/stress/out/soak.txt
  [ "$i" -lt 10 ] && sleep 120
done
echo "soak: sampled $PID 10x over ~20min"