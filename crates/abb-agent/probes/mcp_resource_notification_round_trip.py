#!/usr/bin/env python3
"""端到端：MCP 资源订阅 → 通知 → 读资源 → 上报 abb（`_abb/unstable/resource/updated`）。

判据（不靠模型文本，靠协议事实）：
1. 假 server 的 record 文件里出现 `subscribed`（abb-agent 真的调了 `resources/subscribe`）；
2. 出现 `resource_read`（收到通知后真的去读了资源）；
3. abb-agent 往 stdout 写了一条 `_abb/unstable/resource/updated` 通知，且 server / uri / text
   与假 server 提供的一致。

修前表现：abb-agent 只做 tools/list，`resources/*` 一概不碰，也没有任何资源通知通道。
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

RESOURCE_TEXT = "resource-body-for-probe"
tmp = tempfile.mkdtemp(prefix="abb-mcp-res-")
record_path = os.path.join(tmp, "calls.jsonl")

env = dict(os.environ)
env["RUST_LOG"] = "info"
# session/new 需要供应商（离线用 faux：本探针不跑模型，只需要会话能建起来）
env["ABB_AGENT_FAUX_TEXT"] = "probe-ok"
for key in ("ABB_AGENT_PROVIDER", "ABB_AGENT_FAUX_TOOL"):
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


def wait_for(pred, timeout=15.0, what=""):
    deadline = time.time() + timeout
    while time.time() < deadline:
        for ts, line in list(out):
            try:
                msg = json.loads(line)
            except json.JSONDecodeError:
                continue
            if pred(msg):
                return msg
        time.sleep(0.05)
    print(f"FAIL: 超时未等到 {what}")
    print("stdout:\n" + "\n".join(f"  {l}" for _, l in out[-25:]))
    proc.kill()
    sys.exit(1)


send({"jsonrpc": "2.0", "id": 0, "method": "initialize",
      "params": {"protocolVersion": 2, "clientCapabilities": {},
                 "clientInfo": {"name": "abb-probe", "version": "0"}}})
wait_for(lambda m: m.get("id") == 0, what="initialize 应答")

send({"jsonrpc": "2.0", "id": 1, "method": "session/new",
      "params": {"cwd": tmp, "systemPrompt": None,
                 "mcpServers": [{
                     "name": "fakemcp",
                     "command": sys.executable,
                     "args": [SERVER, "--record", record_path, "--resource", RESOURCE_TEXT],
                     "env": [],
                 }]}})
wait_for(lambda m: m.get("id") == 1, what="session/new 应答")

# 关键：等 abb-agent 上报资源更新（它内部是 订阅 → 通知 → 读 → 上报 的异步链）
note = wait_for(
    lambda m: m.get("method") == "_abb/unstable/resource/updated",
    timeout=20.0, what="resource/updated 通知",
)
params = note.get("params") or {}
problems = []
if params.get("server") != "fakemcp":
    problems.append(f"server 归属错：{params.get('server')}")
if params.get("uri") != "file:///fake/resource.txt":
    problems.append(f"uri 错：{params.get('uri')}")
if params.get("text") != RESOURCE_TEXT:
    problems.append(f"text 与假 server 提供的不一致：{params.get('text')!r}")

# 协议事实：订阅与读都真的发生过
recorded = []
if os.path.exists(record_path):
    with open(record_path, encoding="utf-8") as fh:
        for line in fh:
            line = line.strip()
            if line:
                recorded.append(json.loads(line))
kinds = [sorted(entry.keys())[0] for entry in recorded]
if "subscribed" not in kinds:
    problems.append("假 server 没收到 resources/subscribe")
if "resource_read" not in kinds:
    problems.append("假 server 没收到 resources/read")

proc.kill()
if problems:
    print("FAIL: " + "; ".join(problems))
    print(f"recorded={recorded}")
    sys.exit(1)
print(f"OK: 资源订阅→通知→读取→上报 全链通过（recorded={kinds}）")
