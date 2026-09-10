#!/usr/bin/env bash
# Render regression test for the WASM terminal. Loads res/render-test.html in
# headless Chrome, which feeds known content through the parser and analyzes
# the resulting canvas pixels:
#   1. bold blue renders as bright blue (#5C5CFF) — xterm drawBoldTextInBrightColors
#   2. no dark navy (#0000EE) leaks through
#   3. a ─ row has no horizontal seams
#   4. a │ column has no vertical seams (cell pitch == painted glyph height)
#   5. a T row renders bar-on-top (glyphs not mirrored across the horizontal axis)
#   6. a g row reaches the cell bottom and an _ row sits there too (baseline alignment)
# Requires a wasm build (cargo build at the repo root) and
# chromium. Skips gracefully (exit 0) when chromium is unavailable.
set -euo pipefail
cd "$(dirname "$0")/.."

PORT="${PORT:-$(python3 -c 'import socket;s=socket.socket();s.bind(("127.0.0.1",0));print(s.getsockname()[1]);s.close()')}"
OUT="${TMPDIR:-/tmp}/krust-render.html"
SRV_LOG="${TMPDIR:-/tmp}/httpd-render.log"

CR=$(command -v chromium || command -v chromium-browser || true)
if [ -z "$CR" ]; then
    echo "SKIP: chromium not found (install chromium to run render regression checks)"
    exit 0
fi

setsid nohup python3 res/server.py "$PORT" "$(pwd)" \
  >"$SRV_LOG" 2>&1 < /dev/null &
SRV=$!
trap 'kill $SRV 2>/dev/null || true' EXIT
for i in $(seq 1 20); do
    curl -sf "http://127.0.0.1:$PORT/res/render-test.html" -o /dev/null && break
    sleep 0.5
done

PROFILE="${TMPDIR:-/tmp}/krust-chrome-profile"
rm -rf "$PROFILE"

timeout 60 "$CR" --headless=new --no-sandbox --disable-gpu \
  --user-data-dir="$PROFILE" \
  --virtual-time-budget=8000 \
  --dump-dom "http://127.0.0.1:$PORT/res/render-test.html" \
  >"$OUT" 2>/dev/null || { echo "FAIL: chromium did not dump the page"; exit 1; }

# Extract the test JSON from the #result <pre> element.
JSON=$(grep -o '<pre id="result">[^<]*' "$OUT" | head -1 | sed 's/^<pre id="result">//')
if [ -z "$JSON" ]; then
    echo "FAIL: no KRTEST line in page dump (did the wasm build succeed?)"
    exit 1
fi
echo "$JSON" | python3 -c '
import json,sys
d=json.loads(sys.stdin.read())
m=d.get("metrics",{})
print("cell=%.3fx%.3f" % (m.get("cell_width",0), m.get("cell_height",0)))
for k in ("colors","text","orient","baseline","timing","dash","pipe","block"):
    if k in d: print("  %-22s %s" % (k, d[k]))
for k in ("block_vdim_rows","block_hdim_cols"):
    if k in d: print("  %-22s %s" % (k, d[k]))
if "error" in d: print("  error: %s" % d["error"])
sys.exit(0 if d.get("pass") and "error" not in d else 1)
' && rc=0 || rc=$?

if [ "$rc" = "0" ]; then
    echo "OK: render regression checks passed"
else
    echo "FAIL: render regression checks failed"
fi
exit "$rc"