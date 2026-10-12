# MCP 通知 / 资源读写：工作方式与实测拓扑

本文记录 ABB 的 MCP 通知链（资源订阅 → 通知 → 读资源 → 上报 → 开回合）的实际工作方式。
结论来自 2026-10-12 的真机验证（真 `abb-agent` 二进制 + 真飞书 + 活着的 service），
验证方式见文末「验证记录」。

## 一句话

**MCP server 推 → `abb-agent` 读 + 转发 → ABB 节流后当成一条「事件消息」塞回频道队列 →
走和人类消息同一条回合 / 投递路径**；ABB 不自己产回合，也不认识任何具体 server。

## 进程与连接拓扑

```
                     ┌───────────── ABB 宿主进程（/Applications/ABB.app）─────────────┐
                     │                                                             │
  ┌────────┐  WS     │  bridge ──► harness(pool/queue/dedup) ──► AcpClient          │
  │ 飞书云  │◄───────►│    │                                          │             │
  │(真bot) │ IM事件  │    └── 回复投递 ◄──────────────────────────────┘             │
  └────────┘         └──────────────────────────────────────────┼──────────────────┘
                                                                │ ACP = JSON-RPC over stdio
                                                                │ session/new · session/prompt
                                                                │ session/update · session/cancel
                                                                ▼
                                                 ┌──────────────────────────────┐
                                                 │      abb-agent（执行层）      │
                                                 │  McpSession::connect_all()   │
                                                 │  ResourceUpdateHandler       │
                                                 └──────────────┬───────────────┘
                                                                │ MCP = JSON-RPC over stdio
                                                                │ （随会话建立、随会话销毁）
                                                                ▼
                                                 ┌──────────────────────────────┐
                                                 │   MCP server（孙进程）        │
                                                 │ 来自 bot 配置 mcp_servers     │
                                                 └──────────────────────────────┘
```

- ABB 侧只在 `session/new` 的 `mcpServers` 里**声明** server（`src/config.rs` 的
  `BotConfig::mcp_servers` → `src/service.rs::bot_mcp_servers`）；语义不解释。
- `abb-agent` 用 `rmcp` 连接（`crates/abb-agent/src/mcp.rs`），
  `TokioChildProcess` 起 stdio 子进程，会话内长挂（`RunningService` 被
  `Connected.client` 持有，drop 即关连接），**不做跨会话复用**（因此也不做 `unsubscribe`）。

## 时序：一次资源通知

```
 用户      飞书      bridge     harness      AcpClient      abb-agent       MCP server
  │ "hi"    │          │           │             │               │               │
  │────────►│          │           │             │               │               │
  │         │─IM事件──►│           │             │               │               │
  │         │          │─push─────►│             │               │               │
  │         │          │           │─session/new►│──session/new─►│               │
  │         │          │           │             │ (mcpServers)  │─initialize───►│
  │         │          │           │             │               │─tools/list───►│
  │         │          │           │             │               │─resources/list►│
  │         │          │           │             │               │─subscribe────►│  ① 订阅
  │         │          │           │             │               │◄─notif,updated─│  ② server 主动推
  │         │          │           │             │               │─resources/read►│  ③ 读正文
  │         │          │           │             │               │◄──正文─────────│
  │         │          │           │             │◄─notify───────│  ④ _abb/unstable/
  │         │          │           │◄─_abb/…/resource/updated───│     resource/updated
  │         │          │           │ [dispatch_resource_update]│
  │         │          │           │ [节流 (channel,server,uri)]│
  │         │          │           │ [InboundMsg author=event ]│
  │         │          │           │─新回合 session/prompt────►│               │
  │         │◄─回复投递─│◄──────────│◄──────────────────────────│               │
  │◄────────│          │           │             │               │               │
```

涉及的协议方法：

| 步骤 | 协议 | 方法 / 载荷 |
|---|---|---|
| 会话声明 server | ACP | `session/new` 的 `mcpServers` |
| ① 订阅 | MCP | `resources/list` → `resources/subscribe` |
| ② 通知 | MCP | `notifications/resources/updated` |
| ③ 读正文 | MCP | `resources/read`（文本拼接，blob 只记占位；正文截断到 16 KiB） |
| ④ 上报 | ACP（ABB 私有） | `_abb/unstable/resource/updated`，params = `sessionId` / `server` / `uri` / `text` |

ABB 收到 ④ 后：`dispatch_resource_update`（`src/runtime/acp.rs`）→
`handle_resource_update`（`src/runtime/harness.rs`）→ 节流 →
`resource_update_message()` 造一条 `InboundMsg`（`author_role="event"`、
`prompt_tag="mcp:resource"`、`id_hex` = `(server,uri,text)` 内容哈希）→
`handle_cmd(Cmd::Message)` 进频道队列 → 开回合 → 回复按常规投递。

## 三个方向

| 方向 | 谁发起 | 走哪条链 | 现状 |
|---|---|---|---|
| ① 推（标准 MCP） | MCP server 自己（需先被 `subscribe`） | server → `notifications/resources/updated` → abb-agent read → `_abb/unstable/resource/updated` → ABB 节流 → 入站消息 → 开回合 → 回复飞书 | 已实现并实测 |
| ② 拉（ABB 私有） | agent 循环调工具 | `abb-events`（bridge 内置 stdio server）的 4 个工具：`events_subscribe` / `events_wait` / `events_list` / `events_unsubscribe` | **已删除**（2026-10-11，card `abb-mcp-declarer-only-20261011`） |
| ③ 工具 | agent 调 `tools/call` | MCP **没有 `resources/write`**；要「写」只能通过工具 | 已实现并实测 |

执行层只覆写了 `on_resource_updated`（`crates/abb-agent/src/mcp.rs`）；
`resources/list_changed`、`notifications/message`(logging)、`notifications/progress`
没有对应的处理（rmcp 的默认实现是 no-op），也没有 `logging/setLevel`、没有 progressToken。
即：**「通知族」里真正接通到 ABB 会话的只有 `notifications/resources/updated` 这一种。**

## 关键约束：读是「回合驱动」的

```
  ┌───────────────────────────────────────────────────────────────────────┐
  │ ABB 的 AcpClient 只在【回合进行中】才读 abb-agent 的 stdout。           │
  │ 空闲态没有常驻读泵（stderr 有常驻任务，stdout 没有）→ 资源通知会       │
  │ 压在管道里，直到下一条用户消息的回合开始才被消费。                     │
  │  实测：13:29:52 到的通知，13:31:31 才有回合读它。                      │
  └───────────────────────────────────────────────────────────────────────┘
```

所以「资源变化主动唤醒一个回合」在**空闲态不会立刻发生**：最坏要等下一次用户发言。
这是实现现状（通知不自带回合语义，回合由既有的频道队列 / 去重 / 空闲墙统一管），
不是崩溃；要改成真正的事件唤醒，需要一个常驻的 stdout 读泵。

另注：ABB 侧对资源事件有节流——同一 `(channel, server, uri)` 在 10s 窗口内只放行一次回合，
同一 `(channel, server)` 每窗口还有条数上限（防 uri 轮换绕过），丢弃与窗口汇总都记在
service stdout 的 `[resource]` 行。

## 配置：按渠道的 `mcp.json`（2026-10-12 起）

MCP 配置**按渠道**落在该渠道自己的工作目录里，ABB 只在建会话时读：

```
~/.agent-bridge/workspaces/<bot_key>/<chat_id>/
└── mcp.json     # 顶层数组；不存在 = 该渠道不接任何 server
```

```json
[
  { "name": "myserver", "command": "/usr/local/bin/myserver", "args": ["--stdio"] }
]
```

- 创建/修改由用户在会话里驱动 bot 用文件工具完成（不需要 GUI / 不需要重启 ABB）。
- 生效：每回合开始比对该文件的**内容指纹**，变了就失效该渠道会话——下一条消息的
  `session/new` 按新名单重连 MCP 子进程（旧会话 drop 顺带关掉旧子进程）。
- 受限面：`granted`（授权者受限会话）/ `oneshot` **不读**该文件，恒空名单。
- 校验 fail-visible：解析失败/条目非法只跳过该条并留日志，不静默降级。

## 验证记录（2026-10-12，本机）

1. **执行层（真二进制）**：`crates/abb-agent/probes/` 三条探针全过——
   `mcp_resource_notification_round_trip.py`（单 server 全链）、
   `mcp_resource_multi_server_round_trip.py`（多 server 归属不串）、
   `mcp_tool_round_trip.py`（工具往返）。
   假 server 侧记录到 `subscribed` + `resource_read`，`abb-agent` stdout 抓到
   `_abb/unstable/resource/updated` 原始帧。
2. **宿主层（隔离 HOME）**：`tools/check_test_isolation.sh resource` → 20 passed，
   含 `agent_reported_resource_update_lands_in_origin_session`、
   `resource_event_message_runs_a_turn`、节流与畸形载荷用例。
3. **活服务 + 真飞书**：给 bot 配假 MCP server → 会话带 `mcp server ["fakemcp"]` 建立 →
   假 server 推通知 → ABB 侧 `[resource] 放行 1 条回合` + 丢弃 + 窗口汇总 →
   `runtime 回复已投递` 到真飞书，且假 server 记录到模型用**资源正文**调了 MCP 工具。

**未覆盖**：`resources/list_changed` / logging / progress 三类通知在 ABB 上无处理路径（见上），
不构成回归，但也不会有行为。
