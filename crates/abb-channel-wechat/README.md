# abb-channel-wechat

微信渠道的独立 MCP server：把 ABB 内置的微信渠道（`src/wechat.rs`，1640 行）搬到**独立进程**，
通过 stdio 上的 MCP 契约与 ABB 总线对话。

## 1. 它是什么 / 与内置实现的关系

**一句话**：一个只讲 MCP 的微信渠道服务，ABB 作为 MCP client 拉起它，渠道逻辑一行没改、只换了进程边界。

- **等价搬迁，不是重写**：协议层（`src/weixin.rs`：`get_updates` 长轮询、扫码登录、媒体加解密与上传）
  整份搬自 `src/wechat.rs`；`Cargo.toml` 里的依赖版本与 ABB root 对齐，搬家不改行为。
- **对 ABB 内部的依赖已归零**：原来只用到 5 个小工具（`truncate` / `log` / `unix_secs` /
  `mime_from_name` / `bridge_dir`），本 crate 自备。
- **一个渠道特例被消灭**：微信 `send_text` 需要 `context_token`，而 MCP 契约只有 `{chat_id, text}`。
  每条入站消息都带 `context_token` ⇒ 本进程入站时自己记进 map。这正是原 ABB 里
  `Messenger::note_context` 那个渠道特例的归宿——搬出去之后 ABB 不再需要它。
- **可逆**：进程边界换掉了，实现还在；需要时可以搬回去。

## 2. 契约（实测）

入站走 **`poll`**（不用资源订阅），因为微信的 `get_updates(cursor)` 天然就是游标长轮询。

`tools/list` 实跑输出（5 个工具，顺序即声明顺序）：

```
send_text, poll, send_attachment, login, login_status
```

| 工具 | 说明 |
|---|---|
| `send_text` | 发文本（`{chat_id, text}`；`chat_id` = 微信 user id）。本进程自动带上记下的 `context_token` |
| `poll` | 取入站消息（可选 `{cursor, limit}`，`limit` 默认 100）。返回 `{messages, next_cursor}`；**游标只在成功后推进**，失败却推进等于永久丢消息 |
| `send_attachment` | 发媒体（`{chat_id, name, data_b64}` + 可选 `mime`/`kind`），内容 base64 内联 |
| `login` | 扫码登录：二维码 PNG 写到 `--dir`，返回 `qrcode_path` |
| `login_status` | 轮询扫码状态；`confirmed` 时把 `WX_TOKEN`/`WX_BASE_URL`/`WX_USER_ID` **打到 stderr**（不放进工具结果，避免凭证落进会话历史） |

**刻意不声明的东西**（有单测 `tools_list_declares_exactly_what_we_support` 守着）：

- **`send_thread_reply`**：微信没有话题概念。不声明，ABB 才会自动回落 `send_text`。
- **`resources`**：本进程入站走 `poll`，`resources/list` 返回**空数组**（不是 `null`），
  ABB 据此选路。

`initialize` 实跑回包：

```json
{"jsonrpc":"2.0","id":1,"result":{"capabilities":{"resources":{},"tools":{}},"protocolVersion":"2024-11-05","serverInfo":{"name":"abb-channel-wechat","version":"0.1.0"}}}
```

## 3. 环境变量

```sh
grep -ohE 'env::var\("[A-Z_]+"\)' crates/abb-channel-wechat/src/*.rs | sort -u
```

实测清单（4 个）：

```
env::var("WX_BASE_URL")
env::var("WX_CDN_BASE_URL")
env::var("WX_TOKEN")
env::var("WX_USER_ID")
```

| 变量 | 必需性 | 用途 |
|---|---|---|
| `WX_TOKEN` | **必需**（扫码前可空） | ilink 会话凭证。空则只打一行 stderr 提示、进程照常起（此时应先调 `login` 扫码） |
| `WX_BASE_URL` | 可选 | 微信 API 根。空/缺省回落 `https://ilinkai.weixin.qq.com`（`weixin::FIXED_BASE_URL`） |
| `WX_CDN_BASE_URL` | 可选 | 媒体 CDN 根。空/缺省回落 `https://novac2c.cdn.weixin.qq.com/c2c`（`weixin::DEFAULT_CDN_BASE_URL`） |
| `WX_USER_ID` | **必需**（扫码后可拿） | 自己的 wx user id。用于**防自循环**：`poll` 里丢掉 `from_user_id == WX_USER_ID` 的消息。扫码成功后进程内也会自动更新它 |

命令行参数：**只有本渠道需要 `--dir`**。

| 参数 | 用途 |
|---|---|
| `--dir <path>` / `--dir=<path>` | 二维码与登录态输出目录，默认当前目录 `.`。二维码落地为 `<dir>/wechat-qr.png`（目录不存在会自动创建） |

凭证**不是一开始就有的**——微信要扫码：

1. 先只配 `command` + `args`，`env` 留空；
2. 调 `login` → `<dir>/wechat-qr.png`，用微信扫；
3. 调 `login_status` → `confirmed` 时三行凭证打印在 **stderr**；
4. 把三行填进 `mcp_channel.env`，重启 bot。

`token` 过期时（实测，非推断）：微信返回 `errcode -14`，`poll` 明确回
`poll 失败：ilink 会话超时（errcode -14），需重新扫码登录` —— 而不是回一个空消息列表让人
以为「没人说话」。

## 4. 怎么配进 ABB

无 `--dir` 以外的渠道专属字段：ABB 不认识 `WX_*`，只把 `env` 原样转发给子进程
（依据 `docs/mcp-bus-architecture.md` §4.1）。

`config.json` 里 `bots[]` 的一项：

```jsonc
{
  "name": "我的微信",
  "kind": "mcp",
  "enabled": true,
  "mcp_channel": {
    "name": "wechat",
    "command": "/path/to/abb-channel-wechat",
    "args": ["--dir", "/tmp/wx-channel"],
    "env": [
      { "name": "WX_TOKEN",    "value": "…" },
      { "name": "WX_BASE_URL", "value": "https://ilinkai.weixin.qq.com" },
      { "name": "WX_USER_ID",  "value": "…" }
    ]
  }
}
```

界面里等价操作：「机器人配置」页选一个 bot、类型选 `mcp`，填三行
**可执行文件** / **参数** / **环境变量**（每行 `KEY=VALUE`）。

## 5. 怎么单独跑

构建（自带 manifest + lock，不受 root workspace 约束；中间产物外置到数据卷）：

```sh
export TMPDIR=/Volumes/DataExt/tmp
export CARGO_TARGET_DIR=/Volumes/DataExt/tmp/abb-target-channels
cargo build --release --manifest-path crates/abb-channel-wechat/Cargo.toml
# 产物：$CARGO_TARGET_DIR/release/abb-channel-wechat
```

最小 `initialize` 请求（stdin 一行 NDJSON，stdout 一行回包）：

```sh
echo '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"probe","version":"0"}}}' \
  | ./abb-channel-wechat --dir /tmp/wx-channel
```

配合 `tools/list` 一起看能力（stdout **只出 JSON-RPC**，任何杂输出都会让 ABB 侧解析炸掉；
日志一律走 stderr）：

```sh
printf '%s\n%s\n' \
  '{"jsonrpc":"2.0","id":2,"method":"tools/list"}' \
  '{"jsonrpc":"2.0","id":3,"method":"resources/list"}' \
  | ./abb-channel-wechat --dir /tmp/wx-channel
```

扫码登录：

```sh
echo '{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"login","arguments":{}}}' \
  | ./abb-channel-wechat --dir /tmp/wx-channel
# 二维码在 /tmp/wx-channel/wechat-qr.png，扫完再：
echo '{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"login_status","arguments":{}}}' \
  | WX_TOKEN=… ./abb-channel-wechat --dir /tmp/wx-channel
```

## 测试

```sh
cargo test --manifest-path crates/abb-channel-wechat/Cargo.toml
```

- **单测**（`src/main.rs` 底部）：工具/资源的**精确集合**（含「不许声明 `send_thread_reply`」
  的反向断言）、`resources/list` 是空数组不是 `null`、自己发的不回灌（`own_message_is_ignored`）、
  入站记录 `context_token` 与空发送者被丢、**token 不出现**在工具结果里
  （`token_never_appears_in_tool_result`）、未 `login` 就 `login_status` 报错、
  base64 往返与非法/缺参报错、未知方法报错。
- **契约测试**（`tests/contract.rs`，驱动真二进制）：`stdout` 是纯 JSON-RPC、
  `satisfies_channel_contract`、未知方法报错。
