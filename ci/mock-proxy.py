#!/usr/bin/env python3
"""A stand-in `nucleus-tool-proxy`, so CI can check the checker.

`ccn-mcp --check` is this repo's verification of the mediated path. An
unexercised verifier is a claim, so CI runs it against this, which answers with
the proxy's real field names (see `contracts/tool-proxy-requests.json`) and
nothing else — a body spelled the built-ins' way gets the same `422` the real
proxy gives.

Two modes, because the check has to be right in both directions:

  strict  the escape probe is refused        -> `--check` must exit 0
  broken  the escape probe is SERVED         -> `--check` must exit 1

The second is the one worth having. A checker that passes when containment is
not holding is worse than no checker, and `broken` is how we know this one does
not.
"""

import json
import sys
from http.server import BaseHTTPRequestHandler, HTTPServer

FILES: dict[str, str] = {}
MODE = sys.argv[1] if len(sys.argv) > 1 else "strict"
PORT = int(sys.argv[2]) if len(sys.argv) > 2 else 8799
ROOT = "/work"


class Handler(BaseHTTPRequestHandler):
    def log_message(self, *args):
        pass  # stdout is the test's, not ours

    def _send(self, code, obj):
        body = json.dumps(obj).encode()
        self.send_response(code)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):
        if self.path == "/v1/health":
            self._send(200, {"ok": True})
        else:
            self._send(404, {"error": "no such route"})

    def do_POST(self):
        length = int(self.headers.get("content-length") or 0)
        body = json.loads(self.rfile.read(length) or b"{}")

        if self.path == "/v1/run":
            # RunRequest { args, .. } — a `command` string is a 422, as it was.
            if "args" not in body:
                return self._send(422, {"error": "missing field `args`"})
            return self._send(200, {"status": 0, "success": True, "stdout": "", "stderr": ""})

        if self.path == "/v1/write":
            # WriteRequest { path, contents }
            if "path" not in body or "contents" not in body:
                return self._send(422, {"error": "missing field `path` or `contents`"})
            FILES[body["path"]] = body["contents"]
            return self._send(200, {"ok": True})

        if self.path == "/v1/read":
            # ReadRequest { path }
            path = body.get("path")
            if path is None:
                return self._send(422, {"error": "missing field `path`"})
            # Containment: an absolute path outside the root does not resolve.
            if path.startswith("/") and not path.startswith(ROOT):
                if MODE == "strict":
                    return self._send(
                        403,
                        {
                            "error": "sandbox_escape",
                            "reason": "resolves outside the sandbox root",
                        },
                    )
                return self._send(200, {"contents": "root:!:19000:0:99999:7:::\n"})
            if path not in FILES:
                return self._send(404, {"error": "not found"})
            return self._send(200, {"contents": FILES[path]})

        self._send(404, {"error": "no such route"})


if __name__ == "__main__":
    HTTPServer(("127.0.0.1", PORT), Handler).serve_forever()
