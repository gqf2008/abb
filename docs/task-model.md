# ABB 任务模型（复盘 + 重新设计）

> 状态：**定稿 v3（P0 已完成）**。设计已评审收口、关键决策已拍板；**实现按 #326 批次推进**，
> 本文描述「设计」，某条是否已落地以 #326 的 checklist 与代码为准（**不出现「文档说做了、代码没做」**）。
> 关联：#326（落地批次）· #307（复盘 issue，已关）· #305（常驻进程托管）· #306（后台子代理）·
> #309（job 停不掉，已修）· #310（P1a 统一默认投递，已完成）· #321（waiter 顶替）·
> #158 · #69 · #21 · #137 · #164
>
> v2 说明：v1 对执行链的事实判断有误（把串行归因于 per-chat 锁、把 cancel 标志当有效机制、把
> `sessions.json` 当 ACP 会话的事实源）。经独立评审逐条核对后重写第 1、2 章。
> v3 说明：§4 的 Q1 / Q7 / Q8 / Q9 已按建议拍板（2026-09-14）。P3 / keepalive 的
> Q4 / Q5 / Q11 / Q14 与 Windows 进程树 Q15 已于 2026-09-18 拍板（来源：
> `abb-p3-b1-proc-supervisor-20260918`）；其余 Q 仍待定。

---

## 0. TL;DR

ABB 现有三类「任务」，但**真正决定行为的三件事是分开的**：谁和谁串行（执行容量）、会话上下文挂在哪（channel → session）、能不能被停（cancel 通路）。三者目前都隐式绑在「聊天频道」上。

重新设计要解开的是：

1. **执行容量** —— 现在每个 handle 是**单 slot**（`AgentPool::from_slots(vec![None])`），且 `job` 与聊天**共用同一个 channel**；后台能力天然抢同一个 slot。要么给 task 独立 handle/pool，要么把 harness 扩成多 slot。
2. **会话隔离** —— ACP 会话状态按 **channel UUID** 键控，`sessions.json` 只是镜像；只改 ABB 侧的 key 无效，必须定义独立的 task channel + ChannelMeta/workspace。
3. **取消通路** —— 现在 `run_job` 注册的 cancel 标志**根本没接进 ACP 等待路径**；真正能取消的是 `handle.cancel(channel_id)`。task 必须走真实 cancel token。
4. **投递默认与信任边界** —— 默认回创建者会话要靠**可信来源标记**，不能靠「地址相等」反推（会削弱防循环）。

---

## 1. 复盘

> ⏱️ **本章是 2026-09-14 复盘时的事实快照**，不是「今天的现状」。其中已修的部分（尤其
> **D 的取消通路失效 / S8**）保留原文以说明设计动机，但状态见 **#309** 与 #326 的 checklist——
> 判断「现在能不能停」请看 #309（现状）与 §4（设计），不要据本章下结论。

### 1.1 现在有哪些「任务」

| 名字 | 用户可见 | 载体 | 触发 | 生命周期 | 执行通道 | 投递 | 代码位置 |
|---|---|---|---|---|---|---|---|
| 回合内 agent | 聊天回复 | bridge dispatch | 用户消息 | 一次回合 | `channel_uuid(bot, chat_id)` | 回复原会话 | `src/bridge/virtualbot.rs` |
| `job`（定时任务） | `agent-bridge job` / 聊天自然语言 | `jobs.json` | `once` / `cron` | 到期跑一回合 | **同一个** `channel_uuid(bot, job.chat_id)` | `job.targets` | `src/schedule.rs`、`src/service.rs run_job` |
| `src/tasks.rs` | **否** | 进程内 | — | service 生命周期 | — | — | `src/tasks.rs:1`（Tokio `TaskGovernance`） |
| `deliver` | agent / CLI | 无状态 | 手动 | 一次投递 | — | 显式目标 / `--to-current` | `src/deliver.rs` |

**缺的**：立即触发的后台 agent 回合（#306）、常驻/长跑外部进程托管（#305）。

### 1.2 现有约束（代码事实）

**A. 串行发生在哪里（v1 判断错了，这里更正）**

- `chat_lock()`（`src/bridge/mod.rs:326`，字段 `:84`）在生产路径只覆盖**组装 prompt + push 进 harness**；push 成功后函数就返回（`src/bridge/virtualbot.rs:703-706` 起），**锁不覆盖整个 agent 回合**。
- 真正的单频道串行是两层：
  1. **per-channel in-flight 队列**：`src/buzz/queue.rs`——队列层支持不同频道各自积压/公平选择，但**当前单 slot harness 下，同一 handle 最终仍是全局串行**（真并行要么 B1 独立 handle，要么 B2 多 slot）；
  2. **每个 handle 的单 slot 懒池**：`let pool = AgentPool::from_slots(vec![None]);`（`src/buzz/harness.rs:413-416`）。
- **`run_job` 不取 chat_lock**，直接用 `channel_uuid(bot_key, job.chat_id)`（`src/service.rs:1341`）——**和聊天是同一个 channel**，于是两者在同一个 slot 池里排队。
- 调度侧防重入按 **`job.id`** 键控（`src/service.rs`），不是按 chat：同 chat 的多个 job 可以同时到点被 spawn。但它们**不再并发进入 harness**——`run_job` 全程持 `Bridge::job_gate(chat_id)`（#321 定的「同 chat 单飞串行」，见 B 节），后到的 job 在闸上排队。

**B. 同 channel 的等待者只有一个 ⇒ 同 chat 的 job 单飞串行**

`wait_turn_outcome` 的同步 waiter 是**每 channel 一个**（#309 前叫 `wait_turn_text`，已删）（`src/buzz/harness.rs`）：同一 channel 上多个等待者会**互相顶掉**（后登记者接管，前者拿到 `Closed`）；普通聊天回合结束时若该 channel 有 waiter，回合文本会先送给 waiter 而不是正常投递。

**已定语义（#321，2026-09-15 拍板）：同 chat 多 job = 单飞串行（single-flight serial）**，不允许第二条 job 顶掉第一条的 waiter。落点在**生产者侧**，不改 harness 的 waiter 表：

- `Bridge::job_gate(chat_id)`（`src/bridge/mod.rs`，字段 `job_gates`）——按 **`job.chat_id`（群根频道）** 分片的一把异步锁，跨 normal/granted 两个实例（同 chat 的两条不同 role 的 job 也要串行）。
- `run_job`（`src/service.rs`）在**整段轮次**持有它：`register_job_turn` → 取闸 → `upsert_channel` → `wait_turn_outcome` → 摘登记 → 生成 header/targets → **投递完成**才释放。后到的 job 因此停在闸上，而不是抢进 waiter 表。
- 与 `chat_lock` **分开**：`chat_lock` 是 chat 回合的会话/历史串行锁，让一个可能跑满 `MAX_TURN_DURATION` 的 job 占着它会把同 chat 的用户消息与 `deliver_turn_reply`/`flush_outbox` 一起卡住。
- `JobTurn` 必须在**取闸之前**登记，否则排队中的 job 对停止词不可见（停止词只命中在跑那条，排队的会在之后照跑）。`JobTurn.cancel` 标记即「排队期取消」：取到闸后先查它，命中就静默收尾。

因此 `Closed` 现在只剩**句柄已关闭（服务在关停）**这一个来源，仍按静默收尾处理、不复用「执行超时」文案。

> 仍未闭环（同源、但**不在**本批次）：job 与它所在 chat 的**普通聊天回合**共用同一 channel，若某个聊天回合正在 in-flight，job 的 waiter 会先取走那条聊天回合的文本（反向亦然）。单飞闸只覆盖 job↔job，堵不住 job↔chat——要关这条得改成「每 job 独立 channel」（丢掉与 chat 共享的 ACP session）或让 job 先等 channel 空闲，属独立决策，见 #321 评论。

**C. 会话上下文挂在哪**

- ACP 侧的真实状态是 `AgentPool::SessionState`，键是 **channel UUID**（`src/buzz/pool.rs:82-95`：`sessions: HashMap<Uuid, String>`）。
- `sessions.json` 是 `{chat_id: {session_id, started, sandbox_mode?}}`（`src/sessions.rs:3`），属于 ABB 侧镜像。
- **只改 `sessions.json` 的 key 不会改变 ACP 的 channel→session 映射**——v1 的 D1 就错在这。

**D. 取消通路（复盘时：实际失效，已由 #309 修复）**

- `run_job` 注册了 `_cancel_flag`（`src/service.rs:1321`），但**从未把它传给 ACP 等待路径**；而且紧邻的注释自相矛盾（`:1316-1320` 先说「共用同一 key」，又说「不再注册」）。
- 停止词处理只是置位 AtomicBool 后立即 return（`src/bridge/virtualbot.rs:358-364`），**因此反而不会走到真正的 cancel**。
- 真正能取消 ACP 回合的是 `buzz_cancel_reply` → `handle.cancel(channel_id)`（`src/bridge/virtualbot.rs:1180-1215`）。
- ⇒ **复盘时 job 不能可靠被停止词打断**；这是当时的实际缺陷、不只是设计问题，当时单开了 **#309**。
  **该缺陷已修**（harness 取消终态 → job 接线 → queued 取消三批合入，见 **#309**）；此处保留原文
  是为记录「为什么 task 必须走真实 cancel token」，不代表今天的现状。

**E. 投递目标与自环保护**

- `job.targets`（`src/schedule.rs:24`，`bot_key` 空 = 本 bot）
- `deliver --bot/--chat`（`src/deliver.rs:531` 附近解析）、`--to-current`（`:542-557` 校验来源确实等于目标）
- 自环判据是 `item.in_session && is_self_loop(item)`（`src/deliver.rs:231-236`），且 Router **专门防 `deliveries.json` 伪造 `in_session: true`**（`:252-256`、`:269-271`）

**F. 权限面是两份，不是一份**

- 旧 claude hook 路径：`src/guard.rs:618 check_abb_bin`
- **生产受限路径**：`crates/buzz-agent/src/shell_policy.rs:126 check_abb_bin`（调用点在 `crates/buzz-agent/src/devtools.rs:330-348` 附近）
- ⇒ 任何新子命令的白名单**必须两份都改**，只改 `src/guard.rs` 会漏掉生产路径。

**G. 桥状态文件是信任凭据**

`src/guard.rs:359-390` 把 `jobs.json` / `pending.json` 的 `role` 字段视为执行时信任的凭据（改写 = 把自己任务翻成 owner 执行），因此对受限会话禁写——**注意这是「只禁写不禁读」**（`:363-365`）。

⇒ 对新任务域，光有写保护不够：任务定义 / 运行态 / 日志含 prompt、target、env、cmd 与进程输出，**必须同时做读隔离**（放在 agent 读域之外，或两处策略都加 read deny）。详见 §2.2。

**H. 恢复语义**

`pending.json`：成功 reply 路径是 at-least-once（W1 重跑、W2 只补发、发出后崩溃会重复补发）；**Cancelled / Err 通知可能丢**（`src/pending.rs:10-17`）——不是全路径 at-least-once。

**I. 进程终止与孤儿清理**

- Unix：`process_group(0)` + `killpg`（`src/buzz/acp.rs:381-382`、`:1947-1955`）。
- **Windows：没有 Job Object 实现**，非 Unix 分支只 `start_kill()` 杀直接子进程（`src/buzz/acp.rs:1958-1963`）。#158 是历史方案，**当前代码里没有**。
- `agent-pids.json` 是 **legacy 只读**（写端已随 spawn 退役，`src/agent.rs:446-452`），且清理只认 claude/codex/pi 的命令行（`:477-490`）——**不能拿来托管任意 proc 任务**。

### 1.3 复盘结论：8 个结构性问题

| # | 问题 | 后果 |
|---|---|---|
| S1 | `job` / `task` / `spawn` 术语混用，`tasks.rs` 已被占用 | CLI 与模块撞名，说不清「task 指哪个」 |
| S2 | **执行容量是单 slot，且 job 与聊天共用 channel** | 后台能力天然抢同一 slot；同 chat 的 job 已按 #321 单飞串行，但 job↔chat 仍共用 waiter 槽 |
| S3 | 会话身份只有 channel UUID 一条线 | 想隔离就得造独立 channel + ChannelMeta，只改 `sessions.json` 无效 |
| S4 | 投递目标三套模型 + 自环豁免是硬编码第三态 | 新任务类型要重想一遍投递语义，默认值无处安放 |
| S5 | 生命周期只有「一次回合」 | 常驻/长跑只能 `nohup`/launchd（还丢 TCC，见 #251） |
| S6 | 权限面分散在两份白名单 + 状态文件信任名单 | 新增子命令容易漏改，漏了就是提权 |
| S7 | 可观测性弱（job 只有 `list`） | 没有 status/logs/退出码，跑丢了不知道 |
| S8 | **job 的取消语义实际失效**（注释与实现不一致） | 用户以为能停，其实停不掉（**复盘时的缺陷，已由 #309 修复**） |

---

## 2. 重新设计

### 2.1 术语与分层

**统一到一个概念：`Task`**，用两个正交轴描述，不再造新名词：

| 轴 | 取值 |
|---|---|
| **payload** | `agent`（跑一轮 agent 回合）· `proc`（跑一个外部进程） |
| **trigger** | `now`（立即）· `once` · `cron` · `interval` · `keepalive` |

| 能力 | 表达 | 现状 |
|---|---|---|
| 定时任务 `job` | `agent` + `once`/`cron` | 已实现（迁移为特例） |
| 后台子代理 #306 | `agent` + `now` | 新增 |
| 常驻进程 #305 | `proc` + `keepalive` | 新增 |

**CLI**：统一 `abb task …`；`job` 保留为**兼容别名**（skill `schedule` 与用户脚本依赖它，不能直接删）。
**内部模块改名**：`src/tasks.rs` → `src/svc_tasks.rs`，把 `tasks` 让给用户可见域（纯改名，单独 commit）。

### 2.2 统一数据模型

```jsonc
{
  "schema_version": 1,
  "id": "tk_20260911_a1b2c3",
  "name": "personcam",
  "bot_key": "微信龙虾",
  "created_by": {
    "role": "owner",                       // config::SenderRole（只能区分 Owner/Granted）
    "bot_key": "微信龙虾",
    "chat_id": "wx_chat_xxx",
    "sender_id": "",                       // 当前 agent 环境尚无可靠 sender_id —— 见 Q9
    "capability_hash": ""                  // 若是 capability 方案：只存 hash（明文不进任何 agent 可读文件）
  },
  "payload": { "kind": "agent", "prompt": "……", "cwd": "", "cmd": [], "env": {} },
  "trigger": { "kind": "cron", "expr": "0 9 * * *", "timezone": "" },   // timezone 暂不支持（非空会被 validate 拒）
  "resume_on_boot": true,                                                // false = 逐任务 opt-out 自动恢复
  "delivery": { "targets": [], "default": "creator" },
  "channel": { "mode": "dedicated", "channel_id": "……" },   // 见 D1a
  "limits": { "timeout_secs": 0, "grace_secs": 10, "max_restarts": 3, "backoff": "exponential", "log_max_bytes": 10485760 },
  // state 是运行态投影：实际落在 tasks-state.json，不回写本定义文件（见 Q12）
  "state": { "kind": "idle", "pid": null, "run_seq": 0, "started_at": null, "last_exit_code": null, "restarts": 0 }
}
```

**存储**（按 bot 隔离，**放在 agent 读域之外**）：

- **定义**：`~/.agent-bridge/tasks/<bot>/tasks.json`（由 `jobs.json` 迁移）
- **运行态**：`~/.agent-bridge/tasks/<bot>/tasks-state.json`（pid / 重启计数 / 最近退出码）——**与定义分开**，避免高频写污染定义文件（见 Q12）。数据模型里的 `state` 只是运行态投影，**不写回定义文件**。
- **日志**：`~/.agent-bridge/tasks/<bot>/task-logs/<id>.log`

> ⚠️ **为什么不放在 bot 工作区里**（评审三轮的阻塞项）：
>
> 现有桥状态文件的保护是**只禁写不禁读**（`src/guard.rs:363-365` 明确写「内容本属工作区可读范围」），fork 侧 `ToolPolicy::build` 也把 workspace 放进 `read_roots`（`crates/buzz-agent/src/wire.rs:168` 附近，读检查在 `crates/buzz-agent/src/devtools.rs:923` 附近）。
>
> 如果把 tasks 文件放在工作区里，**同一 bot 下任何受限会话都能直接 `cat` 出其它任务的 prompt / target / env / cmd 和全部日志**——D4 里「`task list/cancel/logs` 只限自己创建」的访问控制会被文件系统读取直接绕过（capability 只存 hash 只能防 token 被偷，防不住元数据与日志泄露）。
>
> 所以这三份文件**必须放在 agent 读域之外**，只通过受权的 `$ABB_BIN task …` 暴露过滤后的视图。
>
> 备选方案（若因迁移成本必须留在工作区内）：在 **root guard 与 buzz-agent fork 两处读策略里都加这些路径的 read deny**，并补「受限会话直接读任务文件被拒」的回归测试。二选一，不接受「既不读保护、又把它写成权限边界」。

### 2.3 关键设计决策

#### D1 后台 agent 任务的隔离（拆成三件，缺一不可）⭐

v1 只写「独立 session key」是**不够的**。三件事必须分开定：

**D1a 独立 channel**：task 使用独立 channel UUID（例如由 `channel_uuid(bot, "task:<id>")` 派生），并显式定义对应的 `ChannelMeta`（workspace / cwd / 指令作用域）。在**当前共享 handle** 的模型下，独立 channel 是隔离 ACP session（`src/buzz/pool.rs:82-95`）的必要手段（走 B1 独立 handle 也能达到同样隔离）。

- ⚠️ **必须设 `adhoc: true`（或等价加入巡检登记）**：harness 频道巡检会删掉「不在 `sync_roots` 中且非 adhoc」的根频道（`src/buzz/harness.rs` 的 `Cmd::SyncRoots`，`:530` 起）。现有 `run_job` 正是这么做的（`src/service.rs:1353-1370` 的 `ChannelMeta { …, adhoc: true }`）。漏了这步，task channel 会在下次巡检被排空队列 / 失效 session。
- task channel 的 UUID 命名空间不要与普通聊天 channel 混用——正式实现建议新增独立的 `task_channel_uuid()`（而不是直接复用 `keys.rs` 的聊天命名空间）。

**D1b 独立执行容量**：当前每个 handle 是单 slot（`src/buzz/harness.rs:413-416`），且 task 与聊天会分到不同 channel 但**仍共享同一个 handle**。所以「不占线」与「同会话并发多个子代理」（#306 验收）在单 slot 下**不可能成立**。两个可选方案：

- **B1**：给 task 独立 handle / 独立 pool（聊天与 task 各一套）——改动小、语义清晰，但每套都是进程与内存开销；
- **B2**：把 harness 扩成多 slot 池——更省资源，但动的是共享执行层，回归面大。

**这条是 #306 能否成立的前提，必须先定（Q7）。**

**D1c 取消**：**不复用 `cancel_flags`**（§1.2 D 已证其失效），改用 `harness.cancel(task_channel)` / 每 task 独立 `CancellationToken`。

但 `handle.cancel(channel_id)` **只作用于 in-flight 那一轮**——排队中的消息不在取消范围（`src/bridge/virtualbot.rs:1185-1193` 注释明说；实现见 `src/buzz/harness.rs:526-529` `signal_in_flight_task`）。后台 task 若因单 slot 排在聊天之后，`task cancel` 会回「没有在跑任务」，而它稍后仍会执行。取消语义必须按状态分别定义：

| task 状态 | 取消动作 |
|---|---|
| `running` | `harness.cancel(task_channel)` |
| `queued`（已进 harness 队列但未开跑） | 排空该 channel 的队列批次（不能只发信号） |
| `scheduled` / `backoff` | 移除任务或取消 token，保证不再触发 |
| 任意 | 取消后不得再产生副作用或投递（幂等键兜底） |

同时在会话内提供显式的 `task cancel <id>`（见 Q3）。

**已落地（#306，2026-09-15）**：`task` 走的是 `buzz::oneshot`（自起独立 handle + 独立
频道，不是聊天 channel），所以上表里的 `queued` 行退化成「任务还在 `next_pending` 待
认领」——实现比原设想简单：

- **通道方向 = CLI 写请求、service 消费**（运行态单写者，Q12）。`task cancel <id>`
  只往 `~/.agent-bridge/tasks/<bot>/cancel-requests/<id>` 落一个请求文件，不改
  `tasks-state.json`；
- 运行中：task worker 在 `run_attempt` 里起一个 watcher 轮询该文件（1s），命中即
  `child_token().cancel()`；子令牌与「service 关停」父令牌合并后交给
  `oneshot::oneshot_turn(..., external_cancel)`，走同一条拆栈通路（cancel 回执 + 杀进程组）；
- 未开跑：worker 每轮 loop 开头 `consume_cancel_requests` 把 `Pending` 任务直接判
  `Cancelled`（不再被认领）；已是终态的任务**不被改写**，只清请求文件；
- 轮次结束时消费掉自己的请求文件（否则同 id 的下一次重跑会立刻又被自己取消）；
- **取消后一律不投递**（用户已明确不要结果；与服务关停联动同口径），只留运行态
  `last_error` + `task-logs`。

**配套（容易漏的）**：

- `session_manage.rs` / `session_gc.rs` 会枚举 `chat_keys`——内部 task channel 必须**排除**，否则会被当成聊天会话列出/回收；
- `agents_md.rs:44-79` 会按 session key 拆分推导 vb session 目录——`task:<id>` 可能丢失创建者的指令作用域，必须显式保存 workspace/instruction scope。

#### D2 默认投递 = 创建者会话（但不要改自环豁免的判据）

老板已拍板：`spawn` / `job` / `deliver` / `task` 缺省**一律回创建者会话**，来源取桥注入的 `AGENT_BRIDGE_BOT_KEY` + `AGENT_BRIDGE_CHAT_ID`；显式 `--to` / `--to-current` 优先。

**已落地（#306，2026-09-15）**：

- `task add --to bot_key:chat_id`（只按第一个冒号切；省略 `bot_key` = 本 bot）/ `--to-current`；
  仍**只支持一个投递目标**（多目标由 `Task::validate` 显式拒绝，等真需求再放开）；
- 执行侧按 `targets[0].bot_key` **真投那个 bot**（旧实现忽略 `bot_key`、一律按本 bot 投，
  于是 `validate` 只能显式拒绝跨 bot；两处一并解除）。`in_session` 豁免只在
  「目标确实等于创建者会话」时置位，`--to` 指到别处一律走显式跨会话（受
  `cross_delivery_enabled` 约束）；
- **第二告警通道**：默认目标 = 创建者会话，`Router` 的回源告警也就发回那个**已失效**的
  会话 ⇒ 等于没有告警。`Router::deliver` 现在返回 `DeliveryOutcome`，task worker 据此在
  投递失败时把原因写进运行态 `last_error`（`task status` 可见）并投该 bot 的
  **主会话**（`bots[bot_key].primary_chat_id`，与 `run_job` 的回落同源）；主会话与失败
  目标相同时跳过（再发一次没有意义），发不出去只记日志。

⚠️ **不要**按 v1 的想法把豁免改成「同一 (bot, chat) 即视为 in_session」——那会让任何同地址项自动绕过 `cross_delivery_enabled` 与 10 分钟防循环去重，而 Router 现在**专门防伪造 `in_session`**（`src/deliver.rs:252-256`）。

正确做法：保留显式、可信的来源标记，在信封里引入内部 **`DeliveryOrigin`**（`InSession` / `Scheduled` / `TaskCompletion`），由 CLI / service 在**确实**目标等于来源时设置，而不是从地址反推。

另外两条：

- 给 task 结果带上稳定的 `task_id + run_seq` 幂等键，并把「已投递」持久化；恢复时**只重投、不重跑**。对外只承诺 at-least-once，避免把 IM 平台能力写成 exactly-once；
- `DeliveryItem` 目前只有 bot + chat，**没有 thread/topic 维度**——默认回创建者时要明确落群根，还是扩展 thread（见 Q10）。

#### D3 proc 由 ABB 派生以继承 TCC（**待验证，验不过就改方向**）

`proc` 由 ABB 直接 spawn（而非 launchd/systemd），使子进程落在 ABB 的 TCC 责任面内。**Step 0 必须先做，且要按下面这些条件验**：

- 用 **release 签名后的 `ABB.app`**（不是裸二进制）；
- 覆盖三类：直接子进程 / 孙进程 / 长跑进程；
- 覆盖三种权限：摄像头 / 麦克风 / AppleEvents；
- 先重置 TCC 状态，确认**不重新弹窗、不直接拒绝**；
- 覆盖三条生命周期：ABB 正常退出 / 崩溃 / 升级重启。

#### D4 权限边界（现状比 v1 写的复杂）

- `proc` 是**任意命令执行入口**：默认 **owner-only**；granted 直接禁止，或仅在可证明的 OS sandbox 内允许（Q8）。
- 「只限自己创建的」**缺少可信身份**：`SenderRole` 只有 `Owner | Granted`（`src/config.rs:823-843`），同一群两个 granted 用户无法区分。可选：创建时下发 **capability token**，或**直接 owner-only 管理**（更简单更严）。
- ⚠️ **capability token 不能明文落盘**：任务文件即便挪到 agent 读域之外，也应按「只存 hash（`capability_hash`）」处理；能读到明文就不构成权限边界。
- 白名单与保护名单**两份都要改**：`src/guard.rs:618` + `crates/buzz-agent/src/shell_policy.rs:126`。
- ⚠️ **只做写保护不够，必须做读隔离**：任务定义 / 运行态 / 日志含 prompt、target、env、cmd 与进程输出；沿用现有「只禁写不禁读」（`src/guard.rs:363-365`）会让「只限自己创建」被 `cat` 直接绕过。落地方式见 §2.2（放 agent 读域之外，或两处策略都加 read deny + 回归测试）。
- v1 那句「`task add` 全放行」不可接受。

#### D5 进程生命周期（需要真正的 supervisor，不能复用 legacy）

- **Windows 目前没有 Job Object**（`src/buzz/acp.rs:1958-1963` 只杀直接子进程）。Q15 已拍板以真做 Job Object 为目标；真机验收通过前，Windows 上 `task add --proc` **显式非零拒绝**，不静默降级、不假装支持。
- `agent-pids.json` 是 legacy 只读且只认 claude/codex/pi 命令行，**不能复用**（`src/agent.rs:446-452`、`:477-490`）。
- 停止语义已拍板：默认 10s 宽限（`limits.grace_secs` 可逐任务覆盖）→ SIGKILL，并**对进程组 / Job Object 整体发信号**；仍需落地退出码回收与 ABB 崩溃后下次启动的清理（**Unix 上父进程被 SIGKILL 不会自动带走独立进程组**）。
- 防自杀：#164 是动机，但现有保障只是 prompt 护栏（`src/agents_md.rs:107-114`）+ 恢复次数冻结（`src/bridge/recover.rs:6-10`、`:137-160`），**不是进程级边界**。
- ⚠️ **owner-only 不等于「禁止 owner 会话里的 agent 建自杀命令」**：owner 会话里的 agent 角色同样是 owner，仍可 `task add --proc` 出 `pkill` / `taskkill` / `kill <ABB pid>`。
- 所以 P3 开工前必须在下面几条里**选一条能落地的**（Q8），而不是只写「supervisor 保证」：
  1. **禁止 agent 创建 `proc`**（只允许 GUI / 人类入口）——最简最严；
  2. `proc` 创建必须经过**不可绕过的二次确认**；
  3. 用 **OS sandbox / 独立用户 / 受限 syscall** 做硬隔离；
  4. 对命令与信号能力做**可执行白名单**（含禁止向 ABB 自身 pid / 进程组发信号）。
- 以上都做不到，就**明确接受该风险并写进文档**，不要声称存在「防自杀边界」。**不要用「pid 大小」这类伪规则**判父子。

#### D6 可观测与日志

基础能力（pid / 退出码 / 日志 sink）**必须与 supervisor 同期落地**，不能留到后面阶段——没有日志 drain 和退出记录的 proc supervisor 不可运行。Q4 已封板 10 MiB × 3 份 / 终态 30 天；`proc` / keepalive 必须 spawn 后**流式写盘**并按字节累计轮转，这是 P3 准入项而非可选项。轮转总量告警/更完整视图仍可后续补。

#### D7 与现有 `job` 的关系

`job` 保留为兼容别名，至少一个大版本。迁移**不能只是一次性文件转换**，必须定义：

- 兼容期 `job add/del` 写哪个文件；
- service 如何双读、谁是单写者；
- 定义文件与运行态是否分离（本文建议分离）；
- 迁移的原子性与回滚路径。

迁移测试要覆盖旧 skill 的 `add / list / del` 三件套在**迁移后仍可用**。

### 2.4 分阶段落地

> 落地追踪：**#326**（批次 issue，含 checklist 与逐阶段验收）。

| 阶段 | 内容 | 依赖 | 风险 | 状态 |
|---|---|---|---|---|
| **P0** | 本设计定稿（事实已按评审修正 + Q1/Q7/Q8/Q9 拍板） | — | 低 | ✅ 已完成 |
| **P1a** | 统一默认投递目标（`deliver` / `job`） | D2 | 低 | ✅ 已完成（#310） |
| **P1b** | 可信 `DeliveryOrigin` + 豁免重构（含防循环回归） | D2 | **中**——碰防循环安全，不能当顺手改 | 待开工 |
| **P2a** | task store + CLI + 权限/身份模型 | D4 | 中 | ✅ 已完成（task store/CLI/guard 白名单，#326） |
| **P2b** | task channel + 执行容量方案 + cancel/GC/workspace 接入 | D1a/b/c、Q7 | **高**（动共享执行层） | 🔶 部分：A(agent 载荷)/B(cancel)/C(触发编排)/D(GC) 已完成；**keepalive 未做**（Q5/Q14 已拍板，待 B2 实现） |
| **P3** | `proc` supervisor（含 Windows Job Object、基础日志/退出记录） | D3 Step 0、D5 | 中高 | 🔶 **进行中（B1 proc supervisor 已开工，`abb-p3-b1-proc-supervisor-20260918`）** |
| **P4** | 日志轮转 / 熔断告警 / 更完整可观测 | D6 | 低 | 待开工 |
| **P5** | `job` → `task` 迁移（别名保留） | D7 | 中（兼容面广） | 待开工 |

> 顺序建议：**P1a 已完成**（#310）；此后按 **#326** 的 checklist 推进——**P3 的 TCC Step 0 先行**（一条命令 + 一次真机验证，决定 P3 是否继续），P1b/P2a 可视情况并行。P2/P3 **共用** task store / CLI / 投递 / 状态层，但**不共用执行引擎**（agent 走 ACP/pool，proc 走 supervisor）。
>
> 「立即返回、不占线、可并发」在 **P2b 完成前不能对外宣称**。

#### P2b-C 触发编排 / P2b-D 回收（2026-09-16 落地）

**触发档 → 认领语义**（`src/task_run.rs::is_due_for_claim`，纯函数、逐档有单测）：

| 触发档 | 何时认领 | 说明 |
|---|---|---|
| `now` | 登记即跑一次 | #306 的后台子代理，行为不变 |
| `once` | `expr` 时间点 `<= now` | **错过后补跑**（与 `job` 的 `Job::is_due` 同语义）；跑过即终态，不重复 |
| `cron` | 当前分钟匹配 5 段表达式 | **同一分钟只触发一次**（`last_fired_at` 分钟桶去重；worker 每 2s 轮询，无此记账会反复触发） |
| `interval` | 首次即跑，之后每 N 秒 | `expr` 支持 `30`/`30s`/`5m`/`2h`/`1d`，**下限 5 秒**（比轮询还密只会把模型打成忙循环） |
| `keepalive` | **不认领** | B1 不做；重启恢复/信号语义（Q5/Q14）已拍板，按 B2 实现，不假装支持 |

记账字段：`TaskRuntime.last_fired_at`（**认领时刻**，serde default 向后兼容）；与 `Running`
状态共同保证同一任务不会并发跑两轮。重复档（cron/interval）跑完一轮回到可认领状态；
`Cancelled` 是用户终态，要再跑得重新登记。

表达式在**登记期**校验（`Task::validate`）：once 必须 `YYYY-MM-DD HH:MM`、cron 必须 5 段、
interval 必须合法且 ≥5 秒——否则当场拒绝，不留「永远不触发」的静默失效。

**P2b-D 回收口径**（安全边界：**绝不自动删用户任务定义**）：

- **日志轮转**：单文件超 `limits.log_max_bytes`（默认 10MB）时 `id.log → id.log.1 → id.log.2`，
  共保留 3 份（含当前）。旧实现是「超上限就静默不再写」——日志是排障唯一入口，静默停写更坏。
  （Q4 已封板为 10 MiB × 3 / 终态 30 天；份数仍由 `LOG_KEEP_FILES` 单点维护。）
- **孤儿清理**：运行态无对应定义（`task rm` 竞态/手改文件）时，状态行与**日志一起**清。
- **终态日志保留期**：终态任务的日志超 30 天（`LOG_RETENTION_DAYS`）回收，**定义与运行态保留**
  （`task status` 仍能看到上次结果与错误）。
- proc 载荷的 pid 账本/退出码回收属 P3，不在此列。

---

## 3. 风险

| 风险 | 说明 | 缓解 |
|---|---|---|
| 执行容量方案选错 | B1/B2 决定 #306 的并发上限与共享层回归面 | Q7 先拍板；P2b 前写性能/并发回归 |
| TCC 继承不成立 | D3 是 #305 的地基 | Step 0 先行，验不过就改方向 |
| 会话隔离的体验落差 | 后台任务看不到聊天上下文 | 文档写明；`--with-context` 观察需求后再做 |
| 迁移破坏 `job` 使用者 | skill 与用户脚本依赖 `job` CLI | 别名保留 + 三件套迁移测试 |
| proc 变成提权入口 | 任意命令 + ABB 权限面 | owner-only + 两份白名单 + 状态文件保护 |
| 任务结果重复投递 | 完成但投递失败后重试 | `task_id + run_seq` 幂等键 + 持久化已投递；只重投不重跑，仅承诺 at-least-once |

---

## 4. 决策表

**✅ 已拍板**（2026-09-14，采纳原「建议」列；如与预期不符请改本表 + #326）

| # | 问题 | 结论 |
|---|---|---|
| Q1 | CLI 命名 | **`abb task`**；`job` 保留为兼容别名；内部 `src/tasks.rs` → `src/svc_tasks.rs` 让出命名 |
| Q7 | **执行容量**：每 bot 几个 task worker？task 与聊天谁优先？超限排队还是拒绝？ | **B1：task 用独立 handle/pool**（不与聊天共用单 slot）→ task 与聊天互不阻塞。B2（harness 多 slot）暂不做，留作后续扩容路径。**超限排队**（不拒绝）；每 bot 默认 1 个 task worker，上限可配 |
| Q8 | **`proc` 权限边界**：owner-only？还是允许 granted 在 OS sandbox 内？ | **禁止 agent 创建 `proc`**（只允许 GUI / 人类入口）；受限会话完全不进白名单。理由：owner 会话里的 agent 角色**也是 owner**，光靠 owner-only 挡不住 agent 写出 `pkill`/`kill <ABB pid>` 这类自伤命令（见 D5） |
| Q9 | **「自己创建的」身份粒度** | **owner-only 管理**，不引入 capability token；后续确有跨用户需求再议 |

**✅ P3 / keepalive 已拍板**（2026-09-18，来源：`abb-p3-b1-proc-supervisor-20260918`）

| # | 问题 | 结论 |
|---|---|---|
| Q4 | 日志上限、保留份数与长跑载荷写盘 | **10 MiB × 3 / 终态 30 天封板**；`proc` / keepalive **必须流式写盘**（spawn 后持续 drain stdout/stderr，按字节累计判轮转），这是 P3 准入项，不是可选项 |
| Q5 | ABB 重启后是否恢复常驻任务 | **默认恢复 + 逐任务 `resume_on_boot=false` opt-out + 不补历史周期**。恢复走独立路径（不复用 `requeue_orphans`）；记录进程代际身份；恢复必须经过 backoff + 熔断；`cancel` 后不得自动拉起 |
| Q11 | 任务完成但投递前崩溃 | **`task_id + run_seq` 稳定键 + 持久化「已投递」+ 只重投不重跑**；对外只承诺 **at-least-once**，不承诺 exactly-once |
| Q14 | 进程退出/信号/补跑语义 | 默认宽限 **10s**（`limits.grace_secs` 可覆盖）→ **SIGKILL**；**对进程组 / Job Object 整体发信号**。补跑保持现状：once 补 1 次，cron/interval 不补历史周期 |
| Q15 | Windows 进程树 / Job Object | **A 为目标 + D 兜底**：Windows 真做 Job Object；未通过真机验收前，`task add --proc` 在 Windows 上**显式非零拒绝**，不许假装支持 |

**⏳ 待定**（有建议默认值，不阻塞 P2a；定稿在对应阶段开工前）

| # | 问题 | 建议默认 / 说明 |
|---|---|---|
| Q2 | 后台 agent 是否支持「带聊天上下文」模式 | **先不做**，观察需求 |
| Q3 | 会话内能否停后台任务 | **能**，但须显式 `task cancel <id>`（避免误杀）——已落地（#306）：CLI 落取消请求文件，worker 消费；在跑的真拆栈、未开跑的不再开跑，取消后不投递 |
| Q6 | 是否允许跨 bot 建 `proc` | **不允许**（bot 归属即权限面） |
| Q10 | **topic/thread 投递**：默认回创建者时回群根还是原话题？`DeliveryItem` 是否要加 thread 维度？ | 现在根本没有 thread 字段，回错地方是静默的 |
| Q12 | **定义与运行态存储**：`tasks.json` 是否单写者？CLI 与 service 严格分离读写？ | 决定并发写与迁移原子性 |
| Q13 | **`payload.agent` 如何承接 #306 的 backend/模型供应商参数** | 现在模型只有 prompt/cwd，接不了供应商 |

---

## 5. 附：本次复盘用到的代码索引

> 行号是**复盘时（2026-09-14）的快照**，已随代码漂移；其中「cancel 标志（失效）」一项所指的代码
> 已被 #309 删除，仅作历史索引保留。

| 主题 | 位置（函数名 + 起始行，行号会漂） |
|---|---|
| per-chat 锁字段 / 取锁 | `src/bridge/mod.rs:84`、`chat_lock():326` |
| 锁只覆盖 push（生产路径） | `src/bridge/virtualbot.rs:703` 附近 |
| per-channel in-flight 队列 | `src/buzz/queue.rs`（模块头注释） |
| 单 slot 懒池 | `src/buzz/harness.rs:413`（`from_slots(vec![None])`）、`:732` 附近 |
| 每 channel 单个同步 waiter | `src/buzz/harness.rs:287`、`:1036` |
| ACP 会话状态（channel UUID 键控） | `src/buzz/pool.rs:82`（`SessionState.sessions`） |
| ABB 侧会话镜像 | `src/sessions.rs:3` |
| 定时任务定义 / 存储 / 热重载 | `src/schedule.rs:24`（`JobTarget`）、`:31`（`Job`）、`:56`（mtime）、`:80`（refresh） |
| 定时任务执行 | `src/service.rs:1285 run_job`（channel 见 `:1341`） |
| ~~cancel 标志（失效）~~ 已由 #309 删除 | 复盘时 `src/service.rs:1316-1321`；真实取消 `src/bridge/virtualbot.rs:1180` |
| 停止词处理 | `src/bridge/virtualbot.rs:358` |
| `job` CLI | `src/main.rs:345`（分发）、`:549`（`run_job_cli`） |
| 投递目标 / 自环判据 / 防伪造 | `src/deliver.rs:231`、`:252`、`:531`、`:542` |
| `$ABB_BIN` 白名单（两份） | `src/guard.rs:618`、`crates/buzz-agent/src/shell_policy.rs:126` |
| 桥状态文件写保护 | `src/guard.rs:359` |
| service 后台任务治理 | `src/tasks.rs:1` |
| 待处理恢复语义 | `src/pending.rs:10` |
| 进程组 / killpg（Unix） | `src/buzz/acp.rs:381`、`:1947` |
| 非 Unix 只杀直接子进程 | `src/buzz/acp.rs:1958` |
| legacy agent-pids（只读） | `src/agent.rs:446`、`:477` |
