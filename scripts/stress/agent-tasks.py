#!/usr/bin/env python3
"""Harness 4: headless agent smoke tasks from docs/agent-tasks.md.

For every `xenon --agent-task "..."` example in the docs, run the debug
binary with `--profile ToshLLM`, a per-task timeout, and capture stdout.
Permissions are printed first (Accessibility via osascript; screen
recording is preflighted by the binary's own warning line).

No system settings are changed; no server is killed.
"""
from __future__ import annotations

import os
import re
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
BIN = ROOT / "src-tauri" / "target" / "debug" / "xenon"
DOC = ROOT / "docs" / "agent-tasks.md"
OUT = Path(os.environ.get("XENON_STRESS_OUT", ROOT / "scripts" / "stress" / "out"))
TIMEOUT = int(os.environ.get("XENON_AGENT_TIMEOUT", "120"))
PROFILE = os.environ.get("XENON_AGENT_PROFILE", "ToshLLM")

TASK_RE = re.compile(r'xenon\s+--agent-task\s+"([^"]+)"')


def accessibility() -> str:
    try:
        r = subprocess.run(
            ["osascript", "-e", 'tell application "System Events" to get UI elements enabled'],
            capture_output=True, text=True, timeout=10,
        )
        return r.stdout.strip() or "unknown"
    except Exception as e:  # noqa: BLE001
        return f"error: {e}"


def main() -> int:
    if not BIN.exists():
        print(f"missing binary: {BIN}", file=sys.stderr)
        return 2
    tasks = TASK_RE.findall(DOC.read_text(encoding="utf-8"))
    # Keep the documented smoke tasks only (unique, in order); drop the
    # usage placeholder `xenon --agent-task "<task>"`.
    seen: set[str] = set()
    tasks = [
        t for t in tasks
        if not t.startswith("<") and not (t in seen or seen.add(t))
    ]
    out = OUT / "agent"
    out.mkdir(parents=True, exist_ok=True)

    print(f"agent-tasks: binary={BIN}")
    print(f"agent-tasks: accessibility={accessibility()}")
    print("agent-tasks: screen_recording=verify in System Settings (binary warns if missing)")
    print(f"agent-tasks: profile={PROFILE} timeout={TIMEOUT}s tasks={len(tasks)}")

    failed = 0
    blocked = 0
    for i, task in enumerate(tasks, 1):
        cmd = [str(BIN), "--agent-task", task, "--profile", PROFILE]
        print(f"\n[{i}/{len(tasks)}] {task}")
        try:
            r = subprocess.run(
                cmd, cwd=ROOT, capture_output=True, text=True, timeout=TIMEOUT,
            )
            rc, stdout, stderr = r.returncode, r.stdout, r.stderr
        except subprocess.TimeoutExpired as e:
            def _text(v):
                if v is None:
                    return ""
                return v.decode("utf-8", "replace") if isinstance(v, bytes) else v

            rc = 124
            stdout = _text(e.stdout)
            stderr = _text(e.stderr) + f"\nTIMEOUT after {TIMEOUT}s"
        safe = re.sub(r"[^a-z0-9]+", "-", task.lower()).strip("-")[:40] or "task"
        (out / f"{i:02d}-{safe}.stdout").write_text(stdout, encoding="utf-8")
        (out / f"{i:02d}-{safe}.stderr").write_text(stderr, encoding="utf-8")
        status = "OK" if rc == 0 else f"FAIL(rc={rc})"
        if rc != 0 and "image input is not supported" in stderr:
            status = f"FAIL(rc={rc})"
        elif rc != 0 and "permissions missing" in stderr and "screen recording: false" in stderr:
            status = "BLOCKED-PERMISSION"
        print(f"  {status}")
        if status == "BLOCKED-PERMISSION":
            blocked += 1
            continue
        if rc != 0:
            failed += 1
            tail = (stderr or stdout).strip().splitlines()[-3:]
            for line in tail:
                print(f"  | {line}")
    print(
        f"\nagent-tasks: {len(tasks) - failed - blocked}/{len(tasks)} reached done; "
        f"{blocked} blocked by permissions; {failed} failed"
    )
    return 0 if failed == 0 else 1


if __name__ == "__main__":
    raise SystemExit(main())