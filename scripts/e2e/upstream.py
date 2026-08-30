"""A recording stand-in for a vendor's subscription backend.

Stands where chatgpt.com and api.anthropic.com would, so a test can read
*exactly* what the gateway put on the wire — which seat's token it used,
which account id it claimed, and what body it sent. That is the half of the
path a unit test cannot see, and the half where "the wrong service's account
served this" would actually show up.

Every request is appended to the log as one JSON line. POST /_control
{"code": 429} makes it fail, so a test can watch what the gateway does when
a seat is refused.
"""
import json, sys, threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

PORT, LOG = int(sys.argv[1]), sys.argv[2]
state = {"code": 200}
lock = threading.Lock()


def record(entry):
    with lock, open(LOG, "a") as f:
        f.write(json.dumps(entry) + "\n")


# The Responses stream a real codex-cli accepts. Both details here were
# learned by pointing the real binary at a real gateway: `completed` must
# carry an id, and the text needs an item lifecycle around it or the client
# has nothing to attach the deltas to.
CODEX_EVENTS = [
    ("response.created", {"type": "response.created",
                          "response": {"id": "resp_up", "model": "gpt-5.6-sol"}}),
    ("response.output_item.added", {"type": "response.output_item.added", "output_index": 0,
                                    "item": {"id": "msg_up", "type": "message", "status": "in_progress",
                                             "role": "assistant", "content": []}}),
    ("response.output_text.delta", {"type": "response.output_text.delta", "item_id": "msg_up",
                                    "output_index": 0, "content_index": 0, "delta": "pong"}),
    ("response.output_item.done", {"type": "response.output_item.done", "output_index": 0,
                                   "item": {"id": "msg_up", "type": "message", "status": "completed",
                                            "role": "assistant",
                                            "content": [{"type": "output_text", "text": "pong",
                                                         "annotations": []}]}}),
    ("response.completed", {"type": "response.completed",
                            "response": {"id": "resp_up", "output": [],
                                         "usage": {"input_tokens": 11, "output_tokens": 1,
                                                   "total_tokens": 12}}}),
]

ANTHROPIC_REPLY = {"id": "msg_up", "type": "message", "role": "assistant",
                   "model": "claude-sonnet-5", "content": [{"type": "text", "text": "pong"}],
                   "stop_reason": "end_turn",
                   "usage": {"input_tokens": 11, "output_tokens": 1}}


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *a):
        pass

    def do_POST(self):
        raw = self.rfile.read(int(self.headers.get("Content-Length") or 0))
        if self.path == "/_control":
            state["code"] = json.loads(raw or b"{}").get("code", 200)
            return self._empty(204)
        try:
            body = json.loads(raw)
        except Exception:
            body = {}
        # The OAuth token endpoint. A seat's refresh token rotates the
        # instant the real endpoint answers, so a refresh can only ever be
        # exercised against a stand-in — the gateway is pointed here with
        # RAPID_CLAUDE_OAUTH_URL. Recorded like everything else, because
        # "did it actually refresh, and with what" is the whole question.
        if self.path.endswith("/oauth/token"):
            try:
                sent = json.loads(raw)
            except Exception:
                sent = {p.split("=", 1)[0]: p.split("=", 1)[1]
                        for p in raw.decode().split("&") if "=" in p}
            record({
                "path": self.path,
                "oauth": True,
                "content_type": self.headers.get("content-type"),
                "grant_type": sent.get("grant_type"),
                "client_id": sent.get("client_id"),
                "scope": sent.get("scope"),
                # Recorded so a test can prove the *stale* token was the one
                # sent for renewal, and that the rotated one came back.
                "sent_refresh_token": sent.get("refresh_token"),
            })
            return self._json(200, {
                "access_token": "sk-ant-oat01-REFRESHED",
                "refresh_token": "sk-ant-ort01-ROTATED",
                "expires_in": 3600,
                "refresh_token_expires_in": 7776000,
                "scope": "user:profile user:inference",
                "token_type": "Bearer",
            })

        auth = self.headers.get("authorization", "")
        record({
            "path": self.path,
            # The seat is identified by the token the gateway presented, not
            # by anything the gateway told us — that is the point.
            "bearer": auth[7:] if auth.startswith("Bearer ") else auth,
            "account_header": self.headers.get("chatgpt-account-id"),
            "originator": self.headers.get("originator"),
            "model": body.get("model"),
            "body_keys": sorted(body) if isinstance(body, dict) else [],
            "bytes": len(raw),
        })
        if state["code"] != 200:
            return self._json(state["code"],
                              {"error": {"message": "injected", "type": "rate_limit_error"}})
        if self.path.endswith("/responses"):
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream")
            self.send_header("Connection", "close")
            self.end_headers()
            for name, data in CODEX_EVENTS:
                self.wfile.write(("event: %s\ndata: %s\n\n" % (name, json.dumps(data))).encode())
                self.wfile.flush()
            return
        return self._json(200, ANTHROPIC_REPLY)

    def do_GET(self):
        return self._json(200, {"object": "list", "data": []})

    def _json(self, code, payload):
        body = json.dumps(payload).encode()
        self.send_response(code)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def _empty(self, code):
        self.send_response(code)
        self.send_header("Content-Length", "0")
        self.end_headers()


ThreadingHTTPServer(("0.0.0.0", PORT), Handler).serve_forever()
