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

两条都只在 `/tmp` 与本地回环上活动，不访问外网、不写仓库文件。
