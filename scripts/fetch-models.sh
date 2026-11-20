#!/usr/bin/env bash
# Download the models Xenon needs: whisper.cpp ggml STT (main + tiny wake model)
# and the Silero VAD, plus the Kokoro neural TTS voice, into
# ~/Library/Application Support/com.xenon.app/models/. Skips files already
# present.
#
# Sources:
#   - ggerganov/whisper.cpp HuggingFace repo (STT)
#   - ggml-org/whisper-vad HuggingFace repo (Silero VAD)
#   - k2-fsa/sherpa-onnx releases (Kokoro)
#
# NOTE: not run automatically — invoke manually before first launch if you
# don't already have the models under ~/models/whisper/.
set -euo pipefail

DEST="${HOME}/Library/Application Support/com.xenon.app/models"
WHISPER_BASE="https://huggingface.co/ggerganov/whisper.cpp/resolve/main"
VAD_BASE="https://huggingface.co/ggml-org/whisper-vad/resolve/main"

mkdir -p "${DEST}"

fail() {
  echo "error: $*" >&2
  exit 1
}

fetch() {
  local url="$1" file="$2"
  if [[ -f "${DEST}/${file}" ]]; then
    echo "skip (present): ${file}"
    return
  fi
  echo "fetch: ${file} -> ${DEST}/${file}"
  local tmp="${DEST}/${file}.part"
  curl -L --fail --retry 3 -o "${tmp}" "${url}" \
    || { rm -f "${tmp}"; fail "failed to download ${url}"; }
  mv "${tmp}" "${DEST}/${file}"
}

fetch "${WHISPER_BASE}/ggml-base.en.bin" "ggml-base.en.bin"
# Wake-word model: tiny.en keeps the main STT model unloaded until a wake fires.
fetch "${WHISPER_BASE}/ggml-tiny.en.bin" "ggml-tiny.en.bin"
fetch "${VAD_BASE}/ggml-silero-v6.2.0.bin" "ggml-silero-v6.2.0.bin"

# Neural TTS voice (Kokoro multi-language v1.0, English + Chinese).
# Ships the lexicons sherpa-onnx 2.0 requires for Kokoro. The older
# kokoro-en-v0_19 stays usable only as a say fallback if already installed.
KOKORO_DIR="${DEST}/kokoro-multi-lang-v1_0"
if [[ -f "${KOKORO_DIR}/model.onnx" && -f "${KOKORO_DIR}/lexicon-us-en.txt" ]]; then
  echo "skip (present): kokoro-multi-lang-v1_0"
else
  echo "fetch: kokoro-multi-lang-v1_0 -> ${KOKORO_DIR}"
  curl -L --fail --retry 3 -o "${DEST}/kokoro-multi-lang-v1_0.tar.bz2" \
    "https://github.com/k2-fsa/sherpa-onnx/releases/download/tts-models/kokoro-multi-lang-v1_0.tar.bz2" \
    || fail "failed to download kokoro-multi-lang-v1_0"
  tar -xjf "${DEST}/kokoro-multi-lang-v1_0.tar.bz2" -C "${DEST}" \
    || fail "failed to unpack kokoro-multi-lang-v1_0"
  rm -f "${DEST}/kokoro-multi-lang-v1_0.tar.bz2"
fi

echo "done."