# Xenon stress report

Date: 2026-11-20. Branch: `feature/xenon-vertical-slice`. No commits made.
Harness output is in `scripts/stress/out/` (gitignored); this report records
summaries and tails only.

## Results

| Harness | Script / test | Result | Evidence |
| --- | --- | --- | --- |
| 1. STT | `scripts/stress/run-stt.sh` → `audio::tests::stress_stt_harness` | PASS (CPU for large-v3; Metal crash reported) | `out/stt-base.en.tsv`, `out/stt-large-v3-turbo.tsv`, `out/stt-summary.tsv` |
| 2. Wake word | `scripts/stress/wake-cpu.sh` → `voice::tests::stress_wake_harness` | FAIL — 8/30 positives fired, 0/30 negatives fired | `out/wake.tsv`, `out/wake.log` |
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
   during `whisper_encode_internal`. `base.en` works on the same GPU. The STT
   harness uses CPU for `large-v3-turbo` so the rest of the matrix can run.
   Needs a device/backend fix or a runtime CPU fallback.
2. **Wake word accuracy.** 8/30 positives fired with `base.en`; 0/30 negatives
   fired. Example transcripts show the wake phrase misheard as “Hazing on” /
   “Hazy-Non”. Needs a better wake model (e.g. `ggml-tiny.en.bin`), a
   wake-specific initial prompt, or a looser matcher that does not increase
   false fires.
3. **Agent vision profile mismatch.** The configured `Primary` profile has
   `supports_vision: true`, but the local Qwen3 server rejects image input
   (`500 image input is not supported`). K1 correctly trusts the profile flag;
   the profile needs `supports_vision: false` or a runtime capability probe.
   Headless tasks also had Screen Recording denied, so desktop verification
   could not run.
4. **Soak skipped.** No Xenon process was running in `wake_word` mode, so the
   20-minute sampler had nothing to attach to.

## Final gates

- `cd src-tauri && cargo test 2>&1 | grep -E '^test result|FAILED'`: 164 passed, 0 failed, 16 ignored.
- `cd src-tauri && cargo clippy --all-targets 2>&1 | tail -3`: `Finished dev profile ...`, exit 0, no warnings.
- `scripts/stress/check-commands.sh`: `OK: 24 invoke commands registered; capabilities valid`.

## Needs human

- Grant Screen Recording (and keep Accessibility granted) in System Settings →
  Privacy & Security, then rerun `scripts/stress/agent-tasks.py`.
- Fix the `Primary` provider profile’s `supports_vision` (or select a
  vision-capable model) before expecting screenshot/click verification.
- Run `scripts/stress/soak.sh` while Xenon is in `wake_word` mode for the
  20-minute sample.
- Decide whether to ship a wake-specific prompt/model change; the current
  `base.en` + strict matcher misses most positive utterances on this host.
- The current `agent-tasks.py` run on this host failed desktop checks because
  the ToshLLM vision endpoint returns 500 (`image input is not supported`) and
  headless capture cannot verify windows. Re-run after the provider/profile fix.