#!/usr/bin/env bash
# Harness 5: provider hub detect/list + dead-port fallback + malformed/500 mocks.
set -euo pipefail
cd "$(dirname "$0")/../.." || exit 2
mkdir -p scripts/stress/out

cd src-tauri

echo "== hub detect/list =="
cargo test --bin xenon stress_hub_detect_and_list -- --ignored --nocapture 2>&1 \
  | tee ../scripts/stress/out/hub.log | tail -n 30

echo "== provider fallback (dead port -> ToshLLM) =="
XENON_MODEL="${XENON_MODEL:-local}" cargo test --bin xenon stress_provider_fallback_dead_port -- --ignored --nocapture 2>&1 \
  | tee ../scripts/stress/out/provider-fallback.log | tail -n 20

echo "== provider malformed/500 =="
cargo test --bin xenon stress_provider_malformed_and_500 -- --ignored --nocapture 2>&1 \
  | tee ../scripts/stress/out/provider-malformed.log | tail -n 20

echo "== updater mock (localhost only, no open) =="
cargo test --bin xenon stress_updater_mock -- --ignored --nocapture 2>&1 \
  | tee ../scripts/stress/out/updater.log | tail -n 20