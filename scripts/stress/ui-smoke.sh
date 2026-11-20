#!/usr/bin/env bash
# Syntax-check every standalone JS file and every inline <script> block.
set -u
cd "$(dirname "$0")/../.." || exit 2
out=scripts/stress/out/ui-smoke.txt
: >"$out"
fail=0

check_js() {
  local file=$1
  if node --check "$file" >/dev/null 2>>"$out"; then
    echo "OK   $file" >>"$out"
  else
    echo "FAIL $file" >>"$out"
    fail=1
  fi
}

while IFS= read -r f; do
  check_js "$f"
done < <(find src -name '*.js' -type f | sort)

python3 - <<'PY' >>"$out" 2>&1
import pathlib, re, subprocess, tempfile
fail = False
for html in sorted(pathlib.Path("src").rglob("*.html")):
    text = html.read_text()
    for i, m in enumerate(re.finditer(r"<script(?![^>]*\bsrc=)[^>]*>(.*?)</script>", text, re.S)):
        with tempfile.NamedTemporaryFile("w", suffix=".js", delete=False) as f:
            f.write(m.group(1))
            path = f.name
        r = subprocess.run(["node", "--check", path], capture_output=True, text=True)
        print(("OK   " if r.returncode == 0 else "FAIL ") + f"{html}:inline#{i+1}")
        if r.returncode:
            print(r.stderr.strip())
            fail = True
raise SystemExit(1 if fail else 0)
PY
[ $? -ne 0 ] && fail=1

tail -5 "$out"
exit "$fail"