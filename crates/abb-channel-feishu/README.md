# abb-channel-feishu

飞书渠道的独立 MCP server：把 ABB 内置的飞书渠道（`src/feishu.rs`，2206 行）搬到**独立进程**，
通过 stdio 上的 MCP 契约与 ABB 总线对话。

## 1. 它是什么 / 与内置实现的关系

**一句话**：一个只讲 MCP 的飞书渠道服务，ABB 作为 MCP client 拉起它，渠道逻辑一行没改、只换了进程边界。

- **等价搬迁，不是重写**：协议层（`src/feishu.rs`：`tenant_access_token`、三条出站 HTTPS
  JSON/multipart、卡片 markdown 清洗、WS 长连接帧编解码 `src/proto.rs` / 连接 `src/wsin.rs`）整份搬来。
- **为什么飞书先于钉钉搬**：依赖面最小。出站只要 token + 普通 HTTPS，没有钉钉那套加签（HMAC）
  与回调加解密（AES）⇒ 协议层可以整份搬，只换依赖。
- **可逆**：进程边界换掉了，实现还在；需要时可以搬回去。

## 2. 契约（实测）

入站走 **`channel://inbox` 资源订阅**，**不是 `poll`**：飞书入站是服务端推的 WS 长连接，
本地没有「按游标取一批」的原语，硬做成 `poll` 只会空转或批量延迟。
（裁决记录：Jev `subscribe_push` p=0.81 / confidence=0.62，rid `gen-dec-1793692596-P6YzJvS4Qo1baj0dfbY8`。）

`tools/list` 实跑输出（3 个工具）：

```
send_text, send_thread_reply, send_attachment
```

| 工具 | 说明 |
|---|---|
| `send_text` | 发文本（`{chat_id, text}`；`chat_id` 是飞书 `oc_…`）。超长自动分段，卡片失败自动降级纯文本 |
| `send_thread_reply` | **在指定消息的话题里回复**（`{chat_id, message_id, text}`，走 `/im/v1/messages/:id/reply`）。飞书有话题概念，声明它才不会被拍平到主聊天 |
| `send_attachment` | 发媒体（`{chat_id, name, data_b64}` + 可选 `mime`/`kind`），图片走内联 `msg_type=image`、其它走文件卡片；判定看**扩展名**而不只看 `kind`（飞书 images 端点对格式有硬校验） |

`resources/list` 实跑输出（1 个资源）：

```
channel://inbox
```

入站流程：WS 收到事件 → 进程内缓冲 → 发 `notifications/resources/updated` → ABB 调
`resources/read` 取走。`resources/read` 把缓冲**整批**返回并清空（`text` 里是数组，可能 0 条）。

- `resources/subscribe` 只认 `channel://inbox`，别的 URI **报错而不是静默成功**——
  ABB 会把「订阅成功」当成入站可用，答应了不存在的资源就会表现成「渠道在线但收不到消息」。
- `resources/read` 读错 URI 同样 fail-closed：读错资源等于把别的内容当消息交出去。
- 订阅响应里若缓冲已有消息，通知在**响应之后**才补发。

`initialize` 实跑回包：

```json
{"jsonrpc":"2.0","id":1,"result":{"capabilities":{"resources":{"listChanged":false,"subscribe":true},"tools":{}},"protocolVersion":"2024-11-05","serverInfo":{"name":"abb-channel-feishu","version":"0.1.0"}}}
```

其中 `capabilities.resources.subscribe = true` 是**必须声明**的：ABB 据它选入站路线。

## 3. 环境变量

```sh
grep -ohE 'env::var\("[A-Z_]+"\)' crates/abb-channel-feishu/src/*.rs | sort -u
```

实测清单（2 个，`wsin.rs` 与 `main.rs` 用的是同一对）：

```
env::var("FS_APP_ID")
env::var("FS_APP_SECRET")
```

| 变量 | 必需性 | 用途 |
|---|---|---|
| `FS_APP_ID` | **必需** | 飞书开放平台应用的 App ID。启动时打 stderr 会**脱敏**（只留头尾各 4 字符） |
| `FS_APP_SECRET` | **必需** | 应用 App Secret（换 `tenant_access_token` 与 WS 连接凭证）。stderr 只报「已设置/空」 |

两者缺任一个：打印一行 stderr 提示，**进程照常起、契约层照常应答**（`tools/list` 可用），
但入站 WS 长连接不启动、出站调用会被飞书拒。这是有意的——契约层不该因为凭证没配就起不来。

命令行参数：**无**，且**忽略**任何参数（`main.rs` 里没有 `std::env::args`；传 `--dir` 也不报错但不起作用）。

## 4. 怎么配进 ABB

ABB 不认识 `FS_*`，只把 `env` 原样转发给子进程（依据 `docs/mcp-bus-architecture.md` §4.1）。

`config.json` 里 `bots[]` 的一项：

```jsonc
{
  "name": "我的飞书",
  "kind": "mcp",
  "enabled": true,
  "mcp_channel": {
    "name": "feishu",
    "command": "/path/to/abb-channel-feishu",
    "args": [],
    "env": [
      { "name": "FS_APP_ID",     "value": "cli_…" },
      { "name": "FS_APP_SECRET", "value": "…" }
    ]
  }
}
```

界面里等价操作：「机器人配置」页选一个 bot、类型选 `mcp`，填三行
**可执行文件** / **参数** / **环境变量**（每行 `KEY=VALUE`）。

凭证来自开放平台后台，直接填 `env` 即可——**没有微信那步扫码**。

## 5. 怎么单独跑

构建（自带 manifest + lock，不受 root workspace 约束）：

```sh
export TMPDIR=/Volumes/DataExt/tmp
export CARGO_TARGET_DIR=/Volumes/DataExt/tmp/abb-target-channels
cargo build --release --manifest-path crates/abb-channel-feishu/Cargo.toml
# 产物：$CARGO_TARGET_DIR/release/abb-channel-feishu
```

最小 `initialize` 请求（stdin 一行 NDJSON，stdout 一行回包）：

```sh
echo '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"probe","version":"0"}}}' \
  | ./abb-channel-feishu
```

看能力与入站资源：

```sh
printf '%s\n%s\n' \
  '{"jsonrpc":"2.0","id":2,"method":"tools/list"}' \
  '{"jsonrpc":"2.0","id":3,"method":"resources/list"}' \
  | ./abb-channel-feishu
```

订阅并取走入站（`stdout` **只出 JSON-RPC**，日志一律 stderr）：

```sh
printf '%s\n%s\n' \
  '{"jsonrpc":"2.0","id":4,"method":"resources/subscribe","params":{"uri":"channel://inbox"}}' \
  '{"jsonrpc":"2.0","id":5,"method":"resources/read","params":{"uri":"channel://inbox"}}' \
  | FS_APP_ID=cli_… FS_APP_SECRET=… ./abb-channel-feishu
```

## 测试

```sh
cargo test --manifest-path crates/abb-channel-feishu/Cargo.toml
```

- **单测**（`src/main.rs` 底部）：三条出站工具的声明
  （`tools_list_declares_all_three_outbound_tools`）、`resources/list` 只声明 inbox、
  订阅后 `resources/read` 返回 JSON 数组、未知 URI 被拒、`inbox` 有上限且丢最旧、
  `send_thread_reply` 缺 `message_id` 报错、附件参数校验与扩展名判定、通知不回包。
- **契约测试**（`tests/contract.rs`，驱动真二进制）：`stdout` 是纯 JSON-RPC、
  三条出站工具、inbox 资源可声明且可读、读未知资源报错、未知方法报错。

协议层本身（`src/feishu.rs`）**不自带单测**：原文件末尾的测试验的是 ABB 侧的 mock server 与
bridge 接线，与只做 MCP server 的本进程无关（见该文件头第 11–13 行）。
