#!/usr/bin/env python3
"""端到端：MCP 工具结果里的**图片真的到了模型**（而不是被降级或换占位文本）。

第五轮独立评审（`abb-reviewer-62`）指出：本包把图片从「base64 正文」改成
`TextContentOrImage::Image` 之后，图片在两条 provider 路径上**都到不了模型**——
因为 `custom_model()` 用 `Model::new` 构造，`Model.input` 只含 `Text`，而 rpi 按
`input.contains(&InputModality::Image)` 做能力门控（anthropic `build_params.rs`、
openai `openai_completions.rs`），不声明就被换成 `(see attached image)` /
`(tool image omitted: …)` 占位文本。仅图片的结果因此比修前**信息更少**。

做法：起一个假 OpenAI chat 网关——第一次请求回 `tool_calls`（让模型调 MCP 工具），
第二次请求把 MCP 返回的文本+图片回灌进去。判据是**第二次请求体里有没有那段图片的
data URL**（`data:image/png;base64,…`），这是「模型真能看见图」的直接证据。

用法：`python3 mcp_image_result_reaches_model.py <abb-agent 二进制>`
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
HERE = os.path.dirname(os.path.abspath(__file__))
SERVER = os.path.join(HERE, "fake_mcp_server.py")
PORT = 18107
# 图片内容用可检索的标记，避免与任何 base64 噪声混淆。
IMAGE_BASE64 = "iVBORw0KGgoAAAANSUhEUgMARKER62"
TOOL_NAME = "fakemcp__echo_query"
bodies = []


class Gateway(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def do_POST(self):
        length = int(self.headers.get("content-length", 0) or 0)
        raw = self.rfile.read(length).decode() if length else ""
        try:
            body = json.loads(raw)
        except Exception:
            body = {"_raw": raw}
        bodies.append(body)
        already = any(
            isinstance(m, dict) and m.get("role") == "tool" for m in body.get("messages", [])
        )
        if not already:
            # 第一次：让模型发一次工具调用（限定名，bare 名由本包回落）。
            chunks = [
                {"id": "c1", "object": "chat.completion.chunk", "created": 1, "model": "m",
                 "choices": [{"index": 0, "delta": {"role": "assistant", "tool_calls": [
                     {"index": 0, "id": "call_1", "type": "function",
                      "function": {"name": TOOL_NAME, "arguments": json.dumps({"q": "看图"})}}]},
                     "finish_reason": None}]},
                {"id": "c1", "object": "chat.completion.chunk", "created": 1, "model": "m",
                 "choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}]},
            ]
        else:
            chunks = [
                {"id": "c2", "object": "chat.completion.chunk", "created": 1, "model": "m",
                 "choices": [{"index": 0, "delta": {"role": "assistant", "content": "收到"},
                              "finish_reason": None}]},
                {"id": "c2", "object": "chat.completion.chunk", "created": 1, "model": "m",
                 "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]},
            ]
        payload = "".join(f"data: {json.dumps(c)}\n\n" for c in chunks) + "data: [DONE]\n\n"
        data = payload.encode()
        self.send_response(200)
        self.send_header("content-type", "text/event-stream")
        self.send_header("content-length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def log_message(self, *args):
        pass


httpd = HTTPServer(("127.0.0.1", PORT), Gateway)
threading.Thread(target=httpd.serve_forever, daemon=True).start()

workdir = tempfile.mkdtemp(prefix="abb-mcp-img-")
env = dict(os.environ)
env.update({
    "BUZZ_AGENT_PROVIDER": "openai",
    "OPENAI_COMPAT_BASE_URL": f"http://127.0.0.1:{PORT}/v1",
    "OPENAI_COMPAT_MODEL": "vendor-native-id-v9",
    "OPENAI_COMPAT_API_KEY": "sk-probe-gateway-key",
    "OPENAI_COMPAT_API": "chat",
    "RUST_LOG": "info",
})
for key in ("ABB_AGENT_FAUX_TEXT", "ABB_AGENT_FAUX_TOOL", "ANTHROPIC_API_KEY"):
    env.pop(key, None)

proc = subprocess.Popen(BIN, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                        stderr=subprocess.PIPE, env=env, text=True, bufsize=1)
out = []
t0 = time.time()
threading.Thread(target=lambda: [out.append((time.time() - t0, l.strip())) for l in proc.stdout],
                 daemon=True).start()


def send(obj):
    proc.stdin.write(json.dumps(obj) + "\n")
    proc.stdin.flush()


def wait(msg_id, timeout):
    end = time.time() + timeout
    while time.time() < end:
        for elapsed, line in list(out):
            if f'"id":{msg_id}' in line.replace(" ", ""):
                return elapsed, json.loads(line)
        time.sleep(0.02)
    return None, None


send({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}})
wait(1, 10)
send({"jsonrpc": "2.0", "id": 2, "method": "session/new", "params": {
    "cwd": workdir,
    "mcpServers": [{
        "name": "fakemcp",
        "command": sys.executable,
        "args": [SERVER, "--image-data", IMAGE_BASE64],
        "env": [],
    }],
}})
elapsed, resp = wait(2, 30)
send({"jsonrpc": "2.0", "id": 3, "method": "session/prompt", "params": {
    "sessionId": (resp or {}).get("result", {}).get("sessionId", "abb-1"),
    "prompt": [{"type": "text", "text": "看图"}],
}})
wait(3, 60)
time.sleep(0.5)
try:
    proc.stdin.close()
except Exception:
    pass
time.sleep(0.3)
proc.kill()

failures = []


def check(label, ok, detail=""):
    print(f"  {'✅' if ok else '❌'} {label}{('：' + detail) if detail else ''}")
    if not ok:
        failures.append(label)


print(f"假网关收到 {len(bodies)} 次请求")
check("工具确实被调用了（有第二跳请求）", len(bodies) >= 2, f"{len(bodies)} 次")
if len(bodies) >= 2:
    second = json.dumps(bodies[1], ensure_ascii=False)
    check("第二跳请求里带着 image data URL",
          f"data:image/png;base64,{IMAGE_BASE64}" in second,
          "data:image/png;base64,<marker> 出现在第二跳请求体里")
    check("没有被换成占位文本",
          "(see attached image)" not in second and "omitted" not in second)
    tool_msgs = [m for m in bodies[1].get("messages", [])
                 if isinstance(m, dict) and m.get("role") == "tool"]
    check("工具文本结果也在", bool(tool_msgs) and "echo" in json.dumps(tool_msgs, ensure_ascii=False))


# ---------------------------------------------------------------------------
# 场景 2：anthropic —— 图片**本来**到不了模型（rpi-ai 0.3.16 把 `Content::Image` 硬编码成
# 占位文本），所以本包必须自己给出**如实**的一行说明，而不是让模型看到
# `(see attached image)`（指着一张不存在的图，比修前更差）。
# ---------------------------------------------------------------------------
PORT_ANTHROPIC = 18108
anth_bodies = []


class AnthropicGateway(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def do_POST(self):
        length = int(self.headers.get("content-length", 0) or 0)
        raw = self.rfile.read(length).decode() if length else ""
        try:
            body = json.loads(raw)
        except Exception:
            body = {"_raw": raw}
        anth_bodies.append(body)
        has_tool_result = any(
            blk.get("type") == "tool_result"
            for m in body.get("messages", [])
            for blk in (m.get("content") if isinstance(m.get("content"), list) else [])
        )
        events = [("message_start", {"type": "message_start", "message": {
            "id": "msg_1", "type": "message", "role": "assistant", "model": "m",
            "content": [], "stop_reason": None,
            "usage": {"input_tokens": 1, "output_tokens": 1}}})]
        if not has_tool_result:
            events += [
                ("content_block_start", {"type": "content_block_start", "index": 0,
                                         "content_block": {"type": "tool_use", "id": "toolu_1",
                                                           "name": TOOL_NAME, "input": {}}}),
                ("content_block_delta", {"type": "content_block_delta", "index": 0,
                                         "delta": {"type": "input_json_delta",
                                                   "partial_json": json.dumps({"q": "看图"})}}),
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


httpd2 = HTTPServer(("127.0.0.1", PORT_ANTHROPIC), AnthropicGateway)
threading.Thread(target=httpd2.serve_forever, daemon=True).start()

env2 = dict(os.environ)
env2.update({
    "BUZZ_AGENT_PROVIDER": "anthropic",
    "ANTHROPIC_BASE_URL": f"http://127.0.0.1:{PORT_ANTHROPIC}",
    "ANTHROPIC_MODEL": "probe-model",
    "ANTHROPIC_API_KEY": "sk-probe",
    "RUST_LOG": "info",
})
for key in ("ABB_AGENT_FAUX_TEXT", "ABB_AGENT_FAUX_TOOL", "OPENAI_COMPAT_MODEL"):
    env2.pop(key, None)

proc2 = subprocess.Popen(BIN, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                         stderr=subprocess.PIPE, env=env2, text=True, bufsize=1)
out2 = []
t0 = time.time()
threading.Thread(target=lambda: [out2.append((time.time() - t0, l.strip())) for l in proc2.stdout],
                 daemon=True).start()


def send2(obj):
    proc2.stdin.write(json.dumps(obj) + "\n")
    proc2.stdin.flush()


def wait2(msg_id, timeout):
    end = time.time() + timeout
    while time.time() < end:
        for elapsed, line in list(out2):
            if f'"id":{msg_id}' in line.replace(" ", ""):
                return elapsed, json.loads(line)
        time.sleep(0.02)
    return None, None


print()
print("=== 场景 2：anthropic —— 图片到不了模型时必须如实交代 ===")
send2({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}})
wait2(1, 10)
send2({"jsonrpc": "2.0", "id": 2, "method": "session/new", "params": {
    "cwd": workdir,
    "mcpServers": [{
        "name": "fakemcp",
        "command": sys.executable,
        "args": [SERVER, "--image-data", IMAGE_BASE64],
        "env": [],
    }],
}})
_, resp2 = wait2(2, 30)
send2({"jsonrpc": "2.0", "id": 3, "method": "session/prompt", "params": {
    "sessionId": (resp2 or {}).get("result", {}).get("sessionId", "abb-1"),
    "prompt": [{"type": "text", "text": "看图"}],
}})
wait2(3, 60)
time.sleep(0.5)
try:
    proc2.stdin.close()
except Exception:
    pass
time.sleep(0.3)
proc2.kill()

print(f"假 anthropic 端点收到 {len(anth_bodies)} 次请求")
check("工具确实被调用了（有第二跳请求）", len(anth_bodies) >= 2, f"{len(anth_bodies)} 次")
if len(anth_bodies) >= 2:
    second = json.dumps(anth_bodies[1], ensure_ascii=False)
    check("没有 `(see attached image)` 这种「指着一张不存在的图」的占位",
          "(see attached image)" not in second)
    check("给出如实说明（image 未传给模型）",
          "image 未传给模型" in second,
          "anthropic 的 agent 层搬不动工具结果图片，必须自己交代")

print()
if failures:
    print(f"⇒ 判定：❌ 失败 {len(failures)} 项：{failures}")
    sys.exit(1)
print("⇒ 判定：✅ openai 路径图片真到模型；anthropic 路径如实交代（无与事实不符的占位）")
