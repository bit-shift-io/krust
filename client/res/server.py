import http.server
import os
import socketserver
import subprocess
import sys

PORT = int(sys.argv[1])
DIR = sys.argv[2]

# The wasm lives in the repo-root target/ dir. render-check.sh starts this
# server with the doc root at client/ (DIR), so compute the wasm path from
# this script's own location rather than assuming DIR is the repo root.
WASM_PATH = os.path.join(
    os.path.dirname(os.path.dirname(os.path.abspath(__file__))),
    "..",
    "target",
    "wasm",
    "wasm32-unknown-unknown",
    "release",
    "terminal_client.wasm",
)


# Mirror the production krust server's fontconfig resolution (handlers.rs):
# same family stack, same acceptance rule (the bare `monospace` generic always
# passes; named families must resolve to *themselves*).
SYSTEM_FONT_STACK = [
    "JetBrains Mono",
    "Fira Code",
    "Menlo",
    "Consolas",
    "monospace",
]


def resolve_system_font():
    for family in SYSTEM_FONT_STACK:
        try:
            out = subprocess.run(
                ["fc-match", "--format=%{family[0]}|%{file}", family],
                capture_output=True, text=True, check=True, timeout=10,
            ).stdout.strip()
        except Exception:
            return None
        if "|" not in out:
            continue
        resolved, file = out.split("|", 1)
        if not file:
            continue
        installed = family.lower() == "monospace" or resolved.lower() == family.lower()
        if not installed:
            continue
        try:
            with open(file, "rb") as f:
                data = f.read()
            if data:
                return data
        except OSError:
            continue
    return None


_SYSTEM_FONT = None


def system_font_bytes():
    global _SYSTEM_FONT
    if _SYSTEM_FONT is None:
        _SYSTEM_FONT = resolve_system_font()
    return _SYSTEM_FONT


class H(http.server.SimpleHTTPRequestHandler):
    def __init__(self, *args, **kwargs):
        super().__init__(*args, directory=DIR, **kwargs)

    def do_GET(self):
        # Mirror the production krust server route /pkg/terminal_client_bg.wasm
        # so the test page loads the raw WASM binary exactly as in production.
        if self.path == "/pkg/terminal_client_bg.wasm":
            try:
                with open(WASM_PATH, "rb") as f:
                    data = f.read()
            except OSError:
                self.send_error(404, "wasm not built (run cargo build first)")
                return
            self.send_response(200)
            self.send_header("Content-Type", "application/wasm")
            self.send_header("Cache-Control", "no-store")
            self.send_header("Content-Length", str(len(data)))
            self.end_headers()
            self.wfile.write(data)
            return
        if self.path == "/system-font":
            data = system_font_bytes()
            if data is None:
                self.send_error(404, "no matching system font")
                return
            self.send_response(200)
            self.send_header("Content-Type", "font/ttf")
            self.send_header("Cache-Control", "no-store")
            self.send_header("Content-Length", str(len(data)))
            self.end_headers()
            self.wfile.write(data)
            return
        return super().do_GET()

    def log_message(self, *args):
        print(*args, flush=True)


with socketserver.ThreadingTCPServer(("127.0.0.1", PORT), H) as httpd:
    httpd.serve_forever()