#!/usr/bin/env python3
"""思考块可见性的端到端验收（卡 abb-syschat-thinking-visible-20261015）。

判据不是「代码里有 thinking_updates」，而是**真二进制真的把 provider 的 reasoning 增量
发成 `agent_thought_chunk` 通知**：假 OpenAI 网关按 SSE 先流 `reasoning_content` 增量、
再流 `content`，断言

  1. abb-agent 发出 `sessionUpdate":"agent_thought_chunk"`，各片**拼接后**与网关的 reasoning 逐字一致
     （rpi 侧会把 `pending_thinking` 批合并：满 64 字节、或**再来一片且距上次发出超过 40ms**
     才 flush —— 注意它没有定时器，最后一片靠边界 flush。所以片数由输入长度/节奏决定，
     但每片只带**新增**文本；拼接检查同时守「不重复、不丢字」）；
  2. 思考**先于**正文到达（顺序不能反——根侧按到达顺序分段到 ChunkKind::Thought / Text）；
  3. 反面：只流 `content` 时**一条** thought chunk 都不许出现（否则正文会被当成思考）。

网关**故意分两次写、中间隔 300ms**（> rpi 的 40ms 批窗）：这样能看到真的两片，而不是
「一次读完、边界处合成一片」——只测合并片会漏掉「每片带增量」这条语义。

`reasoning_content` 是 rpi-ai 认的 DeepSeek 风格字段（另两个别名 `reasoning` / `reasoning_text`
在 rpi 侧同一分支，见 rpi-ai providers/openai_completions.rs）。这条探针走的就是 abb 的
`openai-chat` → `ABB_AGENT_PROVIDER=openai` 真实路径。
"""
import json
import os
import subprocess
import sys
import threading
import time
from http.server import BaseHTTPRequestHandler, HTTPServer

BIN = sys.argv[1]
# 第一片刻意 > 64 字节（中文 3 字节/字 ⇒ 22+ 字）：rpi 的批阈值满 64 字节即发，
# 于是它单独成第一片；第二片在 300ms 后到达（> 40ms 批窗）⇒ 单独成第二片。
# 两次都短于 64 字节的输入会被合成为一片（上游批合并，不是本包的 bug）。
THOUGHTS = ["让我想想……先看用户问了什么，再决定怎么回答，别急着下结论。", "嗯，就这样答。"]
REPLY = "想好了：你好。"
mode = {"value": "thinking"}  # thinking | text-only


def sse(delta, finish=None):
    obj = {"id": "chatcmpl-probe", "object": "chat.completion.chunk", "created": 1,
           "model": "probe-model",
           "choices": [{"index": 0, "delta": delta, "finish_reason": finish}]}
    return f"data: {json.dumps(obj)}\n\n"


class Gateway(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def do_POST(self):
        length = int(self.headers.get("content-length", 0) or 0)
        if length:
            self.rfile.read(length)
        # 逐片（每片一个完整 SSE 事件）写、中间 sleep：content-length 提前算好，
        # 客户端就能**流式**读到前面那几片，而不是等连接关掉再一次性喂给 SSE 解析器
        # ——后者会把 300ms 的间隔抹平，测出来永远只有一片。
        pieces = []
        if mode["value"] == "thinking":
            for t in THOUGHTS:
                pieces.append((sse({"reasoning_content": t}), 0.3))
        pieces.append((sse({"content": REPLY}), 0.0))
        pieces.append((sse({}, finish="stop"), 0.0))
        pieces.append(("data: [DONE]\n\n", 0.0))
        body = "".join(p for p, _ in pieces).encode()
        self.send_response(200)
        self.send_header("content-type", "text/event-stream")
        self.send_header("content-length", str(len(body)))
        self.end_headers()
        try:
            for payload, pause in pieces:
                self.wfile.write(payload.encode())
                self.wfile.flush()
                if pause:
                    time.sleep(pause)  # > rpi 的 40ms 批窗 ⇒ 两片应各自成一条通知
        except BrokenPipeError:
            pass

    def log_message(self, *args):
        pass


httpd = HTTPServer(("127.0.0.1", 0), Gateway)
port = httpd.server_address[1]
threading.Thread(target=httpd.serve_forever, daemon=True).start()

env = dict(os.environ)
env.update({
    "ABB_AGENT_PROVIDER": "openai",
    "OPENAI_COMPAT_BASE_URL": f"http://127.0.0.1:{port}/v1",
    "OPENAI_COMPAT_MODEL": "probe-model",
    "OPENAI_COMPAT_API_KEY": "sk-probe-gateway-key",
    "OPENAI_COMPAT_API": "chat",
    "RUST_LOG": "info",
})


def run_round(label):
    proc = subprocess.Popen(BIN, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                            stderr=subprocess.PIPE, env=env, text=True, bufsize=1)
    events = []

    def reader():
        for line in proc.stdout:
            events.append((time.time(), line.strip()))

    threading.Thread(target=reader, daemon=True).start()

    def send(obj):
        proc.stdin.write(json.dumps(obj) + "\n")
        proc.stdin.flush()

    def wait_for(pred, timeout):
        end = time.time() + timeout
        while time.time() < end:
            for ts, line in list(events):
                if pred(line.replace(" ", "")):
                    return ts, line
            time.sleep(0.05)
        return None, None

    send({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": 2}})
    wait_for(lambda l: '"id":1,' in l, 10)
    send({"jsonrpc": "2.0", "id": 2, "method": "session/new", "params": {}})
    _, created = wait_for(lambda l: '"sessionId"' in l, 10)
    sid = json.loads(created)["result"]["sessionId"] if created else None
    send({"jsonrpc": "2.0", "id": 3, "method": "session/prompt",
          "params": {"sessionId": sid, "prompt": [{"type": "text", "text": "你好"}]}})
    wait_for(lambda l: '"id":3,' in l, 60)

    proc.stdin.close()
    try:
        err = proc.stderr.read()
    except Exception:
        err = ""
    try:
        proc.wait(timeout=5)
    except subprocess.TimeoutExpired:
        proc.kill()
    return events, err


def notify_texts(events, kind):
    out = []
    for ts, line in events:
        if f'"sessionUpdate":"{kind}"' in line.replace(" ", ""):
            try:
                obj = json.loads(line)
                out.append((ts, obj["params"]["update"]["content"]["text"]))
            except Exception:
                pass
    return out


failures = []

# ---- 场景 1：流 reasoning_content ⇒ 必须发 agent_thought_chunk，且在正文之前 ----
mode["value"] = "thinking"
events, err = run_round("thinking")
thoughts = notify_texts(events, "agent_thought_chunk")
texts = notify_texts(events, "agent_message_chunk")
print(f"[thinking] thought chunks = {[t for _, t in thoughts]}")
print(f"[thinking] text chunks    = {[t for _, t in texts]}")
if "".join(t for _, t in thoughts) != "".join(THOUGHTS):
    failures.append(
        f"思考分片拼接后不等于网关文本：{''.join(t for _, t in thoughts)!r} != {''.join(THOUGHTS)!r}")
if len(thoughts) != len(THOUGHTS):
    failures.append(
        f"本输入应成 {len(THOUGHTS)} 片（首片 >64B 立即发、次片 300ms 后触发 40ms 批窗），实际 {len(thoughts)} 片")
if any(not t for _, t in thoughts):
    failures.append("存在空思考分片（空片只是噪音）")
if not texts or REPLY not in texts[0][1]:
    failures.append(f"正文没到：{texts}")
if thoughts and texts and thoughts[-1][0] > texts[0][0]:
    failures.append("思考分片晚于正文到达（顺序反了）")

# ---- 场景 2（反面）：只流 content ⇒ 一条 thought chunk 都不许有 ----
mode["value"] = "text-only"
events2, err2 = run_round("text-only")
thoughts2 = notify_texts(events2, "agent_thought_chunk")
texts2 = notify_texts(events2, "agent_message_chunk")
print(f"[text-only] thought chunks = {[t for _, t in thoughts2]}")
print(f"[text-only] text chunks    = {[t for _, t in texts2]}")
if thoughts2:
    failures.append(f"纯正文回合不该有思考分片：{thoughts2}")
if not texts2 or REPLY not in texts2[0][1]:
    failures.append("纯正文回合的正文没到")

if failures:
    print("\n⇒ 判定：❌ 有断言未满足")
    for f in failures:
        print("   -", f)
    print("--- agent stderr（截断）---")
    print("\n".join(l for l in err.splitlines() if l.strip())[:600])
    sys.exit(1)
print("\n⇒ 判定：思考块端到端可见 ✅（逐片增量、拼接逐字、先于正文、纯正文回合不误报）")
httpd.shutdown()
