# abb-channel-dingtalk

钉钉渠道的独立 MCP server：把 ABB 内置的钉钉渠道（`src/dingtalk.rs`，1764 行）搬到**独立进程**，
通过 stdio 上的 MCP 契约与 ABB 总线对话。

## 1. 它是什么 / 与内置实现的关系

**一句话**：一个只讲 MCP 的钉钉渠道服务，ABB 作为 MCP client 拉起它，渠道逻辑一行没改、只换了进程边界。

- **等价搬迁，不是重写**：协议层（`src/dingtalk.rs`：`open_connection` 换一次性 ticket 的 Stream 长连接、
  v1.0 OpenAPI + 旧网关 topapi、媒体上传、群管理）整份搬来。加签（HMAC）与回调加解密（AES）
  属于**回调模式**，Stream 模式下不经过它们 ⇒ 本 crate 既不带 `hmac` 也不带 `aes`。
- **与飞书版同构**：入站都是服务端推的长连接；差别只在出站工具集（见 §2）。
- **可逆**：进程边界换掉了，实现还在；需要时可以搬回去。

## 2. 契约（实测）

入站走 **`channel://inbox` 资源订阅**，**不是 `poll`**：钉钉 Stream 是服务端推长连接
（CALLBACK 帧），本地没有「按游标取一批」的原语，硬做成 `poll` 只会空转。

`tools/list` 实跑输出（2 个工具）：

```
send_text, send_attachment
```

| 工具 | 说明 |
|---|---|
| `send_text` | 发文本（`{chat_id, text}`）。`chat_id` 语义按前缀分叉：群聊 = `openConversationId`（`cid…`）、单聊 = 对方 `staffId`。超长按 8000 字符分段 |
| `send_attachment` | 发媒体（`{chat_id, name, data_b64}` + 可选 `mime`/`kind`）：图片走 `sampleImageMsg`、其它走 `sampleFile`。判定看**扩展名**而不只看 `kind`（`media/upload` 按后缀校验，固定名 `"image"` 会被判 `40004 不合法的媒体文件类型`） |

**刻意不声明 `send_thread_reply`**（有单测 `tools_list_declares_two_tools_and_no_thread_reply` 守着）：
钉钉没有话题概念，声明它只会让 ABB 走到必然失败的路径。

`resources/list` 实跑输出（1 个资源）：

```
channel://inbox
```

入站流程：Stream 收到事件 → 进程内缓冲（顺手刷新「最近提问者」表 `Channel::last_sender`，
供 @ 用）→ 发 `notifications/resources/updated` → ABB 调 `resources/read` 整批取走并清空。

- `resources/subscribe` 只认 `channel://inbox`，别的 URI **报错而不是静默成功**——
  答应了不存在的资源会表现成「渠道在线但收不到消息」。
- `resources/read` 读错 URI 同样 fail-closed。

`initialize` 实跑回包：

```json
{"jsonrpc":"2.0","id":1,"result":{"capabilities":{"resources":{"listChanged":false,"subscribe":true},"tools":{}},"protocolVersion":"2024-11-05","serverInfo":{"name":"abb-channel-dingtalk","version":"0.1.0"}}}
```

其中 `capabilities.resources.subscribe = true` 是**必须声明**的：ABB 据它选入站路线。

## 3. 环境变量

```sh
grep -ohE 'env::var\("[A-Z_]+"\)' crates/abb-channel-dingtalk/src/*.rs | sort -u
```

实测清单（3 个，`wsin.rs` 与 `main.rs` 用的是同一组）：

```
env::var("DT_APP_ID")
env::var("DT_APP_SECRET")
env::var("DT_ROBOT_CODE")
```

| 变量 | 必需性 | 用途 |
|---|---|---|
| `DT_APP_ID` | **必需** | 企业内部应用的 AppKey。启动时 stderr **脱敏**（头尾各 4 字符） |
| `DT_APP_SECRET` | **必需** | 应用 AppSecret（换 `tenant_access_token` 与 Stream 连接凭证）。stderr 只报「已设置/空」 |
| `DT_ROBOT_CODE` | 可选 | 机器人码（发消息的 `robotCode`）。**空/纯空白时回落 `DT_APP_ID`**——钉钉里两者通常同值（都是应用 AppKey）。两者都空时 `send_text`/`send_attachment` 明确报「未配置机器人码」 |

`DT_APP_ID` / `DT_APP_SECRET` 缺任一个：打印一行 stderr 提示，**进程照常起、契约层照常应答**
（`tools/list` 可用），但入站 Stream 长连接不启动、出站调用会被钉钉拒。

命令行参数：**无**，且**忽略**任何参数（`main.rs` 里没有 `std::env::args`；传 `--dir` 也不报错但不起作用）。

## 4. 怎么配进 ABB

ABB 不认识 `DT_*`，只把 `env` 原样转发给子进程（依据 `docs/mcp-bus-architecture.md` §4.1）。

`config.json` 里 `bots[]` 的一项：

```jsonc
{
  "name": "我的钉钉",
  "kind": "mcp",
  "enabled": true,
  "mcp_channel": {
    "name": "dingtalk",
    "command": "/path/to/abb-channel-dingtalk",
    "args": [],
    "env": [
      { "name": "DT_APP_ID",     "value": "…" },
      { "name": "DT_APP_SECRET", "value": "…" },
      { "name": "DT_ROBOT_CODE", "value": "…" }
    ]
  }
}
```

`DT_ROBOT_CODE` 与 `DT_APP_ID` 同值时可以省略这一行（缺省自动回落）。
界面里等价操作：「机器人配置」页选一个 bot、类型选 `mcp`，填三行
**可执行文件** / **参数** / **环境变量**（每行 `KEY=VALUE`）。

## 5. 怎么单独跑

构建（自带 manifest + lock，不受 root workspace 约束）：

```sh
export TMPDIR=/Volumes/DataExt/tmp
export CARGO_TARGET_DIR=/Volumes/DataExt/tmp/abb-target-channels
cargo build --release --manifest-path crates/abb-channel-dingtalk/Cargo.toml
# 产物：$CARGO_TARGET_DIR/release/abb-channel-dingtalk
```

最小 `initialize` 请求（stdin 一行 NDJSON，stdout 一行回包）：

```sh
echo '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"probe","version":"0"}}}' \
  | ./abb-channel-dingtalk
```

看能力与入站资源：

```sh
printf '%s\n%s\n' \
  '{"jsonrpc":"2.0","id":2,"method":"tools/list"}' \
  '{"jsonrpc":"2.0","id":3,"method":"resources/list"}' \
  | ./abb-channel-dingtalk
```

订阅并取走入站（`stdout` **只出 JSON-RPC**，日志一律 stderr）：

```sh
printf '%s\n%s\n' \
  '{"jsonrpc":"2.0","id":4,"method":"resources/subscribe","params":{"uri":"channel://inbox"}}' \
  '{"jsonrpc":"2.0","id":5,"method":"resources/read","params":{"uri":"channel://inbox"}}' \
  | DT_APP_ID=… DT_APP_SECRET=… ./abb-channel-dingtalk
```

## 测试

```sh
cargo test --manifest-path crates/abb-channel-dingtalk/Cargo.toml
```

单测（`src/main.rs` 底部）覆盖：只声明两个工具且**不许**声明 `send_thread_reply`、
`resources/list` 只列 `channel://inbox`、订阅后 `resources/read` 返回 JSON 数组、未知 URI 被拒、
`inbox` 有上限且丢最旧、`send_attachment` 的后缀/mime 判定与参数校验、
`DT_ROBOT_CODE`/`DT_APP_ID` 都空时报「未配置机器人码」、@ 只对**已知提问者**的群聊生效。
契约测试（`tests/contract.rs`，驱动真二进制）：`stdout` 是纯 JSON-RPC、
两个工具且无 `send_thread_reply`（调用它必须报错而不是静默成功）、inbox 资源可读。
