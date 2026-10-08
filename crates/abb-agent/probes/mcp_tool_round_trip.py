#!/usr/bin/env python3
"""端到端：`session/new` 的 mcpServers → MCP 工具 → 结果回灌。

这是刀 1 的核心验收。与其它探针的差别：它证明的是**工具真的被调用过**，而不只是
「回合回了文本」——判据是假 MCP server 写下的调用记录（`--record` 文件），
因为从 agent 的文本输出无法区分「工具被调用」与「模型自己编了答案」。

链路：abb-agent 按 `session/new` 的 `mcpServers` 起假 server → `tools/list` 拿到
`echo_query` → 暴露给模型（限名 `fakemcp__echo_query`）→ faux provider 脚本先发一次
该工具的调用 → 工具经 MCP `tools/call` 真执行 → 结果回灌 → 模型出收官文本。

修前表现：`session/new` 根本不读 params，abb 送的 server 列表被静默丢弃，
`ABB_AGENT_FAUX_TOOL` 指定的工具不存在 ⇒ 回合失败（或被模型当不存在而跳过）。
"""
import json
import os
import subprocess
import sys
import tempfile
import threading
import time

BIN = sys.argv[1]
HERE = os.path.dirname(os.path.abspath(__file__))
SERVER = os.path.join(HERE, "fake_mcp_server.py")

record_path = os.path.join(tempfile.mkdtemp(prefix="abb-mcp-"), "calls.jsonl")

env = dict(os.environ)
env.update({
    # 离线工具通道：先发一次工具调用，再出文本。工具名必须是**限名**形状。
    "ABB_AGENT_FAUX_TOOL": json.dumps(
        {"name": "fakemcp__echo_query", "arguments": {"q": "hello-from-model"}}
    ),
    "ABB_AGENT_FAUX_TEXT": "工具已执行完毕。",
    "RUST_LOG": "info",
})
# 清掉可能干扰的供应商配置（faux 优先，但不留噪声）。
for key in ("BUZZ_AGENT_PROVIDER", "ANTHROPIC_MODEL", "OPENAI_COMPAT_MODEL"):
    env.pop(key, None)

proc = subprocess.Popen(BIN, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                        stderr=subprocess.PIPE, env=env, text=True, bufsize=1)
out = []
t0 = time.time()


def reader():
    for line in proc.stdout:
        out.append((time.time() - t0, line.strip()))


threading.Thread(target=reader, daemon=True).start()


def send(obj):
    proc.stdin.write(json.dumps(obj) + "\n")
    proc.stdin.flush()


def wait_for(pred, timeout):
    end = time.time() + timeout
    while time.time() < end:
        for el, line in list(out):
            if pred(line.replace(" ", "")):
                return el, line
        time.sleep(0.05)
    return None, None


send({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": 2}})
wait_for(lambda l: '"id":1,' in l, 15)

# 关键：mcpServers 通过 session/new 下发（shape 与 src/buzz/acp.rs 的 McpServer 一致）。
send({
    "jsonrpc": "2.0",
    "id": 2,
    "method": "session/new",
    "params": {
        "cwd": os.getcwd(),
        "mcpServers": [{
            "name": "fakemcp",
            "command": sys.executable or "python3",
            "args": [SERVER, "--record", record_path],
            "env": [],
        }],
    },
})
started = time.time()
_, created = wait_for(lambda l: '"sessionId"' in l or '"error"' in l, 40)
if not created:
    print("❌ session/new 未在 40s 内返回（MCP 握手可能卡住）")
    proc.kill()
    sys.exit(1)
created_value = json.loads(created)
if "error" in created_value:
    print(f"❌ session/new 报错：{created_value['error']}")
    proc.kill()
    sys.exit(1)
sid = created_value["result"]["sessionId"]
print(f"[t={time.time()-t0:.1f}s] 会话建立（含 MCP 握手 {time.time()-started:.1f}s）：{sid}")

send({"jsonrpc": "2.0", "id": 3, "method": "session/prompt",
      "params": {"sessionId": sid, "prompt": [{"type": "text", "text": "用工具查一下"}]}})
el, terminal = wait_for(lambda l: '"id":3,' in l, 60)

# 判据 1：假 server 确实收到了 tools/call，且参数是模型给的那份。
calls = []
if os.path.exists(record_path):
    with open(record_path, encoding="utf-8") as fh:
        calls = [json.loads(line) for line in fh if line.strip()]
tool_called = any(c.get("tool") == "echo_query" and c.get("arguments", {}).get("q") == "hello-from-model"
                  for c in calls)
print(f"工具调用记录：{calls if calls else '(空)'}")

# 判据 2：回合正常收尾（工具结果能回灌，模型才能出收官文本）。
chunks = [l for _, l in out if '"sessionUpdate":"agent_message_chunk"' in l.replace(" ", "")]
print(f"回合应答：{terminal}")
print(f"回复文本块：{chunks[0] if chunks else '(无)'}")

ok = (tool_called and terminal and '"stopReason":"end_turn"' in terminal.replace(" ", ""))
print(f"\n⇒ 判定：{'MCP 工具往返可用 ✅' if ok else '❌ 有断言未满足'}"
      f"（工具被真调用={tool_called}；回合 end_turn={bool(terminal) and 'end_turn' in terminal}）")

proc.stdin.close()
try:
    err = proc.stderr.read()
except Exception:
    err = ""
try:
    proc.wait(timeout=5)
except subprocess.TimeoutExpired:
    proc.kill()
print("--- agent stderr（应能看到 MCP server 已连接与工具数）---")
print("\n".join(l for l in err.splitlines() if l.strip())[:1200])
