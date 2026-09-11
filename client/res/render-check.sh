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
#   7. gl and 2d place glyph ink at the same vertical rows (grid alignment parity)
# Runs the page twice: ?r=2d (Canvas 2D reference) and ?r=gl (WebGL2). Item 7
# compares the per-glyph ink extents reported by the page across the two runs.
# Requires a wasm build (cargo build at the repo root) and
# chromium. Skips gracefully (exit 0) when chromium is unavailable.
set -euo pipefail
cd "$(dirname "$0")/.."

PORT="${PORT:-$(python3 -c 'import socket;s=socket.socket();s.bind(("127.0.0.1",0));print(s.getsockname()[1]);s.close()')}"
OUT="${TMPDIR:-/tmp}/krust-render.html"
SRV_LOG="${TMPDIR:-/tmp}/httpd-render.log"
ALIGN_TOL="${ALIGN_TOL:-2}"

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

run_mode() {
    local mode="$1"
    PROFILE="${TMPDIR:-/tmp}/krust-chrome-profile-${mode}"
    rm -rf "$PROFILE"
    timeout 60 "$CR" --headless=new --no-sandbox --disable-gpu \
      --user-data-dir="$PROFILE" \
      --virtual-time-budget=8000 \
      --dump-dom "http://127.0.0.1:$PORT/res/render-test.html?r=${mode}" \
      >"$OUT" 2>/dev/null || { echo "FAIL: chromium did not dump the page (${mode})"; exit 1; }
    local json
    json=$(grep -o '<pre id="result">[^<]*' "$OUT" | head -1 | sed 's/^<pre id="result">//')
    if [ -z "$json" ]; then
        echo "FAIL: no KRTEST line in page dump (${mode}) (did the wasm build succeed?)"
        exit 1
    fi
    echo "$json"
}

ALIGN_FAIL=0
for RENDER_MODE in 2d gl; do
    JSON=$(run_mode "$RENDER_MODE")
    eval "JSON_${RENDER_MODE}=\"\$JSON\""
    echo "$JSON" | python3 -c '
import json,sys
d=json.loads(sys.stdin.read())
m=d.get("metrics",{})
print("cell=%.3fx%.3f" % (m.get("cell_width",0), m.get("cell_height",0)))
for k in ("colors","text","orient","baseline","align","timing","dash","pipe","block","braille","blockgeo"):
    if k in d: print("  %-22s %s" % (k, d[k]))
if "error" in d: print("  error: %s" % d["error"])
sys.exit(0 if d.get("pass") and "error" not in d else 1)
' && rc=0 || rc=$?
    echo "  renderer=${RENDER_MODE}: $([ "$rc" = "0" ] && echo pass || echo FAIL)"
    if [ "$rc" != "0" ]; then rc_total=1; else rc_total=0; fi
done

# Cross-renderer alignment: the WebGL2 and Canvas 2D paths must paint each
# probe glyph's ink at the same vertical rows relative to the cell top.
if python3 - "$JSON_2d" "$JSON_gl" "${ALIGN_TOL}" <<'PY'
import json, sys
d2 = json.loads(sys.argv[1]); dg = json.loads(sys.argv[2])
tol = float(sys.argv[3])
a2, ag = d2.get("align", {}), dg.get("align", {})
worst = 0
deltas = {}
for k in ("T", "g", "_", "x"):
    if k not in a2 or k not in ag:
        print(f"  align: missing glyph {k}"); sys.exit(1)
    dt = abs(a2[k]["top"] - ag[k]["top"])
    db = abs(a2[k]["bottom"] - ag[k]["bottom"])
    worst = max(worst, dt, db)
    deltas[k] = {"dTop": dt, "dBottom": db}
print("  align  %s (tol %g)" % (json.dumps(deltas), tol))
if worst > tol:
    print(f"FAIL: gl-vs-2d glyph alignment drift {worst}px > tol {tol}px")
    sys.exit(1)
print("  align  gl/2d vertical parity OK (max drift %g px)" % worst)
sys.exit(0)
PY
then
    ac=0
else
    ac=$?
fi

if [ "$rc_total" = "0" ] && [ "$ac" = "0" ]; then
    echo "OK: render regression checks passed"
    exit 0
fi
echo "FAIL: render regression checks failed"
exit 1