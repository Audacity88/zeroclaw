#!/usr/bin/env python3
# OpenAI-compatible stub: fixed priced usage per call; "SPAWN" in the last user message -> shell tool call.
import json, sys, time, uuid
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
LOG = open('/workspace/zeroclaw/ev11589/stub.log', 'a', buffering=1)
USAGE = {"prompt_tokens": 5000, "completion_tokens": 100, "total_tokens": 5100}
class H(BaseHTTPRequestHandler):
    def log_message(self, *a): pass
    def _send(self, code, obj):
        b = json.dumps(obj).encode(); self.send_response(code)
        self.send_header('Content-Type', 'application/json'); self.send_header('Content-Length', str(len(b))); self.end_headers(); self.wfile.write(b)
    def do_GET(self):
        self._send(200, {"object": "list", "data": [{"id": "stub-model", "object": "model"}]})
    def do_POST(self):
        req = json.loads(self.rfile.read(int(self.headers.get('Content-Length', 0))) or b'{}')
        msgs = req.get('messages', [])
        last = msgs[-1] if msgs else {}
        lu = next((m for m in reversed(msgs) if m.get('role') == 'user'), {})
        text = lu.get('content') if isinstance(lu.get('content'), str) else json.dumps(lu.get('content'))
        msg = {"role": "assistant", "content": None}
        if last.get('role') == 'user' and 'SPAWN' in (text or ''):
            msg["tool_calls"] = [{"id": "call_" + uuid.uuid4().hex[:8], "type": "function",
                "function": {"name": "shell", "arguments": json.dumps({"command": "/workspace/zeroclaw/ev11589/child.sh", "approved": True})}}]
            fin = "tool_calls"
        else:
            msg["content"] = "Stub reply after %d messages." % len(msgs); fin = "stop"
        LOG.write("%s POST %s stream=%s last=%s fin=%s\n" % (time.strftime('%H:%M:%S'), self.path, req.get('stream'), last.get('role'), fin))
        cid = "chatcmpl-" + uuid.uuid4().hex[:8]
        if req.get('stream'):
            self.send_response(200); self.send_header('Content-Type', 'text/event-stream'); self.end_headers()
            delta = dict(msg)
            if "tool_calls" in delta:
                delta["tool_calls"] = [dict(delta["tool_calls"][0], index=0)]
            for ch in ({"index": 0, "delta": delta, "finish_reason": None}, {"index": 0, "delta": {}, "finish_reason": fin}):
                self.wfile.write(b"data: " + json.dumps({"id": cid, "object": "chat.completion.chunk", "model": "stub-model", "choices": [ch]}).encode() + b"\n\n")
            self.wfile.write(b"data: " + json.dumps({"id": cid, "object": "chat.completion.chunk", "model": "stub-model", "choices": [], "usage": USAGE}).encode() + b"\n\n")
            self.wfile.write(b"data: [DONE]\n\n"); self.wfile.flush()
        else:
            self._send(200, {"id": cid, "object": "chat.completion", "model": "stub-model", "choices": [{"index": 0, "message": msg, "finish_reason": fin}], "usage": USAGE})
ThreadingHTTPServer(('127.0.0.1', 18089), H).serve_forever()
