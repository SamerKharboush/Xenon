# Headless agent tasks

`xenon --agent-task "<task>" [--profile <name>]` runs one agent turn without the
pet UI. It prints every step to stdout and exits `0` when the agent calls `done`,
`1` on `fail` or error.

Build first:

```bash
cd src-tauri && cargo build
```

Then run a task:

```bash
./target/debug/xenon --agent-task "Open Safari"
./target/debug/xenon --agent-task "Open Safari" --profile ToshLLM
```

## Requirements

- Grant **Accessibility** and **Screen Recording** in System Settings →
  Privacy & Security. Without them the agent prints a warning and desktop steps
  fail with a clear message.
- At least one provider profile must be configured (Settings → AI Provider Hub,
  or a local server running on one of the detected ports).

## Smoke tasks

These cover the Open-APA-parity tool surface (PRD §3.3) and the provider hub
(PRD §3.4). Run each and confirm the agent reaches `done` (exit 0).

1. **Open an app** — `xenon --agent-task "Open Safari"`
   Exercises `open` / `switch_app` and a verification screenshot.

2. **Open a URL** — `xenon --agent-task "Open https://example.com in the browser"`
   Exercises `browser_open` or `open` with a URL.

3. **Read the screen** — `xenon --agent-task "Take a screenshot and tell me what is on the screen"`
   Exercises `look` (perception only; no click needed).

4. **Click and type** — `xenon --agent-task "Click the search field and type hello"`
   Exercises `click` (grounding), `type_text`, and the verification loop.

5. **Keyboard chord** — `xenon --agent-task "Press command-space to open Spotlight"`
   Exercises `hotkey` / `press_keys`.

6. **Scroll** — `xenon --agent-task "Scroll down three times"`
   Exercises `scroll`.

7. **Remember then finish** — `xenon --agent-task "Remember that my name is Sam, then say done"`
   Exercises `remember` (notes fed back) and the `done` terminal action.

8. **Browser read** — `xenon --agent-task "Read the current browser page and summarize it"`
   Exercises `browser_read`. Requires the browser's
   *Allow JavaScript from Apple Events* setting; otherwise the agent emits a
   `xenon-notice` and falls back to vision.

## Notes

- `--profile <name>` pins all three roles (chat, agent, vision) to that profile
  for the run.
- Native tool calling is used when the profile's `supports_tools` flag is set;
  otherwise the agent uses the JSON text protocol automatically.
- `run_script` stays off unless `allow_scripts` is enabled in Settings; when on,
  every call shows a native Run/Cancel dialog.