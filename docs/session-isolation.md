# 会话隔离机制（群 / 话题 / 用户）

> 结论日期：2026-08-09。对应 issue #5。
> 本文说明 ABB 当前「群 / 话题 / 用户」三类会话是如何隔离的、存在哪些串线风险，
> 以及话题隔离要不要做（结论：**要做飞书话题维度**，已实施，见文末 #14 方案）。

## 1. 当前隔离模型

会话持久化在 `src/sessions.rs`，key 按 bot + 后端分槽。非话题消息 key 是 **chat_id 单维度**；
飞书话题消息（#14 起）key 是 **`{chat_id}:{thread_id}`**：

```
sessions.json = { key: { claude: {session_id, started}, codex: {session_id, started} } }
key = chat_id（非话题）或 chat_id:thread_id（飞书话题）
```

并发控制 `bridge.rs::chat_lock`、打断标志 `cancel_flags` 同样按 key 一把锁/一个标志。
即：**同一个 key 的所有消息共享同一个 agent 会话、串行排队、共享打断；同一群不同话题互不串线**。

## 2. 三平台 chat_id 规则

| 平台 | 单聊 | 群聊 | 话题/线程 | chat_type |
|---|---|---|---|---|
| 飞书 | 平台 `chat_id`（p2p） | 平台 `chat_id`（group） | **未使用**（同一群内所有话题共用群 chat_id） | 事件 `chat_type`（p2p/group） |
| 微信 | 对方 `ilink_user_id` | 未实现（当前全部按 dm 处理） | 无 | 硬编码 `"dm"` |
| 钉钉 | 对方 `staffId` | `openConversationId`（`cid` 开头） | 无（钉钉群无话题） | `is_group_chat` 判定（cid 开头=group） |

代码位置：
- 飞书：`bridge.rs::on_payload`（`message["chat_id"]` 直接取平台 chat_id）
- 微信：`bridge.rs::on_weixin`（`chat_id = from_user_id`，`chat_type: "dm"` 硬编码）
- 钉钉：`dingtalk.rs::chat_id()`（单聊=staffId、群聊=openConversationId）+ `is_group_chat()`

## 3. 隔离矩阵

| 维度 | 当前隔离？ | 说明 |
|---|---|---|
| 用户（跨私聊） | ✅ 隔离 | 每个用户的私聊是不同 chat_id，会话/锁互不串 |
| 群（跨群） | ✅ 隔离 | 每个群是不同 chat_id |
| 群内（同群多人） | ❌ 不隔离 | 群内所有成员共享同一 chat_id → 同一 agent 会话、同一把锁。A 的提问带着 B 的上下文；A 跑长任务时 B 的消息排队 |
| 话题/线程（飞书） | ✅ 已隔离（#14） | 话题消息按 `chat_id:thread_id` 独立会话/锁/打断，回复走 reply 接口落在原话题 |
| 微信群聊 | N/A | 当前按私聊处理（若平台推群消息会被当成 dm 会话） |

补充观察：飞书群聊回复**不会 @ 提问者**（`FeishuMessenger.send_text` 无 @ 参数；
只有钉钉群聊有 `note_sender` + @ 机制）。群里多人使用时容易看不出回复给谁。

## 4. 风险确认

1. **话题串线（飞书）成立**：同一群内不同 thread 共用同一 agent 会话，上下文互相污染——
   属于真实风险，且飞书是主用通道（庆小丰），值得做。
2. **群内多人共享会话成立**：与话题串线同源（都是 chat_id 单维度），但当前主要单用户使用，
   影响有限；多用户场景需产品确认后再做。
3. **微信无群聊维度**：当前不影响（微信 bot 只处理私聊消息），平台侧群消息能力确认后再补。

## 5. 结论与实施方案（#14 已完成）

**结论：飞书话题维度要做**（P2），方案（最小改动，不碰跨端架构）：

1. **解析层**：`bridge.rs::on_payload` 从消息事件取 `message["thread_id"]`（官方事件字段，`omt_`
   开头，话题消息才有），`Ev` 增加 `thread_id: String`（空=非话题）。
   ⚠️ 不用 `root_id`：它是根消息的 `message_id`（`om_` 开头），只表示回复树层级，不是话题标识。
2. **会话 key**：话题消息用 `{chat_id}:{thread_id}` 作为 sessions/chat_lock/cancel_flags 的 key
   （`Ev::key()`）；非话题消息保持原 chat_id，互不干扰。
3. **发送层**：create 发送接口不支持 thread 参数，话题回复必须走
   `POST /im/v1/messages/:message_id/reply`，body 带 `reply_in_thread: true`（显式置 true，
   避免落到群根会话）。`FeishuClient` 新增 `reply_text(message_id, text)`（含分段），
   `Messenger` 新增 `send_thread_reply(chat_id, message_id, text)`（其它通道默认回落 `send_text`）。
4. **兼容**：老 sessions.json 键不变；话题 key 是新键，无迁移成本。
5. **不做**（本轮）：群内按提问者拆会话（多用户场景待确认）、微信群聊维度（平台能力未确认）。

已核实（官方文档 2026-08-10）：事件字段 `event.message.thread_id`；回复接口
`im/v1/messages/:message_id/reply` 请求体 `reply_in_thread`（boolean）；回复响应含 `thread_id`。

## 6. 相关代码索引

- 会话持久化：`src/sessions.rs`（chat_id 单维度、per-backend 槽位）
- 并发/打断：`src/bridge.rs`（`chat_lock` / `cancel_flags` / `handle`）
- 各平台入站：`src/bridge.rs`（`on_payload` / `on_weixin` / `on_dingtalk`）、`src/dingtalk.rs`
- 发送：`src/feishu.rs`（`send_text` / `reply_text`）、`src/messenger.rs`（`send_thread_reply`、WeixinMessenger context_token）
