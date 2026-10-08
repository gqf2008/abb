#!/usr/bin/env python3
"""端到端：`dev__delegate` 真把任务交给本机 CLI 并把它**报告**回灌给模型。

判据分两处：①假 CLI 自己写下的「我被调用了 + 收到的参数」（子进程侧证据）；
②假 anthropic 端点第二跳请求体里出现的报告正文（模型侧证据）。

五个场景：
1. 正常委派（`BUZZ_AGENT_DELEGATE_CLAUDE_BIN` 指向假 CLI）⇒ 第二跳里出现报告标记，
   且假 CLI 记录的参数含 `--` 分隔与 task；
2. CLI 不可用（覆盖指向不存在路径 + PATH 清空）⇒ 工具结果里是**可纠偏**错误（列出可用后端）；
3. 取消（假 CLI 里 sleep）⇒ 回合以 `cancelled` 收尾，且不留下挂死的子进程（可观察回合结束）；
4. **非零退出** ⇒ 工具结果是**错误**（模型侧看到 `tool error:` + exit 码 + 报告正文），不是成功；
5. **孙进程攥着管道**（假 CLI 后台 sleep 后自己退出）⇒ 工具在宽限内返回并标注 `output incomplete`，
   回合**不挂死**。

用法：`python3 delegate_round_trip.py <abb-agent 二进制>`
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
PORT = 18113
REPORT = "DELEGATE-REPORT-MARKER-e71a"
bodies = []
tool_call = {"name": None, "args": None}


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
            "id": "m", "type": "message", "role": "assistant", "model": "m", "content": [],
            "stop_reason": None, "usage": {"input_tokens": 1, "output_tokens": 1}}})]
        if not has_tool_result and tool_call["name"]:
            events += [
                ("content_block_start", {"type": "content_block_start", "index": 0,
                                         "content_block": {"type": "tool_use", "id": "toolu_1",
                                                           "name": tool_call["name"], "input": {}}}),
                ("content_block_delta", {"type": "content_block_delta", "index": 0,
                                         "delta": {"type": "input_json_delta",
                                                   "partial_json": json.dumps(tool_call["args"])}}),
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


threading.Thread(target=HTTPServer(("127.0.0.1", PORT), Gateway).serve_forever, daemon=True).start()
failures = []


def check(label, ok, detail=""):
    print(f"  {'✅' if ok else '❌'} {label}{('：' + detail) if detail else ''}")
    if not ok:
        failures.append(label)


def make_fake_cli(dir_path, sleep_secs=0, exit_code=0, lingering=False, tag="fake-claude"):
    path = os.path.join(dir_path, tag)
    body = "#!/bin/sh\n"
    body += f'echo "argv: $*" >> "{os.path.join(dir_path, "calls.txt")}"\n'
    if lingering:
        body += "sleep 30 &\n"           # 后台孙进程继承管道
    if sleep_secs:
        body += f"sleep {sleep_secs}\n"
    body += f'echo "{REPORT}"\n'
    body += f"exit {exit_code}\n"
    with open(path, "w", encoding="utf-8") as fh:
        fh.write(body)
    os.chmod(path, 0o755)
    return path


def run_turn(workspace, args, extra_env=None, cancel_after=None):
    before = len(bodies)
    tool_call["name"] = "dev__delegate"
    tool_call["args"] = args
    env = dict(os.environ)
    env.update({
        "BUZZ_AGENT_PROVIDER": "anthropic",
        "ANTHROPIC_BASE_URL": f"http://127.0.0.1:{PORT}",
        "ANTHROPIC_MODEL": "probe-model",
        "ANTHROPIC_API_KEY": "sk-probe",
        "HOME": tempfile.mkdtemp(prefix="abb-delegate-home-"),
        "RUST_LOG": "info",
    })
    env["USERPROFILE"] = env["HOME"]
    for key in ("ABB_AGENT_FAUX_TEXT", "ABB_AGENT_FAUX_TOOL", "OPENAI_COMPAT_MODEL"):
        env.pop(key, None)
    env.update(extra_env or {})

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
    send({"jsonrpc": "2.0", "id": 2, "method": "session/new",
          "params": {"cwd": workspace, "mcpServers": []}})
    wait(2, 20)
    send({"jsonrpc": "2.0", "id": 3, "method": "session/prompt",
          "params": {"sessionId": "abb-1", "prompt": [{"type": "text", "text": "委派"}]}})
    if cancel_after is not None:
        time.sleep(cancel_after)
        send({"jsonrpc": "2.0", "method": "session/cancel", "params": {"sessionId": "abb-1"}})
    answer = wait(3, 60)
    try:
        proc.stdin.close()
    except Exception:
        pass
    time.sleep(0.3)
    proc.kill()
    hops = bodies[before:]
    return (hops[0] if hops else {}), (hops[1] if len(hops) > 1 else {}), answer


print("=== 场景 1：正常委派（假 claude CLI）===")
workspace = tempfile.mkdtemp(prefix="abb-delegate-ws-")
cli = make_fake_cli(workspace)
first, second, answer = run_turn(
    workspace,
    {"backend": "claude", "task": "把 README 改好", "timeout_secs": 60},
    extra_env={"BUZZ_AGENT_DELEGATE_CLAUDE_BIN": cli},
)
second_text = json.dumps(second, ensure_ascii=False)
check("模型侧看到了委派报告", REPORT in second_text, "第二跳里没有报告正文")
calls_path = os.path.join(workspace, "calls.txt")
check("假 CLI 真被调用过", os.path.isfile(calls_path))
if os.path.isfile(calls_path):
    with open(calls_path, encoding="utf-8") as fh:
        calls = fh.read()
    check("参数里有 `--` 分隔与 task", "--" in calls and "把 README 改好" in calls, calls.strip())
check("工具通知里出现 dev__delegate", "dev__delegate" in json.dumps(first, ensure_ascii=False))

print()
print("=== 场景 2：某个后端不可用时给可纠偏错误 ===")
# claude 可用（保证工具被暴露）而 codex 不可用 ⇒ 请求 codex 应得到可纠偏错误。
workspace = tempfile.mkdtemp(prefix="abb-delegate-nocli-")
good_cli = make_fake_cli(workspace, tag="good-claude")
first, second, _ = run_turn(
    workspace,
    {"backend": "codex", "task": "x"},
    extra_env={
        "BUZZ_AGENT_DELEGATE_CLAUDE_BIN": good_cli,
        "BUZZ_AGENT_DELEGATE_CODEX_BIN": "/definitely/not/here-codex",
        "PATH": "/nonexistent-dir-for-probe",
    },
)
second_text = json.dumps(second, ensure_ascii=False)
check("工具结果里是「CLI 不可用」而不是静默成功",
      "not available" in second_text, second_text[-200:])
check("错误里列出可用后端（可纠偏）", "available: claude" in second_text, second_text[-200:])

print()
print("=== 场景 2b：两个 CLI 都没有 ⇒ 不暴露 dev__delegate ===")
workspace = tempfile.mkdtemp(prefix="abb-delegate-none-")
first, _, _ = run_turn(
    workspace,
    {"backend": "claude", "task": "x"},
    extra_env={
        "BUZZ_AGENT_DELEGATE_CLAUDE_BIN": "/definitely/not/here-claude",
        "BUZZ_AGENT_DELEGATE_CODEX_BIN": "/definitely/not/here-codex",
        "PATH": "/nonexistent-dir-for-probe",
    },
)
names = [t.get("name") for t in first.get("tools", []) if isinstance(t, dict)]
check("工具表里没有 dev__delegate（没有可用后端时不暴露）",
      "dev__delegate" not in names, f"{names}")
check("其它内置工具仍在", "dev__shell" in names, f"{names}")

print()
print("=== 场景 3：取消长委派 ⇒ 回合收尾为 cancelled ===")
workspace = tempfile.mkdtemp(prefix="abb-delegate-cancel-")
cli = make_fake_cli(workspace, sleep_secs=30)
first, second, answer = run_turn(
    workspace,
    {"backend": "claude", "task": "慢任务", "timeout_secs": 120},
    extra_env={"BUZZ_AGENT_DELEGATE_CLAUDE_BIN": cli},
    cancel_after=1.0,
)
check("回合应答到了（没有挂死）", answer is not None)
check("收尾为 cancelled", answer is not None and '"cancelled"' in answer.replace(" ", ""),
      (answer or "")[-160:])

print()
print("=== 场景 4：非零退出必须是错误结果（fail 位不能丢）===")
workspace = tempfile.mkdtemp(prefix="abb-delegate-exit-")
cli = make_fake_cli(workspace, exit_code=3, tag="failing-claude")
first, second, answer = run_turn(
    workspace,
    {"backend": "claude", "task": "会失败的任务", "timeout_secs": 60},
    extra_env={"BUZZ_AGENT_DELEGATE_CLAUDE_BIN": cli},
)
second_text = json.dumps(second, ensure_ascii=False)
check("模型侧看到的是工具**错误**（tool error …）", "tool error:" in second_text, second_text[-200:])
check("错误里带 exit 码", "exit: 3" in second_text)
check("报告正文仍随错误带回（模型能看到输出）", REPORT in second_text)

print()
print("=== 场景 5：孙进程攥着管道时回合不挂死 ===")
workspace = tempfile.mkdtemp(prefix="abb-delegate-linger-")
cli = make_fake_cli(workspace, lingering=True, tag="linger-claude")
started = time.time()
first, second, answer = run_turn(
    workspace,
    {"backend": "claude", "task": "留下孙进程", "timeout_secs": 60},
    extra_env={"BUZZ_AGENT_DELEGATE_CLAUDE_BIN": cli},
)
elapsed = time.time() - started
check("回合在宽限内有应答（不挂死）", answer is not None and elapsed < 40, f"{elapsed:.1f}s")
second_text = json.dumps(second, ensure_ascii=False)
check("如实标注输出不完整", "output incomplete" in second_text, second_text[-200:])
# 清掉假 CLI 留下的 sleep（探针纪律）。
os.system("pkill -f 'sleep 30' >/dev/null 2>&1")

print()
if failures:
    print(f"⇒ 判定：❌ 失败 {len(failures)} 项：{failures}")
    sys.exit(1)
print("⇒ 判定：✅ dev__delegate 真调用本机 CLI、报告回灌、不可用时可纠偏、可取消")
