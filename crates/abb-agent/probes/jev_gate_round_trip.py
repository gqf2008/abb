#!/usr/bin/env python3
"""端到端：受限会话的工具调用受 Jev 决策门禁（allow / deny / 不可用 fail-closed）。

判据分两处：①假 Jev Decisions API server 自己收到的请求体（`questions.allowed.type
== "noul"` + `state` 带 tool/args/workspace）；②模型侧（假 anthropic 网关第二跳）：
deny/不可用时第二跳里出现拒绝 reason，且**工具没有真执行**（工作区没有落盘文件）；
allow 时工具真执行（落盘）。

四个场景：
1. allow（Jev 回 noul=0.9）⇒ `dev__shell` 真执行（落盘标记文件）；
2. deny（Jev 回 noul=0.1）⇒ 工具不执行，第二跳出现「不应放行」拒绝 reason；
3. 不可用（Jev 回 500）⇒ 工具不执行，第二跳出现「fail-closed」拒绝 reason；
4. 全权限（session/new 不带 _meta）⇒ 不套门禁，工具直接执行（零开销）。

用法：`python3 jev_gate_round_trip.py <abb-agent 二进制>`
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
LLM_PORT = 18115
JEV_PORT = 18116
MARKER = "JEV-GATE-EXECUTED-b4c2"
failures = []
created_dirs = []

# 假 Jev 行为：由环境外不可见，直接由全局变量控制（每个场景设一次）。
jev_mode = {"noul": 0.9}  # 或 {"status": 500}


class JevServer(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def do_POST(self):
        length = int(self.headers.get("content-length", 0) or 0)
        raw = self.rfile.read(length).decode() if length else ""
        try:
            jev_mode["last_body"] = json.loads(raw)
        except Exception:
            jev_mode["last_body"] = {"_raw": raw}
        if jev_mode.get("status"):
            payload = json.dumps({"error": {"code": jev_mode["status"]}})
            self.send_response(jev_mode["status"])
        else:
            payload = json.dumps({
                "model": "typesafe/jev-1.13",
                "answers": {"allowed": {"type": "noul", "noul": jev_mode["noul"]}},
                "usage": {"input_tokens": 1, "output_tokens": 1},
            })
            self.send_response(200)
        data = payload.encode()
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def log_message(self, *args):
        pass


class LlmGateway(BaseHTTPRequestHandler):
    """假 anthropic 网关：第一跳让模型发起一次 `dev__shell`，第二跳结束回合。"""
    protocol_version = "HTTP/1.1"

    def do_POST(self):
        length = int(self.headers.get("content-length", 0) or 0)
        raw = self.rfile.read(length).decode() if length else ""
        try:
            body = json.loads(raw)
        except Exception:
            body = {"_raw": raw}
        jev_mode.setdefault("bodies", []).append(body)
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
                                                           "name": "dev__shell", "input": {}}}),
                ("content_block_delta", {"type": "content_block_delta", "index": 0,
                                         "delta": {"type": "input_json_delta",
                                                   "partial_json": json.dumps({
                                                       "command": f"echo {MARKER} > executed.txt"
                                                   })}}),
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


threading.Thread(target=HTTPServer(("127.0.0.1", JEV_PORT), JevServer).serve_forever,
                 daemon=True).start()
threading.Thread(target=HTTPServer(("127.0.0.1", LLM_PORT), LlmGateway).serve_forever,
                 daemon=True).start()


def check(label, ok, detail=""):
    print(f"  {'✅' if ok else '❌'} {label}{('：' + detail) if detail else ''}")
    if not ok:
        failures.append(label)


def run_turn(workspace, restricted):
    """跑一个回合，返回（第二跳请求体文本、工作区 executed.txt 是否落盘）。"""
    jev_mode["bodies"] = []
    before = len(jev_mode.get("bodies", []))
    env = dict(os.environ)
    env.update({
        "BUZZ_AGENT_PROVIDER": "anthropic",
        "ANTHROPIC_BASE_URL": f"http://127.0.0.1:{LLM_PORT}",
        "ANTHROPIC_MODEL": "probe-model",
        "ANTHROPIC_API_KEY": "sk-probe",
        "JEV_API_KEY": "sk-jev-probe",
        "JEV_BASE_URL": f"http://127.0.0.1:{JEV_PORT}",
        "HOME": tempfile.mkdtemp(prefix="abb-jev-home-"),
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
          "params": {"sessionId": "abb-1", "prompt": [{"type": "text", "text": "执行"}]}})
    wait(3, 60)
    try:
        proc.stdin.close()
    except Exception:
        pass
    time.sleep(0.3)
    proc.kill()
    bodies = jev_mode.get("bodies", [])
    hops = bodies[before:]
    second = hops[1] if len(hops) > 1 else {}
    executed = os.path.isfile(os.path.join(workspace, "executed.txt"))
    return json.dumps(second, ensure_ascii=False), executed


print("=== 场景 1：allow（Jev 回 noul=0.9）⇒ 工具真执行 ===")
jev_mode["noul"] = 0.9
ws = tempfile.mkdtemp(prefix="abb-jev-allow-")
created_dirs.append(ws)
second, executed = run_turn(ws, restricted=True)
check("工具真执行（落盘）", executed)
check("Jev 收到 Decisions 形状请求",
      jev_mode.get("last_body", {}).get("questions", {}).get("allowed", {}).get("type") == "noul",
      json.dumps(jev_mode.get("last_body", {}))[:200])
check("state 带 workspace", "workspace" in jev_mode.get("last_body", {}).get("state", {}))

print()
print("=== 场景 2：deny（Jev 回 noul=0.1）⇒ 工具不执行 + 拒绝 reason ===")
jev_mode["noul"] = 0.1
ws = tempfile.mkdtemp(prefix="abb-jev-deny-")
created_dirs.append(ws)
second, executed = run_turn(ws, restricted=True)
check("工具未执行", not executed)
check("模型侧看到拒绝 reason", "不应放行" in second, second[-200:])

print()
print("=== 场景 3：不可用（Jev 回 500）⇒ 工具不执行 + fail-closed ===")
jev_mode["status"] = 500
ws = tempfile.mkdtemp(prefix="abb-jev-down-")
created_dirs.append(ws)
second, executed = run_turn(ws, restricted=True)
check("工具未执行（fail-closed）", not executed)
check("模型侧看到 fail-closed reason", "fail-closed" in second, second[-200:])

print()
print("=== 场景 4：全权限（无 _meta）⇒ 不套门禁，工具直接执行 ===")
jev_mode.pop("status", None)
jev_mode["noul"] = 0.1  # 就算 Jev 拒绝，全权限会话也不该被拦
ws = tempfile.mkdtemp(prefix="abb-jev-owner-")
created_dirs.append(ws)
second, executed = run_turn(ws, restricted=False)
check("全权限会话工具直接执行（零开销）", executed)

print()
if failures:
    print(f"⇒ 判定：❌ {len(failures)} 项失败：{failures}")
    sys.exit(1)
print("⇒ 判定：✅ Jev 门禁三态 + 全权限零开销全部符合预期")
