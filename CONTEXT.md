# Xenon

Xenon is a lightweight, local-first desktop companion: a transparent always-on-top pet that listens, speaks, thinks, and controls the computer. This glossary is the canonical vocabulary for the project. Use these terms in issues, specs, code, and ADRs; avoid the synonyms listed under each.

## Language

### Identity

**Xenon**:
The desktop companion application — the Tauri shell, the pet sprite, and the agent loop together.
_Avoid_: the assistant, the bot, Xenon.ai

**Pet**:
The on-screen sprite the user sees and talks to. Xenon is the whole application; the pet is the face.
_Avoid_: avatar, character, mascot

**Companion**:
A running Xenon instance as experienced by the user (pet + voice + agent). Used when the distinction between the app and the running session matters.
_Avoid_: assistant, daemon

### Pet states

**Pet State**:
One of five modes the pet sprite displays, driven by the runtime. The state machine is: `Idle`, `Listening`, `Thinking`, `Speaking`, `Agentic`.
_Avoid_: mood, mode, status

**Idle**:
The pet is awake but not doing anything. The window is click-through in this state.
_Avoid_: resting, sleeping, standby

**Listening**:
The microphone is open and the VAD is detecting speech. The pet shows a waveform.
_Avoid_: recording, hearing, awake

**Thinking**:
The LLM is generating a response. The pet shows a pulsing/processing animation.
_Avoid_: processing, computing, working

**Speaking**:
The TTS engine is playing audio. The pet lip-syncs to the audio.
_Avoid_: talking, narrating, vocalizing

**Agentic**:
The agent loop is active — taking screenshots, reasoning, clicking. The pet's eyes track the action.
_Avoid_: acting, controlling, automating, working

### Audio

**Audio Pipeline**:
The end-to-end path from microphone to text: capture (cpal) → VAD (sherpa-onnx) → STT (whisper-rs) → text event.
_Avoid_: the mic pipeline, voice input, speech recognition pipeline

**VAD**:
Voice Activity Detection — decides when the user starts and stops speaking, so STT only runs on speech.
_Avoid_: silence detection, voice detection, the gate

**STT**:
Speech-to-Text — transcribes captured audio into text via whisper-rs.
_Avoid_: ASR, transcription, speech recognition, whisper

**TTS**:
Text-to-Speech — synthesizes spoken audio from text via sherpa-onnx (neural voices).
_Avoid_: voice synthesis, speech synthesis, the voice, talking

**Utterance**:
One complete thing the user says, from VAD start to VAD end. The unit the agent loop reasons over.
_Avoid_: phrase, sentence, query, command

### Agent

**Agent Loop**:
The ReAct (Reason + Act) cycle: the LLM reasons about an utterance, calls a tool, observes the result, and repeats until it produces a final spoken response. Implemented in Rust, not a framework.
_Avoid_: the brain, the agent, kalosm, the orchestrator

**Tool**:
A capability the agent loop can invoke — `take_screenshot`, `click_mouse`, `open_app`. Each has a JSON schema, a Rust implementation, and a description fed to the LLM.
_Avoid_: action, command, function, capability

**Grounding**:
The step where a screenshot is sent to a vision-language model, which returns JSON coordinates identifying where to click.
_Avoid_: vision, targeting, coordinate resolution, the VLM step

**Screenshot**:
A captured image of the screen, downscaled to 512×512 before being sent to the VLM. The unit of perception for the agent.
_Avoid_: capture, screen grab, frame, image

**ReAct**:
Reasoning + Acting — the agent's loop pattern. The LLM reasons in text, acts via tools, observes results, repeats. Not a library; a loop we write.
_Avoid_: the agent pattern, chain-of-thought, the think-act loop

### Provider

**Provider**:
An OpenAI-compatible HTTP endpoint that serves LLM completions. Configurable `base_url` + `api_key`. Works with Ollama, LM Studio, ToshLLM, OpenAI, Gemini's compat endpoint. The single integration point for all LLM traffic.
_Avoid_: backend, model server, LLM endpoint, inference provider

**Provider Hub**:
The Rust module that owns the Provider abstraction — one `reqwest` client, streaming chat completions, tool-call parsing. The only place that talks HTTP to an LLM.
_Avoid_: the API client, the LLM client, the network layer

**Vision-Language Model (VLM)**:
The model the agent calls during Grounding to identify click coordinates. May be the same Provider or a different one (cloud fallback for heavy vision tasks).
_Avoid_: the vision model, the image model, the grounding model

### Runtime

**Runtime**:
The Tokio-based Rust core that owns the audio pipeline, the agent loop, and the TTS/UI sync. Runs as the Tauri app's backend.
_Avoid_: the backend, the engine, the core, the server

**Audio Event**:
A message on the audio channel — `Speech(Utterance)` or `Silence`. Produced by the Audio Pipeline, consumed by the agent loop.
_Avoid_: voice event, mic event, input event

**Agent Action**:
A message on the agent channel — `Speak(String)`, `SetPetState(PetState)`, `ExecuteTool(ToolCall)`. Produced by the agent loop, consumed by the TTS/UI sync.
_Avoid_: command, output event, response event

### Build & Distribution

**Universal Binary**:
A single `.dmg` containing both `x86_64` (Intel) and `aarch64` (Apple Silicon) slices, produced by `tauri build --target universal-apple-darwin`. The default distribution target.
_Avoid_: fat binary, multi-arch binary, universal app

**Idle Shell**:
The Tauri window + empty audio buffers, no models loaded. The "<40MB RAM" figure applies here. Not the number a user experiences while the pet is working.
_Avoid_: baseline, idle RAM, empty state

**Active with Models**:
The RAM footprint when whisper-rs and/or the LLM are loaded. Several hundred MB to several GB. The honest number for "what does Xenon use while thinking".
_Avoid_: active RAM, loaded state, working set
