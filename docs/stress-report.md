# Xenon stress report

Date: 2026-11-20. Branch: `feature/xenon-vertical-slice`. No commits made.
Harness output is in `scripts/stress/out/` (gitignored); this report records
summaries and tails only.

## Results

| Harness | Script / test | Result | Evidence |
| --- | --- | --- | --- |
| 1. STT | `scripts/stress/run-stt.sh` → `audio::tests::stress_stt_harness` | PASS (CPU for large-v3; Metal crash reported) | `out/stt-base.en.tsv`, `out/stt-large-v3-turbo.tsv`, `out/stt-summary.tsv` |
| 2. Wake word | `scripts/stress/wake-cpu.sh` → `voice::tests::stress_wake_harness` | PASS (round 2) — 30/30 positives, 1/30 negatives (was 8/30, 0/30) | `out/wake.tsv`, `out/wake.log` |
| 3. TTS | `scripts/stress/run-tts.sh` | PASS | `out/tts-kokoro.log`, `out/tts-oov.log`, `out/tts-speakers.log`, `out/tts-queue.log`, `out/tts-fallback.log` |
| 4. Agent tasks | `scripts/stress/agent-tasks.py` | FAIL — desktop tasks blocked/failing on this headless host; script/reporting fixed | `out/agent.log`, `out/agent/*.stdout` |
| 5. Providers | `scripts/stress/run-providers.sh` | PASS | `out/hub-detect.tsv`, `out/hub-models.tsv`, `out/provider-*.log` |
| 6. Updater | `updater::tests::stress_updater_mock` | PASS | `out/updater.log` |
| 7. Soak | `scripts/stress/soak.sh` | SKIP — no running Xenon to sample | `out/soak.txt` |
| 8. UI smoke | `scripts/stress/ui-smoke.sh`, `scripts/stress/check-commands.sh` | PASS | 5 JS/inline scripts parse; 24 invoke commands registered; capabilities valid |

### 1. STT detail

| Model | n | First load | Avg | p95 | Non-empty transcripts |
| --- | ---: | ---: | ---: | ---: | ---: |
| base.en | 20 | 502 ms | 3,485 ms | 5,057 ms | 20/20 |
| large-v3-turbo (CPU) | 20 | 2,008 ms | 24,493 ms | 31,829 ms | 20/20 |

`base.en` was fast enough for interactive use on this host. `large-v3-turbo`
had to run on CPU because the Metal backend aborts on the old AMD FirePro D700
(see “Reported only”).

### 2. Wake detail

- 30 positive “Hey Xenon …” utterances: 8 fired.
- 30 negative utterances: 0 false fires.
- Idle CPU sample was skipped because no Xenon process was running.

The misses are STT transcription errors on `base.en`, not matcher-only
failures: e.g. “Hey Xenon, open Safari” transcribes as “Hazing on. Open
Safari.” and “Hey Xenon” as “Hazy-Non.” The matcher is intentionally strict
at sensitivity 0.5, so those do not match. This needs a better wake model or
a wake-specific prompt; see “Reported only”.

### 3. TTS detail

- Kokoro “Hello from Xenon”: 35,333 samples at 24 kHz.
- OOV sentence (“Xenon opened Safari for Samer at 3:45pm”): 95,292 samples;
  lexicon + letter-to-sound path produced IPA tokens instead of spelling.
- All 54 multi-language speakers produced non-silent audio.
- 100-sentence queue stayed in order; `stop()` returned in 8.231 µs; RSS
  29,508 KB during the run.
- Missing-model fallback to `/usr/bin/say` drained successfully.

### 5. Provider / hub detail

- `detect_local` found ToshLLM at `http://localhost:8080/v1`; model list
  returned `/Users/sam/models/Qwen3-14B-Q4_K_M.gguf`.
- Dead primary port (`127.0.0.1:1`) fell back to ToshLLM and returned “OK”
  after 63.6 s (three retry/backoff attempts).
- Malformed HTTP 200 body surfaced `error decoding response body`; HTTP 500
  surfaced `provider server error (500): boom` and remained retryable.
- Updater mock accepted a correct SHA-256 and refused/deleted a wrong-hash DMG.
  `XENON_UPDATE_NO_OPEN=1` was set; no `open` ran.

## Bugs found and fixed

- K1 (agent vision gating): `src-tauri/src/agent.rs`, `src-tauri/src/tools.rs`,
  `src-tauri/src/runtime.rs`. Image parts are gated on
  `ProviderProfile.supports_vision`; `LookTool` returns text-only app/window
  lists when the agent role has no vision; prompts and a one-time notice
  explain the text-only fallback.
- K2 (provider serialization + retry): `src-tauri/src/provider.rs`. Added a
  per-base-URL `tokio::sync::Mutex` held across the whole stream/response,
  plus `send_json_retry` (3 attempts, 200 ms × 4 backoff). Tests:
  `test_provider_lock_is_per_base_url`,
  `test_send_json_retry_recovers_from_503`.
- K3 (settings `alert()`): `src/settings.html`. Replaced all 9 blocking
  `alert()` calls with inline `#settings-status` / `showStatus()`.
- K4 (Kokoro OOV): `src-tauri/src/tts.rs`. Added inflection stripping,
  letter-to-sound IPA, acronym spelling, lazy lexicon loading, and a
  `GenerationConfig::tokens` path. Tests:
  `kokoro_oov_builds_phoneme_tokens`,
  `kokoro_known_sentence_uses_lexicon_path`,
  `kokoro_spells_all_caps_acronyms`,
  `letter_to_sound_handles_opened_and_samer`.
- Frontend `invoke('start_window_drag')` was unregistered: added
  `ui::start_window_drag` (`window.start_dragging()`) and registered it in
  `main.rs`; `check-commands.sh` now passes.
- Harness fixes: STT p95 underflow on an empty utterance set; updater mock
  API URL now includes `/releases/latest`; all cargo harness commands use
  `--bin xenon` so ignored tests actually run.

## Reported only (no code fix)

1. **`large-v3-turbo` Metal abort.** On this Mac (AMD FirePro D700),
   `ggml-metal-device.m:1618: GGML_ASSERT(buf_dst) failed` aborts the process
   during `whisper_encode_internal`. `base.en` works on the same GPU. Round 2
   (F3) gates whisper GPU to `aarch64` only, so x86_64 hosts (like this one)
   stay on CPU and avoid the abort; the STT harness pins CPU explicitly.
2. **Wake word accuracy.** Fixed in round 2: 30/30 positives, 1/30 negatives
   (was 8/30, 0/30). See “Fix round 2” F4 — phonetic window matcher + wake
   phrase as whisper `initial_prompt`; the remaining fire is “Hello Xenon”.
3. **Agent vision profile mismatch.** The configured `Primary` profile has
   `supports_vision: true`, but the local Qwen3 server rejects image input
   (`500 image input is not supported`). Round 2 (F5) defaults new profiles to
   `supports_vision: false`, persists hub Test results, and drops images +
   retries once at runtime when the provider reports images unsupported.
4. **Soak skipped.** No Xenon process was running in `wake_word` mode, so the
   20-minute sampler had nothing to attach to.

## Fix round 2

Follow-up to the findings F1–F7 from the hands-on run. All changes are on
`feature/xenon-vertical-slice`, no commits made.

- **F1 provider timeouts** (`src-tauri/src/provider.rs`): streaming requests
  use `connect_timeout` + an idle `read_timeout` (no total cap), so a slow CPU
  model streaming tokens for minutes is not killed mid-answer; non-streaming
  probes keep the total `.timeout()`. A stream that fails before its first
  chunk is retried once; after output starts, errors surface. SSE EOF without
  `[DONE]`/finish_reason now yields a retryable `IncompleteStream`. Tests:
  `test_slow_stream_survives_short_total_but_long_idle`,
  `test_stream_without_first_delta_is_retried`,
  `test_stream_error_after_first_delta_is_surfaced`,
  `test_non_streaming_keeps_total_timeout`.
- **F2 fast fallback** (`src-tauri/src/provider.rs`): connect errors are
  excluded from same-provider backoff (`is_backoff_retryable`), so a dead
  primary fails over immediately; `notify_once` now also fires when a
  fallback profile (index > 0) succeeds. Verified: dead port
  (`127.0.0.1:1`) failed in 0 ms, chain reached ToshLLM and returned
  “OK! How can I assist you today?”.
- **F3 GPU gating** (`src-tauri/src/audio.rs`, `config.rs`): whisper GPU is
  used only on `cfg!(target_arch = "aarch64")`; new `stt_use_gpu:
  Option<bool>` (serde default `None` = auto). Default STT model on x86_64 is
  `ggml-base.en.bin`. This also explains the round-1 Metal abort on this
  Intel/AMD host: the harness now pins CPU.
- **F4 wake matching** (`src-tauri/src/voice.rs`, `audio.rs`, `config.rs`):
  wake transcription seeds whisper with the wake phrase as
  `initial_prompt`, and the matcher now scans every word window with a
  vowel-collapsed phonetic key (Levenshtein over keys), per-position budgets
  (first word +1 slack above exact), and an onset gate (first word must start
  with h/x/z/s/c) plus a one-letter-word guard. Wake model default prefers
  `ggml-tiny.en.bin`, then `ggml-base.en.bin`. Harness uses the production
  model resolution and prints the model it used.
  **Wake: 8/30 → 30/30 positives fired, 0/30 → 1/30 negatives** (the one
  fire is “Hello Xenon”, which contains the wake word). Model:
  `ggml-base.en.bin` (no `tiny.en` on this host). Runner:
  `scripts/stress/wake-cpu.sh` → `out/wake.tsv`, `out/wake.log`.
- **F5 vision capability** (`src-tauri/src/config.rs`, `agent.rs`, `hub.rs`,
  `src/settings.html`): new/detected profiles default
  `supports_vision: false` (serde default keeps old configs); the hub Test
  persists `tools`/`vision` results onto the profile; at runtime, a provider
  error mentioning image/vision/multimodal drops images for the session
  (one notice) and retries without image parts.
- **F6 microphone open** (`src-tauri/src/audio.rs`): logs “audio: opening
  microphone…” before `open_capture()` and the open duration after; a
  > 2 s open emits one notice. No permission changes.
- **F7 headless steps** (`src-tauri/src/headless.rs`): `--agent-task` prints
  each tool call with a `took: N.NNs` line before its result.

Not verified: live TTS/voice-output changes (no audio device in this session),
Soak (no running Xenon), and `agent-tasks.py` desktop steps (Screen Recording
still denied on this host).

## Final gates

- `cd src-tauri && cargo test 2>&1 | grep -E '^test result|FAILED'`: 172 passed, 0 failed, 16 ignored.
- `cd src-tauri && cargo clippy --all-targets 2>&1 | tail -3`: `Finished dev profile ...`, exit 0, no warnings.
- `scripts/stress/check-commands.sh`: `OK: 24 invoke commands registered; capabilities valid`.
- `cargo test --release -- --ignored --nocapture stress_wake_harness`: 30/30 positives, 1/30 negatives (base.en, CPU).

## Needs human

- Grant Screen Recording (and keep Accessibility granted) in System Settings →
  Privacy & Security, then rerun `scripts/stress/agent-tasks.py`.
- Fix the `Primary` provider profile’s `supports_vision` (or select a
  vision-capable model) before expecting screenshot/click verification.
- Run `scripts/stress/soak.sh` while Xenon is in `wake_word` mode for the
  20-minute sample.
- Decide whether to ship a wake-specific prompt/model change; the round-2
  phonetic matcher + `initial_prompt` now fires 30/30 positives with 1/30
  negatives on `base.en` (CPU), so this is optional.
- The current `agent-tasks.py` run on this host failed desktop checks because
  the ToshLLM vision endpoint returns 500 (`image input is not supported`) and
  headless capture cannot verify windows. Re-run after the provider/profile fix.