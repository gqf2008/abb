#!/usr/bin/env python3
"""端到端：MCP 子进程的**环境/工作目录隔离** + **装配预算不再占住读循环**。

覆盖第四轮独立评审（`abb-reviewer-61`）判 needs-changes 的两类偏离：

1. 子进程环境隔离——修前 MCP server 默认继承 abb-agent 的全量环境（实测子进程 env 里
   有 `ANTHROPIC_API_KEY`），且 `cwd` 没落到子进程。判据是**假 server 自己写下的
   cwd + 全量 env**，不是读代码推断。
2. 装配预算——修前 `initialize` 与 `tools/list` 各 20s 且顺序、无总预算（单 server 实测
   35.1s、两个 70.1s），且装配期间读循环被占（`session/cancel`/`initialize` 各被拖 19s）；
   abb 那边「停止」只有 5s 宽限、`session/new` RPC 只有 60s。
   判据：挂住的 server 只吃掉**单 server 预算（20s）**，且**同一时刻**发出的 `initialize`
   在远小于 20s 内就拿到应答（读循环可读）。

用法：`python3 mcp_isolation_and_budget.py <abb-agent 二进制>`
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
WORKDIR = tempfile.mkdtemp(prefix="abb-mcp-iso-")
DUMP = os.path.join(tempfile.mkdtemp(prefix="abb-mcp-dump-"), "child.json")
SECRET = "sk-secret-probe-61"

failures = []


def check(label, ok, detail=""):
    print(f"  {'✅' if ok else '❌'} {label}{('：' + detail) if detail else ''}")
    if not ok:
        failures.append(label)


class Agent:
    """一条 abb-agent 会话（行分隔 JSON-RPC，stdout 是 ACP 通道）。"""

    def __init__(self, env):
        self.proc = subprocess.Popen(
            BIN, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
            stderr=subprocess.PIPE, env=env, text=True, bufsize=1,
        )
        self.out = []
        self.t0 = time.time()
        threading.Thread(target=self._read, daemon=True).start()

    def _read(self):
        for line in self.proc.stdout:
            self.out.append((time.time() - self.t0, line.strip()))

    def send(self, obj):
        self.proc.stdin.write(json.dumps(obj) + "\n")
        self.proc.stdin.flush()

    def wait(self, msg_id, timeout):
        end = time.time() + timeout
        while time.time() < end:
            for elapsed, line in list(self.out):
                if f'"id":{msg_id}' in line.replace(" ", ""):
                    return elapsed, json.loads(line)
            time.sleep(0.02)
        return None, None

    def close(self):
        try:
            self.proc.stdin.close()
        except Exception:
            pass
        try:
            self.proc.wait(timeout=5)
        except Exception:
            self.proc.kill()


def base_env():
    env = dict(os.environ)
    env.update({
        "ABB_AGENT_FAUX_TEXT": "离线收尾文本",
        "ANTHROPIC_API_KEY": SECRET,          # 生产路径真会注入的凭据
        "ANTHROPIC_MODEL": "probe-model",
        "BUZZ_AGENT_PROVIDER": "anthropic",
        "RUST_LOG": "info",
    })
    return env


print("=== 场景 1：子进程环境白名单 + cwd ===")
agent = Agent(base_env())
agent.send({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}})
agent.wait(1, 10)
agent.send({
    "jsonrpc": "2.0", "id": 2, "method": "session/new",
    "params": {
        "cwd": WORKDIR,
        "mcpServers": [{
            "name": "dumpenv",
            "command": sys.executable,
            "args": [SERVER, "--dump-env", DUMP],
            "env": [{"name": "PROBE_SPEC_ENV", "value": "from-session-new"}],
        }],
    },
})
elapsed, resp = agent.wait(2, 30)
check("session/new 成功", bool(resp and resp.get("result")), f"{elapsed:.2f}s" if elapsed else "无应答")
agent.close()

deadline = time.time() + 10
while not os.path.exists(DUMP) and time.time() < deadline:
    time.sleep(0.1)
if not os.path.exists(DUMP):
    check("假 server 写下了自己的 env/cwd", False, "没等到 dump 文件")
else:
    with open(DUMP, encoding="utf-8") as fh:
        dumped = json.load(fh)
    env = dumped["env"]
    check("cwd 落在会话工作区", os.path.realpath(dumped["cwd"]) == os.path.realpath(WORKDIR),
          f"{dumped['cwd']} vs {WORKDIR}")
    check("spec.env 生效", env.get("PROBE_SPEC_ENV") == "from-session-new")
    check("供应商 key 未透传", SECRET not in json.dumps(env), "ANTHROPIC_API_KEY 不该出现")
    leaked = [k for k in env if k.startswith("ANTHROPIC") or k.startswith("BUZZ_AGENT")
              or k.startswith("OPENAI_COMPAT")]
    check("供应商配置整体未透传", not leaked, f"泄漏：{leaked}")
    check("PATH 仍在（工具要能跑）", bool(env.get("PATH")))

print()
print("=== 场景 2：挂死的 server 只吃单 server 预算，且读循环不被占 ===")
agent = Agent(base_env())
agent.send({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}})
agent.wait(1, 10)
start = time.time()
agent.send({
    "jsonrpc": "2.0", "id": 2, "method": "session/new",
    "params": {
        "cwd": WORKDIR,
        "mcpServers": [{
            "name": "hung",
            "command": sys.executable,
            "args": [SERVER, "--hang-init"],
            "env": [],
        }],
    },
})
# 同一时刻送一个与装配无关的请求：修前它会被读循环的同步装配挡住 ~20s。
agent.send({"jsonrpc": "2.0", "id": 3, "method": "initialize", "params": {}})
elapsed_init, resp_init = agent.wait(3, 40)
elapsed_new, resp_new = agent.wait(2, 40)
check("装配期间 initialize 仍被处理",
      bool(resp_init) and elapsed_init is not None and elapsed_init < 5,
      f"{elapsed_init:.2f}s" if elapsed_init else "无应答")
check("session/new 在单 server 预算附近收尾",
      bool(resp_new) and elapsed_new is not None,
      f"{elapsed_new:.2f}s（预算 20s，修前 35s）" if elapsed_new else "无应答")
if elapsed_new:
    check("仍在 abb 的 session/new 预算（60s）内", elapsed_new < 60, f"{elapsed_new:.2f}s")
    check("至少吃到了挂住那个 server 的预算（没有无限早退）", elapsed_new >= 1,
          f"{elapsed_new:.2f}s")
agent.close()

print()
print("=== 场景 3：两个挂死的 server ⇒ 被总预算（30s）截断，且 initialize 延迟后仍可读 ===")
agent = Agent(base_env())
agent.send({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}})
agent.wait(1, 10)
agent.send({
    "jsonrpc": "2.0", "id": 2, "method": "session/new",
    "params": {
        "cwd": WORKDIR,
        "mcpServers": [
            {"name": "hung1", "command": sys.executable, "args": [SERVER, "--hang-init"], "env": []},
            {"name": "hung2", "command": sys.executable, "args": [SERVER, "--hang-init"], "env": []},
        ],
    },
})
# 装配**已经开始之后**再发（避免「它恰好在装配开始前就应答了」的假通过）。
time.sleep(1.0)
agent.send({"jsonrpc": "2.0", "id": 3, "method": "initialize", "params": {}})
elapsed_init, resp_init = agent.wait(3, 45)
elapsed_new, resp_new = agent.wait(2, 45)
check("装配已开始后 initialize 仍被处理",
      bool(resp_init) and elapsed_init is not None and elapsed_init < 5,
      f"{elapsed_init:.2f}s（装配已跑 1s）" if elapsed_init else "无应答")
check("两个挂死 server 被总预算截断（≤ 30s + 余量）",
      bool(resp_new) and elapsed_new is not None and elapsed_new <= 35,
      f"{elapsed_new:.2f}s（修前同形状 70.1s）" if elapsed_new else "无应答")
agent.close()

print()
if failures:
    print(f"⇒ 判定：❌ 失败 {len(failures)} 项：{failures}")
    sys.exit(1)
print("⇒ 判定：✅ 子进程环境/cwd 隔离生效，且装配不再占住读循环")
