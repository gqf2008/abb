# ABB 任务模型（复盘 + 重新设计）

> 状态：**草案 v2，待评审**（#307）。本文只描述现状、设计与迁移路径，**不代表已实现**。
> 关联：#305（常驻进程托管）· #306（后台子代理）· #158 · #69 · #21 · #137 · #164
>
> v2 说明：v1 对执行链的事实判断有误（把串行归因于 per-chat 锁、把 cancel 标志当有效机制、把
> `sessions.json` 当 ACP 会话的事实源）。经独立评审逐条核对后重写第 1、2 章。

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
  1. **per-channel in-flight 队列**：`src/buzz/queue.rs`（同频道排队、攒批；不同频道可并行）；
  2. **每个 handle 的单 slot 懒池**：`let pool = AgentPool::from_slots(vec![None]);`（`src/buzz/harness.rs:413-416`）。
- **`run_job` 不取 chat_lock**，直接用 `channel_uuid(bot_key, job.chat_id)`（`src/service.rs:1341`）——**和聊天是同一个 channel**，于是两者在同一个 slot 池里排队。
- 调度侧防重入按 **`job.id`** 键控（`src/service.rs:813-854`），不是按 chat：不同 job 可以在 Rust 层并发进入 `run_job`，然后一起在 harness 队列里排队。

**B. 同 channel 的等待者只有一个**

`wait_turn_text` 的同步 waiter 是**每 channel 一个**（`src/buzz/harness.rs:287-301`）：同一 channel 上多个等待者会互相顶掉；普通聊天回合结束时若该 channel 有 waiter，回合文本会先送给 waiter 而不是正常投递（`src/buzz/harness.rs:1036-1043`）。

**C. 会话上下文挂在哪**

- ACP 侧的真实状态是 `AgentPool::SessionState`，键是 **channel UUID**（`src/buzz/pool.rs:82-95`：`sessions: HashMap<Uuid, String>`）。
- `sessions.json` 是 `{chat_id: {session_id, started, sandbox_mode?}}`（`src/sessions.rs:3`），属于 ABB 侧镜像。
- **只改 `sessions.json` 的 key 不会改变 ACP 的 channel→session 映射**——v1 的 D1 就错在这。

**D. 取消通路（当前实际失效）**

- `run_job` 注册了 `_cancel_flag`（`src/service.rs:1321`），但**从未把它传给 ACP 等待路径**；而且紧邻的注释自相矛盾（`:1316-1320` 先说「共用同一 key」，又说「不再注册」）。
- 停止词处理只是置位 AtomicBool 后立即 return（`src/bridge/virtualbot.rs:358-364`），**因此反而不会走到真正的 cancel**。
- 真正能取消 ACP 回合的是 `buzz_cancel_reply` → `handle.cancel(channel_id)`（`src/bridge/virtualbot.rs:1180-1215`）。
- ⇒ **当前 job 不能可靠被停止词打断**。这是现存缺陷，不只是设计问题（建议单开 bug）。

**E. 投递目标与自环保护**

- `job.targets`（`src/schedule.rs:24`，`bot_key` 空 = 本 bot）
- `deliver --bot/--chat`（`src/deliver.rs:531` 附近解析）、`--to-current`（`:542-557` 校验来源确实等于目标）
- 自环判据是 `item.in_session && is_self_loop(item)`（`src/deliver.rs:231-236`），且 Router **专门防 `deliveries.json` 伪造 `in_session: true`**（`:252-256`、`:269-271`）

**F. 权限面是两份，不是一份**

- 旧 claude hook 路径：`src/guard.rs:618 check_abb_bin`
- **生产受限路径**：`crates/buzz-agent/src/shell_policy.rs:126 check_abb_bin`（调用点在 `crates/buzz-agent/src/devtools.rs:330-348` 附近）
- ⇒ 任何新子命令的白名单**必须两份都改**，只改 `src/guard.rs` 会漏掉生产路径。

**G. 桥状态文件是信任凭据**

`src/guard.rs:359-390` 把 `jobs.json` / `pending.json` 的 `role` 字段视为执行时信任的凭据（改写 = 把自己任务翻成 owner 执行），因此对受限会话禁写。`tasks.json` / 任务日志 / 运行态必须进入同一名单——**并且 fork 侧也要保护**。

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
| S2 | **执行容量是单 slot，且 job 与聊天共用 channel** | 后台能力天然抢同一 slot；同 channel 的 waiter 还会互相顶掉 |
| S3 | 会话身份只有 channel UUID 一条线 | 想隔离就得造独立 channel + ChannelMeta，只改 `sessions.json` 无效 |
| S4 | 投递目标三套模型 + 自环豁免是硬编码第三态 | 新任务类型要重想一遍投递语义，默认值无处安放 |
| S5 | 生命周期只有「一次回合」 | 常驻/长跑只能 `nohup`/launchd（还丢 TCC，见 #251） |
| S6 | 权限面分散在两份白名单 + 状态文件信任名单 | 新增子命令容易漏改，漏了就是提权 |
| S7 | 可观测性弱（job 只有 `list`） | 没有 status/logs/退出码，跑丢了不知道 |
| S8 | **job 的取消语义实际失效**（注释与实现不一致） | 用户以为能停，其实停不掉（现存缺陷） |

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
    "sender_id": "",                       // 当前 agent 环境尚无可靠 sender_id —— 见 Q3
    "capability": "cap_xxx"                // 创建时下发的管理凭据（若采用 capability 方案）
  },
  "payload": { "kind": "agent", "prompt": "……", "cwd": "", "cmd": [], "env": {} },
  "trigger": { "kind": "cron", "expr": "0 9 * * *", "timezone": "Asia/Shanghai" },
  "delivery": { "targets": [], "default": "creator" },
  "channel": { "mode": "dedicated", "channel_id": "……" },   // 见 D1a
  "limits": { "timeout_secs": 0, "max_restarts": 3, "backoff": "exponential", "log_max_bytes": 10485760 },
  "state": { "kind": "idle", "pid": null, "started_at": null, "last_exit_code": null, "restarts": 0 }
}
```

**存储**（按 bot 隔离，与现有目录约定一致）：

- **定义**：`~/.agent-bridge/workspaces/<bot>/tasks.json`（由 `jobs.json` 迁移）
- **运行态**：`~/.agent-bridge/workspaces/<bot>/tasks-state.json`（pid/重启计数/最近退出码）——**与定义分开**，避免高频写污染定义文件（见 Q6）
- **日志**：`~/.agent-bridge/workspaces/<bot>/task-logs/<id>.log`

三份文件都要进 §1.2 G 的状态文件写保护名单（**root guard + fork 两份**）。

### 2.3 关键设计决策

#### D1 后台 agent 任务的隔离（拆成三件，缺一不可）⭐

v1 只写「独立 session key」是**不够的**。三件事必须分开定：

**D1a 独立 channel**：task 使用独立 channel UUID（例如由 `channel_uuid(bot, "task:<id>")` 派生），并显式定义对应的 `ChannelMeta`（workspace / cwd / 指令作用域）。这是隔离 ACP session 的**唯一有效手段**（`src/buzz/pool.rs:82-95`）。

**D1b 独立执行容量**：当前每个 handle 是单 slot（`src/buzz/harness.rs:413-416`），且 task 与聊天会分到不同 channel 但**仍共享同一个 handle**。所以「不占线」与「同会话并发多个子代理」（#306 验收）在单 slot 下**不可能成立**。两个可选方案：

- **B1**：给 task 独立 handle / 独立 pool（聊天与 task 各一套）——改动小、语义清晰，但每套都是进程与内存开销；
- **B2**：把 harness 扩成多 slot 池——更省资源，但动的是共享执行层，回归面大。

**这条是 #306 能否成立的前提，必须先定（Q1）。**

**D1c 取消**：用 `harness.cancel(task_channel)` 或每 task 独立 `CancellationToken`；**不复用 `cancel_flags`**（§1.2 D 已证其失效）。同时在会话内提供显式的 `task cancel <id>`（见 Q3）。

**配套（容易漏的）**：

- `session_manage.rs` / `session_gc.rs` 会枚举 `chat_keys`——内部 task channel 必须**排除**，否则会被当成聊天会话列出/回收；
- `agents_md.rs:44-79` 会按 session key 拆分推导 vb session 目录——`task:<id>` 可能丢失创建者的指令作用域，必须显式保存 workspace/instruction scope。

#### D2 默认投递 = 创建者会话（但不要改自环豁免的判据）

老板已拍板：`spawn` / `job` / `deliver` / `task` 缺省**一律回创建者会话**，来源取桥注入的 `AGENT_BRIDGE_BOT_KEY` + `AGENT_BRIDGE_CHAT_ID`；显式 `--to` / `--to-current` 优先。

⚠️ **不要**按 v1 的想法把豁免改成「同一 (bot, chat) 即视为 in_session」——那会让任何同地址项自动绕过 `cross_delivery_enabled` 与 10 分钟防循环去重，而 Router 现在**专门防伪造 `in_session`**（`src/deliver.rs:252-256`）。

正确做法：保留显式、可信的来源标记，在信封里引入内部 **`DeliveryOrigin`**（`InSession` / `Scheduled` / `TaskCompletion`），由 CLI / service 在**确实**目标等于来源时设置，而不是从地址反推。

另外两条：

- 给 task 结果带上稳定 `task_id`（幂等键），避免「任务完成但投递失败后重试」重复投递；
- `DeliveryItem` 目前只有 bot + chat，**没有 thread/topic 维度**——默认回创建者时要明确落群根，还是扩展 thread（见 Q4）。

#### D3 proc 由 ABB 派生以继承 TCC（**待验证，验不过就改方向**）

`proc` 由 ABB 直接 spawn（而非 launchd/systemd），使子进程落在 ABB 的 TCC 责任面内。**Step 0 必须先做，且要按下面这些条件验**：

- 用 **release 签名后的 `ABB.app`**（不是裸二进制）；
- 覆盖三类：直接子进程 / 孙进程 / 长跑进程；
- 覆盖三种权限：摄像头 / 麦克风 / AppleEvents；
- 先重置 TCC 状态，确认**不重新弹窗、不直接拒绝**；
- 覆盖三条生命周期：ABB 正常退出 / 崩溃 / 升级重启。

#### D4 权限边界（现状比 v1 写的复杂）

- `proc` 是**任意命令执行入口**：默认 **owner-only**；granted 直接禁止，或仅在可证明的 OS sandbox 内允许（Q2）。
- 「只限自己创建的」**缺少可信身份**：`SenderRole` 只有 `Owner | Granted`（`src/config.rs:823-843`），同一群两个 granted 用户无法区分。可选：创建时下发 **capability token**（`task list/cancel/logs` 凭 token），或**直接 owner-only 管理**（更简单更严）。
- 白名单与保护名单**两份都要改**：`src/guard.rs:618` + `crates/buzz-agent/src/shell_policy.rs:126`；`tasks.json` / `tasks-state.json` / 日志目录加入 `src/guard.rs:359-390` 同类保护。
- v1 那句「`task add` 全放行」不可接受。

#### D5 进程生命周期（需要真正的 supervisor，不能复用 legacy）

- **Windows 目前没有 Job Object**（`src/buzz/acp.rs:1958-1963` 只杀直接子进程）——要在 `proc` 落地时**新做**，否则 Windows 上的 proc 任务停不干净。
- `agent-pids.json` 是 legacy 只读且只认 claude/codex/pi 命令行，**不能复用**（`src/agent.rs:446-452`、`:477-490`）。
- 需要定义：SIGTERM 宽限 → SIGKILL；进程组/Job Object 归属；退出码与退出回收；ABB 崩溃后下次启动的清理（**Unix 上父进程被 SIGKILL 不会自动带走独立进程组**）。
- 防自杀：#164 是动机，但现有保障只是 prompt 护栏（`src/agents_md.rs:107-114`）+ 恢复次数冻结（`src/bridge/recover.rs:6-10`、`:137-160`），**不是进程级边界**。supervisor 要保证 ABB 自身 PID / 自身会话不被纳入可杀集合——**不要用「pid 大小」这类伪规则**。

#### D6 可观测与日志

基础能力（pid / 退出码 / 日志 sink）**必须与 supervisor 同期落地**，不能留到后面阶段——没有日志 drain 和退出记录的 proc supervisor 不可运行。轮转/告警/更完整视图可以后续补（Q4 定上限）。

#### D7 与现有 `job` 的关系

`job` 保留为兼容别名，至少一个大版本。迁移**不能只是一次性文件转换**，必须定义：

- 兼容期 `job add/del` 写哪个文件；
- service 如何双读、谁是单写者；
- 定义文件与运行态是否分离（本文建议分离）；
- 迁移的原子性与回滚路径。

迁移测试要覆盖旧 skill 的 `add / list / del` 三件套在**迁移后仍可用**。

### 2.4 分阶段落地

| 阶段 | 内容 | 依赖 | 风险 |
|---|---|---|---|
| **P0** | 本设计定稿（事实已按评审修正） | — | 低 |
| **P1a** | 统一默认投递目标（`deliver` / `job`） | D2 | 低 |
| **P1b** | 可信 `DeliveryOrigin` + 豁免重构（含防循环回归） | D2 | **中**——碰防循环安全，不能当顺手改 |
| **P2a** | task store + CLI + 权限/身份模型 | D4 | 中 |
| **P2b** | task channel + 执行容量方案 + cancel/GC/workspace 接入 | D1a/b/c、Q1 | **高**（动共享执行层） |
| **P3** | `proc` supervisor（含 Windows Job Object、基础日志/退出记录） | D3 Step 0、D5 | 中高 |
| **P4** | 日志轮转 / 熔断告警 / 更完整可观测 | D6 | 低 |
| **P5** | `job` → `task` 迁移（别名保留） | D7 | 中（兼容面广） |

> 顺序建议：**P1a 先做**（低风险高体感）；**P3 的 TCC Step 0 并行做**（一个脚本 + 一次真机验证，决定 P3 是否继续）。P2/P3 **共用** task store / CLI / 投递 / 状态层，但**不共用执行引擎**（agent 走 ACP/pool，proc 走 supervisor）。
>
> 「立即返回、不占线、可并发」在 **P2b 完成前不能对外宣称**。

---

## 3. 风险

| 风险 | 说明 | 缓解 |
|---|---|---|
| 执行容量方案选错 | B1/B2 决定 #306 的并发上限与共享层回归面 | Q1 先拍板；P2b 前写性能/并发回归 |
| TCC 继承不成立 | D3 是 #305 的地基 | Step 0 先行，验不过就改方向 |
| 会话隔离的体验落差 | 后台任务看不到聊天上下文 | 文档写明；`--with-context` 观察需求后再做 |
| 迁移破坏 `job` 使用者 | skill 与用户脚本依赖 `job` CLI | 别名保留 + 三件套迁移测试 |
| proc 变成提权入口 | 任意命令 + ABB 权限面 | owner-only + 两份白名单 + 状态文件保护 |
| 任务结果重复投递 | 完成但投递失败后重试 | 稳定 `task_id` 幂等键 |

---

## 4. 待拍板

**基础（v1 已列）**

| # | 问题 | 建议 |
|---|---|---|
| Q1 | CLI 命名：`abb task`（+ 内部模块改名）还是 `abb proc` | **`abb task`** |
| Q2 | 后台 agent 是否支持「带聊天上下文」模式 | **先不做**，观察需求 |
| Q3 | 会话内能否停后台任务 | **能**，但须显式 `task cancel <id>`（避免误杀） |
| Q4 | 日志上限与保留份数 | 单文件 10MB / 保留 3 份（待定） |
| Q5 | ABB 重启后是否恢复常驻任务 | **恢复** |
| Q6 | 是否允许跨 bot 建 `proc` | **不允许**（bot 归属即权限面） |

**补充（评审指出，v1 缺失）**

| # | 问题 | 为什么必须先定 |
|---|---|---|
| Q7 | **执行容量**：每 bot 几个 task worker？task 与聊天谁优先？超限排队还是拒绝？ | 决定 #306「并发多个子代理」是否可能，以及 B1/B2 选型 |
| Q8 | **`proc` 权限边界**：owner-only？还是允许 granted 在 OS sandbox 内？granted 是否完全禁 `proc`？ | 直接决定提权面大小 |
| Q9 | **「自己创建的」身份粒度**：owner-only 管理，还是 capability token？需要 `sender_id/bot/chat/thread` 到什么程度？ | 现状只有 Owner/Granted，做不到区分同群用户 |
| Q10 | **topic/thread 投递**：默认回创建者时回群根还是原话题？`DeliveryItem` 是否要加 thread 维度？ | 现在根本没有 thread 字段，回错地方是静默的 |
| Q11 | **任务完成但投递前崩溃**：如何不重跑副作用又可重试投递？ | 涉及幂等键与恢复语义 |
| Q12 | **定义与运行态存储**：`tasks.json` 是否单写者？CLI 与 service 严格分离读写？ | 决定并发写与迁移原子性 |
| Q13 | **`payload.agent` 如何承接 #306 的 backend/模型供应商参数** | 现在模型只有 prompt/cwd，接不了供应商 |
| Q14 | **进程退出/信号/补跑语义**：SIGTERM 宽限、SIGKILL、进程组归属；once/cron 错过触发点是否补跑 | supervisor 的核心契约 |

---

## 5. 附：本次复盘用到的代码索引

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
| cancel 标志（失效） | `src/service.rs:1316-1321`；真实取消 `src/bridge/virtualbot.rs:1180` |
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
