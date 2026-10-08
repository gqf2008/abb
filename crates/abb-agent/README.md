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
cargo test   --manifest-path crates/abb-agent/Cargo.toml --locked
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
| `_meta.steering` | **声明 `supported: false`** | 本刀未实现 `_session/steering`；abb 那边该标志是写该方法的唯一闸门，报 true 只会让「回合中追加消息」多一次无效往返 |
| LLM 层 / agent loop | 交给 rpi（`rpi-ai` / `rpi-agent`） | 不自持 7.9k 行 `llm.rs` |
| 内置工具 | 第二刀接 `rpi-tools` | 尚未使用，故当前不在依赖里 |
| 输出上限 | 默认 **65_536**，认 `BUZZ_AGENT_MAX_OUTPUT_TOKENS` | 与 fork 默认值一致；写小了会变成**静默截断**（见下） |

`initialize` 因此**不含** `_meta.abbSandbox`：abb 侧 `parse_abb_sandbox_modes` 得到
`None` → `SandboxSupport::Unsupported`。这是**如实声明**——声明支持却不执行，正是
abb 注释里点名的安全事故形态。

## 进程形状

stdin 读一行、分发一行；**只有回合**（`session/prompt`）被派到独立任务，读循环始终可读。
`in_flight` 在**读循环内同步抢**（不是在被派出的任务里）——否则同一 burst 中紧随其后的
`session/cancel` 会被读循环先读到，而那时任务还没被调度，取消会被「无在途回合」分支丢弃
（`probes/cancel_same_burst.py` 就是这个复现：修前 0 间隔 6/6 丢弃，修后 8/8 收尾）。

取消的完整语义要注意一个库层事实：`Agent::abort()` 只作用于**当前 run 的 per-run
token**。若 cancel 在回合任务**被 poll 之前**就到，那一次 abort 会落空——所以回合循环里按
取消标记**每轮补发 abort**，并挂一个 100ms 的 tick 作固定唤醒源（卡在网络上时不会再有事件
来唤醒我们）。

## 环境契约（以 abb 侧 `src/agent.rs::buzz_provider_env` 真值为准）

| 变量 | 取值 / 作用 |
| --- | --- |
| `BUZZ_AGENT_PROVIDER` | `anthropic` 或 `openai`（`openai-chat`/`openai-responses`/`openrouter`/`deepseek` 是**配置 kind**，env 里一律归并成 `openai`） |
| `ANTHROPIC_API_KEY` | 必填（显式取用，不依赖 rpi 的 env 兜底） |
| `ANTHROPIC_BASE_URL` | 可选；自定义/自建网关，**填主机根**（如 `https://gw.example.com`）。缺省 `https://api.anthropic.com` |
| `ANTHROPIC_MODEL` | **必填**；任意 id（见下） |
| `OPENAI_COMPAT_API_KEY` / `OPENAI_COMPAT_MODEL` | **必填**（openai 家族） |
| `OPENAI_COMPAT_BASE_URL` | 可选；缺省 `https://api.openai.com` |
| `OPENAI_COMPAT_API` | `responses` 走 responses 端点，其余（含 `openrouter`/`deepseek` 两个预置）都是 chat |
| `BUZZ_AGENT_MAX_ROUNDS` | 回合上界（abb 默认送 200） |
| `BUZZ_AGENT_MAX_OUTPUT_TOKENS` | 可选；单回合输出上限。abb **今天不注入**，但被替代的 fork 会读它（默认 65536），故同样认 |
| `ABB_AGENT_FAUX_TEXT` | **本包自己的离线通道**：命中即用 rpi faux provider（不联网、无凭据），供 smoke 与测试 |
| `RUST_LOG` | 日志级别（默认 `info`；**日志只走 stderr**，stdout 是 ACP 协议通道） |

### 模型 id 不查目录（重要）

模型是用 `Model::new(id, name, api, provider, base_url)` **显式构造**的，与 rpi 官方对
`models.json` 条目的做法（`rpi-cli/src/config.rs::provider_to_models`）一致：**id 由调用方
给，rpi 不限制命名空间**。所以自定义网关（one-api、自建反代…）用厂商原生 id 是正常用法。

模型的 `context_window` 保持 0（同官方对未声明条目的处理），但 `max_tokens` **必须给正数**：
`clamp_max_tokens_to_context` 在 `context_window == 0` 时返回 `max(1, max_tokens)`，
于是 `0` 会静默变成「只准回 1 个 token」。故默认 `8192`（`DEFAULT_MAX_OUTPUT_TOKENS`）。

模型/API key 缺失时**直接报错**（`session/new` 返回 -32000），不静默挑一个——被替代的
`crates/buzz-agent/src/config.rs` 在同路径就是 `"config: ANTHROPIC_MODEL required"` 硬失败。
静默替用户挑模型等于替用户花钱。

### base_url 语义（anthropic 与 openai 不一样！）

**anthropic：填主机根。** rpi 自己拼 `/v1/messages`（`format!("{base}/v1/messages")`），所以
`https://gw.example.com` → `/v1/messages`，而 `https://gw.example.com/v1` → **`/v1/v1/messages`**。
这与被替代的 fork 同构（它也是 `{base}/v1/messages`），不是回归，但是个陷阱：
单元测试一度拿 `…/v1` 当「可用网关」的样本，那是在示范一个会坏掉的形状（评审指出后已改）。

**openai：两边一致。**

abb 的预置端点是**带版本前缀**的（`openrouter` → `https://openrouter.ai/api/v1`，
`deepseek` → `https://api.deepseek.com/v1`），而 rpi 对 `/v1` 结尾只再补
`/chat/completions`（对 host 根才补 `/v1/chat/completions`）——所以能正确拼成
`https://api.deepseek.com/v1/chat/completions`。

## 失败与取消的语义

- **provider 失败**（401/429/5xx/超时）：回 **JSON-RPC error**（`-32002`，message 带真实原因）。
  注意 rpi 的 `Agent::prompt` 会把 `LoopOutcome::Failed` 折叠成 `Ok(())`，所以本包从事件流里的
  `error_message` 自己认。**不能**回 `stopReason: "end_turn"` + 空文本：abb 会把空文本当
  「纯工具回合、不投递」并 `record_success` ⇒ 用户收不到东西、系统记成功。
  `stopReason` 也不许自造取值——abb 只认
  `end_turn` / `cancelled` / `max_tokens` / `max_turn_requests` / `refusal`。
- **取消**：`session/cancel`（通知，无 `id`，**不产生应答**）置位正在跑的回合并 `abort()`，
  回合以 `stopReason: "cancelled"` 收尾。无在途回合时忽略。
- 并发提交同一会话的第二个回合会被拒（`-32004`）：abb 按频道串行，这是协议误用。
- **超过输出上限是静默截断**：rpi 把 `StopReason::Length` 归入 `Completed`、`error_message`
  为空 ⇒ 本包只能报成 `stopReason: "end_turn"`，abb 看到 `end_turn` 就 `record_success`。
  所以上限**必须**与被替代组件对齐（65_536），写小了就是「长回答被截半截而系统记成功」。
- **stdin EOF 即退出**：父进程关掉 stdin 视为收摊，**在途回合的应答不会被等**。
  写 smoke / 手测时要注意（见下）。

## 当前进度

**已实现并验证**：`initialize`、`session/new`、`session/prompt`（回文本）、`session/cancel`
（回合中可用，含同 burst）、`session/update` 的 `agent_message_chunk`、provider 失败如实报错、
anthropic 与 openai 两个家族（含自定义网关 + 任意模型 id）。

**尚未实现（第二刀）**：

- **MCP 客户端**——`abb-events` 与 wassette 两个 tool surface 现在会**缺失**；
- **内置工具接线**（`read`/`write`/`edit`/`bash`/`grep`/`find`/`ls` + `OsExecutionEnv` 的 cwd）；
- **约定链**：`~/AGENTS.md` 作为全局层 + cwd→root 逐级；技能目录对齐
  `~/.agents/skills` / `<cwd>/.agents/skills`（buzz `hints.rs` 的现状）。
  **不能**沿用 rpi 的默认位（`<agent_dir>/AGENTS.md` + `~/.rpi/agent/skills`），
  否则既有用户的约定会静默失效。
  ⚠️ **前置条件**：约定链一落地就**必须同时认 `BUZZ_AGENT_NO_HINTS`**（或等价的 per-session
  开关）——abb 用它把「授权者看不到 owner 私有约定」这件事在进程级收口，忽略它会造成
  owner 的 `~/AGENTS.md` 与技能泄漏给授权者；
- `_session/steering`（现在如实声明为不支持，走 abb 的 cancel+merge 回退）；
- `tool_call` / `tool_call_update` 通知；
- 逐 token 流式（当前与 buzz-agent 一致：非流式，整条 `agent_message_chunk`）；
- `StopReason::Length`（截断）目前会被报成 `end_turn`（abb 的 `max_tokens` 分支收不到）。
- **openai-chat + 官方 OpenAI 推理模型**可能因字段名被拒：rpi 默认发 `max_tokens`，而被替代的
  fork 对 openai 发 `max_completion_tokens`。**未实测**（无 key/无网络），仅登记为风险。

`BUZZ_AGENT_MAX_ROUNDS` 的护栏代码已就位且**边界与被替代组件对齐**（在 `TurnStart` 即本轮
请求之前按**已完成**的请求数判，做满 `max_rounds` 次就不再开新一轮），但**当前不可达**：
没有工具时一个回合只会有 1 次模型请求（评审用计数探针实测：3 个回合共 3 次 POST），
超界分支不会被触发。接上工具后才成为实际护栏。

## 离线 smoke

**注意**：`printf ... | abb-agent` 会在回合跑完前关闭 stdin，进程随即退出、
**在途回合的应答被丢弃**（只会看到 2 行）。要保持 stdin 打开：

```sh
BIN="$CARGO_TARGET_DIR/debug/abb-agent"
{ printf '%s\n' \
  '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":2}}' \
  '{"jsonrpc":"2.0","id":2,"method":"session/new","params":{}}' \
  '{"jsonrpc":"2.0","id":3,"method":"session/prompt","params":{"sessionId":"abb-1","prompt":[{"type":"text","text":"你好"}]}}'
  sleep 2
} | ABB_AGENT_FAUX_TEXT="来自 faux provider 的回复" "$BIN"
```

期望输出（顺序敏感，共 4 行）：`initialize` 应答（`steering.supported=false`、无
`abbSandbox`）→ `sessionId` → `session/update`（`agent_message_chunk`）→
`stopReason: "end_turn"`。

## 端到端探针（`probes/`）

单测用替身，探针跑**真二进制**——两者职责不同。四个探针各自对应一条被评审反证过的行为：

| 探针 | 覆盖 |
| --- | --- |
| `cancel_during_turn.py` | 回合在途时读循环仍可服务 `initialize`；cancel 延迟 ≈0 收尾成 `cancelled` |
| `cancel_same_burst.py` | prompt 与 cancel 在同一次 write（0 间隔）时取消不被丢弃（修前 6/6 丢弃） |
| `provider_error_is_visible.py` | provider 失败回 JSON-RPC error，而非「成功 + 空文本」 |
| `openai_family_round_trip.py` | openai 家族端到端：URL 拼法、鉴权头、任意厂商模型 id、文本送达 |

判定已收紧（只认 `cancelled`／断言不得出现 `result`）——早先出现过「非 cancelled 的其它结论
被算作通过」的假通过窗口。

详见 `probes/README.md`。
