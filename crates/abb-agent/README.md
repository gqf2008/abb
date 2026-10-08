# abb-agent

ABB 的 **ACP 执行层**，由 [rpi](https://github.com/bigfish1913/pi-rust) 的库实现，用于替代
`crates/buzz-agent`（自维护分叉，含 7.9k 行自持 LLM 层与 1.9k 行自持 agent loop）。

设计依据与决策记录：walgit 线程 **`abb-agent-rpi-acp-20261008`**。

## 为什么是独立包

root `Cargo.toml` 没有 `[workspace]`，所以本包与 `crates/buzz-agent` 同构：自带
`Cargo.lock`，root 的 `cargo fmt/clippy/test` 不会碰到它。构建与门禁一律走
`--manifest-path`：

```sh
cargo build --manifest-path crates/abb-agent/Cargo.toml
cargo test  --manifest-path crates/abb-agent/Cargo.toml
cargo clippy --manifest-path crates/abb-agent/Cargo.toml --all-targets -- -D warnings
cargo fmt --manifest-path crates/abb-agent/Cargo.toml -- --check
```

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
| 内置工具 | 交给 rpi（`rpi-tools`） | 同上 |

`initialize` 因此**不含** `_meta.abbSandbox`：abb 侧 `parse_abb_sandbox_modes` 得到
`None` → `SandboxSupport::Unsupported`，受限（granted）会话会 fail-closed 被拒答。
这是**如实声明**——声明支持却不执行，正是 abb 注释里点名的安全事故形态。

## 边界：谁管什么

rpi 库层**不管路径**：`AgentHarnessOptions` 里没有 cwd、没有目录、没有配置路径
（`resources.skills` 直接收 `Vec<Skill>`，`Session` 的存储是调用方给的 trait 对象），
`~/.rpi/agent` 与 `RPI_CODING_AGENT_DIR` 的解析全在 `rpi-cli` 那个应用层。

所以下面这些由**本包**负责，装到哪是我们自己的决定：

- 工作目录（`OsExecutionEnv::with_cwd`）
- 约定链扫描：`~/AGENTS.md` 作为全局层 + cwd→root 逐级；技能目录对齐
  `~/.agents/skills` / `<cwd>/.agents/skills`（buzz `hints.rs` 的现状）。
  **不能**沿用 rpi 的默认位（`<agent_dir>/AGENTS.md` + `~/.rpi/agent/skills`），
  否则既有用户的约定会静默失效。
- 会话存储、provider 装配、MCP 客户端、ACP 帧

## 当前进度（第一刀）

已实现：`initialize`（不声明档位 + `_meta.steering.supported`）、`session/new`、
`session/prompt`（回文本）、`session/cancel`、`session/update` 的
`agent_message_chunk`。

尚未实现（第二刀）：

- **MCP 客户端**——`abb-events` 与 wassette 两个 tool surface 现在会**缺失**；
- **内置工具接线**（`read`/`write`/`edit`/`bash`/`grep`/`find`/`ls` + `OsExecutionEnv` 的 cwd）；
- **约定链**（上表）；
- `tool_call` / `tool_call_update` 通知；
- openai 系列 provider（`openai-chat` / `openai-responses` / `openrouter` / `deepseek`）；
- 逐 token 流式（当前与 buzz-agent 一致：非流式，整条 `agent_message_chunk`）。

## 环境契约

| 变量 | 作用 |
| --- | --- |
| `BUZZ_AGENT_PROVIDER` | 供应商（第一刀只接 `anthropic`） |
| `ABB_AGENT_MODEL` | 可选：按 id 或 name 选模型；缺省取该 provider 模型表第一个 |
| `ABB_AGENT_FAUX_TEXT` | **离线通道**：命中即用 rpi faux provider（不联网、无凭据），供 smoke 与测试 |
| `RUST_LOG` | 日志级别（默认 `info`；**日志只走 stderr**，stdout 是 ACP 协议通道） |

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
