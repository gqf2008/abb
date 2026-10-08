#!/usr/bin/env python3
"""端到端：约定链真的进了模型请求，且 `BUZZ_AGENT_NO_HINTS=1` 真的把它关掉。

为什么这条必须有：abb 对**授权者（granted）**会话只能在**进程级**收口约定链
（fork 的 hints 发生在 `session/new` 之前，per-session `_meta` 管不到 —— 见
`src/service.rs:180-234`，它给 granted 的 agent 进程多塞一个 `BUZZ_AGENT_NO_HINTS=1`）。
所以 abb-agent 不认这个变量 = **owner 的 `~/AGENTS.md` 会静默泄漏给授权者**。
判据是**假 anthropic 端点收到的请求体里的 `system` 字段**，不是读代码。

三个场景（同一进程两份二进制调用、完全隔离的 HOME 与会话目录）：
1. 默认：会话 cwd 的 `AGENTS.md` + `$HOME/AGENTS.md`（全局层）都必须出现在 `system` 里；
2. `BUZZ_AGENT_NO_HINTS=1`：两个标记都**不得**出现（且默认系统提示仍在——是关约定链，
   不是把提示也弄丢）；
3. 没有任何 `AGENTS.md`：`system` 就是默认提示，不出现空标题之类的噪声。

用法：`python3 hints_and_no_hints.py <abb-agent 二进制>`
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
PORT = 18109
CWD_MARKER = "CWD-HINTS-KNOWN-MARKER-81c4"
HOME_MARKER = "HOME-HINTS-KNOWN-MARKER-5f27"
bodies = []


class Gateway(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def do_POST(self):
        length = int(self.headers.get("content-length", 0) or 0)
        raw = self.rfile.read(length).decode() if length else ""
        try:
            bodies.append(json.loads(raw))
        except Exception:
            bodies.append({"_raw": raw})
        events = [
            ("message_start", {"type": "message_start", "message": {
                "id": "msg_1", "type": "message", "role": "assistant", "model": "m",
                "content": [], "stop_reason": None,
                "usage": {"input_tokens": 1, "output_tokens": 1}}}),
            ("content_block_start", {"type": "content_block_start", "index": 0,
                                     "content_block": {"type": "text", "text": ""}}),
            ("content_block_delta", {"type": "content_block_delta", "index": 0,
                                     "delta": {"type": "text_delta", "text": "收到"}}),
            ("content_block_stop", {"type": "content_block_stop", "index": 0}),
            ("message_delta", {"type": "message_delta",
                               "delta": {"stop_reason": "end_turn", "stop_sequence": None},
                               "usage": {"output_tokens": 1}}),
            ("message_stop", {"type": "message_stop"}),
        ]
        payload = "".join(f"event: {t}\ndata: {json.dumps(d)}\n\n" for t, d in events)
        data = payload.encode()
        self.send_response(200)
        self.send_header("content-type", "text/event-stream")
        self.send_header("content-length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def log_message(self, *args):
        pass


threading.Thread(target=HTTPServer(("127.0.0.1", PORT), Gateway).serve_forever, daemon=True).start()

failures = []


def check(label, ok, detail=""):
    print(f"  {'✅' if ok else '❌'} {label}{('：' + detail) if detail else ''}")
    if not ok:
        failures.append(label)


def system_text(body):
    """把 anthropic 请求体里的 system 字段压成一段文本。"""
    system = body.get("system")
    if isinstance(system, str):
        return system
    if isinstance(system, list):
        return "\n".join(
            blk.get("text", "") for blk in system if isinstance(blk, dict)
        )
    return ""


rc = {"last": None}


def run_turn(cwd, home, extra_env):
    """起一个 abb-agent，跑一次 initialize/session/new/prompt。

    返回该回合的请求体（没有则 None）；进程退出码见 `rc["last"]`（配置错误场景用）。
    """
    before = len(bodies)
    env = dict(os.environ)
    env.update({
        "BUZZ_AGENT_PROVIDER": "anthropic",
        "ANTHROPIC_BASE_URL": f"http://127.0.0.1:{PORT}",
        "ANTHROPIC_MODEL": "probe-model",
        "ANTHROPIC_API_KEY": "sk-probe",
        "HOME": home,
        "USERPROFILE": home,   # Windows 的 home 读法
        "RUST_LOG": "info",
    })
    for key in ("ABB_AGENT_FAUX_TEXT", "ABB_AGENT_FAUX_TOOL", "OPENAI_COMPAT_MODEL"):
        env.pop(key, None)
    env.update(extra_env)

    proc = subprocess.Popen(BIN, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                            stderr=subprocess.PIPE, env=env, text=True, bufsize=1)
    out = []
    t0 = time.time()
    threading.Thread(
        target=lambda: [out.append((time.time() - t0, l.strip())) for l in proc.stdout],
        daemon=True,
    ).start()

    def send(obj):
        """写一行；进程已退出（配置错误场景）时返回 False 而不是抛 BrokenPipe。"""
        try:
            proc.stdin.write(json.dumps(obj) + "\n")
            proc.stdin.flush()
            return True
        except (BrokenPipeError, OSError, ValueError):
            return False

    def wait(msg_id, timeout):
        end = time.time() + timeout
        while time.time() < end:
            for elapsed, line in list(out):
                if f'"id":{msg_id}' in line.replace(" ", ""):
                    return elapsed, json.loads(line)
            time.sleep(0.02)
        return None, None

    if send({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}}):
        wait(1, 10)
        if send({"jsonrpc": "2.0", "id": 2, "method": "session/new",
                 "params": {"cwd": cwd, "mcpServers": []}}):
            _, resp = wait(2, 20)
            session_id = (resp or {}).get("result", {}).get("sessionId", "abb-1")
            send({"jsonrpc": "2.0", "id": 3, "method": "session/prompt",
                  "params": {"sessionId": session_id,
                             "prompt": [{"type": "text", "text": "你好"}]}})
            wait(3, 30)
    try:
        proc.stdin.close()
    except Exception:
        pass
    try:
        code = proc.wait(timeout=5)
    except Exception:
        proc.kill()
        code = None
    rc["last"] = code
    body = bodies[before] if len(bodies) > before else None
    return body


def make_cwd(with_agents_md):
    cwd = tempfile.mkdtemp(prefix="abb-hints-cwd-")
    if with_agents_md:
        with open(os.path.join(cwd, "AGENTS.md"), "w", encoding="utf-8") as fh:
            fh.write(f"# cwd 约定\n{CWD_MARKER}\n")
    return cwd


def make_home(with_agents_md):
    home = tempfile.mkdtemp(prefix="abb-hints-home-")
    if with_agents_md:
        with open(os.path.join(home, "AGENTS.md"), "w", encoding="utf-8") as fh:
            fh.write(f"# 全局约定\n{HOME_MARKER}\n")
    return home


print("=== 场景 1：默认（约定链开）===")
body = run_turn(make_cwd(True), make_home(True), {})
if not body:
    check("回合请求到达假端点", False)
else:
    system = system_text(body)
    check("会话目录的 AGENTS.md 进了 system", CWD_MARKER in system)
    check("$HOME/AGENTS.md（全局层）也进了 system", HOME_MARKER in system)
    check("全局层在会话层之前（fork 同序）",
          system.find(HOME_MARKER) < system.find(CWD_MARKER))
    check("默认系统提示仍在", "ABB（agent-bridge）的执行层 agent" in system,
          "abb 未下发 systemPrompt 时应回落默认提示")

print()
print("=== 场景 2：BUZZ_AGENT_NO_HINTS=1（granted 会话的收口）===")
body = run_turn(make_cwd(True), make_home(True), {"BUZZ_AGENT_NO_HINTS": "1"})
if not body:
    check("回合请求到达假端点", False)
else:
    system = system_text(body)
    check("会话约定不得泄漏", CWD_MARKER not in system)
    check("全局约定不得泄漏（owner 的 ~/AGENTS.md）", HOME_MARKER not in system)
    check("默认系统提示仍在（关的是约定链，不是提示）",
          "ABB（agent-bridge）的执行层 agent" in system)

print()
print("=== 场景 3：没有任何 AGENTS.md ===")
body = run_turn(make_cwd(False), make_home(False), {})
if not body:
    check("回合请求到达假端点", False)
else:
    system = system_text(body)
    check("不出现空标题/噪声",
          "# Additional Instructions" not in system and "Project Hints" not in system)
    check("默认系统提示仍在", "ABB（agent-bridge）的执行层 agent" in system)

print()
print("=== 场景 4：读法对齐 fork（非零即关）===")
body = run_turn(make_cwd(True), make_home(True), {"BUZZ_AGENT_NO_HINTS": "2"})
if not body:
    check("回合请求到达假端点", False)
else:
    system = system_text(body)
    check("NO_HINTS=2 也判为关闭（fork 是 parse u8 + 非零即关）",
          CWD_MARKER not in system and HOME_MARKER not in system)
    check("默认系统提示仍在", "ABB（agent-bridge）的执行层 agent" in system)

print()
print("=== 场景 5：读不懂的取值必须响亮失败（不能静默当「没关」）===")
for bad in ("true", "yes", "-1", "256", " 1 "):
    body = run_turn(make_cwd(True), make_home(True), {"BUZZ_AGENT_NO_HINTS": bad})
    check(f"NO_HINTS={bad!r} 时不发任何模型请求（约定不可能外泄）", body is None)
    check(f"NO_HINTS={bad!r} 时进程以 2 退出（与 fork 的 die() 同款）",
          rc["last"] == 2, f"rc={rc['last']}")

print()
print("=== 场景 6：空 cwd 不得向祖先链扩散（钉住 acp 的接线处）===")
# 进程 cwd 设成 git 子目录；根与 cwd 各有一份 AGENTS.md。空 cwd 时只该读到 cwd 那一层。
root = tempfile.mkdtemp(prefix="abb-hints-root-")
os.makedirs(os.path.join(root, ".git"), exist_ok=True)
inner = os.path.join(root, "inner")
os.makedirs(inner, exist_ok=True)
ROOT_MARKER = "ROOT-LAYER-MARKER-b7e2"
INNER_MARKER = "INNER-LAYER-MARKER-3d19"
with open(os.path.join(root, "AGENTS.md"), "w", encoding="utf-8") as fh:
    fh.write(f"{ROOT_MARKER}\n")
with open(os.path.join(inner, "AGENTS.md"), "w", encoding="utf-8") as fh:
    fh.write(f"{INNER_MARKER}\n")

home = make_home(False)
previous = os.getcwd()
os.chdir(inner)
try:
    body = run_turn("", home, {})
finally:
    os.chdir(previous)
if not body:
    check("回合请求到达假端点（空 cwd + 进程 cwd 在 git 子目录）", False)
else:
    system = system_text(body)
    check("空 cwd 时读到进程工作目录那一层", INNER_MARKER in system)
    check("空 cwd 时**不得**向祖先链扩散（读不到 git 根的 AGENTS.md）",
          ROOT_MARKER not in system,
          "根层标记是否出现在请求体里")
    check("空 cwd 时也不读 $HOME（home 未给约定）", HOME_MARKER not in system)

print()
if failures:
    print(f"⇒ 判定：❌ 失败 {len(failures)} 项：{failures}")
    sys.exit(1)
print("⇒ 判定：✅ 约定链真进请求体；NO_HINTS=1 时既不加载会话约定也不加载 ~/AGENTS.md")
