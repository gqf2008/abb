#!/usr/bin/env python3
"""端到端：**多 server** 的资源订阅互不串（评审 F4 —— 本卡 AC 第 4 条的机器判据）。

两个假 MCP server（名字不同、资源 uri 与正文都不同）挂在同一个会话上：
1. 两个 server 都必须收到 `resources/subscribe`（各自的 record 文件里各有一条）；
2. abb-agent 必须为**每个** server 各上报一条 `_abb/unstable/resource/updated`，
   且 `server` / `uri` / `text` 三元组与各自 server 提供的一致（归属于 server 而不是混在一起）。

修前表现：订阅清单/事件只有一个全局池子，两个 server 的事件会互相覆盖或只报一个。
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

tmp = tempfile.mkdtemp(prefix="abb-mcp-multi-")
specs = [
    ("alpha", os.path.join(tmp, "alpha.jsonl"), "mock://alpha", "body-ALPHA"),
    ("beta", os.path.join(tmp, "beta.jsonl"), "mock://beta", "body-BETA"),
]

env = dict(os.environ)
env["RUST_LOG"] = "info"
env["ABB_AGENT_FAUX_TEXT"] = "probe-ok"
env.pop("ABB_AGENT_PROVIDER", None)

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


def wait_for(pred, timeout=25.0, what=""):
    deadline = time.time() + timeout
    while time.time() < deadline:
        for _, line in list(out):
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
                     "name": name,
                     "command": sys.executable,
                     "args": [SERVER, "--record", rec, "--resource", text,
                              "--resource-uri", uri],
                     "env": [],
                 } for name, rec, uri, text in specs]}})
wait_for(lambda m: m.get("id") == 1, what="session/new 应答")

# 两个 server 各上报一条（顺序不定，按 (server, uri) 收齐）
seen = {}

def collect():
    deadline = time.time() + 25
    while time.time() < deadline and len(seen) < 2:
        for _, line in list(out):
            try:
                msg = json.loads(line)
            except json.JSONDecodeError:
                continue
            if msg.get("method") != "_abb/unstable/resource/updated":
                continue
            p = msg.get("params") or {}
            seen[(p.get("server"), p.get("uri"))] = p.get("text")
        time.sleep(0.05)


collect()
proc.kill()

problems = []
for name, rec, uri, text in specs:
    if seen.get((name, uri)) != text:
        problems.append(f"{name} 的上报缺失或内容不符：{seen.get((name, uri))!r} != {text!r}")
    # 协议事实：该 server 真的被订阅过
    recorded = []
    if os.path.exists(rec):
        with open(rec, encoding="utf-8") as fh:
            recorded = [json.loads(l) for l in fh if l.strip()]
    if not any("subscribed" in entry for entry in recorded):
        problems.append(f"{name} 没收到 resources/subscribe")

if problems:
    print("FAIL: " + "; ".join(problems))
    print(f"seen={seen}")
    sys.exit(1)
print(f"OK: 多 server 资源订阅互不串（seen={sorted(seen.keys())}）")
