#!/usr/bin/env python3
"""ABB 渠道 MCP server —— **可用的参考实现**（不是测试桩）。

与 `tests/fake_mcp_channel.py` 的区别：那个是端到端测试用的桩（预置消息、只回固定
字符串）；这个是**真的能收发**的渠道，用来证明总线不是纸上架构。

它把「一个目录」当成渠道：

    收件箱  <dir>/inbox/*.json     每个文件一条入站消息（读完移到 <dir>/done/）
    发件箱  <dir>/outbox/*.txt     每次 send_text 写一个文件（内容 = "chat_id\\n正文"）

于是不需要任何外部服务（不需要 IM 账号、不需要网络），就能：
- 从别的进程/脚本投递消息给 ABB（写一个 json 文件即可）
- 观察 ABB 回复了什么（看 outbox）

这既是**可用的渠道**，也是接入真实渠道时的**模板**：把它换成"读某个 IM 的 API"、
把 `send_text` 换成"调某个 IM 的发送接口"，其余（协议帧、能力自报、游标、通知）
一行都不用改。

## 为什么它同时实现了两条入站路径

契约允许订阅或轮询二选一。这里**两条都给**（并默认走订阅）：
- 订阅：文件出现时推 `notifications/resources/updated`
- 轮询：`poll` 按文件名的字典序游标推进

这样它既能验证 A 方案，也能在资源面被关掉时靠 B 方案继续工作 —— 也顺便证明了
ABB 的「按能力选路」不是只有测试桩才走得到。

用法：
    python3 tools/channel-mcp-dir/channel_mcp_dir.py --dir /tmp/my-channel
    # 然后在 ABB 里把 bot 的 kind 设为 mcp，
    # mcp_channel = {name:"dir", command:"python3",
    #                args:["<repo>/tools/channel-mcp-dir/channel_mcp_dir.py",
    #                      "--dir","/tmp/my-channel"]}
"""
import argparse
import json
import os
import sys
import threading
import time


def log(msg):
    """诊断走 stderr —— stdout 是协议通道，绝不能污染。"""
    print(f"[channel-mcp-dir] {msg}", file=sys.stderr, flush=True)


class DirChannel:
    def __init__(self, root):
        self.root = root
        self.inbox = os.path.join(root, "inbox")
        self.done = os.path.join(root, "done")
        self.outbox = os.path.join(root, "outbox")
        for d in (self.inbox, self.done, self.outbox):
            os.makedirs(d, exist_ok=True)
        self.seq = 0
        self.lock = threading.Lock()

    # ── 入站 ──────────────────────────────────────────────

    def pending_files(self):
        """按名字排序（游标是字典序，稳定可重放）。"""
        try:
            names = [n for n in os.listdir(self.inbox) if n.endswith(".json")]
        except FileNotFoundError:
            return []
        return sorted(names)

    def take(self, cursor, limit):
        """取一批：cursor 是上一次最后一个文件名（不含）。"""
        names = self.pending_files()
        if cursor:
            names = [n for n in names if n > cursor]
        names = names[:limit]
        msgs = []
        last = cursor
        for n in names:
            path = os.path.join(self.inbox, n)
            try:
                with open(path, "r", encoding="utf-8") as f:
                    msg = json.load(f)
            except (OSError, json.JSONDecodeError) as e:
                # **坏消息不阻塞后面的**：移到 done 并记日志，而不是永远卡在这里
                # （卡住会让这个渠道看起来"在线但没反应"，最难查）。
                log(f"跳过坏文件 {n}: {e}")
                self._move(n)
                continue
            msgs.append(msg)
            last = n
            self._move(n)
        return msgs, last

    def _move(self, name):
        try:
            os.replace(
                os.path.join(self.inbox, name), os.path.join(self.done, name)
            )
        except OSError as e:
            log(f"归档 {name} 失败: {e}")

    # ── 出站 ──────────────────────────────────────────────

    def send(self, chat_id, text, message_id=None):
        with self.lock:
            self.seq += 1
            n = f"{time.time():.6f}-{self.seq:04d}.txt"
            path = os.path.join(self.outbox, n)
            # 头两行是元信息，之后是正文（保持正文原样，不转义）
            with open(path, "w", encoding="utf-8") as f:
                f.write(json.dumps(
                    {"chat_id": chat_id, "message_id": message_id},
                    ensure_ascii=False) + "\n")
                f.write(text)
        log(f"send_text → {chat_id} ({len(text)} chars) → {n}")
        return n


TOOLS_SUBSCRIBE = [
    {"name": "send_text", "description": "发文本到会话",
     "inputSchema": {"type": "object",
                     "properties": {"chat_id": {"type": "string"},
                                    "text": {"type": "string"}},
                     "required": ["chat_id", "text"]}},
    {"name": "send_thread_reply", "description": "会话内话题回复",
     "inputSchema": {"type": "object",
                     "properties": {"chat_id": {"type": "string"},
                                    "message_id": {"type": "string"},
                                    "text": {"type": "string"}},
                     "required": ["chat_id", "message_id", "text"]}},
    {"name": "poll", "description": "取入站消息（B 方案）",
     "inputSchema": {"type": "object",
                     "properties": {"cursor": {"type": "string"},
                                    "limit": {"type": "integer"}}}},
]

TOOLS_NO_RESOURCES = [
    t for t in TOOLS_SUBSCRIBE if t["name"] != "send_thread_reply"
]


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--dir", required=True, help="渠道工作目录")
    ap.add_argument("--no-resources", action="store_true",
                    help="不暴露 resources（强制 ABB 走 poll 兜底）")
    ap.add_argument("--poll-interval", type=float, default=0.5,
                    help="订阅模式下轮询文件系统的间隔（秒）")
    args = ap.parse_args()

    ch = DirChannel(args.dir)
    use_resources = not args.no_resources
    log(f"启动 dir={args.dir} resources={use_resources}")

    def emit(obj):
        sys.stdout.write(json.dumps(obj, ensure_ascii=False) + "\n")
        sys.stdout.flush()

    # 订阅模式：盯着 inbox，发现新文件就推通知（**只带 uri，不带内容**，符合规范）
    def watcher():
        if not use_resources:
            return
        seen = set(ch.pending_files())
        while True:
            time.sleep(args.poll_interval)
            cur = set(ch.pending_files())
            if cur - seen:
                emit({"jsonrpc": "2.0",
                      "method": "notifications/resources/updated",
                      "params": {"uri": "channel://inbox"}})
            seen = cur

    threading.Thread(target=watcher, daemon=True).start()

    def handle(req):
        method = req.get("method")
        rid = req.get("id")
        params = req.get("params") or {}
        if rid is None:
            return None  # 通知不回

        if method == "initialize":
            return {"jsonrpc": "2.0", "id": rid, "result": {
                "protocolVersion": "2024-11-05",
                "capabilities": {"tools": {},
                                 "resources": {} if use_resources else {}},
                "serverInfo": {"name": "channel-mcp-dir", "version": "1.0.0"},
            }}
        if method == "tools/list":
            return {"jsonrpc": "2.0", "id": rid, "result": {
                "tools": TOOLS_SUBSCRIBE if use_resources else TOOLS_NO_RESOURCES}}
        if method == "resources/list":
            if not use_resources:
                return {"jsonrpc": "2.0", "id": rid, "result": {"resources": []}}
            return {"jsonrpc": "2.0", "id": rid, "result": {"resources": [
                {"uri": "channel://inbox", "name": "inbox",
                 "mimeType": "application/json"}]}}
        if method == "resources/subscribe":
            if not use_resources:
                return {"jsonrpc": "2.0", "id": rid, "error": {
                    "code": -32601, "message": "resources not supported"}}
            log(f"订阅 {params.get('uri')}")
            return {"jsonrpc": "2.0", "id": rid, "result": {}}
        if method == "resources/read":
            if params.get("uri") != "channel://inbox":
                return {"jsonrpc": "2.0", "id": rid, "error": {
                    "code": -32602, "message": "unknown uri"}}
            msgs, _ = ch.take(None, 100)
            return {"jsonrpc": "2.0", "id": rid, "result": {"contents": [
                {"uri": "channel://inbox", "mimeType": "application/json",
                 "text": json.dumps(msgs, ensure_ascii=False)}]}}
        if method == "tools/call":
            name = params.get("name")
            a = params.get("arguments") or {}
            if name == "send_text":
                ch.send(a.get("chat_id", ""), a.get("text", ""))
                return {"jsonrpc": "2.0", "id": rid,
                        "result": {"content": [{"type": "text", "text": "ok"}]}}
            if name == "send_thread_reply":
                ch.send(a.get("chat_id", ""), a.get("text", ""),
                        a.get("message_id"))
                return {"jsonrpc": "2.0", "id": rid,
                        "result": {"content": [{"type": "text", "text": "ok"}]}}
            if name == "poll":
                limit = int(a.get("limit") or 100)
                msgs, last = ch.take(a.get("cursor"), limit)
                return {"jsonrpc": "2.0", "id": rid, "result": {"content": [
                    {"type": "text", "text": json.dumps(
                        {"messages": msgs, "next_cursor": last},
                        ensure_ascii=False)}]}}
            return {"jsonrpc": "2.0", "id": rid, "error": {
                "code": -32601, "message": f"no tool {name}"}}
        return {"jsonrpc": "2.0", "id": rid, "error": {
            "code": -32601, "message": f"no method {method}"}}

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
