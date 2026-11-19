#!/usr/bin/env bash
# Download the whisper.cpp ggml models Xenon needs (STT + Silero VAD) into
# ~/Library/Application Support/com.xenon.app/models/. Skips files already
# present. curl -L, from the official ggerganov/whisper.cpp HuggingFace repo.
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

echo "done."