#!/usr/bin/env python3
"""最小 MCP server（stdio，行分隔 JSON-RPC 2.0），供探针使用。

只实现一次真实工具往返所需的四步：`initialize` → `notifications/initialized`
→ `tools/list` → `tools/call`。工具名叫 `echo_query`，它会：把调用参数原样回显，
并把「被调用了」这件事追加进 `--record <path>` 指定的文件——**那是探针唯一的
「工具真的被调用过」的证据**（只看 agent 的文本输出无法区分「工具被调用」与
「模型自己编了答案」）。

用法：`python3 fake_mcp_server.py --record /tmp/mcp-calls.jsonl`

另有两个给「环境/工作目录隔离」与「装配预算」探针用的开关：

- `--dump-env <path>`：启动时把自己的 **cwd 与全量环境** 写成 JSON，供探针断言
  「白名单 `/ spec.env` 生效」与「供应商凭据没被继承」；
- `--hang-init`：收到 `initialize` 后**永不回包**，用来把装配预算耗满
  （验证这期间读循环仍然可读、且 `session/new` 会在预算内收尾）；
- `--image-data <base64>`：`tools/call` 的结果里除文本外再带一个 `image/png` 内容块
  （验证工具结果里的图片真能到模型，而不是被降级成 base64 正文或占位文本）。
"""
import json
import os
import sys
import time

TOOL_NAME = "echo_query"


def record(path, entry):
    if not path:
        return
    with open(path, "a", encoding="utf-8") as fh:
        fh.write(json.dumps(entry, ensure_ascii=False) + "\n")


def dump_env(path):
    """把启动时的 cwd 与全量环境落盘（探针据此断言隔离是否生效）。"""
    if not path:
        return
    with open(path, "w", encoding="utf-8") as fh:
        json.dump({"cwd": os.getcwd(), "env": dict(os.environ)}, fh, ensure_ascii=False)


def main():
    record_path = None
    dump_path = None
    image_data = None
    hang_init = False
    argv = sys.argv[1:]
    for i, item in enumerate(argv):
        if item == "--record" and i + 1 < len(argv):
            record_path = argv[i + 1]
        elif item == "--dump-env" and i + 1 < len(argv):
            dump_path = argv[i + 1]
        elif item == "--image-data" and i + 1 < len(argv):
            image_data = argv[i + 1]
        elif item == "--hang-init":
            hang_init = True

    dump_env(dump_path)

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
            if hang_init:
                # 永不回包：把 agent 的装配预算耗满（读循环必须仍然可读）。
                time.sleep(3600)
                continue
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
            content = [
                {"type": "text", "text": f"echo: {json.dumps(arguments, ensure_ascii=False)}"}
            ]
            if image_data:
                content.append({"type": "image", "data": image_data, "mimeType": "image/png"})
            reply = {
                "jsonrpc": "2.0",
                "id": msg_id,
                "result": {"content": content, "isError": False},
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
