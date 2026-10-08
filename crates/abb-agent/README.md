# abb-agent

ABB 的 **ACP 执行层**，由 [rpi](https://github.com/bigfish1913/pi-rust) 的库实现，用于替代
`crates/buzz-agent`（自维护分叉，含 7.9k 行自持 LLM 层与 1.9k 行自持 agent loop）。

设计依据与决策记录：walgit 线程 **`abb-agent-rpi-acp-20261008`**。

## 为什么是独立包

root `Cargo.toml` 没有 `[workspace]`，所以本包与 `crates/buzz-agent` 同构：自带
`Cargo.lock`，root 的 `cargo fmt/clippy/test` 不会碰到它。构建与门禁一律走
`--manifest-path`：

```sh
cargo build  --manifest-path crates/abb-agent/Cargo.toml
cargo test   --manifest-path crates/abb-agent/Cargo.toml
cargo clippy --manifest-path crates/abb-agent/Cargo.toml --all-targets --locked -- -D warnings
cargo fmt    --manifest-path crates/abb-agent/Cargo.toml -- --check
```

CI 侧对应 `.walgit/ci.toml` 的 `abb-agent-lint` 任务（**真的跑测试**，不是 `--no-run`）。

按仓库策略，构建产物不要落在启动卷上：

```sh
export TMPDIR=/Volumes/DataExt/tmp
export CARGO_TARGET_DIR=/Volumes/DataExt/tmp/abb-target
```

## 与 buzz-agent 的语义差异（都是决策，不是遗漏）

| 面 | 处置 | 理由 |
| --- | --- | --- |
| OS 沙箱档位 | **不实现、不声明** `_meta.abbSandbox` | 权限是操作系统的事，agent 不碰 |
| `session/request_permission` | **不发问** | 授权约定由 `AGENTS.md` 承担 |
| LLM 层 / agent loop | 交给 rpi（`rpi-ai` / `rpi-agent`） | 不自持 7.9k 行 `llm.rs` |
| 内置工具 | 第二刀接 `rpi-tools` | 尚未使用，故当前不在依赖里 |

`initialize` 因此**不含** `_meta.abbSandbox`：abb 侧 `parse_abb_sandbox_modes` 得到
`None` → `SandboxSupport::Unsupported`。这是**如实声明**——声明支持却不执行，正是
abb 注释里点名的安全事故形态。

## 进程形状

stdin 读一行、分发一行；**只有回合**（`session/prompt`）被派到独立任务，读循环始终可读。
这样回合进行中送进来的 `session/cancel` 才能立刻生效（串行写法的实测后果是取消完全失效，
abb 只能退化成「drain 超时 → 杀进程重拉」）。

## 环境契约（以 abb 侧 `src/agent.rs::buzz_provider_env` 真值为准）

| 变量 | 取值 / 作用 |
| --- | --- |
| `BUZZ_AGENT_PROVIDER` | `anthropic` 或 `openai`（`openai-chat`/`openrouter`/`deepseek` 等是**配置 kind**，env 里一律归并成 `openai`） |
| `ANTHROPIC_API_KEY` | 由 abb 注入；rpi 的 anthropic provider 默认回落到该 env，本包不显式读 |
| `ANTHROPIC_BASE_URL` | 自定义/自建网关（覆盖 `model.base_url`） |
| `ANTHROPIC_MODEL` | 选模型；**未知 id 直接报错**，不静默改选 |
| `OPENAI_COMPAT_*` | `openai` 家族契约，**第二刀接入** |
| `BUZZ_AGENT_MAX_ROUNDS` | 回合上界（abb 默认送 200）；超过即以 `stopReason: "max_turn_requests"` 收尾 |
| `ABB_AGENT_FAUX_TEXT` | **本包自己的离线通道**：命中即用 rpi faux provider（不联网、无凭据），供 smoke 与测试 |
| `RUST_LOG` | 日志级别（默认 `info`；**日志只走 stderr**，stdout 是 ACP 协议通道） |

## 失败与取消的语义（两条都被实测反证过，现已修好并锁住）

- **provider 失败**（401/429/5xx/超时）：回 **JSON-RPC error**（`-32002`，message 带真实原因）。
  注意 rpi 的 `Agent::prompt` 会把 `LoopOutcome::Failed` 折叠成 `Ok(())`，所以本包从事件流里的
  `error_message` 自己认。**不能**回 `stopReason: "end_turn"` + 空文本：abb 会把空文本当
  「纯工具回合、不投递」并 `record_success` ⇒ 用户收不到东西、系统记成功。
  `stopReason` 也不许自造取值——abb 只认
  `end_turn` / `cancelled` / `max_tokens` / `max_turn_requests` / `refusal`。
- **取消**：`session/cancel`（通知，无 `id`，**不产生应答**）置位正在跑的回合并 `abort()`，
  回合以 `stopReason: "cancelled"` 收尾。无在途回合时忽略。
- 并发提交同一会话的第二个回合会被拒（`-32004`）：abb 按频道串行，这是协议误用。

## 当前进度

**已实现并验证**：`initialize`、`session/new`、`session/prompt`（回文本）、`session/cancel`
（回合中可用）、`session/update` 的 `agent_message_chunk`、provider 失败如实报错、
回合上界。

**尚未实现（第二刀）**：

- **MCP 客户端**——`abb-events` 与 wassette 两个 tool surface 现在会**缺失**；
- **内置工具接线**（`read`/`write`/`edit`/`bash`/`grep`/`find`/`ls` + `OsExecutionEnv` 的 cwd）；
- **约定链**：`~/AGENTS.md` 作为全局层 + cwd→root 逐级；技能目录对齐
  `~/.agents/skills` / `<cwd>/.agents/skills`（buzz `hints.rs` 的现状）。
  **不能**沿用 rpi 的默认位（`<agent_dir>/AGENTS.md` + `~/.rpi/agent/skills`），
  否则既有用户的约定会静默失效；
- `openai` 家族 provider；
- `tool_call` / `tool_call_update` 通知；
- 逐 token 流式（当前与 buzz-agent 一致：非流式，整条 `agent_message_chunk`）。

## 离线 smoke

```sh
printf '%s\n' \
'{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":2}}' \
'{"jsonrpc":"2.0","id":2,"method":"session/new","params":{}}' \
'{"jsonrpc":"2.0","id":3,"method":"session/prompt","params":{"sessionId":"abb-1","prompt":[{"type":"text","text":"你好"}]}}' \
| ABB_AGENT_FAUX_TEXT="来自 faux provider 的回复" abb-agent
```

期望输出（顺序敏感）：`initialize` 应答 → `sessionId` → `session/update`
（`agent_message_chunk`）→ `stopReason: "end_turn"`。
