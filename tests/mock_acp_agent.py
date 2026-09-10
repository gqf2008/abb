#!/usr/bin/env python3
"""最小 ACP stdio agent（测试 fixture）：initialize/session/new/session/prompt。
把收到的 prompt 原文追加记录到 MOCK_RECORD_FILE（测试断言用），并 echo 回复。
协议：JSON-RPC 2.0 over stdin/stdout，行分隔。"""
import json, os, sys, time

rec_file = os.environ.get("MOCK_RECORD_FILE", "/tmp/mock-acp-record.jsonl")
with open(rec_file, "a") as _f:
    _f.write(json.dumps({"event": "started"}) + "\n")

def record(entry):
    with open(rec_file, "a") as f:
        f.write(json.dumps(entry, ensure_ascii=False) + "\n")

def send(msg):
    sys.stdout.write(json.dumps(msg, ensure_ascii=False) + "\n")
    sys.stdout.flush()

session_counter = 0

for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    try:
        req = json.loads(line)
    except json.JSONDecodeError:
        continue
    method = req.get("method", "")
    rid = req.get("id")
    params = req.get("params", {}) or {}
    if method == "initialize":
        # 能力位与真实 fork 对齐（crates/buzz-agent/src/lib.rs）：响应**顶层**
        # `_meta.abbSandbox` 声明支持的档位词表 ⇒ ABB 认 `_meta.sandbox/shell`
        # 并允许受限会话。MOCK_NO_ABB_SANDBOX=1 模拟「旧 fork 未声明」——
        # P2.3 硬闸回归锁用；MOCK_ABB_SANDBOX_MODES 覆盖词表（逗号分隔）——
        # P1-3 词表校验回归锁用。
        result = {"protocolVersion": 1, "agentCapabilities": {}}
        if not os.environ.get("MOCK_NO_ABB_SANDBOX"):
            modes = os.environ.get("MOCK_ABB_SANDBOX_MODES")
            result["_meta"] = {"abbSandbox": (modes.split(",") if modes
                                              else ["read-only", "workspace-write",
                                                    "full-access"])}
        send({"jsonrpc": "2.0", "id": rid, "result": result})
    elif method == "session/new":
        session_counter += 1
        sid = f"mock-ses-{session_counter}"
        record({"event": "session_new", "cwd": params.get("cwd"), "sessionId": sid,
                "meta": params.get("_meta")})
        send({"jsonrpc": "2.0", "id": rid,
              "result": {"sessionId": sid, "modes": {"currentModeId": "default",
                          "availableModes": [{"id": "default", "name": "default"}]}}})
    elif method == "session/prompt":
        chunks = params.get("prompt", [])
        text = "".join(c.get("text", "") for c in chunks if isinstance(c, dict))
        record({"event": "prompt", "sessionId": params.get("sessionId"), "text": text})
        sid = params.get("sessionId")
        if os.environ.get("MOCK_HANG_PROMPT"):
            # P3.1 oneshot Timeout 路径：只记录不应答（session/cancel 照收，
            # 排水宽限由 ABB 侧兜底）。
            continue
        if os.environ.get("MOCK_AUTH_ERROR"):
            # P3.1 oneshot Failed 路径：401 认证错误属不可重试终态 → ABB 侧
            # 立即死信（不重试），同步等待者应收 Err 而非挂到超时。
            send({"jsonrpc": "2.0", "id": rid,
                  "error": {"code": -32000,
                            "message": "API Error: 401 authentication failed"}})
            continue
        respond = os.environ.get("MOCK_RESPOND_JSON")
        if respond is not None:
            # P3.4 teambuilder 全链路：不 echo，以该 env 值作为助手文本原样
            # 应答（记录照常）——合法/非法方案 JSON 两例驱动 extract_json 剥
            # fence + schema 校验。
            send({"jsonrpc": "2.0", "method": "session/update", "params": {
                "sessionId": sid, "update": {"sessionUpdate": "agent_message_chunk",
                                              "content": {"type": "text", "text": respond}}}})
            send({"jsonrpc": "2.0", "id": rid,
                  "result": {"stopReason": "end_turn"}})
            continue
        send({"jsonrpc": "2.0", "method": "session/update", "params": {
            "sessionId": sid, "update": {"sessionUpdate": "agent_message_chunk",
                                          "content": {"type": "text", "text": f"echo: {text}"}}}})
        send({"jsonrpc": "2.0", "id": rid,
              "result": {"stopReason": "end_turn"}})
    elif method == "session/cancel":
        record({"event": "cancel", "sessionId": params.get("sessionId")})
    elif rid is not None:
        send({"jsonrpc": "2.0", "id": rid, "error": {"code": -32601, "message": "not found"}})
