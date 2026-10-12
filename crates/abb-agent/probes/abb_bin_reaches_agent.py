#!/usr/bin/env python3
"""探针：`$ABB_BIN` 是否真的能到达 agent 的 shell（卡 `abb-mcp-orchestrator-p0-20261012`）。

## 为什么需要它

提示词与受限白名单都按 `$ABB_BIN` 工作（agent 用它调 `task`/`job`/`deliver`；编排器 MCP server
也要用它驱动 CLI），但 **ABB 此前从未把它注入 agent 进程**，abb-agent 的白名单也不透传它
⇒ 线上 `"$ABB_BIN" task add …` 会展开成空。本探针端到端验证这条链：

    abb-agent 进程 env（ABB 注入） → child_env 白名单 → shell 工具 env → `printenv ABB_BIN`

## 同步方式（**不依赖 sleep**）

上一版手搓探针用 `(cat …; sleep N) | abb-agent` 造成过假结论（stdin 提前 EOF ⇒ 应答被取消）。
这里按 JSON-RPC **id 同步**：每一步都阻塞读到对应 id 的应答再发下一步，整轮有 deadline；
stdin 只在全部步骤完成后关闭。

## 判别力（负向对照，见 --negative-control）

`--negative-control` 会在子进程 env 里**去掉** `ABB_BIN`，此时探针**必须失败**
（否则说明它根本没在验证这条链）。也可用变异验证：把 `child_env::PASSTHROUGH_ENV` 里的
`ABB_BIN` 删掉后重跑，探针同样必须失败。

用法：
    python3 crates/abb-agent/probes/abb_bin_reaches_agent.py [--agent <abb-agent 路径>] [--negative-control]
退出码：0 = 通过；非 0 = 失败（附原因）。
"""

from __future__ import annotations

import argparse
import json
import os
import selectors
import subprocess
import sys
import time

DEADLINE_SECS = float(os.environ.get("ABB_PROBE_DEADLINE", "60"))


class Agent:
    """按 id 同步的 NDJSON 客户端。

    IO 用**字节级非阻塞 + selectors**（`readline()` 在 select 返回可读但行未完整时会阻塞——第一版
    就是这样假死的）：每次 select 只 `os.read` 已可读的 fd，自行按 `\n` 切行；所有循环都以
    deadline 收口，绝不无界等待。
    """

    def __init__(self, binary: str, env: dict[str, str]) -> None:
        self.proc = subprocess.Popen(
            [binary],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            env=env,
        )
        assert self.proc.stdin and self.proc.stdout and self.proc.stderr
        for stream in (self.proc.stdout, self.proc.stderr):
            os.set_blocking(stream.fileno(), False)
        self.sel = selectors.DefaultSelector()
        self.sel.register(self.proc.stdout, selectors.EVENT_READ)
        self.sel.register(self.proc.stderr, selectors.EVENT_READ)
        self.buf_out = b""
        self.buf_err = b""
        self.next_id = 0
        self.stderr_tail: list[str] = []
        self.messages: list[dict] = []

    def send(self, method: str, params: dict) -> int:
        self.next_id += 1
        msg = {"jsonrpc": "2.0", "id": self.next_id, "method": method, "params": params}
        self.proc.stdin.write((json.dumps(msg) + "\n").encode())
        self.proc.stdin.flush()
        return self.next_id

    def _pump(self, deadline: float) -> list[dict]:
        out: list[dict] = []
        for key, _ in self.sel.select(timeout=max(0.0, min(0.5, deadline - time.monotonic()))):
            stream = key.fileobj
            try:
                chunk = os.read(stream.fileno(), 65536)
            except BlockingIOError:
                continue
            if chunk == b"":
                self.sel.unregister(stream)
                continue
            if stream is self.proc.stderr:
                self.buf_err += chunk
                lines = self.buf_err.split(b"\n")
                self.buf_err = lines.pop()
                for line in lines:
                    text = line.decode(errors="replace").strip()
                    if text:
                        self.stderr_tail.append(text)
                        del self.stderr_tail[:-40]
                continue
            self.buf_out += chunk
            lines = self.buf_out.split(b"\n")
            self.buf_out = lines.pop()
            for line in lines:
                text = line.decode(errors="replace").strip()
                if not text:
                    continue
                try:
                    out.append(json.loads(text))
                except json.JSONDecodeError:
                    self.stderr_tail.append(f"non-json stdout: {text[:200]}")
        return out

    def wait_for(self, predicate, what: str) -> dict:
        deadline = time.monotonic() + DEADLINE_SECS
        seen: list[str] = []
        while time.monotonic() < deadline:
            for msg in self._pump(deadline):
                self.messages.append(msg)
                seen.append(json.dumps(msg, ensure_ascii=False)[:300])
                if predicate(msg):
                    return msg
            if self.proc.poll() is not None:
                # 进程已退：把剩余可读数据排空后再判失败
                for msg in self._pump(time.monotonic() + 0.5):
                    self.messages.append(msg)
                break
        self.close()
        raise SystemExit(
            f"❌ 等不到 {what}（deadline {DEADLINE_SECS:.0f}s）。已收到的消息：\n  "
            + "\n  ".join(seen[-8:] or ["(无)"])
            + "\nstderr 尾部：\n  "
            + "\n  ".join(self.stderr_tail[-6:] or ["(无)"])
        )

    def drain(self, secs: float = 1.0) -> None:
        """把 prompt 应答之后可能仍在路上的 update 读干净（有界）。"""
        deadline = time.monotonic() + secs
        while time.monotonic() < deadline:
            for msg in self._pump(deadline):
                self.messages.append(msg)

    def close(self) -> None:
        try:
            if self.proc.stdin:
                self.proc.stdin.close()
            self.proc.wait(timeout=10)
        except Exception:
            self.proc.kill()


def collect_tool_output(messages: list[dict]) -> str:
    """把一轮会话里**工具输出**的文本拼出来（ACP `tool_call_update` 的 content）。

    真实形状（本探针实测，`sessionUpdate = tool_call_update`）：

        "content": [{"type": "content", "content": {"type": "text", "text": "…"}}]

    即文本在 `content[i].content.text`（**嵌套两层**）；这里同时容忍 `content[i].text`
    与字符串形态，免得上游换形状时探针假红。
    """
    chunks: list[str] = []

    def take(node) -> None:
        if isinstance(node, str):
            chunks.append(node)
        elif isinstance(node, list):
            for item in node:
                take(item)
        elif isinstance(node, dict):
            if isinstance(node.get("text"), str):
                chunks.append(node["text"])
            for key in ("content", "rawOutput"):
                if key in node:
                    take(node[key])

    for msg in messages:
        update = (msg.get("params") or {}).get("update") or {}
        if update.get("sessionUpdate") in ("tool_call_update", "tool_call"):
            take(update.get("content"))
            take(update.get("rawOutput"))
    return "\n".join(chunks)


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--agent", default=os.environ.get("ABB_AGENT_BIN", "target/debug/abb-agent"))
    ap.add_argument(
        "--negative-control",
        action="store_true",
        help="子进程 env 里去掉 ABB_BIN（探针必须失败，否则它没在验证这条链）",
    )
    args = ap.parse_args()
    if not os.path.exists(args.agent):
        print(f"❌ 找不到 abb-agent：{args.agent}（先 cargo build，或用 --agent 指定）")
        return 2

    expected = os.path.abspath("/tmp/fake-abb-bin-for-probe")
    env = dict(os.environ)
    env["ABB_AGENT_FAUX_TEXT"] = "probe-done"
    env["ABB_AGENT_FAUX_TOOL"] = json.dumps(
        {"name": "dev__shell", "arguments": {"command": "printenv ABB_BIN"}}
    )
    if args.negative_control:
        env.pop("ABB_BIN", None)
    else:
        env["ABB_BIN"] = expected

    agent = Agent(args.agent, env)
    try:
        agent.send("initialize", {"protocolVersion": 2})
        agent.wait_for(lambda m: m.get("id") == 1, "initialize 应答")
        agent.send("session/new", {"cwd": os.getcwd()})
        sess = agent.wait_for(lambda m: m.get("id") == 2, "session/new 应答")
        session_id = (sess.get("result") or {}).get("sessionId")
        if not session_id:
            raise SystemExit(f"❌ session/new 没给 sessionId：{sess}")
        agent.send(
            "session/prompt",
            {"sessionId": session_id, "prompt": [{"type": "text", "text": "printenv ABB_BIN"}]},
        )
        agent.wait_for(lambda m: m.get("id") == 3, "prompt 应答")
    finally:
        agent.close()

    agent.drain(1.5)  # prompt 应答后仍可能有 tool_call_update 在路上（有界排空）
    # 判据：本轮会话里**所有**收到的消息（`Agent.wait_for`/`drain` 逐条累积）里出现工具输出文本。
    text = collect_tool_output(agent.messages)
    if args.negative_control:
        if expected in text:
            print("❌ 负向对照失败：没设 ABB_BIN 却在输出里看到了它")
            return 1
        print("✅ 负向对照通过（未设 ABB_BIN ⇒ 输出里没有它）")
        return 0
    if expected not in text:
        print(
            "❌ 探针失败：agent 的 shell 里没有 $ABB_BIN\n"
            f"   期望输出含：{expected}\n   实际工具输出：{text!r}"
        )
        return 1
    print(f"✅ `$ABB_BIN` 到达 agent 的 shell：{expected}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
