#!/usr/bin/env python3
"""最小 MCP server（stdio，行分隔 JSON-RPC 2.0），供探针使用。

只实现一次真实工具往返所需的四步：`initialize` → `notifications/initialized`
→ `tools/list` → `tools/call`。工具名叫 `echo_query`，它会：把调用参数原样回显，
并把「被调用了」这件事追加进 `--record <path>` 指定的文件——**那是探针唯一的
「工具真的被调用过」的证据**（只看 agent 的文本输出无法区分「工具被调用」与
「模型自己编了答案」）。

用法：`python3 fake_mcp_server.py --record /tmp/mcp-calls.jsonl`
"""
import json
import sys

TOOL_NAME = "echo_query"


def record(path, entry):
    if not path:
        return
    with open(path, "a", encoding="utf-8") as fh:
        fh.write(json.dumps(entry, ensure_ascii=False) + "\n")


def main():
    record_path = None
    argv = sys.argv[1:]
    for i, item in enumerate(argv):
        if item == "--record" and i + 1 < len(argv):
            record_path = argv[i + 1]

    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue
        try:
            msg = json.loads(line)
        except json.JSONDecodeError:
            continue
        method = msg.get("method")
        msg_id = msg.get("id")

        if method == "initialize":
            reply = {
                "jsonrpc": "2.0",
                "id": msg_id,
                "result": {
                    "protocolVersion": "2024-11-05",
                    "capabilities": {"tools": {}},
                    "serverInfo": {"name": "fake-mcp", "version": "0.1.0"},
                },
            }
        elif method == "tools/list":
            reply = {
                "jsonrpc": "2.0",
                "id": msg_id,
                "result": {
                    "tools": [
                        {
                            "name": TOOL_NAME,
                            "description": "回显查询参数（探针用假工具）",
                            "inputSchema": {
                                "type": "object",
                                "properties": {"q": {"type": "string"}},
                                "required": ["q"],
                            },
                        }
                    ]
                },
            }
        elif method == "tools/call":
            params = msg.get("params") or {}
            arguments = params.get("arguments") or {}
            record(record_path, {"tool": params.get("name"), "arguments": arguments})
            reply = {
                "jsonrpc": "2.0",
                "id": msg_id,
                "result": {
                    "content": [
                        {"type": "text", "text": f"echo: {json.dumps(arguments, ensure_ascii=False)}"}
                    ],
                    "isError": False,
                },
            }
        elif method and method.startswith("notifications/"):
            continue  # 通知无需应答
        else:
            reply = {
                "jsonrpc": "2.0",
                "id": msg_id,
                "error": {"code": -32601, "message": f"未实现：{method}"},
            }

        sys.stdout.write(json.dumps(reply, ensure_ascii=False) + "\n")
        sys.stdout.flush()


if __name__ == "__main__":
    main()
