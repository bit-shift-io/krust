import http.server
import socketserver
import sys
import time

PORT = int(sys.argv[1])
DIR = sys.argv[2]


class H(http.server.SimpleHTTPRequestHandler):
    def __init__(self, *args, **kwargs):
        super().__init__(*args, directory=DIR, **kwargs)

    def do_GET(self):
        if self.path.endswith("slow.png"):
            time.sleep(12)
        return super().do_GET()

    def log_message(self, *args):
        print(*args, flush=True)


with socketserver.ThreadingTCPServer(("127.0.0.1", PORT), H) as httpd:
    httpd.serve_forever()