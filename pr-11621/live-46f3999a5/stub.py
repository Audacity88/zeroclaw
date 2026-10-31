#!/usr/bin/env python3
# OpenAI-compatible stub: echoes the last user line. "SLOW" delays the reply 25 s; "FAILSLOW" waits 20 s, then returns HTTP 400.
import json, sys, time, uuid
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
LOG = open('/workspace/zeroclaw/ev11621/stub.log', 'a', buffering=1)
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
        lastline = ([l for l in (text or '').replace('\\n','\n').split('\n') if l.strip()][-1:] or [''])[0]
        if 'FAILSLOW' in lastline:
            time.sleep(20)
            LOG.write("%s POST %s -> 400 (FAILSLOW)\n" % (time.strftime('%H:%M:%S'), self.path))
            return self._send(400, {"error": {"message": "stub: forced provider failure", "type": "invalid_request_error"}})
        if 'SLOW' in lastline:
            time.sleep(25)
        q = [l for l in (text or '').replace('\\n','\n').split('\n') if l.strip()][-1:] or ['']; q = q[0].strip().strip('"')[:60]
        msg["content"] = "Answer to: %s" % q; fin = "stop"
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
ThreadingHTTPServer(('127.0.0.1', 18092), H).serve_forever()
