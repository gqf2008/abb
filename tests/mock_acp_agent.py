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
        send({"jsonrpc": "2.0", "id": rid,
              "result": {"protocolVersion": 1, "agentCapabilities": {}}})
    elif method == "session/new":
        session_counter += 1
        sid = f"mock-ses-{session_counter}"
        record({"event": "session_new", "cwd": params.get("cwd"), "sessionId": sid})
        send({"jsonrpc": "2.0", "id": rid,
              "result": {"sessionId": sid, "modes": {"currentModeId": "default",
                          "availableModes": [{"id": "default", "name": "default"}]}}})
    elif method == "session/prompt":
        chunks = params.get("prompt", [])
        text = "".join(c.get("text", "") for c in chunks if isinstance(c, dict))
        record({"event": "prompt", "sessionId": params.get("sessionId"), "text": text})
        sid = params.get("sessionId")
        send({"jsonrpc": "2.0", "method": "session/update", "params": {
            "sessionId": sid, "update": {"sessionUpdate": "agent_message_chunk",
                                          "content": {"type": "text", "text": f"echo: {text}"}}}})
        send({"jsonrpc": "2.0", "id": rid,
              "result": {"stopReason": "end_turn"}})
    elif method == "session/cancel":
        record({"event": "cancel", "sessionId": params.get("sessionId")})
    elif rid is not None:
        send({"jsonrpc": "2.0", "id": rid, "error": {"code": -32601, "message": "not found"}})
