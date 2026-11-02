# ABB 作为 MCP 总线：架构方向与渠道契约

> 状态：**方向已定（owner 2026-11-02），实施未开始**。本文是契约与分期，不是既成事实。
> 取代 `docs/mcp-resource-notification.md` 里「执行层转发」的旧模型（见该文顶部告示）。

## 0. 一句话

**ABB 是总线**：一边接**支持 ACP 的智能体**，一边接**各种 MCP 渠道服务**（IM / 邮件 /
GitHub issue / GitLab issue / …）。ABB 自己**不再内置任何渠道**，也不再在界面里配置渠道；
界面只配置**模型供应商**。渠道怎么配由**提供 MCP 的那一方**决定，ABB 的义务是**用自然语言
告诉用户该怎么配**。

## 1. 为什么这是对的（而不是上一版的补丁）

旧模型把 MCP 连接放在**执行层**（`abb-agent`），再让执行层用一个**私有 ACP 通知**
`_abb/unstable/resource/updated` 把事件转发给 ABB。两个后果：

1. **标准之外**：ACP 规范里没有这个通知 ⇒ 任何**其它** ACP agent（ki / Claude / Codex）
   都不可能实现它。换执行层，能力就归零。
2. **层级放错**：MCP 连接是 ABB 的关切（它要拿到渠道事件），不是 agent 的关切
   （agent 只需要能调工具）。

2026-10-23 执行层换成 ki 之后，第 ② 层照常收到 `notifications/resources/updated`，
但**第 ④ 层没人发了**（ki 二进制里 `_abb/unstable` 零命中），第 ⑤ 层代码完好却永远收不到输入
——这正是旧模型脆弱性的实证，不是 ki 的缺陷。

## 2. 目标拓扑

```
                         ┌────────────── ABB 宿主进程（总线）───────────────┐
  ┌──────────────┐       │                                                   │
  │ 渠道 MCP 服务 │◄──────┼── MCP client（ABB 自己）                          │
  │ 飞书/邮件/…   │  MCP  │      ├─ tools/call        ← 出站：发消息          │
  │ （外部进程）  │       │      └─ resources/*       ← 入站：订阅收通知       │
  └──────────────┘       │             │                                     │
                         │             ▼                                     │
                         │   入站消息 → 去重 → 节流 → 频道队列               │
                         │                              │                    │
                         │                              ▼                    │
                         │                       回合（session/prompt）       │
                         │                              │  ACP（标准）        │
                         │                              ▼                    │
                         │  ┌───────────────────────────────────────────┐    │
                         │  │  支持 ACP 的智能体（ki / Claude / Codex…）  │    │
                         │  │  只负责「思考 + 调工具」，不认识任何渠道     │    │
                         │  └───────────────────────────────────────────┘    │
                         └───────────────────────────────────────────────────┘
```

三条链**互相独立**，任一环节可替换：

| 链 | 协议 | 谁连谁 | 换掉会怎样 |
|---|---|---|---|
| 入站（渠道 → ABB） | MCP 通知/资源 | **ABB** → 渠道 MCP server | 换执行层**无影响** |
| 出站（ABB → 渠道） | MCP 工具 | **ABB** → 渠道 MCP server | 同上 |
| 对话（ABB ↔ 智能体） | ACP | ABB ↔ agent | 换渠道**无影响** |

> agent 仍然可以自己连同一个 MCP server 调工具（`tools/call`）——那是**工具链**，
> 与上表两条**渠道链**是不同用途的独立连接。

## 3. 渠道 MCP 服务契约（本仓对外定义的部分）

一个「渠道服务」就是一个普通 MCP server，除了 MCP 通用要求外只需满足：

### 3.1 出站（必须）

| 能力 | 形式 | 说明 |
|---|---|---|
| 发文本 | 工具 `send_text`，参数 `{ chat_id, text }` | 最小必需 |
| 发会话内话题回复 | 工具 `send_thread_reply`，参数 `{ chat_id, message_id, text }` | 可选；缺失时回落 `send_text` |

### 3.2 入站（二选一）

| 方案 | 形式 | 适用 |
|---|---|---|
| **A. 资源订阅**（首选） | 资源 `channel://inbox`（或 `channel://<chat_id>/inbox`）+ `notifications/resources/updated` | server 支持 resources |
| **B. 工具轮询**（兜底） | 工具 `poll`，参数 `{ cursor?, limit? }` → `{ messages, next_cursor }` | 不支持 resources 的 server |

ABB 启动时先试 A（`resources/list` 里有 `channel://` + `resources/subscribe` 成功），
失败则退 B。**两者都不可用 ⇒ 该渠道判不可用并明确报错**（不静默降级成「收不到消息但显示在线」）。

### 3.3 入站消息形状（统一）

```json
{ "chat_id": "…", "message_id": "…", "sender_id": "…", "sender_name": "…",
  "text": "…", "ts": 1793612338,
  "attachments": [ { "kind": "image|file", "name": "…", "url_or_path": "…" } ] }
```

字段语义照 `crate::messenger::Messenger` 既有约定（`chat_id` 是渠道内会话键）。
**ABB 不认识渠道差异**：任何在 `mcp.json` 里声明、满足本契约的 server 都能接。

### 3.4 可选能力（缺失即回落）

`get_quoted_message` / `get_chat_info` / `chat_brief` / `create_chat` / `typing` / `done` /
`user_display_name` —— 一一对应 `Messenger` trait 的默认方法，缺失时用 trait 默认实现。

## 4. ABB 侧的职责边界

| ABB **负责** | ABB **不负责** |
|---|---|
| 连接渠道 MCP server、订阅、收事件 | 各渠道的鉴权、协议细节、限流策略 |
| 去重 / 节流 / 开回合 / 投递 | 渠道账号与凭证的保管 |
| 告诉用户「要接某渠道该装什么、怎么配」 | 决定用户该接哪些渠道 |
| 模型供应商配置（界面里唯一的配置面） | 渠道的界面配置 |

**「告知用户怎么配」是硬要求**，不是文档附属品：用户说「我要接飞书」时，abb 里的 assistant
必须能用自然语言讲清：装哪个 MCP server、`mcp.json` 写什么、缺哪些权限。这条能力要有测试。

## 5. 迁移分期（**减法在最后，且需 owner 授权**）

| 期 | 内容 | 是否可逆 |
|---|---|---|
| **S1** | ABB 内置 MCP client；新增 `McpChannelMessenger`（`impl Messenger`），`bot.kind = "mcp"` | 纯增量，可逆 |
| **S2** | 渠道契约稳定 + 至少一个真实渠道 MCP server 跑通（入站+出站） | 纯增量，可逆 |
| **S3** | 界面去掉渠道配置，只留模型供应商；渠道配置引导改由 assistant 自然语言输出 | 可逆（UI 回退） |
| **S4** | 内置 `feishu` / `wechat` / `dingtalk` 退役（`src/feishu.rs` 2206 行、`src/wechat.rs` 1640、`src/dingtalk.rs` 1764，另 `messenger.rs` 三个 impl 与 `bridge/` 里的耦合） | **不可逆，需 owner 逐项授权删除** |

现状量化（2026-11-02）：

```
src/ 总计 112,590 行
  src/feishu.rs        2206   ┐
  src/wechat.rs        1640   ├ 协议实现，S4 才动
  src/dingtalk.rs      1764   ┘
  src/messenger.rs       ~850  （含三个 impl；trait 本身保留）
  src/bridge/mod.rs     5520  （通用层保留，渠道耦合点待清）
  ui/app.slint          5892  （渠道配置面在 S3 移除）
引用渠道实现的文件：feishu 23 个 / wechat 13 个 / dingtalk 14 个
```

**S4 之前不删任何文件。** 前置条件是 S1–S3 完成且真实渠道跑通——否则是拿掉能用的去换没验证的。

## 6. 与本次 D68 改动的关系

两者正交、可叠加：

- D68（已完成）：**协作面**从本地 CLI 改为 MCP（这是仓库**开发流程**的事）
- 本文：**运行面**从内置渠道改为 MCP 渠道（这是**产品架构**的事）

共同点只有一个：都把 MCP 当成与「某一种实现」解耦的稳定接口。
