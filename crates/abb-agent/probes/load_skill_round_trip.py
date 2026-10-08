#!/usr/bin/env python3
"""端到端：技能清单进系统提示、`load_skill` 真能按需读到正文、越界读被拒、`NO_HINTS` 同关。

判据是**假 anthropic 端点抓到的请求体**：第二跳里带着 `load_skill` 的工具结果（模型看到的内容），
第一跳里带着工具表与 system 字段。

五个场景：
1. 默认：system 里有 `# Additional Instructions` + `## Available Skills`（技能名与描述），
   工具表里有**裸名** `load_skill`；用它读技能正文 ⇒ 第二跳结果里有正文标记、**没有** frontmatter；
2. 支持文件形式 `demo/references/foo.md` ⇒ 结果里有支持文件内容与 `## Supporting Files` 清单；
3. 越界形式 `demo/../../secret.md` ⇒ 结果是错误（`not found`），且工作区外的秘密内容**不出现**；
4. **工作区里没有任何技能**时不暴露 `load_skill`（参照物只在 `skills` 非空时注册），
   system 里也没有 `## Available Skills`；
5. `BUZZ_AGENT_NO_HINTS=1` ⇒ 工具表里**没有** `load_skill`、system 里没有 `## Available Skills`，
   且技能正文标记在整个请求体里都不出现（技能与约定链同关）。

用法：`python3 load_skill_round_trip.py <abb-agent 二进制>`
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
PORT = 18111
SKILL_MARKER = "SKILL-BODY-MARKER-9f31"
SUPPORT_MARKER = "SKILL-SUPPORT-MARKER-2c07"
SECRET_MARKER = "OUTSIDE-SECRET-7b4d"
bodies = []
current_tool_call = {"value": None}


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
            for message in body.get("messages", [])
            for blk in (message.get("content") if isinstance(message.get("content"), list) else [])
        )
        events = [("message_start", {"type": "message_start", "message": {
            "id": "msg_1", "type": "message", "role": "assistant", "model": "m",
            "content": [], "stop_reason": None,
            "usage": {"input_tokens": 1, "output_tokens": 1}}})]
        if not has_tool_result and current_tool_call["value"]:
            name, arguments = current_tool_call["value"]
            events += [
                ("content_block_start", {"type": "content_block_start", "index": 0,
                                         "content_block": {"type": "tool_use", "id": "toolu_1",
                                                           "name": name, "input": {}}}),
                ("content_block_delta", {"type": "content_block_delta", "index": 0,
                                         "delta": {"type": "input_json_delta",
                                                   "partial_json": json.dumps(arguments)}}),
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


def make_workspace():
    """建一个会话工作区，带一个技能（含支持文件）与一个工作区外的「秘密」文件。"""
    workspace = tempfile.mkdtemp(prefix="abb-skills-ws-")
    skill_dir = os.path.join(workspace, ".agents", "skills", "demo")
    os.makedirs(os.path.join(skill_dir, "references"), exist_ok=True)
    with open(os.path.join(skill_dir, "SKILL.md"), "w", encoding="utf-8") as fh:
        fh.write(
            "---\nname: demo\ndescription: 演示技能（MARKER-DESC-4e88）\n---\n"
            f"# demo 技能\n{SKILL_MARKER}\n"
        )
    with open(os.path.join(skill_dir, "references", "foo.md"), "w", encoding="utf-8") as fh:
        fh.write(f"参考内容 {SUPPORT_MARKER}\n")
    # 技能目录的上一级（工作区内）放一个不该被读到的文件：越界读要拿不到它。
    with open(os.path.join(workspace, ".agents", "secrets.md"), "w", encoding="utf-8") as fh:
        fh.write(f"{SECRET_MARKER}\n")
    return workspace


def run_turn(workspace, tool_name, arguments, extra_env=None):
    """跑一个回合，返回 (第一跳请求体, 第二跳请求体)。"""
    before = len(bodies)
    current_tool_call["value"] = (tool_name, arguments)
    env = dict(os.environ)
    env.update({
        "BUZZ_AGENT_PROVIDER": "anthropic",
        "ANTHROPIC_BASE_URL": f"http://127.0.0.1:{PORT}",
        "ANTHROPIC_MODEL": "probe-model",
        "ANTHROPIC_API_KEY": "sk-probe",
        "HOME": tempfile.mkdtemp(prefix="abb-skills-home-"),   # 隔离：不读跑测机器的 ~/.agents/skills
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
          "params": {"sessionId": "abb-1", "prompt": [{"type": "text", "text": "用技能"}]}})
    wait(3, 40)
    try:
        proc.stdin.close()
    except Exception:
        pass
    time.sleep(0.3)
    proc.kill()
    hops = bodies[before:]
    return (hops[0] if hops else {}), (hops[1] if len(hops) > 1 else {})


def system_text(body):
    system = body.get("system")
    if isinstance(system, str):
        return system
    if isinstance(system, list):
        return "\n".join(blk.get("text", "") for blk in system if isinstance(blk, dict))
    return ""


def tool_names(body):
    return [tool.get("name") for tool in body.get("tools", []) if isinstance(tool, dict)]


print("=== 场景 1：技能清单进系统提示 + load_skill 读到正文 ===")
workspace = make_workspace()
first, second = run_turn(workspace, "load_skill", {"name": "demo"})
system = system_text(first)
check("system 有约定链/技能段标题", "# Additional Instructions" in system, system[:120])
check("system 列出技能名与描述",
      "## Available Skills" in system and "- demo:" in system and "MARKER-DESC-4e88" in system)
check("提示里给出用法", "Use the `load_skill` tool" in system)
names = tool_names(first)
check("工具表里有**裸名** load_skill", "load_skill" in names, f"{names}")
check("工具表里没有 dev__load_skill（参照物是裸名）", "dev__load_skill" not in names)
second_text = json.dumps(second, ensure_ascii=False)
check("第二跳带着技能正文（模型真读到了）", SKILL_MARKER in second_text)
check("frontmatter 被剥掉（不该看到 name: demo 这种元数据）",
      "description: 演示技能" not in second_text)
check("附上 Supporting Files 清单", "## Supporting Files" in second_text)

print()
print("=== 场景 2：支持文件按 skill/relative/path 读 ===")
workspace = make_workspace()
first, second = run_turn(workspace, "load_skill", {"name": "demo/references/foo.md"})
second_text = json.dumps(second, ensure_ascii=False)
check("支持文件内容真读到", SUPPORT_MARKER in second_text)
check("没有把技能正文也塞进来（只读请求的那个文件）", SKILL_MARKER not in second_text)

print()
print("=== 场景 3：越界形式必须拿不到工作区外的文件 ===")
workspace = make_workspace()
first, second = run_turn(workspace, "load_skill", {"name": "demo/../../secrets.md"})
second_text = json.dumps(second, ensure_ascii=False)
check("越界读**没有**拿到秘密内容", SECRET_MARKER not in second_text)
check("第二跳里有可纠偏的错误信息（not found）", "not found" in second_text)

print()
print("=== 场景 4：没有任何技能时不暴露 load_skill ===")
bare_ws = tempfile.mkdtemp(prefix="abb-skills-bare-")
first, _ = run_turn(bare_ws, "load_skill", {"name": "demo"})
names = tool_names(first)
check("工具表里没有 load_skill（无技能时不暴露）", "load_skill" not in names, f"{names}")
check("system 里没有技能段", "## Available Skills" not in system_text(first))
check("内置工具仍在（只少了 load_skill）", "dev__read" in names, f"{names}")

print()
print("=== 场景 5：BUZZ_AGENT_NO_HINTS=1 时技能与约定链同关 ===")
workspace = make_workspace()
first, second = run_turn(workspace, "load_skill", {"name": "demo"},
                         extra_env={"BUZZ_AGENT_NO_HINTS": "1"})
system = system_text(first)
names = tool_names(first)
check("工具表里没有 load_skill", "load_skill" not in names, f"{names}")
check("system 里没有技能段", "## Available Skills" not in system)
check("system 里也没有约定链段", "# Additional Instructions" not in system)
whole = json.dumps(first, ensure_ascii=False) + json.dumps(second, ensure_ascii=False)
check("技能正文标记在整个请求里都不出现", SKILL_MARKER not in whole)

print()
if failures:
    print(f"⇒ 判定：❌ 失败 {len(failures)} 项：{failures}")
    sys.exit(1)
print("⇒ 判定：✅ 技能清单进系统提示、load_skill 真读正文、越界读被拒、NO_HINTS 同关")
