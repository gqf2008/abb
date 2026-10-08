#!/usr/bin/env python3
"""验证评审反证 (a)：provider 失败必须如实报错，而不是「成功 + 空文本」。

做法：起一个只回 401 的本地 anthropic 假端点，跑一个回合。
第一版行为：`stopReason:"end_turn"` + 零 session/update + 零日志
（abb 会把它当「纯工具回合」不投递、还记成功）。
修复后预期：JSON-RPC **error**（abb 的 parse_prompt_response 会转成 Err ⇒ 回合 Failed）。
"""
import json
import os
import subprocess
import sys
import threading
import time
from http.server import BaseHTTPRequestHandler, HTTPServer

BIN = sys.argv[1]
PORT = 18100


class Unauthorized(BaseHTTPRequestHandler):
    def do_POST(self):
        length = int(self.headers.get("content-length", 0) or 0)
        self.rfile.read(length)
        body = json.dumps({
            "type": "error",
            "error": {"type": "authentication_error", "message": "invalid x-api-key (probe)"},
        }).encode()
        self.send_response(401)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *args):
        pass


httpd = HTTPServer(("127.0.0.1", PORT), Unauthorized)
threading.Thread(target=httpd.serve_forever, daemon=True).start()

env = dict(os.environ)
env.update({
    "BUZZ_AGENT_PROVIDER": "anthropic",
    "ANTHROPIC_BASE_URL": f"http://127.0.0.1:{PORT}",
    "ANTHROPIC_API_KEY": "sk-invalid-probe",
    "ANTHROPIC_MODEL": "claude-haiku-4-5",  # 顺带验证 ANTHROPIC_MODEL 生效
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
sid = json.loads(created)["result"]["sessionId"]
send({"jsonrpc": "2.0", "id": 3, "method": "session/prompt",
      "params": {"sessionId": sid, "prompt": [{"type": "text", "text": "在吗"}]}})
el, line = wait_for(lambda l: '"id":3,' in l, 90)

print(f"[t={el if line else -1:.1f}s] 回合应答：{line}")
if line:
    text = line.replace(" ", "")
    contains_error = '"error"' in text and '"result"' not in text
    if contains_error:
        print("  ⇒ 判定：如实报错 ✅（不是成功空回合）")
    else:
        print("  ⇒ 判定：❌ 仍是 result（第一版的缺陷形态）")

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

print("--- agent stderr（应能看到失败原因）---")
print("\n".join(l for l in err.splitlines() if l.strip())[:1200])
