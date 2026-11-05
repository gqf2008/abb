# abb-channel

ABB 的渠道 MCP server：**飞书 / 钉钉 / 微信，一个二进制**，用 `--transport` 分派。

```sh
abb-channel --transport feishu              # 读 FS_APP_ID / FS_APP_SECRET
abb-channel --transport dingtalk            # 读 DT_APP_ID / DT_APP_SECRET / DT_ROBOT_CODE
abb-channel --transport wechat --dir ~/.agent-bridge/state/wechat   # 读 WX_TOKEN / …
```

ABB 侧（`bots[].mcp_channel`）：

```jsonc
{
  "command": "/绝对路径/abb-channel",
  "args": ["--transport", "feishu"],
  "env": [
    { "name": "FS_APP_ID", "value": "cli_…" },
    { "name": "FS_APP_SECRET", "value": "…" }
  ]
}
```

`--transport` **必填**，缺值/未知值 ⇒ stderr 打 usage、退出码 **2**（2 = 调用错了；渠道自己跑失败是 1）。
不按环境变量猜渠道：猜错时表现为「连不上平台」而不是「参数错了」，排障要多绕一圈。

## 为什么是一个二进制

三个渠道曾经各是一个 crate、各出一个二进制。合并的唯一动机是**体积**：

| | 未 strip | strip -x |
|---|---|---|
| 旧：`abb-channel-feishu` + `-dingtalk` + `-wechat` | 5.5 + 5.1 + 4.7 = **15.3 MB** | 12.6 MB |
| 新：`abb-channel`（三合一） | **6.9 MB** | **5.5 MB** |

省掉 8.4 MB（55%）。原因不是「代码写短了」，是 **`rustls + ring + webpki-roots` 那约 4 MB 的
HTTPS 地板过去被链了三遍**。合起来链接一次只算一份。业务代码一行没改，契约没动。

顺带收掉的重复：`rt()` 三份**逐字相同**（`md5` 确认），现已并入 `src/jsonrpc.rs`。
`base64_decode` 三份**实现有差异**（填充/容错边界不同），**故意不合并**——为省几百字节去动
协议解析的容错，是拿正确性换体积。

## 目录

```
src/main.rs          --transport 解析与分派
src/jsonrpc.rs       三个渠道共用的运行时（rt()）
src/feishu/          飞书：mod.rs（会话循环 + 工具）、client.rs（HTTP）、wsin.rs（WS 长连接）、proto.rs（帧编解码）
src/dingtalk/        钉钉：mod.rs、client.rs、wsin.rs
src/wechat/          微信：mod.rs、client.rs
tests/               三条 contract（各自验自己的工具集/入站方式）+ transport_dispatch（分派与退出码）
```

约定：`<渠道>/mod.rs` 是会话循环与工具实现，`<渠道>/client.rs` 是该平台的 HTTP/WS 客户端
（**不叫 `<渠道>/<渠道>.rs`**：那会触发 `clippy::module_inception`）。

## 契约（`docs/mcp-bus-architecture.md` §3）

| | 飞书 | 钉钉 | 微信 |
|---|---|---|---|
| 必需 `send_text` | ✅ | ✅ | ✅ |
| 入站方式 | `channel://inbox` 资源订阅 | 同左 | `poll` 工具 |
| 话题回复 `send_thread_reply` | ✅ | ❌ 无此概念 | ❌ 无此概念 |
| 声明工具数 | 15 + `inbox` | 9 + `inbox` | 7 |

工具名必须与 `src/mcp_bus.rs` 的 `TOOL_*` 常量逐字一致——`tools/check_channel_tool_names.sh`
双向核对（crate 声明 ⊆ 契约常量，且契约常量都有渠道声明）。

## 门禁

```sh
# 单独跑（门禁里由 tools/gates.sh 带 --manifest-path 跑，产物外置）
export TMPDIR=/Volumes/DataExt/tmp
export CARGO_TARGET_DIR=/Volumes/DataExt/tmp/abb-target-channel
cargo fmt --check && cargo clippy --locked --all-targets -- -D warnings
cargo build --locked && cargo test --locked
```

集成测试会**起真子进程**（`CARGO_BIN_EXE_abb-channel`）验「stdout 只有 JSON-RPC」「退出码契约」
「`--transport` 真的分派到了对应渠道」。`src/mcp_channel_dir_e2e_test.rs` 的三条
`abb_negotiates_real_*` 会用本 crate 的产物跑 ABB 的生产协商路径；`tools/gates.sh` 通过
`ABB_CHANNEL_BIN` 把路径传下去，并断言标记 `ABB_*_E2E_RAN` 真的出现（不出现就红，
因为「产物在但 e2e 静默 skip」是假绿）。
