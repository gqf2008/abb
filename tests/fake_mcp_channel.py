#!/usr/bin/env python3
"""最小的**真 MCP server**（stdio + JSON-RPC 2.0），用于 abb 总线端到端验证。

它不依赖任何 MCP SDK —— 手写协议，正好用来验证「ABB 按规范理解对端」而不是
「ABB 按自己造的服务理解自己」。

暴露的契约（见 docs/mcp-bus-architecture.md §3）：
- 工具 `send_text`（必需）/ `send_thread_reply`（可选）/ `poll`（可选）
- 资源 `channel://inbox`（可选，配合 resources/subscribe）

用法：
    python3 tests/fake_mcp_channel.py --mode subscribe   # A 方案
    python3 tests/fake_mcp_channel.py --mode poll        # B 方案

收到的 `send_text` 会打印到 stdout（一行 JSON），便于校验端到端是否真的发出去。
"""
import json
import sys
import threading
import time

MODE = "subscribe"
STATE = {
    "cursor": 0,
    # 预置 3 条待取消息；取完为止
    "pending": [
        {"chat_id": "mcp-chat", "message_id": "m1", "sender_id": "u1",
         "sender_name": "A", "text": "第一条", "ts": 1793600001},
        {"chat_id": "mcp-chat", "message_id": "m2", "sender_id": "u1",
         "sender_name": "A", "text": "第二条", "ts": 1793600002},
        {"chat_id": "mcp-chat", "message_id": "m3", "sender_id": "u1",
         "sender_name": "A", "text": "第三条", "ts": 1793600003},
    ],
    "sent": [],
}


def log(msg):
    """诊断输出走 stderr —— stdout 是协议通道，绝不能污染。"""
    print(f"[fake-mcp] {msg}", file=sys.stderr, flush=True)


def tools_list():
    if MODE == "poll":
        # 轮询模式下**不暴露 resources**：验证 ABB 会正确退到 B 方案
        return {"tools": [{"name": "send_text"}, {"name": "poll"}]}
    return {"tools": [{"name": "send_text"}, {"name": "send_thread_reply"}]}


def resources_list():
    if MODE == "poll":
        return {"resources": []}
    return {"resources": [{"uri": "channel://inbox",
                           "name": "inbox", "mimeType": "application/json"}]}


def call_tool(params):
    name = params.get("name")
    args = params.get("arguments") or {}
    if name == "send_text":
        STATE["sent"].append({"chat_id": args.get("chat_id"), "text": args.get("text")})
        # 回显到 stderr 便于外部观测（stdout 是协议通道）
        log(f"send_text → chat={args.get('chat_id')} text={args.get('text')!r}")
        return {"content": [{"type": "text", "text": "ok"}]}
    if name == "send_thread_reply":
        log(f"send_thread_reply → {args}")
        return {"content": [{"type": "text", "text": "ok"}]}
    if name == "poll":
        # 游标语义：只推进不倒退；取完返回空
        cur = int(args.get("cursor") or 0)
        limit = int(args.get("limit") or 100)
        msgs = STATE["pending"][cur:cur + limit]
        nxt = cur + len(msgs)
        return {"content": [{"type": "text", "text": json.dumps(
            {"messages": msgs, "next_cursor": str(nxt)})}]}
    return None


def read_resource(params):
    uri = params.get("uri")
    if uri != "channel://inbox":
        return None
    # A 方案：一次把待取消息放进 contents[0].text（JSON 数组），然后清空
    msgs = STATE["pending"]
    STATE["pending"] = []
    return {"contents": [{"uri": uri, "mimeType": "application/json",
                          "text": json.dumps(msgs, ensure_ascii=False)}]}


def handle(req):
    """返回 response dict，或 None（通知不需要回）。"""
    method = req.get("method")
    rid = req.get("id")
    params = req.get("params") or {}
    if rid is None:
        # 通知：不回复
        return None

    if method == "initialize":
        return {"jsonrpc": "2.0", "id": rid, "result": {
            "protocolVersion": "2024-11-05",
            "capabilities": {"tools": {}, "resources": {}},
            "serverInfo": {"name": "fake-mcp-channel", "version": "0.1.0"},
        }}
    if method == "tools/list":
        return {"jsonrpc": "2.0", "id": rid, "result": tools_list()}
    if method == "resources/list":
        return {"jsonrpc": "2.0", "id": rid, "result": resources_list()}
    if method == "resources/subscribe":
        if MODE == "poll":
            return {"jsonrpc": "2.0", "id": rid,
                    "error": {"code": -32601, "message": "no resources"}}
        log(f"subscribed {params.get('uri')}")
        return {"jsonrpc": "2.0", "id": rid, "result": {}}
    if method == "resources/read":
        r = read_resource(params)
        if r is None:
            return {"jsonrpc": "2.0", "id": rid,
                    "error": {"code": -32602, "message": "unknown uri"}}
        return {"jsonrpc": "2.0", "id": rid, "result": r}
    if method == "tools/call":
        r = call_tool(params)
        if r is None:
            return {"jsonrpc": "2.0", "id": rid,
                    "error": {"code": -32601, "message": f"no tool {params.get('name')}"}}
        return {"jsonrpc": "2.0", "id": rid, "result": r}

    return {"jsonrpc": "2.0", "id": rid,
            "error": {"code": -32601, "message": f"no method {method}"}}


def emit(obj):
    sys.stdout.write(json.dumps(obj, ensure_ascii=False) + "\n")
    sys.stdout.flush()


def main():
    global MODE
    for a in sys.argv[1:]:
        if a.startswith("--mode="):
            MODE = a.split("=", 1)[1]
    log(f"启动 mode={MODE}")

    def notifier():
        """A 方案：订阅后推一条 updated 通知（模拟渠道来新消息）。"""
        if MODE != "subscribe":
            return
        time.sleep(0.8)
        emit({"jsonrpc": "2.0", "method": "notifications/resources/updated",
              "params": {"uri": "channel://inbox"}})
        log("推送 notifications/resources/updated")
        # 再推一次同样的通知：验证 ABB 不会重复 read / 重复投递
        time.sleep(0.5)
        emit({"jsonrpc": "2.0", "method": "notifications/resources/updated",
              "params": {"uri": "channel://inbox"}})
        log("再推一次相同通知")

    threading.Thread(target=notifier, daemon=True).start()

    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue
        try:
            req = json.loads(line)
        except json.JSONDecodeError as e:
            log(f"非法 JSON: {e}")
            continue
        resp = handle(req)
        if resp is not None:
            emit(resp)
    log("stdin 关闭，退出")


if __name__ == "__main__":
    main()
