# 端到端探针（需要真二进制，故不入 `cargo test`）

这两条脚本是**评审反证 (a)(b) 的复现实验**，跑的是构建产物而不是单测替身。用法：

```sh
export TMPDIR=/Volumes/DataExt/tmp CARGO_TARGET_DIR=/Volumes/DataExt/tmp/abb-target
cargo build --manifest-path crates/abb-agent/Cargo.toml
python3 crates/abb-agent/probes/cancel_during_turn.py \
    "$CARGO_TARGET_DIR/debug/abb-agent"
python3 crates/abb-agent/probes/provider_error_is_visible.py \
    "$CARGO_TARGET_DIR/debug/abb-agent"
```

- `cancel_during_turn.py`：把 `ANTHROPIC_BASE_URL` 指到一个「接受连接但永不回响应」的
  本地黑洞，造出长回合，然后在回合进行中送 `initialize`（验证读循环没被占死）与
  `session/cancel`（验证收尾成 `stopReason:"cancelled"`）。
  **修前表现**：两者都不被处理，回合挂到客户端超时。
- `provider_error_is_visible.py`：起一个只回 401 的本地假端点，断言回合应答是
  JSON-RPC **error**（而不是「成功 + 空文本」）。顺带用日志验证 `ANTHROPIC_MODEL`
  与 `ANTHROPIC_BASE_URL` 真的生效。
  **修前表现**：`stopReason:"end_turn"` + 零文本 + 零日志 ⇒ abb 当「纯工具回合」不投递
  并 `record_success`。

- `cancel_same_burst.py`：把 `session/prompt` 与 `session/cancel` 放进**同一次 write**
  （0 间隔）配黑洞端点。修前 `in_flight` 在被派出的任务里抢，读循环能在任务被调度前先读到
  cancel ⇒ 取消被静默丢弃（0 间隔 6/6）；修后 8/8 收敛成 `cancelled`。
- `openai_family_round_trip.py`：起一个假 OpenAI 兼容网关（回最小 SSE），断言请求打到
  `{base}/v1/chat/completions`、鉴权头是 abb 注入的 key、请求体里的 model 就是我们给的
  **任意厂商 id**，且文本被发成 `session/update` 并以 `end_turn` 收尾。这条覆盖的是 abb 把
  `openai-chat`/`openrouter`/`deepseek` 全归并成 `BUZZ_AGENT_PROVIDER=openai` 的真实路径。

- `mcp_tool_round_trip.py`（配 `fake_mcp_server.py`）：刀 1 的核心验收。按 `session/new` 的
  `mcpServers` 起一个真 MCP server（stdio JSON-RPC），用 `ABB_AGENT_FAUX_TOOL` 让模型发一次
  工具调用，**判据是假 server 写下的 `tools/call` 记录**——从 agent 的文本输出无法区分
  「工具被调用」与「模型自己编了答案」。
- `fake_mcp_server.py`：上述探针用的最小 MCP server（`initialize` / `tools/list` / `tools/call`）。

全部只在 `/tmp` 与本地回环上活动，不访问外网、不写仓库文件。
