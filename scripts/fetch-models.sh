#!/usr/bin/env bash
# Download the models Xenon needs: whisper.cpp ggml STT + Silero VAD, and the
# Kokoro neural TTS voice, into
# ~/Library/Application Support/com.xenon.app/models/. Skips files already
# present. curl -L, from the official ggerganov/whisper.cpp HuggingFace repo
# and the k2-fsa/sherpa-onnx releases.
#
# NOTE: not run automatically — invoke manually before first launch if you
# don't already have the models under ~/models/whisper/.
set -euo pipefail

DEST="${HOME}/Library/Application Support/com.xenon.app/models"
BASE="https://huggingface.co/ggerganov/whisper.cpp/resolve/main"

mkdir -p "${DEST}"

fetch() {
  local file="$1"
  if [[ -f "${DEST}/${file}" ]]; then
    echo "skip (present): ${file}"
    return
  fi
  echo "fetch: ${file} -> ${DEST}/${file}"
  curl -L --fail --retry 3 -o "${DEST}/${file}" "${BASE}/${file}"
}

fetch "ggml-base.en.bin"
fetch "ggml-silero-v6.2.0.bin"

# Neural TTS voice (Kokoro v0.19 English). The tarball unpacks into
# kokoro-en-v0_19/ (model.onnx, voices.bin, tokens.txt, espeak-ng-data, ...).
KOKORO_DIR="${DEST}/kokoro-en-v0_19"
if [[ -f "${KOKORO_DIR}/model.onnx" && -f "${KOKORO_DIR}/voices.bin" ]]; then
  echo "skip (present): kokoro-en-v0_19"
else
  echo "fetch: kokoro-en-v0_19 -> ${KOKORO_DIR}"
  curl -L --fail --retry 3 -o "${DEST}/kokoro-en-v0_19.tar.bz2" \
    "https://github.com/k2-fsa/sherpa-onnx/releases/download/tts-models/kokoro-en-v0_19.tar.bz2"
  tar -xjf "${DEST}/kokoro-en-v0_19.tar.bz2" -C "${DEST}"
  rm -f "${DEST}/kokoro-en-v0_19.tar.bz2"
fi

echo "done."