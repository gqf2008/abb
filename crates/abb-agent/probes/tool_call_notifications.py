#!/usr/bin/env python3
"""端到端：工具调用通知真的发出来了（`session/update` 的 `tool_call` / `tool_call_update`）。

判据是 **abb-agent 自己的 stdout**——那是 abb（ACP 客户端）会收到的东西。参照物
（`crates/buzz-agent/src/agent.rs`）有四个发射点，缺了它们 abb 的 `acp::tool` 日志与 UI
就是空的。

三个场景：
1. 成功的工具调用（`dev__write` 写一个文件）⇒ 必须看到 `tool_call`（`status: pending`、
   `title` 是**限定名**、带 `rawInput`）与 `tool_call_update`（`in_progress` → `completed`，
   完成那条带 `content` 与 `rawOutput.isError: false`）；
2. **失败**的工具调用（写工作区外 ⇒ 被拒）⇒ 收到 `failed` + `rawOutput.error`；
3. MCP 工具同样有通知（证明不是只给内置工具打的补丁）。

用法：`python3 tool_call_notifications.py <abb-agent 二进制>`
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

failures = []


def check(label, ok, detail=""):
    print(f"  {'✅' if ok else '❌'} {label}{('：' + detail) if detail else ''}")
    if not ok:
        failures.append(label)


def run_turn(workspace, tool_call, extra_env=None, mcp=None, timeout=40):
    """跑一个回合，返回从 stdout 抓到的所有 `session/update` 载荷。"""
    env = dict(os.environ)
    env.update({
        "ABB_AGENT_FAUX_TOOL": json.dumps(tool_call),
        "ABB_AGENT_FAUX_TEXT": "回合结束",
        "RUST_LOG": "info",
    })
    for key in ("ANTHROPIC_MODEL", "OPENAI_COMPAT_MODEL", "BUZZ_AGENT_PROVIDER"):
        env.pop(key, None)
    env.update(extra_env or {})

    proc = subprocess.Popen(BIN, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                            stderr=subprocess.PIPE, env=env, text=True, bufsize=1)
    lines = []
    t0 = time.time()
    threading.Thread(target=lambda: [lines.append((time.time() - t0, l.strip()))
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
            for _, line in list(lines):
                if f'"id":{msg_id}' in line.replace(" ", ""):
                    return line
            time.sleep(0.02)
        return None

    send({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}})
    wait(1, 10)
    send({"jsonrpc": "2.0", "id": 2, "method": "session/new",
          "params": {"cwd": workspace, "mcpServers": mcp or []}})
    wait(2, 20)
    send({"jsonrpc": "2.0", "id": 3, "method": "session/prompt",
          "params": {"sessionId": "abb-1", "prompt": [{"type": "text", "text": "干活"}]}})
    wait(3, timeout)
    try:
        proc.stdin.close()
    except Exception:
        pass
    time.sleep(0.3)
    proc.kill()

    updates = []
    for _, line in lines:
        if '"session/update"' not in line.replace(" ", ""):
            continue
        try:
            payload = json.loads(line)
        except json.JSONDecodeError:
            continue
        update = payload.get("params", {}).get("update")
        if isinstance(update, dict):
            updates.append(update)
    return updates


def find(updates, kind, **fields):
    for update in updates:
        if update.get("sessionUpdate") != kind:
            continue
        if all(update.get(key) == value for key, value in fields.items()):
            return update
    return None


print("=== 场景 1：成功的工具调用（dev__write）===")
workspace = tempfile.mkdtemp(prefix="abb-toolcall-ok-")
updates = run_turn(workspace, {
    "name": "dev__write",
    "arguments": {"path": "out.txt", "content": "TOOLCALL-MARKER"},
})
started = find(updates, "tool_call")
check("收到 tool_call 通知", started is not None, f"实际收到：{[u.get('sessionUpdate') for u in updates]}")
if started:
    check("title 是模型看到的限定名", started.get("title") == "dev__write", str(started.get("title")))
    check("status = pending", started.get("status") == "pending")
    check("kind = other（与参照物同）", started.get("kind") == "other")
    check("带 rawInput（入参可见）", isinstance(started.get("rawInput"), dict), str(started.get("rawInput")))
check("收到 in_progress", find(updates, "tool_call_update", status="in_progress") is not None)
completed = find(updates, "tool_call_update", status="completed")
check("收到 completed", completed is not None)
if completed:
    check("completed 带 rawOutput.isError=false",
          completed.get("rawOutput", {}).get("isError") is False, str(completed.get("rawOutput")))
    content = completed.get("content")
    check("completed 的 content 是参照物的嵌套形状",
          isinstance(content, list) and content and content[0].get("type") == "content",
          str(content))
check("文件真的落盘（通知不是空承诺）", os.path.isfile(os.path.join(workspace, "out.txt")))

print()
print("=== 场景 2：失败的工具调用（写出工作区 ⇒ 被拒）===")
workspace = tempfile.mkdtemp(prefix="abb-toolcall-fail-")
updates = run_turn(workspace, {
    "name": "dev__write",
    "arguments": {"path": "../escape.txt", "content": "X"},
})
failed = find(updates, "tool_call_update", status="failed")
check("收到 failed 通知", failed is not None, f"实际：{[u.get('status') for u in updates]}")
if failed:
    check("failed 带 rawOutput.error", isinstance(failed.get("rawOutput", {}).get("error"), str),
          str(failed.get("rawOutput"))[:120])

print()
print("=== 场景 3：MCP 工具同样有通知 ===")
workspace = tempfile.mkdtemp(prefix="abb-toolcall-mcp-")
record = os.path.join(workspace, "calls.jsonl")
updates = run_turn(
    workspace,
    {"name": "fakemcp__echo_query", "arguments": {"q": "hi"}},
    mcp=[{
        "name": "fakemcp",
        "command": sys.executable,
        "args": [SERVER, "--record", record],
        "env": [],
    }],
)
started = find(updates, "tool_call")
check("MCP 工具也发 tool_call", started is not None)
if started:
    check("title 是 MCP 限定名", started.get("title") == "fakemcp__echo_query", str(started.get("title")))
check("MCP 工具也发 completed", find(updates, "tool_call_update", status="completed") is not None)
check("假 server 真被调用过（记录文件非空）",
      os.path.isfile(record) and os.path.getsize(record) > 0)

print()
if failures:
    print(f"⇒ 判定：❌ 失败 {len(failures)} 项：{failures}")
    sys.exit(1)
print("⇒ 判定：✅ 工具调用通知（pending→in_progress→completed/failed）真发出，内置与 MCP 工具都有")
