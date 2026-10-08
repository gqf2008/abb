#!/usr/bin/env python3
"""验证 openai 家族端到端可用（abb 把 openai-chat/openrouter/deepseek 归并成
`BUZZ_AGENT_PROVIDER=openai`；本机 config 就是 `openai-chat` + `deepseek-flash`）。

做法：起一个假网关，记录请求（URL / 鉴权头 / model），并回一段最小可用的
OpenAI chat-completions SSE。断言：
  1. 请求打到 `{base}/v1/chat/completions`（abb 的 base_url 带 /v1，rpi 只补路径）；
  2. 请求体里的 model 就是我们给的自定义 id（不是 rpi 内置目录里的任何东西）；
  3. 鉴权头是 abb 注入的 key；
  4. abb-agent 把网关回的文本发成 session/update，并以 stopReason=end_turn 收尾。
"""
import json
import os
import subprocess
import sys
import threading
import time
from http.server import BaseHTTPRequestHandler, HTTPServer

BIN = sys.argv[1]
PORT = 18102
REPLY = "来自假网关的回复"
captured = {}


class Gateway(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def do_POST(self):
        length = int(self.headers.get("content-length", 0) or 0)
        raw = self.rfile.read(length).decode() if length else ""
        captured["path"] = self.path
        captured["auth"] = self.headers.get("authorization")
        try:
            captured["body"] = json.loads(raw)
        except Exception:
            captured["body"] = {"_raw": raw}

        chunks = [
            {"id": "chatcmpl-probe", "object": "chat.completion.chunk", "created": 1,
             "model": captured["body"].get("model", "?"),
             "choices": [{"index": 0, "delta": {"role": "assistant", "content": REPLY},
                          "finish_reason": None}]},
            {"id": "chatcmpl-probe", "object": "chat.completion.chunk", "created": 1,
             "model": captured["body"].get("model", "?"),
             "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]},
        ]
        payload = "".join(f"data: {json.dumps(c)}\n\n" for c in chunks) + "data: [DONE]\n\n"
        body = payload.encode()
        self.send_response(200)
        self.send_header("content-type", "text/event-stream")
        self.send_header("content-length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *args):
        pass


httpd = HTTPServer(("127.0.0.1", PORT), Gateway)
threading.Thread(target=httpd.serve_forever, daemon=True).start()

env = dict(os.environ)
env.update({
    "BUZZ_AGENT_PROVIDER": "openai",
    "OPENAI_COMPAT_BASE_URL": f"http://127.0.0.1:{PORT}/v1",
    "OPENAI_COMPAT_MODEL": "vendor-native-id-v9",   # 刻意用不在任何内置目录里的 id
    "OPENAI_COMPAT_API_KEY": "sk-probe-gateway-key",
    "OPENAI_COMPAT_API": "chat",
    "RUST_LOG": "info",
})

proc = subprocess.Popen(BIN, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                        stderr=subprocess.PIPE, env=env, text=True, bufsize=1)
events = []
t0 = time.time()


def reader():
    for line in proc.stdout:
        events.append((time.time() - t0, line.strip()))


threading.Thread(target=reader, daemon=True).start()


def send(obj):
    proc.stdin.write(json.dumps(obj) + "\n")
    proc.stdin.flush()


def wait_for(pred, timeout):
    end = time.time() + timeout
    while time.time() < end:
        for el, line in list(events):
            if pred(line.replace(" ", "")):
                return el, line
        time.sleep(0.05)
    return None, None


send({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": 2}})
wait_for(lambda l: '"id":1,' in l, 10)
send({"jsonrpc": "2.0", "id": 2, "method": "session/new", "params": {}})
_, created = wait_for(lambda l: '"sessionId"' in l, 10)
sid = json.loads(created)["result"]["sessionId"] if created else None
send({"jsonrpc": "2.0", "id": 3, "method": "session/prompt",
      "params": {"sessionId": sid, "prompt": [{"type": "text", "text": "你好"}]}})
el, line = wait_for(lambda l: '"id":3,' in l, 60)

print(f"请求 URL        : {captured.get('path')}")
print(f"鉴权头          : {captured.get('auth')}")
print(f"请求体 model    : {(captured.get('body') or {}).get('model')}")
print(f"回合应答        : {line}")
chunks = [l for _, l in events if '"sessionUpdate":"agent_message_chunk"' in l.replace(" ", "")]
print(f"回复文本块      : {chunks[0] if chunks else '(无)'}")

ok = (
    captured.get("path") == f"/v1/chat/completions"
    and captured.get("auth") == "Bearer sk-probe-gateway-key"
    and (captured.get("body") or {}).get("model") == "vendor-native-id-v9"
    and chunks
    and REPLY in chunks[0]
    and line
    and '"stopReason":"end_turn"' in line.replace(" ", "")
)
print(f"\n⇒ 判定：{'端到端可用 ✅' if ok else '❌ 有断言未满足'}")

proc.stdin.close()
try:
    err = proc.stderr.read()
except Exception:
    err = ""
try:
    proc.wait(timeout=5)
except subprocess.TimeoutExpired:
    proc.kill()
httpd.shutdown()
print("--- agent stderr ---")
print("\n".join(l for l in err.splitlines() if l.strip())[:800])
