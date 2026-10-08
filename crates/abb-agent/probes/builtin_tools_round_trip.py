#!/usr/bin/env python3
"""端到端：内置工具（`dev__*`）真的在会话工作区里干活，且**写不出工作区**。

判据是**文件系统**（不是 agent 的文本输出）：工具调用到底有没有真落盘、逃逸尝试有没有真被拦住。

六个场景：
1. `dev__write` 写入相对路径 ⇒ 文件真出现且内容一致（含自动创建父目录）；
2. `dev__write` 用 `../escape.txt` ⇒ 工作区外**不得**出现该文件；工作区外绝对路径同样被拒；
3. `dev__shell` 跑一条真命令（`echo … > shell.txt`）⇒ 文件真出现、内容一致；
4. `BUZZ_AGENT_DEV_TOOLS=0` ⇒ `dev__*` 工具不存在（模型调用它会得到「工具不存在」），
   且**不产生任何文件**（与 fork 的开关语义一致）；
5. **末段符号链接**（`workspace/link.txt -> 工作区外文件`）⇒ `dev__write` 必须被拒，
   工作区外的目标文件**内容原封不动**（修前会跟随链接写穿）；
6. **shell 不得继承宿主环境**：`env > env.txt` 后断言 `ANTHROPIC_API_KEY`（探针设的哨兵）
   **不在**子进程环境里，而 `PATH` 在（对照）。

用法：`python3 builtin_tools_round_trip.py <abb-agent 二进制>`
"""
import json
import os
import subprocess
import sys
import tempfile
import threading
import time

BIN = sys.argv[1]
failures = []


def check(label, ok, detail=""):
    print(f"  {'✅' if ok else '❌'} {label}{('：' + detail) if detail else ''}")
    if not ok:
        failures.append(label)


def run_turn(workspace, tool_call, extra_env=None, timeout=40):
    """起一个 abb-agent 跑一次回合（faux provider 先发一次工具调用，再出文本）。

    返回 True/False 表示有没有等到回合应答（工具是否存在不影响回合结束）。
    """
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
    out = []
    stderr = []
    t0 = time.time()
    threading.Thread(target=lambda: [out.append((time.time() - t0, l.strip()))
                                     for l in proc.stdout], daemon=True).start()
    threading.Thread(target=lambda: stderr.append(proc.stderr.read()), daemon=True).start()

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
          "params": {"sessionId": "abb-1", "prompt": [{"type": "text", "text": "干活"}]}})
    answer = wait(3, timeout)
    try:
        proc.stdin.close()
    except Exception:
        pass
    time.sleep(0.3)
    proc.kill()
    return answer is not None, "".join(stderr)


def tempdir(tag):
    return tempfile.mkdtemp(prefix=f"abb-builtin-{tag}-")


print("=== 场景 1：dev__write 真落盘（相对路径 + 自动建父目录）===")
workspace = tempdir("write")
ok, _ = run_turn(workspace, {
    "name": "dev__write",
    "arguments": {"path": "nested/probe.txt", "content": "BUILTIN-WRITE-MARKER-4a91"},
})
check("回合完成", ok)
target = os.path.join(workspace, "nested", "probe.txt")
check("文件真出现（父目录也建了）", os.path.isfile(target), target)
if os.path.isfile(target):
    with open(target, encoding="utf-8") as fh:
        check("内容逐字一致", fh.read() == "BUILTIN-WRITE-MARKER-4a91")

print()
print("=== 场景 2：写入限定（逃逸必须失败）===")
workspace = tempdir("escape")
parent = os.path.dirname(workspace)
escape_target = os.path.join(parent, "abb-builtin-escape-should-not-exist.txt")
if os.path.exists(escape_target):
    os.unlink(escape_target)
ok, _ = run_turn(workspace, {
    "name": "dev__write",
    "arguments": {"path": "../abb-builtin-escape-should-not-exist.txt", "content": "ESCAPED"},
})
check("回合完成（拒绝被当成工具错误，而不是把回合打挂）", ok)
check("工作区外**没有**该文件", not os.path.exists(escape_target), escape_target)

outside = os.path.join(tempdir("outside"), "absolute.txt")
ok, _ = run_turn(workspace, {
    "name": "dev__write",
    "arguments": {"path": outside, "content": "ESCAPED-ABSOLUTE"},
})
check("回合完成（绝对路径逃逸）", ok)
check("工作区外绝对路径**没有**落盘", not os.path.exists(outside), outside)

print()
print("=== 场景 3：dev__shell 真跑命令（cwd = 会话工作区）===")
workspace = tempdir("shell")
ok, _ = run_turn(workspace, {
    "name": "dev__shell",
    "arguments": {"command": "echo SHELL-MARKER-77c2 > shell.txt && pwd", "timeout_secs": 30},
})
check("回合完成（含 timeout_secs 参数名，与参照物同形）", ok)
shell_target = os.path.join(workspace, "shell.txt")
check("shell 在会话工作区里造出了文件", os.path.isfile(shell_target), shell_target)
if os.path.isfile(shell_target):
    with open(shell_target, encoding="utf-8") as fh:
        check("内容一致", fh.read().strip() == "SHELL-MARKER-77c2")

print()
print("=== 场景 4：BUZZ_AGENT_DEV_TOOLS=0 时不该有内置工具 ===")
workspace = tempdir("off")
ok, stderr = run_turn(
    workspace,
    {"name": "dev__write", "arguments": {"path": "should-not-exist.txt", "content": "X"}},
    extra_env={"BUZZ_AGENT_DEV_TOOLS": "0"},
)
check("回合完成", ok)
check("开关关闭时不产生文件", not os.path.exists(os.path.join(workspace, "should-not-exist.txt")))
check("日志如实说明内置工具已关闭", "内置工具已关闭" in stderr, "stderr 里有没有那句关闭说明")
check("也确实没有装配内置工具（对数）", "内置工具 7 个" not in stderr)

print()
print("=== 场景 5：末段符号链接（写穿工作区）必须被拒 ===")
workspace = tempdir("symlink-file")
outside_dir = tempdir("symlink-target")
outside_file = os.path.join(outside_dir, "target.txt")
with open(outside_file, "w", encoding="utf-8") as fh:
    fh.write("ORIGINAL-CONTENT")
link = os.path.join(workspace, "link.txt")
try:
    os.symlink(outside_file, link)
    ok, _ = run_turn(workspace, {
        "name": "dev__write",
        "arguments": {"path": "link.txt", "content": "WRITTEN-THROUGH-LINK"},
    })
    check("回合完成（拒绝是工具错误，不把回合打挂）", ok)
    with open(outside_file, encoding="utf-8") as fh:
        check("工作区外目标文件**内容原封不动**", fh.read() == "ORIGINAL-CONTENT")
except OSError:
    print("  （本平台不支持建符号链接，跳过）")

print()
print("=== 场景 6：shell 不得继承宿主环境（含供应商凭据）===")
workspace = tempdir("shellenv")
ok, _ = run_turn(
    workspace,
    {"name": "dev__shell", "arguments": {"command": "env > env.txt", "timeout_secs": 30}},
    extra_env={"ANTHROPIC_API_KEY": "sk-probe-should-not-leak",
               "ABB_AGENT_CONTEXT": "host-value"},
)
check("回合完成", ok)
dump = os.path.join(workspace, "env.txt")
check("shell 真的跑起来并落盘了环境快照", os.path.isfile(dump), dump)
if os.path.isfile(dump):
    with open(dump, encoding="utf-8", errors="replace") as fh:
        text = fh.read()
    check("凭据**不在** shell 环境里", "sk-probe-should-not-leak" not in text)
    check("PATH 仍在（工具要能跑）", "PATH=" in text)
    # abb 的 proc 闸（Q8）以这个变量为**主判据**：少了它，「agent 派生的 shell 去建 --proc 任务」
    # 就会 fail-open（参照物在同一个共享入口里写它）。
    check("agent 上下文标记由我们写入（ABB_AGENT_CONTEXT=1）",
          "ABB_AGENT_CONTEXT=1" in text, "shell 里没有该标记 ⇒ abb 的 proc 闸会 fail-open")
    check("宿主里若有别的标记值也被覆盖成 1", "ABB_AGENT_CONTEXT=host-value" not in text)

print()
print("=== 场景 7：dev__edit 真改文件（能力真到位，判据是文件内容）===")
workspace = tempdir("edit")
target = os.path.join(workspace, "doc.txt")
with open(target, "w", encoding="utf-8") as fh:
    fh.write("hello OLD world")
ok, _ = run_turn(workspace, {
    "name": "dev__edit",
    "arguments": {"path": "doc.txt", "edits": [{"oldText": "OLD", "newText": "NEW"}]},
})
check("回合完成", ok)
with open(target, encoding="utf-8") as fh:
    check("文件内容真被改", fh.read() == "hello NEW world")

print()
if failures:
    print(f"⇒ 判定：❌ 失败 {len(failures)} 项：{failures}")
    sys.exit(1)
print("⇒ 判定：✅ 内置工具真落盘、写不出工作区、shell 真执行、开关真生效")
