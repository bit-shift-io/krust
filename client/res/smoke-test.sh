#!/usr/bin/env bash
# Smoke-test the WASM terminal: serves client/, opens res/ in headless
# Firefox, screenshots it, and checks the status bar for green (glyphs drawn)
# vs red (error / no glyphs).
set -euo pipefail
cd "$(dirname "$0")/.."

PORT="${PORT:-8123}"
OUT="${TMPDIR:-/tmp}/krust-smoke.png"
SRV_LOG="${TMPDIR:-/tmp}/httpd.log"

setsid nohup python3 res/server.py "$PORT" "$(pwd)" \
  >"$SRV_LOG" 2>&1 < /dev/null &
SRV=$!
trap 'kill $SRV 2>/dev/null || true' EXIT
sleep 1

PROFILE="${TMPDIR:-/tmp}/ff-profile"
rm -rf "$PROFILE"
mkdir -p "$PROFILE"

# Poll: capture a screenshot repeatedly until the page reports "done" (glyphs
# rendered) or we run out of attempts. A single early screenshot races the
# async wasm init and would always read an idle/black page.
SRV="http://127.0.0.1:$PORT/res/?s=smoke"
PASS=0
for attempt in $(seq 1 10); do
    timeout 50 firefox -no-remote -profile "$PROFILE" --headless \
      --window-size=800,600 --screenshot "$OUT" "$SRV" >/dev/null 2>&1
    # Status bar is a fixed label at top-right (12px monospace "PASS" text in
    # #0f0). Probe that 140x24 region: the PASS text should yield >1% bright-green
    # pixels, with negligible red (backgrounds are dark).
    CROP="$OUT[140x24+650+4]"
    green=$(magick "$CROP" -channel G -separate -threshold 60% -format "%[fx:mean]" info: 2>/dev/null || echo 0)
    red=$(magick "$CROP" -channel R -separate -threshold 60% -format "%[fx:mean]" info: 2>/dev/null || echo 0)
    if [ "$(echo "$green > 0.01" | bc)" = "1" ] && [ "$(echo "$red < 0.01" | bc)" = "1" ]; then
        PASS=1
        echo "green_pixels=$green red_pixels=$red"
        break
    fi
    sleep 1
done

if [ "$PASS" = "1" ]; then
    echo "OK: glyphs rendered (green status bar)"
else
    echo "FAIL: no glyphs rendered (status bar not green; green=$green red=$red)"
    exit 1
fi