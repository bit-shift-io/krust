#!/usr/bin/env bash
# Usage: fftest.sh <port> <html-file> <out-png>
set -uo pipefail
PORT=$1; HTML=$2; OUT=$3
cd "$(dirname "$0")/.."

for p in $(ss -tlnp 2>/dev/null | grep ":$PORT " | grep -oP 'pid=\K[0-9]+'); do kill "$p" 2>/dev/null; done
sleep 1

setsid nohup python3 demo/server.py "$PORT" "$(pwd)" >/tmp/httpd.log 2>&1 </dev/null &
SRV=$!
sleep 1.2
if grep -q "address already in use" /tmp/httpd.log; then
    echo "BINDFAIL port=$PORT"
    kill "$SRV" 2>/dev/null
    exit 2
fi
echo "server up port=$PORT"

rm -rf /tmp/ff-profile; mkdir -p /tmp/ff-profile
cat > /tmp/ff-profile/user.js <<'EOF'
user_pref("browser.dom.window.dump.enabled", true);
user_pref("dom.max_script_run_time", 60);
user_pref("dom.max_chrome_script_run_time", 60);
user_pref("services.settings.enabled", false);
user_pref("app.normandy.enabled", false);
user_pref("app.normandy.api_url", "");
user_pref("datareporting.policy.dataSubmissionEnabled", false);
user_pref("network.captive-portal-service.enabled", false);
user_pref("browser.shell.checkDefaultBrowser", false);
user_pref("browser.startup.blankWindow", false);
user_pref("browser.snippets.enabled", false);
EOF

/usr/bin/timeout 70 firefox -no-remote -profile /tmp/ff-profile --headless \
    --window-size=800,600 --screenshot "$OUT" "http://127.0.0.1:$PORT/demo/$HTML" >/tmp/ff.out 2>&1
rc=$?
echo "firefox rc=$rc"
grep -aE "ms\]|terminated|Script terminated|CAUGHT|REJECTED|FAILED|PAGEERR|UNHANDLED" /tmp/ff.out | head -25

for p in $(ss -tlnp 2>/dev/null | grep ":$PORT " | grep -oP 'pid=\K[0-9]+'); do kill "$p" 2>/dev/null; done
sleep 0.3
exit $rc