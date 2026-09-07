#!/usr/bin/env bash
# Smoke-test the WASM terminal: serves client-wasm/, opens demo/ in headless
# Firefox, screenshots it, and checks the status bar for green (glyphs drawn)
# vs red (error / no glyphs).
set -euo pipefail
cd "$(dirname "$0")/.."

PORT="${PORT:-8123}"
OUT="${TMPDIR:-/tmp}/krust-smoke.png"
SRV_LOG="${TMPDIR:-/tmp}/httpd.log"

setsid nohup python3 demo/server.py "$PORT" "$(pwd)" \
  >"$SRV_LOG" 2>&1 < /dev/null &
SRV=$!
trap 'kill $SRV 2>/dev/null || true' EXIT
sleep 1

PROFILE="${TMPDIR:-/tmp}/ff-profile"
rm -rf "$PROFILE"
mkdir -p "$PROFILE"
timeout 50 firefox -no-remote -profile "$PROFILE" --headless \
  --window-size=800,600 --screenshot "$OUT" "http://127.0.0.1:$PORT/demo/" >/dev/null 2>&1

REGION="$OUT[500x30+0+570]"
green=$(magick "$REGION" -channel G -separate -threshold 45% -format "%[fx:mean]" info:)
red=$(magick "$REGION" -channel R -separate -threshold 45% -format "%[fx:mean]" info:)
echo "green_pixels=$green red_pixels=$red"
if [ "$(echo "$green > 0.3" | bc)" = "1" ] && [ "$(echo "$red < 0.3" | bc)" = "1" ]; then
    echo "OK: glyphs rendered (green status bar)"
else
    echo "FAIL: no glyphs rendered (status bar not green)"
    exit 1
fi