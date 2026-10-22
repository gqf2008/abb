# 会话隔离机制（群 / 话题 / 用户）

> 结论日期：2026-08-09。对应 issue #5。
> 本文说明 ABB 当前「群 / 话题 / 用户」三类会话是如何隔离的、存在哪些串线风险，
> 以及话题隔离要不要做（结论：**要做飞书话题维度**，已实施，见文末 #14 方案）。

## 1. 当前隔离模型

会话持久化在 `src/sessions.rs`，key 单槽（单后端化 P4.2 前按后端分槽）。非话题消息 key 是 **chat_id 单维度**；
飞书话题消息（#14 起）key 是 **`{chat_id}:{thread_id}`**：

```
sessions.json = { key: { session_id, started, sandbox_mode? } }   // P4.2 单槽
（老四槽 {claude, codex, pi, buzz} 与旧扁平 {backend, ...} 在 load 时折叠迁移：
 `buzz` 槽（历史 wire 键）逐字保留，其余后端槽位舍弃，原件归档 sessions.json.legacy.bak）
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

补充（2026-08-12）：
- **话题内回复免 @**：群聊里 `thread_id` 非空的消息（用户在话题内回复 bot）不再要求 @
  ——`on_payload` 只对顶层群消息做 @ 校验；话题内回复按 `chat_id:thread_id` 进原会话。
- **应用/bot 消息过滤**：飞书事件中应用发的消息 `sender_type="app"`（不是 `"bot"`），
  `on_payload` 现在丢弃 `app`/`bot` 及 sender open_id == 本 bot open_id 的消息，
  防止 bot 自己的回复 echo 触发新一轮 agent（自说自话）。
- **引用/回复上下文**：引用一条消息再 @ bot 时，被引用内容进 agent prompt——
  飞书按事件 `parent_id` 调 `GET /im/v1/messages/:id` 拉取（best-effort，失败不阻塞回复）；
  微信 `item.ref_msg`（title + message_item）、钉钉 `text.isReplyMsg/repliedMsg` 随事件直接携带。
  `Ev.quoted` 随 pending.json 持久化，重启恢复不丢引用上下文。

## 6. 渠道工作目录（2026-10-12 起）

会话 key 早就是渠道级（chat_id / chat_id:thread_id），但**工作目录**此前只有虚拟 Bot 群独立；
普通群/频道/DM 共用 bot 目录 `workspaces/<bot>/`。2026-10-12 起统一为：

```
~/.agent-bridge/workspaces/<bot_key>/<dir_name(chat_id)>/
   chat.json     # 原始 chat_id / bot_key / chat_type / thread_id（sanitize 不可逆，反查用）
   mcp.json      # 该渠道的 MCP 配置（卡 abb-channel-mcp-config-20261012；LLM 自己读写）
   AGENTS.md     # 工作区指引（ensure_workspace_guide）
```

- 目录名 = `sanitize(chat_id)`；被改写/为空/Windows 保留名时追加 `-<8hex>`（`src/channel_dir.rs`）。
  不用派生 uuid：`(bot_key, chat_id)` 唯一性由父目录提供，可读性更重要。
- 会话 cwd = 该目录（`ChannelMeta.workspace` 的三个登记点：群/p2p/话题 都走它）；
  受限档（read-only / workspace-write）的 `writable_roots` 随会话 cwd（`pool.rs` 建会话时
  用 `SessionSandboxMeta::retarget_writable_root` 换成渠道目录）——否则受限会话写不了自己的 cwd。
- 旧 vb 目录 `workspaces/<bot>/vb/<fnv_uuid>/` 一次性改名迁到新路径
  （`virtualbot.rs::migrate_legacy_vb_location`，失败保留旧目录并留日志）。
- **会话文件（`sessions.json`）仍在 bot 级**：owner 明确会话文件不必进渠道目录。

## 7. 相关代码索引

- 会话持久化：`src/sessions.rs`（chat_id 单维度、单槽；老四槽/旧扁平 load 时折叠迁移）
- 并发/打断：`src/bridge.rs`（`chat_lock` / `cancel_flags` / `handle`）
- 各平台入站：`src/bridge.rs`（`on_payload` / `on_weixin` / `on_dingtalk`）、`src/dingtalk.rs`
- 发送：`src/feishu.rs`（`send_text` / `reply_text`）、`src/messenger.rs`（`send_thread_reply`、WeixinMessenger context_token）

## 8. 执行层并发与池化（2026-10-22 补，含一处易误读勘误）

> 起因：ki（pi-rust）对接评估中，外部协作者据 `pool.rs` 的 `channel_id → session_id`
> 映射误读为「一个句柄 = 一个 agent 子进程、渠道在进程内多路复用」，进而错误推出
> 「多渠道对单会话执行层（`ki acp serve`）会撞其单进程单会话限制」。本节如实记载
> 执行层并发模型，全部判据可在现文复核。

### 8.1 渠道内：单回合

同一渠道（chat_id / chat_id:thread_id）同时**只有一个在途回合**（`runtime/queue.rs`
每渠道队列 + 每渠道在途跟踪）。在途期间的新消息不并发执行：入队，经 steer 合并
（执行层声明 `_meta.steering.supported` 时原生插入；否则 cancel+merge 重提示）。
配套预算：单批 ≤50 条、渠道积压 ≤500 条（超限丢最老）、在途死线 = max_turn+100s、
重试 10 次（5s 倍增，300s 封顶）。

### 8.2 渠道间：不设并发上限（owner 2026-10-04 裁定）

`runtime/harness.rs::wanted_slots`：`pending.max(1)` —— **队列里有几个待跑会话就扩到
几个 agent**，回合经 `join_set` 并行执行。资源边界不靠限并发，靠：

1. **闲置回收**：额外槽位空闲 300s（`IDLE_REAP_AFTER`）回收，60s 巡检；0 号槽位是
   热实例，永不回收（免冷启动）。稳态 agent 进程数 = 正在跑的会话数。
2. **spawn 守卫**（v2.23.89）：同 key ≥500ms 启动间隔 + 连续失败熔断——「不限并发」
   不等于「瞬间起一堆」。

裁定理由（原注）：限制并发会把「会话互不影响」打折（第 9 个会话得排别人后面）；
**回收闲置进程才是真正的资源边界**。

### 8.3 勘误：`channel_id → session_id` 不是「一个进程多渠道复用」

`runtime/pool.rs` 的 `channel_id → session_id` 映射是**单个 agent 进程内部**「本进程
当前服务哪个渠道会话」的记账，不是跨渠道共享进程。池子是**会话级隔离的多槽位**
（`agents: Vec<Option<OwnedAgent>>`，每个槽位一个独立子进程 + 独立 AcpClient）；
跨渠道并发 = **多个 agent 子进程并行**。

推论：外部**单进程单活动会话**的执行层（如 `ki acp serve`，pi-rust `acp/mod.rs` 的
session/new 第二次拒绝）与本池子**天然契合**——每个并跑渠道各起一个执行层进程，
会话按各自 cwd 隔离，不存在「第二渠道被拒」的路径；不需要执行层为此支持进程内
多会话。
