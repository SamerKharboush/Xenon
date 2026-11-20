#!/usr/bin/env bash
# Render docs/screenshots/*.png from the app's own frontend.
#
# No Screen Recording permission needed: the pet preview, settings and
# onboarding pages are loaded in the Playwright Chromium headless shell against
# a stubbed window.__TAURI__. The DMG shot is composed from the real DMG
# background asset + icon.
#
# Usage: scripts/screenshots/render.sh
set -euo pipefail
cd "$(dirname "$0")/../.." || exit 2

SHELL_BIN="$HOME/Library/Caches/ms-playwright/chromium_headless_shell-1248/chrome-headless-shell-mac-x64/chrome-headless-shell"
[[ -x "$SHELL_BIN" ]] || { echo "chrome-headless-shell not found at $SHELL_BIN" >&2; exit 1; }

OUT="docs/screenshots"
mkdir -p "$OUT"

PREVIEW_DIR="scripts/screenshots"
OUT="docs/screenshots"
mkdir -p "$OUT"

# The pet renderer loads `/sprites/light/...` and `/pet/renderer.js` with
# absolute paths, so serve the real `src/` over HTTP and put the preview page
# inside it. This also lets the module script + image decode finish reliably.
SRC="src"
PREVIEW_SRC="$SRC/__screenshot_preview.html"
cp "$PREVIEW_DIR/preview.html" "$PREVIEW_SRC"

PORT=8791
python3 -m http.server "$PORT" --directory "$SRC" >/dev/null 2>&1 &
SRV=$!
trap 'kill "$SRV" 2>/dev/null; rm -f "$PWD/$PREVIEW_SRC"' EXIT
sleep 1

PREVIEW="http://127.0.0.1:$PORT/__screenshot_preview.html"
SETTINGS_SRC="$SRC/settings.html"
ONBOARDING_SRC="$SRC/onboarding.html"

# Render one URL to a PNG with a transparent (or white) background.
# $1=url $2=out.png $3=WxH $4=wait-ms $5=bg(00000000=transparent)
shot() {
    local url="$1" out="$2" size="$3" wait="$4" bg="${5:-FFFFFFFF}"
    "$SHELL_BIN" \
        --headless \
        --disable-gpu \
        --no-sandbox \
        --hide-scrollbars \
        --screenshot="$PWD/$out" \
        --window-size="$size" \
        --default-background-color="$bg" \
        --virtual-time-budget="$wait" \
        "$url" >/dev/null 2>&1 || true
    if [ -s "$PWD/$out" ]; then
        echo "  $out ($(du -h "$PWD/$out" | cut -f1))"
    else
        echo "  $out FAILED" >&2
    fi
}

echo "Rendering pet states (transparent bg)…"
for state in Idle Listening Thinking Speaking Agentic; do
    shot "$PREVIEW?state=$state" "$OUT/state-$(echo "$state" | tr 'A-Z' 'a-z').png" "320,300" 2500 00000000
done

echo "Rendering settings + onboarding (opaque)…"
# settings/onboarding need a __TAURI__ stub; render.sh injects one via a temp copy
# served from the same HTTP root so relative asset paths resolve.
STUB="$(mktemp)"
cat > "$STUB" <<'JS'
<script>
window.__TAURI__ = {
  core: { invoke: async (cmd, args) => {
    if (cmd === 'get_permissions') return { accessibility:{granted:true,state:'granted',requestable:false}, screenRecording:{granted:true,state:'granted',requestable:true}, microphone:{granted:false,state:'denied',requestable:true} };
    if (cmd === 'get_config') return { pet_name:'Xenon', provider_url:'http://localhost:8080/v1', model_name:'qwen3', providers:[{name:'ToshLLM',base_url:'http://localhost:8080/v1',api_key:null,model:'qwen3',vision_model:'qwen3',supports_tools:true,supports_vision:false}], fallback_order:['ToshLLM'], listen_mode:'wake_word', voice_enabled:true, tts_enabled:true, wake_sensitivity:0.5, max_steps:15, light_sprites:true, lazy_audio:true, idle_power_saver:true, auto_check_updates:true, last_update_check:0, skipped_version:null, whisper_model_path:'', vad_model_path:'', tts_model_dir:'', language:'en', silence_ms:700, stt_unload_minutes:5, tts_unload_minutes:5, roles:{chat:'',agent:'',vision:''}, allow_scripts:false, browser_eval_enabled:false, wake_phrase:'', wake_chime:true, click_to_talk:true, wake_model_path:'', system_voice:'' };
    if (cmd === 'provider_presets') return [
      {name:'LM Studio',base_url:'http://localhost:1234/v1',needs_key:false,notes:'Local server.',local:true},
      {name:'ToshLLM',base_url:'http://localhost:8080/v1',needs_key:false,notes:'Local llama-server (Qwen3-14B).',local:true},
      {name:'OpenAI',base_url:'https://api.openai.com/v1',needs_key:true,notes:'Cloud.',local:false},
      {name:'Anthropic',base_url:'https://api.anthropic.com/v1',needs_key:true,notes:'Cloud.',local:false}
    ];
    if (cmd === 'detect_local_providers') return [{name:'ToshLLM',base_url:'http://localhost:8080/v1',models:['qwen3']}];
    if (cmd === 'list_voices') return {speakers:[],count:0};
    if (cmd === 'get_update_status') return {currentVersion:'0.2.0',available:false,message:'Up to date.'};
    return null;
  } },
  event: { listen: () => Promise.resolve(() => {}) },
  window: { getCurrentWindow: () => ({ close: () => {} }) },
};
</script>
JS

# Inject the stub after <head>, write into src/ so relative assets resolve, render, clean up.
render_stubbed() {
    local src="$1" out="$2" size="$3" name="$4"
    local tmp="$SRC/__shot_$name.html"
    python3 - "$src" "$STUB" "$tmp" <<'PY'
import sys, pathlib
src, stub, dst = sys.argv[1], pathlib.Path(sys.argv[2]).read_text(), sys.argv[3]
html = pathlib.Path(src).read_text()
html = html.replace('<head>', '<head>\n' + stub, 1)
pathlib.Path(dst).write_text(html)
PY
    shot "http://127.0.0.1:$PORT/__shot_$name.html" "$out" "$size" 3000 FFFFFFFF
    rm -f "$tmp"
}
render_stubbed "$SETTINGS_SRC" "$OUT/settings.png" "650,780" settings
render_stubbed "$ONBOARDING_SRC" "$OUT/onboarding.png" "540,520" onboarding

echo "Rendering DMG background (from the real asset)…"
# Compose a 660x400 canvas: the dmg-bg.png as background + app icon + Applications folder glyph.
python3 - "$PWD/src-tauri/icons/dmg-bg.png" "$OUT/dmg.png" <<'PY'
import sys, struct, zlib
# Minimal PNG composer without PIL: build from the source bg scaled to 660x400
# is complex; instead just copy the source background scaled via sips, then
# this script reports. sips handles the scaling + compositing shell-side.
PY
# Use sips to scale the bg to 660x400, then overlay the app icon + a folder glyph.
BG="$PWD/src-tauri/icons/dmg-bg.png"
ICON="$PWD/src-tauri/icons/128x128@2x.png"
TMPD="$(mktemp -d)"
sips -z 400 660 "$BG" --out "$TMPD/bg.png" >/dev/null 2>&1
# Compose: place icon at (180,170) and a folder-ish rounded rect at (480,170).
python3 - "$TMPD/bg.png" "$ICON" "$OUT/dmg.png" <<'PY'
import sys, zlib, struct
bg_path, icon_path, out_path = sys.argv[1], sys.argv[2], sys.argv[3]

def read_png(path):
    import struct, zlib
    with open(path,'rb') as f: data=f.read()
    assert data[:8]==b'\x89PNG\r\n\x1a\n'
    pos=8; w=h=None; idat=b''
    while pos < len(data):
        ln=struct.unpack('>I',data[pos:pos+4])[0]; typ=data[pos+4:pos+8]
        chunk=data[pos+8:pos+8+ln]
        if typ==b'IHDR':
            w,h,bd,ct=struct.unpack('>IIBB',chunk[:10])
        elif typ==b'IDAT': idat+=chunk
        pos += 12+ln
    raw=zlib.decompress(idat)
    # assume RGBA8, filter 0 rows
    stride=w*4; out=bytearray(h*stride)
    prev=bytearray(stride)
    p=0
    for y in range(h):
        ft=raw[p]; p+=1
        line=bytearray(raw[p:p+stride]); p+=stride
        if ft==0: pass
        elif ft==1:
            for i in range(4,stride): line[i]=(line[i]+line[i-4])&255
        elif ft==2:
            for i in range(stride): line[i]=(line[i]+prev[i])&255
        elif ft==3:
            for i in range(stride): line[i]=(line[i]+((line[i-4] if i>=4 else 0)+prev[i])//2)&255
        elif ft==4:
            for i in range(stride):
                a=line[i-4] if i>=4 else 0; b=prev[i]; c=prev[i-4] if i>=4 else 0
                pa=abs(b-c); pb=abs(a-c); pc=abs(a+b-2*c)
                pr=a if (pa<=pb and pa<=pc) else (b if pb<=pc else c)
                line[i]=(line[i]+pr)&255
        out[y*stride:(y+1)*stride]=line; prev=line
    return w,h,out

def read_rgba(path):
    return read_png(path)

def paste(dst, dw, dh, src, sw, sh, ox, oy):
    for y in range(sh):
        for x in range(sw):
            si=(y*sw+x)*4; di=((oy+y)*dw+(ox+x))*4
            a=src[si+3]
            if a==0: continue
            if a==255:
                dst[di:di+4]=src[si:si+4]
            else:
                for k in range(3):
                    dst[di+k]=(src[si+k]*a+dst[di+k]*(255-a))//255
                dst[di+3]=255

def write_png(path,w,h,buf):
    stride=w*4
    raw=b''.join(b'\x00'+bytes(buf[y*stride:(y+1)*stride]) for y in range(h))
    comp=zlib.compress(raw,9)
    def chunk(typ,data):
        c=struct.pack('>I',len(data))+typ+data
        c+=struct.pack('>I',zlib.crc32(typ+data)&0xffffffff)
        return c
    png=b'\x89PNG\r\n\x1a\n'
    png+=chunk(b'IHDR',struct.pack('>IIBBBBB',w,h,8,6,0,0,0))
    png+=chunk(b'IDAT',comp)
    png+=chunk(b'IEND',b'')
    open(path,'wb').write(png)

w,h,buf=read_rgba(bg_path)
iw,ih,ibuf=read_rgba(icon_path)
# icon ~256x256 at (180,170) center-ish; scale down by simple nearest to 96px
scale=96/ih
nw,nh=int(iw*scale),int(ih*scale)
ib=bytearray(nw*nh*4)
for y in range(nh):
    for x in range(nw):
        sx=int(x/scale); sy=int(y/scale)
        ib[(y*nw+x)*4:(y*nw+x)*4+4]=ibuf[(sy*iw+sx)*4:(sy*iw+sx)*4+4]
paste(buf,w,h,ib,nw,nh,180-48,170-48)
# Applications folder: draw a simple rounded blue-grey folder rectangle at (480,170)
fx,fy=480-48,170-40; fw,fh=96,80
for y in range(fh):
    for x in range(fw):
        di=((fy+y)*w+(fx+x))*4
        edge = 1 if (x<2 or y<2 or x>=fw-2 or y>=fh-2) else 0
        r,g,b = (120,170,230) if not edge else (90,130,190)
        buf[di]=r; buf[di+1]=g; buf[di+2]=b; buf[di+3]=255
write_png(out_path,w,h,buf)
print(f"wrote {out_path} {w}x{h}")
PY
rm -rf "$TMPD"

echo
echo "Done. Files in $OUT:"
ls -la "$OUT"/*.png