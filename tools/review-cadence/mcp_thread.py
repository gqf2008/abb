#!/usr/bin/env python3
"""从 walgit MCP 的 `collab_thread` 工具取一条 thread 的条目数组，写 stdout。

## 为什么存在（card abb-d68-local-commands-20261102）

walgit 的协作面正在变成**纯 git + MCP**：`walgit collab thread` 这类本地子命令
自 D68 起默认拒绝执行（需要 `WALGIT_LOCAL_COMMANDS=1`），而本地二进制将不再部署
（主机搬到服务器上）。门禁若只会调 `walgit collab thread`，那时整道闸就不可用。

MCP 的 `collab_thread` 返回的正是**服务端权威聚合**：按 parent 链排好序，并且每条
带 `verified` / `broken_refs`。用它读，就不必在本仓复刻 walgit 的聚合与验签逻辑
——复刻判据语义出错的风险，比依赖 MCP 大得多。

顺带解决另一件事：MCP 读的是服务端状态，不存在「本地 refs 陈旧」问题
（`collab_publish` 不写本地 refs ⇒ 读本地 refs 会漏判）。

## 端点与凭据

  WALGIT_MCP_URL    端点。**默认从 `origin` 的远程地址推导**（`<scheme>://<host>/mcp`），
                    而不是写死 `127.0.0.1:8081`——本机 walgit 服务退役后那个地址就不存在了，
                    而 `origin` 一定会跟着迁移一起改（否则 git push 先就不工作了）。
                    推不出来（本地路径 / 没有 origin）就报错并给指引，**不猜**。
  WALGIT_MCP_TOKEN  Bearer token（必填；不要把 token 放进命令行，它会进 ps）

## 退出码

  0  成功（stdout = entries 的 JSON 数组）
  3  缺凭据（调用方应回落到别的方式或 fail-closed）
  4  MCP 调用失败（网络 / 认证 / 工具错误）
"""

import json
import os
import subprocess
import sys
import urllib.error
import urllib.request

PROTOCOL_VERSION = "2024-11-05"


def _rpc(url: str, token: str, payload: dict, sid: str) -> tuple[list[dict], str]:
    """发一条 JSON-RPC；返回（解析出的消息列表，会话 id）。响应可能是 SSE 或纯 JSON。"""
    headers = {
        "Authorization": f"Bearer {token}",
        "Content-Type": "application/json",
        "Accept": "application/json, text/event-stream",
    }
    if sid:
        headers["Mcp-Session-Id"] = sid
    req = urllib.request.Request(
        url, data=json.dumps(payload).encode(), headers=headers, method="POST"
    )
    with urllib.request.urlopen(req, timeout=60) as resp:
        new_sid = resp.headers.get("Mcp-Session-Id") or sid
        raw = resp.read().decode("utf-8", "replace")

    msgs: list[dict] = []
    for line in raw.splitlines():
        line = line.strip()
        if line.startswith("data:"):
            line = line[5:].strip()
        if not line:
            continue
        try:
            msgs.append(json.loads(line))
        except json.JSONDecodeError:
            continue  # SSE 的心跳/注释行
    return msgs, new_sid


def fetch_thread(repo: str, thread_id: str, url: str, token: str) -> list[dict]:
    sid = ""
    # initialize 的响应头带 `Mcp-Session-Id`，后续每个请求都要回带它；
    # 这里必须接住返回值，否则第二个请求会 400 “Session ID is required”。
    _, sid = _rpc(
        url,
        token,
        {
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": {"name": "abb-review-cadence-gate", "version": "1"},
            },
        },
        sid,
    )
    msgs, _ = _rpc(
        url,
        token,
        {
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": {
                "name": "collab_thread",
                "arguments": {"repo": repo, "id": thread_id},
            },
        },
        sid,
    )
    for msg in msgs:
        if msg.get("id") != 2:
            continue
        if "error" in msg:
            raise RuntimeError(json.dumps(msg["error"], ensure_ascii=False))
        result = msg.get("result")
        if not isinstance(result, dict):
            continue
        if result.get("isError"):
            raise RuntimeError(json.dumps(result, ensure_ascii=False)[:500])
        content = result.get("content") or []
        if not content:
            raise RuntimeError("collab_thread 返回空 content")
        payload = json.loads(content[0]["text"])
        entries = payload.get("entries")
        if not isinstance(entries, list):
            raise RuntimeError("collab_thread 的 entries 不是数组")
        return entries
    raise RuntimeError("MCP 没有返回 id=2 的结果")


def derive_mcp_url(origin_url: str) -> str:
    """从 git remote 地址推导 MCP 端点；推不出来返回空串（调用方据此 fail-closed）。"""
    u = origin_url.strip()
    if not u:
        return ""
    u = u[:-4] if u.endswith(".git") else u
    u = u.rstrip("/")
    if "://" in u:
        scheme, rest = u.split("://", 1)
        host = rest.split("/", 1)[0]
        return f"{scheme}://{host}/mcp" if host else ""
    # scp 形态 git@host:owner/repo
    if "@" in u and ":" in u:
        host = u.split("@", 1)[1].split(":", 1)[0]
        return f"https://{host}/mcp" if host else ""
    return ""  # 本地路径 / 其它形态：不猜


def resolve_mcp_url() -> tuple[str, str]:
    """返回 (url, 来源说明)。显式 env 优先，其次从 origin 推导。"""
    explicit = os.environ.get("WALGIT_MCP_URL", "").strip()
    if explicit:
        return explicit, "WALGIT_MCP_URL"
    try:
        out = subprocess.run(
            ["git", "remote", "get-url", "origin"],
            capture_output=True, text=True, timeout=10,
        )
        origin = out.stdout.strip() if out.returncode == 0 else ""
    except (OSError, subprocess.SubprocessError):
        origin = ""
    url = derive_mcp_url(origin)
    if url:
        return url, f"origin={origin}"
    return "", origin


def main(argv: list[str]) -> int:
    if len(argv) != 3:
        print(f"用法: {argv[0]} <owner>/<repo> <thread-id>", file=sys.stderr)
        return 2
    repo, thread_id = argv[1], argv[2]
    token = os.environ.get("WALGIT_MCP_TOKEN", "").strip()
    if not token:
        print("缺 WALGIT_MCP_TOKEN（MCP 需要 Bearer 凭据）", file=sys.stderr)
        return 3
    url, src = resolve_mcp_url()
    if not url:
        print(
            "无法确定 MCP 端点：既没有 WALGIT_MCP_URL，也无法从 origin 推导"
            + (f"（origin={src!r}）" if src else "（没有 origin remote）"),
            file=sys.stderr,
        )
        print("  ↳ 显式设置：export WALGIT_MCP_URL=https://<host>/mcp", file=sys.stderr)
        return 4
    try:
        entries = fetch_thread(repo, thread_id, url, token)
    except urllib.error.HTTPError as e:
        body = e.read().decode("utf-8", "replace")[:300]
        print(f"MCP HTTP {e.code}: {body}", file=sys.stderr)
        return 4
    except (urllib.error.URLError, RuntimeError, json.JSONDecodeError) as e:
        print(f"MCP 调用失败: {e}", file=sys.stderr)
        return 4
    json.dump(entries, sys.stdout, ensure_ascii=False)
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
