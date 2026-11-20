#!/usr/bin/env bash
# Harness 2: wake-word stress (30 pos + 30 neg) + idle CPU% over 2 minutes.
# The CPU sampler only attaches to an already-running Xenon; it never starts one.
set -euo pipefail
cd "$(dirname "$0")/../.." || exit 2

mkdir -p scripts/stress/out

cd src-tauri
cargo test --bin xenon stress_wake_harness -- --ignored --nocapture 2>&1 \
  | tee ../scripts/stress/out/wake.log | tail -n 30

PID=$(pgrep -f 'Xenon.app/Contents/MacOS/Xenon' | head -n1 || true)
if [ -z "${PID:-}" ]; then
  echo "wake-cpu: no running Xenon found; skipping idle CPU sample" \
    | tee ../scripts/stress/out/wake-cpu.txt
  exit 0
fi

: >../scripts/stress/out/wake-cpu.txt
for i in $(seq 1 24); do
  # %cpu, rss in KB for the existing Xenon process.
  ps -o %cpu,rss= -p "$PID" >>../scripts/stress/out/wake-cpu.txt
  sleep 5
done
echo "wake-cpu: sampled $PID 24x (2min) -> scripts/stress/out/wake-cpu.txt"