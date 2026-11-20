#!/usr/bin/env bash
# Harness 3: TTS stress (all speakers, OOV, 100-sentence queue, stop latency).
set -euo pipefail
cd "$(dirname "$0")/../.." || exit 2
mkdir -p scripts/stress/out

cd src-tauri

run() {
  name=$1; filter=$2
  echo "== $name =="
  cargo test --bin xenon "$filter" -- --ignored --nocapture 2>&1 \
    | tee "../scripts/stress/out/$name.log" | tail -n 25
}

run tts-kokoro tts_kokoro_synthesizes_hello
run tts-oov tts_kokoro_oov_sentence
run tts-speakers kokoro_all_speakers_non_silent
run tts-queue tts_queue_stress_harness
run tts-fallback tts_say_fallback_harness