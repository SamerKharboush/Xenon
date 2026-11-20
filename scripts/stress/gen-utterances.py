#!/usr/bin/env python3
"""Generate ~20 stress-test utterances with `say` + `afconvert`.

Outputs raw f32le 16 kHz mono .f32 + expected-text .txt pairs into
scripts/stress/out/stt/ (or XENON_STRESS_OUT). No network, no services.
"""
from __future__ import annotations

import os
import struct
import subprocess
import sys
from pathlib import Path

UTTERANCES = [
    "hello xenon",
    "hey xenon are you there",
    "what time is it",
    "open safari",
    "open example dot com",
    "stop",
    "thanks xenon",
    "good morning",
    "good night",
    "tell me a joke",
    "read this page",
    "remember this",
    "can you hear me",
    "the quick brown fox jumps over the lazy dog",
    "this is a stress test of the speech recognition",
    "turn it down",
    "turn it up",
    "next",
    "done",
    "yes no maybe",
]


def say_to_f32(text: str, dst: Path) -> bool:
    tmp_aiff = dst.with_suffix(".aiff")
    tmp_wav = dst.with_suffix(".wav")
    # say -> AIFF, afconvert -> 16k mono f32le WAV, strip the 44-byte header.
    try:
        subprocess.run(
            ["/usr/bin/say", "-v", "Alex", "-o", str(tmp_aiff), text],
            check=True,
            capture_output=True,
        )
        subprocess.run(
            [
                "/usr/bin/afconvert",
                "-f", "WAVE",
                "-d", "LEF32@16000",
                "-c", "1",
                str(tmp_aiff),
                str(tmp_wav),
            ],
            check=True,
            capture_output=True,
        )
        raw = tmp_wav.read_bytes()
        # Walk RIFF chunks; afconvert inserts a FLLR chunk before data.
        i = 12
        payload = b""
        while i + 8 <= len(raw):
            cid = raw[i:i + 4]
            sz = struct.unpack("<I", raw[i + 4:i + 8])[0]
            if cid == b"data":
                payload = raw[i + 8:i + 8 + sz]
                break
            i += 8 + sz + (sz & 1)
        if not payload:
            return False
        dst.write_bytes(payload)
    except (subprocess.CalledProcessError, FileNotFoundError, OSError) as e:
        print(f"skip {text!r}: {e}", file=sys.stderr)
        return False
    finally:
        for p in (tmp_aiff, tmp_wav):
            if p.exists():
                p.unlink(missing_ok=True)
    dst.with_suffix(".txt").write_text(text + "\n", encoding="utf-8")
    return True


def main() -> int:
    out = Path(os.environ.get("XENON_STRESS_OUT", Path(__file__).parent / "out"))
    stt = out / "stt"
    stt.mkdir(parents=True, exist_ok=True)
    # Clear old artifacts so the harness never mixes runs.
    for p in stt.glob("*.f32"):
        p.unlink(missing_ok=True)
    for p in stt.glob("*.txt"):
        p.unlink(missing_ok=True)

    n = 0
    for i, text in enumerate(UTTERANCES, 1):
        dst = stt / f"u{i:02d}.f32"
        if say_to_f32(text, dst):
            n += 1
            print(f"[{i:02d}/{len(UTTERANCES)}] {text}")
        else:
            print(f"[{i:02d}/{len(UTTERANCES)}] SKIP {text}", file=sys.stderr)
    print(f"wrote {n}/{len(UTTERANCES)} to {stt}")
    return 0 if n else 1


if __name__ == "__main__":
    raise SystemExit(main())