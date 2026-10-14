#!/usr/bin/env python3
"""最小 ACP stdio agent（测试 fixture）：initialize/session/new/session/prompt。
把收到的 prompt 原文追加记录到 MOCK_RECORD_FILE（测试断言用），并 echo 回复。
协议：JSON-RPC 2.0 over stdin/stdout，行分隔。"""
import json, os, select, sys, time

rec_file = os.environ.get("MOCK_RECORD_FILE", "/tmp/mock-acp-record.jsonl")
with open(rec_file, "a") as _f:
    _f.write(json.dumps({"event": "started"}) + "\n")

# 模拟「冷启动 / 高负载」下的慢启动：在应答 initialize 之前先睡一会儿。
# 供 #309 的端到端用例确定性复现「登记可见 ≠ 回合已在跑」的时序窗口。
_delay_ms = int(os.environ.get("MOCK_STARTUP_DELAY_MS", "0") or "0")
if _delay_ms > 0:
    time.sleep(_delay_ms / 1000.0)

def record(entry):
    with open(rec_file, "a") as f:
        f.write(json.dumps(entry, ensure_ascii=False) + "\n")

def send(msg):
    sys.stdout.write(json.dumps(msg, ensure_ascii=False) + "\n")
    sys.stdout.flush()

def emit_chunks(sid, session_update, body, n):
    """把 body 近似均分成 n 片，逐片发同 kind 的 session/update。

    E2 夹具：一个回合可以同时有**多片思考**与**多片正文**（验证 ABB 侧的同一条
    sink 按 kind 分段 + 客户端打字机补字）。n <= 1 时只发一片，与旧单条发送逐字节一致。
    """
    if not body:
        return
    n = max(1, int(n))
    step = max(1, (len(body) + n - 1) // n)
    for i in range(0, len(body), step):
        send({"jsonrpc": "2.0", "method": "session/update", "params": {
            "sessionId": sid,
            "update": {"sessionUpdate": session_update,
                       "content": {"type": "text", "text": body[i:i + step]}}}})

def handle_steer(req):
    """处理 ACP 跨适配层 steer 扩展 `_session/steering`（卡 d10 的插入路径）。

    真适配器（claude-agent-acp / codex-acp）在 initialize 里声明
    `_meta.steering.supported=true`，并回 `{outcome:"injected"}`。夹具照此实现，
    让 ABB 侧的 `try_native_steer` 拿到 Success（否则会回落到 cancel+merge，
    测到的就不是“插入”了）。
    """
    params = req.get("params", {}) or {}
    prompt = params.get("prompt", []) or []
    text = "".join(c.get("text", "") for c in prompt if isinstance(c, dict))
    record({"event": "steer", "sessionId": params.get("sessionId"), "text": text})
    if req.get("id") is not None:
        send({"jsonrpc": "2.0", "id": req.get("id"), "result": {"outcome": "injected"}})

def wait_reading_stdin(ms):
    """在“等待回合”期间继续读 stdin 并处理 steer/cancel。

    为什么必须这么做：mock 是单线程的。若只 `time.sleep`，`_session/steering` 那行
    会一直待在管道里、直到 prompt 应答之后才被读到，ABB 侧就永远拿不到 steer ack
    ——端到端夹具会退化成“插不进去”。用 select 轮询 stdin，边等边把 steer 处理掉。
    """
    deadline = time.time() + ms / 1000.0
    while True:
        remaining = deadline - time.time()
        if remaining <= 0:
            return
        ready, _, _ = select.select([sys.stdin], [], [], remaining)
        if not ready:
            return
        line = sys.stdin.readline()
        if not line:
            return
        line = line.strip()
        if not line:
            continue
        try:
            req = json.loads(line)
        except json.JSONDecodeError:
            continue
        method = req.get("method", "")
        if method == "_session/steering":
            handle_steer(req)
        elif method == "session/cancel":
            record({"event": "cancel",
                    "sessionId": (req.get("params") or {}).get("sessionId")})
        elif req.get("id") is not None:
            send({"jsonrpc": "2.0", "id": req.get("id"),
                  "error": {"code": -32601, "message": "not found"}})

# 主循环用显式 readline（不用 `for line in sys.stdin`）：延迟窗口里的
# `wait_reading_stdin` 也读同一个文件对象，两种读法交替时绝不能有各自的预读缓冲。
session_counter = 0

while True:
    line = sys.stdin.readline()
    if not line:
        break
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
        meta = {}
        if not os.environ.get("MOCK_NO_ABB_SANDBOX"):
            modes = os.environ.get("MOCK_ABB_SANDBOX_MODES")
            meta["abbSandbox"] = (modes.split(",") if modes
                                  else ["read-only", "workspace-write",
                                        "full-access"])
        # 卡 d10：MOCK_STEER=1 模拟“真支持 _session/steering 的适配器”
        #（claude-agent-acp / codex-acp）；默认**不声明**，对齐随包 abb-agent
        #（它如实报 supported=false）⇒ 走排队回落。
        if os.environ.get("MOCK_STEER"):
            meta["steering"] = {"supported": True}
        if meta:
            result["_meta"] = meta
        send({"jsonrpc": "2.0", "id": rid, "result": result})
    elif method == "session/new":
        session_counter += 1
        sid = f"mock-ses-{session_counter}"
        record({"event": "session_new", "cwd": params.get("cwd"), "sessionId": sid,
                "meta": params.get("_meta"), "mcpServers": params.get("mcpServers")})
        send({"jsonrpc": "2.0", "id": rid,
              "result": {"sessionId": sid, "modes": {"currentModeId": "default",
                          "availableModes": [{"id": "default", "name": "default"}]}}})
    elif method == "session/prompt":
        chunks = params.get("prompt", [])
        text = "".join(c.get("text", "") for c in chunks if isinstance(c, dict))
        record({"event": "prompt", "sessionId": params.get("sessionId"), "text": text})
        # 可选：记录 prompt 之后、应答之前再睡一会儿，造出确定的「回合已 in-flight
        # 但尚未收尾」窗口（#321 的「A 在跑 + B 排队」时序用例需要它；默认 0 不影响
        # 其它用例）。
        _prompt_delay_ms = int(os.environ.get("MOCK_PROMPT_DELAY_MS", "0") or "0")
        if _prompt_delay_ms > 0:
            # 卡 d10：延迟期间**继续读 stdin**（处理插入在途回合的 `_session/steering`），
            # 而不是盲睡——否则 steer 行会等到本回合结束才被读到，ABB 侧拿不到 ack。
            wait_reading_stdin(_prompt_delay_ms)
        sid = params.get("sessionId")
        # 可选：回合进行中由 agent **主动**发一条资源更新（模拟常驻 MCP 客户端读到
        # notifications/resources/updated 后的上报）。用途：验证 ABB 在回合内读到它、
        # 落成同一会话的一条入站消息（steer/下一回合），而不是丢掉。
        if os.environ.get("MOCK_RESOURCE_UPDATE"):
            # `MOCK_RESOURCE_BURST=N`：同一回合内连发 N 条（内容各异 ⇒ 同内容 -> 同 id 的 id 语义不适用；
# 真正合并它们的是 ABB 侧 (channel, server, uri) 的 10s 节流窗口，队列本身不按 id 去重）——
            # 用来验证 ABB 侧的 `(channel,server,uri)` 节流确实把窗口内的后续更新丢掉。
            burst = int(os.environ.get("MOCK_RESOURCE_BURST", "1") or "1")
            for i in range(burst):
                text = os.environ["MOCK_RESOURCE_UPDATE"] + (f"#{i}" if burst > 1 else "")
                record({"event": "resource_notify_sent", "sessionId": sid, "text": text})
                send({"jsonrpc": "2.0", "method": "_abb/unstable/resource/updated", "params": {
                    "sessionId": sid,
                    "server": os.environ.get("MOCK_RESOURCE_SERVER", "mockmcp"),
                    "uri": os.environ.get("MOCK_RESOURCE_URI", "mock://res"),
                    "text": text,
                }})
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
        # E2 端到端夹具：可选发多片思考（MOCK_THOUGHT_TEXT / MOCK_THOUGHT_CHUNKS）
        # 与多片正文（MOCK_TEXT_TEXT / MOCK_TEXT_CHUNKS）。默认不发思考、正文单片，
        # 与既有用例逐字节一致。
        if os.environ.get("MOCK_THOUGHT_TEXT") is not None:
            record({"event": "thought_sent", "sessionId": sid,
                    "text": os.environ["MOCK_THOUGHT_TEXT"]})
            emit_chunks(sid, "agent_thought_chunk",
                        os.environ["MOCK_THOUGHT_TEXT"],
                        os.environ.get("MOCK_THOUGHT_CHUNKS", "1"))
        body = os.environ.get("MOCK_TEXT_TEXT") or f"echo: {text}"
        emit_chunks(sid, "agent_message_chunk", body,
                    os.environ.get("MOCK_TEXT_CHUNKS", "1"))
        send({"jsonrpc": "2.0", "id": rid,
              "result": {"stopReason": "end_turn"}})
    elif method == "_session/steering":
        # 卡 d10：生成中插入在途回合（不在 prompt 延迟窗口内的正常到达路径）。
        handle_steer(req)
    elif method == "session/cancel":
        record({"event": "cancel", "sessionId": params.get("sessionId")})
    elif rid is not None:
        send({"jsonrpc": "2.0", "id": rid, "error": {"code": -32601, "message": "not found"}})
