#!/usr/bin/env python3
"""插入（`_session/steering`）的端到端验收（卡 `abb-agent-project-steering-20261015`）。

判据不是「代码里有 steer 分支」，而是**真二进制真的把一条插入的消息送进了在途回合**：

假 OpenAI 网关按回合脚本回答——**第 1 跳**先流一小片正文，然后**卡住**（等探针写下的
「steer 已回执」标记文件，带超时），再收尾；**第 2 跳**回第二段。于是：

  1. `_session/steering` 必须回 `{"outcome":"injected"}`（abb 只认这个取值，回空 `{}` 会被
     当拒收、释放扣住的事件走 cancel+merge 回退）；
  2. **同一回合内模型被再叫了一次**：网关收到 **2 跳**请求，且**第 2 跳的请求体里带着插入的文本**
     （`messages` 里出现探针那句话）——这才是「插入」与「另起一轮」的分水岭：另起一轮会多一条
     `session/prompt` 应答，而这里**只有 1 条**、`stopReason` 仍是 `end_turn`；
  3. 两段正文都作为 `agent_message_chunk` 流出（第 2 段只有在 steering 被抽干、消息进上下文之后
     才可能出现）；
  4. 反面：**没有在途回合**时，同一条请求必须**响亮回错**（不是 `injected`、也不是空成功），
     并且**一个模型请求都不发**（否则 abb 会以为插进去了）。

「网关卡住等标记」这条握手是故意的：探针只看 abb-agent 的 stdout 无法知道回合是否在流
（正文是**整条**投影的，第一片正文出现时消息已经流完了），所以同步点放在网关侧——它才是
「模型正在吐字」的真实现场。
"""
import json
import os
import subprocess
import sys
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

BIN = sys.argv[1]
MODE = {"value": "in-flight"}  # in-flight | no-turn
FIRST = "第一段：我先慢慢说一遍，把条件列清楚。"
SECOND = "第二段：收到你的插话，我改一下方向。"
STEER_TEXT = "插一句话：请在下一条消息里说「收到」。"
MARKER = f"/tmp/abb-steer-probe-{os.getpid()}.ack"

posts = []          # 每跳的请求体（dict）
turn1_streaming = threading.Event()  # 网关第 1 跳已经吐了第一片、正卡着等握手


def sse(delta, finish=None):
    obj = {"id": "chatcmpl-probe", "object": "chat.completion.chunk", "created": 1,
           "model": "probe-model",
           "choices": [{"index": 0, "delta": delta, "finish_reason": finish}]}
    return f"data: {json.dumps(obj)}\n\n"


class Gateway(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def do_POST(self):
        length = int(self.headers.get("content-length", 0) or 0)
        raw = self.rfile.read(length) if length else b""
        try:
            body = json.loads(raw.decode("utf-8"))
        except Exception:
            body = {"_raw": raw.decode("utf-8", "replace")}
        posts.append(body)
        hop = len(posts)

        if hop == 1:
            # 第 1 片先发出去，再**卡住**等探针写完握手标记（<=10s）。
            self.send_response(200)
            self.send_header("content-type", "text/event-stream")
            self.send_header("connection", "close")
            self.end_headers()
            self.wfile.write(sse({"content": FIRST}).encode())
            self.wfile.flush()
            turn1_streaming.set()
            deadline = time.time() + 10
            while not os.path.exists(MARKER) and time.time() < deadline:
                time.sleep(0.05)
            body_out = (sse({}, finish="stop") + "data: [DONE]\n\n").encode()
        else:
            self.send_response(200)
            self.send_header("content-type", "text/event-stream")
            self.send_header("connection", "close")
            self.end_headers()
            body_out = (sse({"content": SECOND}) + sse({}, finish="stop")
                        + "data: [DONE]\n\n").encode()
        try:
            self.wfile.write(body_out)
            self.wfile.flush()
        except BrokenPipeError:
            pass

    def log_message(self, *args):
        pass


httpd = ThreadingHTTPServer(("127.0.0.1", 0), Gateway)
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


class Agent:
    """一个真 abb-agent 进程 + 逐行读 stdout。"""

    def __init__(self):
        self.proc = subprocess.Popen(BIN, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                     stderr=subprocess.PIPE, env=env, text=True, bufsize=1)
        self.lines = []
        threading.Thread(target=self._read, daemon=True).start()

    def _read(self):
        for line in self.proc.stdout:
            self.lines.append(line.strip())

    def send(self, obj):
        self.proc.stdin.write(json.dumps(obj) + "\n")
        self.proc.stdin.flush()

    def wait(self, pred, timeout):
        end = time.time() + timeout
        while time.time() < end:
            for line in list(self.lines):
                if pred(line.replace(" ", "")):
                    return json.loads(line)
            time.sleep(0.02)
        return None

    def close(self):
        try:
            self.proc.stdin.close()
        except Exception:
            pass
        try:
            self.proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            self.proc.kill()

    def stderr(self):
        try:
            return self.proc.stderr.read()
        except Exception:
            return ""


def texts_of(agent):
    out = []
    for line in agent.lines:
        if '"sessionUpdate":"agent_message_chunk"' in line.replace(" ", ""):
            try:
                out.append(json.loads(line)["params"]["update"]["content"]["text"])
            except Exception:
                pass
    return out


failures = []

# ---- 场景 1：在途回合里插入 ⇒ 必须 injected、第 2 跳带着插入文本、只有 1 条回合应答 ----
agent = Agent()
agent.send({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": 2}})
init = agent.wait(lambda l: '"id":1,' in l, 10)
if not init:
    failures.append("initialize 没有应答")
elif not init.get("result", {}).get("_meta", {}).get("steering", {}).get("supported"):
    failures.append(f"initialize 未声明 steering.supported=true：{init}")

agent.send({"jsonrpc": "2.0", "id": 2, "method": "session/new", "params": {}})
created = agent.wait(lambda l: '"sessionId"' in l, 10)
sid = created["result"]["sessionId"] if created else None
if not sid:
    failures.append("session/new 没有给出 sessionId")

agent.send({"jsonrpc": "2.0", "id": 3, "method": "session/prompt",
            "params": {"sessionId": sid, "prompt": [{"type": "text", "text": "你好"}]}})

if not turn1_streaming.wait(15):
    failures.append("网关第 1 跳没被调用（回合没跑起来）")
else:
    # 同步点：模型正在吐字（网关卡在第 1 跳）。
    agent.send({"jsonrpc": "2.0", "id": 4, "method": "_session/steering",
                "params": {"sessionId": sid, "prompt": [{"type": "text", "text": STEER_TEXT}]}})
    steer = agent.wait(lambda l: '"id":4,' in l, 10)
    if not steer:
        failures.append("steer 请求没有应答")
    elif steer.get("result", {}).get("outcome") != "injected":
        failures.append(f"steer 未回 injected：{steer}")
    # 写握手标记 ⇒ 放网关收尾第 1 跳。
    with open(MARKER, "w") as f:
        f.write("ok\n")

prompt_ack = agent.wait(lambda l: '"id":3,' in l, 30)
if not prompt_ack:
    failures.append("回合没有在 30s 内给出应答")
elif prompt_ack.get("result", {}).get("stopReason") != "end_turn":
    failures.append(f"回合应答不是 end_turn：{prompt_ack}")

streamed = "".join(texts_of(agent))
print(f"[in-flight] 模型跳数 = {len(posts)}；流出正文 = {streamed!r}")
if len(posts) != 2:
    failures.append(f"应当恰好 2 跳模型请求（第 2 跳是插入触发的），实际 {len(posts)} 跳")
if len(posts) >= 2:
    second_body = json.dumps(posts[1], ensure_ascii=False)
    if STEER_TEXT not in second_body:
        failures.append("第 2 跳请求体里没有插入的文本（消息没进模型上下文）")
if FIRST not in streamed:
    failures.append(f"第一段没流出：{streamed!r}")
if SECOND not in streamed:
    failures.append(f"第二段没流出（插入没生效）：{streamed!r}")
acks = [l for l in agent.lines if '"id":3,' in l.replace(" ", "")]
if len(acks) != 1:
    failures.append(f"回合应答应恰好 1 条（插入不是另起一轮），实际 {len(acks)} 条：{acks}")
agent.close()

# ---- 场景 2（反面）：没有在途回合 ⇒ 必须响亮回错，且不产生任何模型请求 ----
MODE["value"] = "no-turn"
before = len(posts)
agent2 = Agent()
agent2.send({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": 2}})
agent2.wait(lambda l: '"id":1,' in l, 10)
agent2.send({"jsonrpc": "2.0", "id": 2, "method": "session/new", "params": {}})
created2 = agent2.wait(lambda l: '"sessionId"' in l, 10)
sid2 = created2["result"]["sessionId"] if created2 else None
agent2.send({"jsonrpc": "2.0", "id": 3, "method": "_session/steering",
             "params": {"sessionId": sid2, "prompt": [{"type": "text", "text": STEER_TEXT}]}})
steer2 = agent2.wait(lambda l: '"id":3,' in l, 10)
print(f"[no-turn] 应答 = {json.dumps(steer2, ensure_ascii=False) if steer2 else None}")
if not steer2:
    failures.append("无在途回合时 steer 没有应答")
else:
    if steer2.get("result") is not None:
        failures.append(f"无在途回合不得回成功（abb 会当已投递）：{steer2}")
    if steer2.get("error", {}).get("code") != -32600:
        failures.append(f"无在途回合应回 -32600，实际：{steer2}")
if len(posts) != before:
    failures.append("无在途回合的 steer 不得触发模型请求")
agent2.close()

if failures:
    print("\n⇒ 判定：❌ 有断言未满足")
    for f in failures:
        print("   -", f)
    err = agent.stderr() if 'agent' in dir() else ""
    print("--- agent stderr（截断）---")
    print("\n".join(l for l in err.splitlines() if l.strip())[:600])
    sys.exit(1)
print("\n⇒ 判定：插入端到端可见 ✅（injected + 同回合第 2 跳带插入文本 + 仅 1 条回合应答 + 无在途回合响亮回错）")
httpd.shutdown()
