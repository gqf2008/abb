#!/usr/bin/env python3
"""端到端：受限会话的读域闸（read/ls 只能读工作区内），owner 全权限读任意。

判据是模型侧（假 anthropic 网关第二跳）：受限会话读工作区外路径 ⇒ 工具结果是
错误（「拒绝读取会话工作区之外」）；owner 全权限读同一路径 ⇒ 工具结果正常（不拦）。

用法：`python3 read_confinement_round_trip.py <abb-agent 二进制>`
"""
import json
import os
import subprocess
import sys
import tempfile
import threading
import time
from http.server import BaseHTTPRequestHandler, HTTPServer

BIN = sys.argv[1]
PORT = 18117
JEV_PORT = 18118
failures = []
created_dirs = []

# 每个场景要读的「工作区外绝对路径」：探针自己建的临时文件。
OUTSIDE_MARKER = "READ-CONFINE-OUTSIDE-a19d"
tool_name = {"name": "dev__read", "args": {}}
bodies = []


class JevServer(BaseHTTPRequestHandler):
    """假 Jev：恒 allow（读域闸探针只测读域闸，不测 Jev 门禁）。"""
    protocol_version = "HTTP/1.1"

    def do_POST(self):
        length = int(self.headers.get("content-length", 0) or 0)
        if length:
            self.rfile.read(length)
        payload = json.dumps({
            "model": "typesafe/jev-1.13",
            "answers": {"allowed": {"type": "noul", "noul": 0.9}},
            "usage": {"input_tokens": 1, "output_tokens": 1},
        })
        data = payload.encode()
        self.send_response(200)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def log_message(self, *args):
        pass


class Gateway(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def do_POST(self):
        length = int(self.headers.get("content-length", 0) or 0)
        raw = self.rfile.read(length).decode() if length else ""
        try:
            bodies.append(json.loads(raw))
        except Exception:
            bodies.append({"_raw": raw})
        body = bodies[-1]
        has_tool_result = any(
            blk.get("type") == "tool_result"
            for m in body.get("messages", [])
            for blk in (m.get("content") if isinstance(m.get("content"), list) else [])
        )
        events = [("message_start", {"type": "message_start", "message": {
            "id": "m", "type": "message", "role": "assistant", "model": "m",
            "content": [], "stop_reason": None,
            "usage": {"input_tokens": 1, "output_tokens": 1}}})]
        if not has_tool_result:
            events += [
                ("content_block_start", {"type": "content_block_start", "index": 0,
                                         "content_block": {"type": "tool_use", "id": "toolu_1",
                                                           "name": tool_name["name"], "input": {}}}),
                ("content_block_delta", {"type": "content_block_delta", "index": 0,
                                         "delta": {"type": "input_json_delta",
                                                   "partial_json": json.dumps(tool_name["args"])}}),
                ("content_block_stop", {"type": "content_block_stop", "index": 0}),
                ("message_delta", {"type": "message_delta",
                                   "delta": {"stop_reason": "tool_use", "stop_sequence": None},
                                   "usage": {"output_tokens": 1}}),
            ]
        else:
            events += [
                ("content_block_start", {"type": "content_block_start", "index": 0,
                                         "content_block": {"type": "text", "text": ""}}),
                ("content_block_delta", {"type": "content_block_delta", "index": 0,
                                         "delta": {"type": "text_delta", "text": "收到"}}),
                ("content_block_stop", {"type": "content_block_stop", "index": 0}),
                ("message_delta", {"type": "message_delta",
                                   "delta": {"stop_reason": "end_turn", "stop_sequence": None},
                                   "usage": {"output_tokens": 1}}),
            ]
        events.append(("message_stop", {"type": "message_stop"}))
        payload = "".join(f"event: {t}\ndata: {json.dumps(d)}\n\n" for t, d in events)
        data = payload.encode()
        self.send_response(200)
        self.send_header("content-type", "text/event-stream")
        self.send_header("content-length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def log_message(self, *args):
        pass


threading.Thread(target=HTTPServer(("127.0.0.1", PORT), Gateway).serve_forever,
                 daemon=True).start()
threading.Thread(target=HTTPServer(("127.0.0.1", JEV_PORT), JevServer).serve_forever,
                 daemon=True).start()


def check(label, ok, detail=""):
    print(f"  {'✅' if ok else '❌'} {label}{('：' + detail) if detail else ''}")
    if not ok:
        failures.append(label)


def run_turn(workspace, restricted, read_path):
    before = len(bodies)
    tool_name["name"] = "dev__read"
    tool_name["args"] = {"path": read_path}
    env = dict(os.environ)
    env.update({
        "BUZZ_AGENT_PROVIDER": "anthropic",
        "ANTHROPIC_BASE_URL": f"http://127.0.0.1:{PORT}",
        "ANTHROPIC_MODEL": "probe-model",
        "ANTHROPIC_API_KEY": "sk-probe",
        "JEV_API_KEY": "sk-jev-probe",
        "JEV_BASE_URL": f"http://127.0.0.1:{JEV_PORT}",
        "HOME": tempfile.mkdtemp(prefix="abb-read-home-"),
        "RUST_LOG": "info",
    })
    env["USERPROFILE"] = env["HOME"]
    for key in ("ABB_AGENT_FAUX_TEXT", "ABB_AGENT_FAUX_TOOL", "OPENAI_COMPAT_MODEL"):
        env.pop(key, None)

    proc = subprocess.Popen(BIN, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                            stderr=subprocess.PIPE, env=env, text=True, bufsize=1)
    out = []
    t0 = time.time()
    threading.Thread(target=lambda: [out.append((time.time() - t0, l.strip()))
                                     for l in proc.stdout], daemon=True).start()

    def send(obj):
        try:
            proc.stdin.write(json.dumps(obj) + "\n")
            proc.stdin.flush()
            return True
        except (BrokenPipeError, OSError, ValueError):
            return False

    def wait(msg_id, limit):
        end = time.time() + limit
        while time.time() < end:
            for _, line in list(out):
                if f'"id":{msg_id}' in line.replace(" ", ""):
                    return line
            time.sleep(0.02)
        return None

    send({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}})
    wait(1, 10)
    params = {"cwd": workspace, "mcpServers": []}
    if restricted:
        params["_meta"] = {"sandbox": "workspace-write"}
    send({"jsonrpc": "2.0", "id": 2, "method": "session/new", "params": params})
    wait(2, 20)
    send({"jsonrpc": "2.0", "id": 3, "method": "session/prompt",
          "params": {"sessionId": "abb-1", "prompt": [{"type": "text", "text": "读"}]}})
    wait(3, 60)
    try:
        proc.stdin.close()
    except Exception:
        pass
    time.sleep(0.3)
    proc.kill()
    hops = bodies[before:]
    second = hops[1] if len(hops) > 1 else {}
    return json.dumps(second, ensure_ascii=False)


outside = tempfile.mkdtemp(prefix="abb-read-outside-")
created_dirs.append(outside)
outside_file = os.path.join(outside, "secret.txt")
with open(outside_file, "w", encoding="utf-8") as fh:
    fh.write(OUTSIDE_MARKER)

print("=== 场景 1：受限会话读工作区外绝对路径 ⇒ 拒绝 ===")
ws = tempfile.mkdtemp(prefix="abb-read-restricted-")
created_dirs.append(ws)
second = run_turn(ws, restricted=True, read_path=outside_file)
check("受限会话读工作区外被拒", "拒绝读取会话工作区之外" in second, second[-200:])
check("受限会话没读到越界内容", OUTSIDE_MARKER not in second)

print()
print("=== 场景 2：owner 全权限读同一路径 ⇒ 放行（对齐参照物 FullAccess）===")
ws = tempfile.mkdtemp(prefix="abb-read-owner-")
created_dirs.append(ws)
second = run_turn(ws, restricted=False, read_path=outside_file)
check("全权限会话能读到任意路径", OUTSIDE_MARKER in second, second[-200:])

print()
print("=== 场景 3：read-only 档工具面——shell/write/edit 不注入（模型看不见）===")
ws = tempfile.mkdtemp(prefix="abb-read-ro-")
created_dirs.append(ws)
ro_bodies_before = len(bodies)
tool_name["name"] = "dev__read"
tool_name["args"] = {"path": "whatever.txt"}
env = dict(os.environ)
env.update({
    "BUZZ_AGENT_PROVIDER": "anthropic",
    "ANTHROPIC_BASE_URL": f"http://127.0.0.1:{PORT}",
    "ANTHROPIC_MODEL": "probe-model",
    "ANTHROPIC_API_KEY": "sk-probe",
    "JEV_API_KEY": "sk-jev-probe",
    "JEV_BASE_URL": f"http://127.0.0.1:{JEV_PORT}",
    "HOME": tempfile.mkdtemp(prefix="abb-read-ro-home-"),
    "RUST_LOG": "info",
})
env["USERPROFILE"] = env["HOME"]
for key in ("ABB_AGENT_FAUX_TEXT", "ABB_AGENT_FAUX_TOOL", "OPENAI_COMPAT_MODEL"):
    env.pop(key, None)
proc = subprocess.Popen(BIN, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                        stderr=subprocess.PIPE, env=env, text=True, bufsize=1)
out_lines = []
threading.Thread(target=lambda: [out_lines.append(l.strip()) for l in proc.stdout],
                 daemon=True).start()

def ro_send(obj):
    try:
        proc.stdin.write(json.dumps(obj) + "\n")
        proc.stdin.flush()
        return True
    except (BrokenPipeError, OSError, ValueError):
        return False

ro_send({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}})
time.sleep(0.5)
ro_send({"jsonrpc": "2.0", "id": 2, "method": "session/new",
         "params": {"cwd": ws, "mcpServers": [], "_meta": {"sandbox": "read-only"}}})
time.sleep(0.5)
ro_send({"jsonrpc": "2.0", "id": 3, "method": "session/prompt",
         "params": {"sessionId": "abb-1", "prompt": [{"type": "text", "text": "读"}]}})
time.sleep(3)
proc.kill()
# 第一跳请求体里是模型看到的 tools 定义：read-only 下不该有 shell/write/edit。
ro_hops = bodies[ro_bodies_before:]
first = ro_hops[0] if ro_hops else {}
tools_seen = [t.get("name") for t in first.get("tools", [])]
for absent in ("dev__shell", "dev__write", "dev__edit"):
    check(f"read-only 档不暴露 {absent}", absent not in tools_seen, str(tools_seen))
for present in ("dev__read", "dev__ls"):
    check(f"read-only 档保留 {present}", present in tools_seen, str(tools_seen))

print()
if failures:
    print(f"⇒ 判定：❌ {len(failures)} 项失败：{failures}")
    sys.exit(1)
print("⇒ 判定：✅ 读域闸（受限拦截 / 全权限放行）符合预期")
