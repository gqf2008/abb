# ABB 任务模型（复盘 + 重新设计）

> 状态：**草案，待评审**（#307）。本文只描述现状、设计与迁移路径，**不代表已实现**。
> 关联：#305（常驻进程托管）· #306（后台子代理）· #158 · #69 · #21 · #137 · #164

---

## 0. TL;DR

ABB 现在有三类「任务」在跑，但**只有一套并发维度（chat_id）**，导致任何「后台」能力都被迫二选一：要么占线，要么另起一套。

重新设计的核心不是加功能，而是**把三个耦合解开**：

1. **执行位置** 从 chat 解耦 —— 后台 agent 任务用**独立 session**，不再复用聊天槽位；
2. **投递目标** 收敛成一套模型 —— 默认回创建者会话，显式 `--to` 优先；
3. **术语分层** —— 统一到 `task`，`job` 降级为兼容别名，内部 `tasks.rs` 改名让位。

---

## 1. 复盘

### 1.1 现在有哪些「任务」

| 名字 | 用户可见 | 载体 | 触发 | 生命周期 | 占会话？ | 投递 | 代码位置 |
|---|---|---|---|---|---|---|---|
| 回合内 agent | 聊天回复 | bridge dispatch | 用户消息 | 一次回合 | **是** | 回复原会话 | `src/bridge/mod.rs` |
| `job`（定时任务） | `agent-bridge job` / 聊天自然语言 | `jobs.json` | `once` / `cron` | 到期跑一回合 | **是** | `job.targets`（多目标） | `src/schedule.rs`、`src/service.rs:1285` |
| `src/tasks.rs` | **否** | 进程内 | — | service 生命周期 | 否 | — | `src/tasks.rs:1`（Tokio `TaskGovernance`） |
| `deliver` | agent / CLI | 无状态 | 手动 | 一次投递 | 否 | 显式 `--bot/--chat` 或 `--to-current` | `src/deliver.rs` |

**缺的**：立即触发的后台 agent 回合（#306）、常驻/长跑外部进程托管（#305）。

### 1.2 现有约束（代码事实）

1. **per-chat 串行锁**：同一 chat 的并发消息在这里排队（`src/bridge/mod.rs:81 chat_locks`；`:326 chat_lock()`）。
2. **chat_id 级单飞 + 共用 cancel 标志**：`run_job` 把 cancel 标志注册到**目标 chat_id**，注释明确「与聊天任务共用同一 key——同一 chat 同一时刻只有一个在跑任务」（`src/service.rs:1316-1321`；注册接口见 `src/bridge/mod.rs:194-195`）。
3. **会话单槽**：`sessions.json` 是 `{chat_id: {session_id, started, sandbox_mode?}}`（`src/sessions.rs:3`）——**一个 chat 只有一份 agent 上下文**。
4. **投递目标三套并存**：
   - `job.targets`（`src/schedule.rs:17 JobTarget`，`bot_key` 空 = 本 bot）；
   - `deliver --bot/--chat` 显式目标（`src/deliver.rs:531`）；
   - `deliver --to-current`（`src/deliver.rs:542`，要求桥注入 `AGENT_BRIDGE_BOT_KEY`/`AGENT_BRIDGE_CHAT_ID`）。
5. **自环豁免是硬编码第三态**：投递回自己会话要靠 `in_session` 或「定时任务」豁免才不被循环保护拒掉（`src/deliver.rs:253`）。
6. **受限会话的 `$ABB_BIN` 白名单**（`src/guard.rs:618 check_abb_bin`）：只放 `job add` / `session reset` / `deliver`；`job list|del` 被拒（会暴露 owner 任务的 prompt/note），`deliver --file` 必须工作区内。
7. **投递恢复语义**：`pending.json` 提供 at-least-once 重放（`src/pending.rs:1-20`），定义了三阶段窗口的各种恢复行为。
8. **进程防护与孤儿清理**：agent 子进程走 `process_group(0)` + `killpg`（`src/buzz/acp.rs:1947-1955`，Windows 侧 Job Object，见 #158）；legacy `agent-pids.json` 在下次启动清理残留（`src/agent.rs:448`）。
9. **job 的持久化与刷新**：`jobs.json` + mtime 热重载（`src/schedule.rs:53-78`），因为 CLI 在**另一个进程**写文件。
10. **外部依赖**：`~/.codex/skills/schedule` 明确要求用 `agent-bridge job add --once/--cron --prompt [--note]`、`job list`、`job del`——**`job` CLI 不能直接删**。

### 1.3 复盘结论：7 个结构性问题

| # | 问题 | 后果 |
|---|---|---|
| S1 | `job` / `task` / `spawn` 三层术语混用，且 `tasks.rs` 已被占用 | 说不清「task 指哪个」，CLI 与模块撞名 |
| S2 | **执行位置 ≡ 会话**：唯一并发维度就是 chat_id | 后台能力要么占线，要么另起一套平行体系 |
| S3 | 投递目标三套模型 + 硬编码豁免 | 新增任务类型要重新想一遍投递语义，且默认值无处安放 |
| S4 | 生命周期只有「一次回合」 | 常驻/长跑只能靠 `nohup`/launchd（还丢 TCC，见 #251） |
| S5 | 权限模型没有延伸到派生进程 | 后台任务不在 ABB 的 TCC 责任面内 |
| S6 | 可观测性弱（job 只有 `list`） | 没有 status/logs/历史/退出码，跑丢了不知道 |
| S7 | 安全边界散落硬编码（白名单、自环豁免） | 每加一个子命令都要手动同步 guard，容易漏 |

---

## 2. 重新设计

### 2.1 术语与分层

**统一到一个概念：`Task`。** 用两个正交轴描述，避免再造新名词：

| 轴 | 取值 |
|---|---|
| **payload** | `agent`（跑一轮 agent 回合）· `proc`（跑一个外部进程） |
| **trigger** | `now`（立即）· `once`（某个时刻）· `cron`（周期）· `interval`（每 N 秒）· `keepalive`（常驻，退出即拉起） |

于是现有与新增能力各自归位：

| 能力 | 表达 | 现状 |
|---|---|---|
| 定时任务 `job` | `agent` + `once`/`cron` | 已实现（迁移为 task 的特例） |
| 后台子代理 #306 | `agent` + `now` | 新增 |
| 常驻进程 #305 | `proc` + `keepalive` | 新增 |
| 周期脚本 | `proc` + `cron`/`interval` | 新增 |

**CLI**：统一为 `abb task …`；`job` 保留为**兼容别名**（`job add --once` → `task add --agent-now/--at`），至少保留一个大版本，skill 与用户脚本不破。

**内部模块改名**：`src/tasks.rs`（service 内部 async 治理）→ `src/svc_tasks.rs`，把 `tasks` 这个词让给用户可见的任务域。纯改名，无行为变化，单独一个 commit。

### 2.2 统一数据模型

```jsonc
{
  "id": "tk_20260911_a1b2c3",
  "name": "personcam",                    // 可选，人读
  "bot_key": "微信龙虾",                   // 归属：决定工作区 / 权限剖面 / 日志目录
  "created_by": {                          // 创建者：默认投递目标 + 权限继承来源
    "role": "owner",                       // config::SenderRole
    "bot_key": "微信龙虾",
    "chat_id": "wx_chat_xxx"
  },
  "payload": {
    "kind": "agent",                       // agent | proc
    "prompt": "……",                        // agent：本轮要做什么
    "cwd": "",                             // 空 = 所属 bot 的工作区
    "cmd": "",                             // proc：可执行 + 参数（数组，避免 shell 注入）
    "env": {}                              // proc：附加环境变量
  },
  "trigger": {
    "kind": "cron",                        // now | once | cron | interval | keepalive
    "expr": "0 9 * * *",                   // 依 kind 解释
    "timezone": "Asia/Shanghai"
  },
  "delivery": {
    "targets": [],                         // 空 = 用 default
    "default": "creator"                   // creator | none（显式覆盖见 §2.3 D2）
  },
  "isolation": "dedicated_session",        // agent 任务的会话隔离策略（见 D1）
  "limits": {
    "timeout_secs": 0,                     // 0 = 不限（agent 回合仍受 harness 上限）
    "max_restarts": 3,                     // proc/keepalive
    "backoff": "exponential",
    "log_max_bytes": 10485760
  },
  "state": {
    "kind": "idle",                        // idle | running | succeeded | failed | cancelled | backoff | stopped
    "pid": null,
    "started_at": null,
    "last_exit_code": null,
    "restarts": 0
  }
}
```

**存储**（与现有目录约定一致，按 bot 隔离）：

- 定义：`~/.agent-bridge/workspaces/<bot>/tasks.json`（由 `jobs.json` 迁移而来）
- 日志：`~/.agent-bridge/workspaces/<bot>/task-logs/<id>.log`（轮转上限见 D6）
- 运行态（pid/重启计数）：与定义同文件即可（写少、读多），崩溃后靠启动清理校准

### 2.3 关键设计决策

#### D1 会话隔离：后台 agent 任务必须用独立 session ⭐

**问题**：`sessions.json` 一个 chat 只有一份上下文（`src/sessions.rs:3`），且 chat_id 是单飞键（`src/service.rs:1316-1321`）。后台任务若复用聊天槽位，会同时造成三个 bug：占线、污染聊天历史、被用户一句「停」误杀。

**设计**：后台 agent 任务的 session key 取 **`task:<task_id>`**（而非 `chat_id`），与聊天槽位完全隔离：

- 不写聊天历史 → 聊天上下文不被污染；
- 不共用 cancel 标志 → 用户 `/cancel` 只停聊天回合，后台任务用 `task cancel <id>`；
- 不抢 per-chat 锁 → 真正不占线。

**代价**：后台任务看不到聊天上下文。这符合语义（“丢到后台跑”本就是独立任务），需要上下文的场景应在 prompt 里显式带上。

#### D2 统一投递默认：创建者会话

老板已拍板（2026-09-11）：`spawn` / `job` / `deliver` / `task` 的**投递目标缺省一律回创建者会话**，来源取桥注入的 `AGENT_BRIDGE_BOT_KEY` + `AGENT_BRIDGE_CHAT_ID`，显式 `--to bot:chat` / `--to-current` 优先。

**实现注意（坑）**：默认目标 = 当前会话时，必须走 `deliver` 现有的 `in_session` 通路（`src/deliver.rs:253` 的自环豁免只认「定时任务」和 `--to-current`），否则会被消息循环保护拒掉。**建议把这条豁免改成「同一 (bot, chat) 即视为 in_session」**，而不是继续靠“定时任务”这种例外列表。

无桥注入变量（CLI 手跑）→ 显式报错，不静默丢。

#### D3 进程由 ABB 派生（继承 TCC）

`proc` 类任务由 **ABB 进程直接 spawn**（而非 launchd/systemd），让子进程落在 ABB 的 TCC 责任面内，复用 ABB 已有的摄像头/麦克风授权（#251 已补齐 entitlements）。

⚠️ **此决策成立的前提必须先验证**（#305 Step 0）：用一个最小脚本实测「ABB spawn 的 ffmpeg 能抓到帧且不重新弹窗」。**验证不过则整个 #305 的方向要改**，不能先投入实现。

#### D4 权限继承创建者角色

任务按 `created_by.role` 执行：授权者（granted）建的任务走受限剖面（与现有 `job.role` 一致，见 `src/schedule.rs:46`），不得借 owner 全权限执行。

`$ABB_BIN` 白名单（`src/guard.rs:618`）需要为 `task` 重新划定：

- `task add`：允许（创建者角色由 env 追溯）
- `task list`：**需谨慎**——会暴露他人任务的 prompt/note（现有 `job list` 正因此被拒）。建议：受限会话只列**自己创建的**任务。
- `task cancel`：只允许取消**自己创建的**任务。
- `task logs`：只允许自己创建的。

#### D5 生命周期

- **默认「随 ABB」**：ABB 退出时带走全部托管任务（含子进程树），启动时按 `tasks.json` 恢复常驻项。
- **孤儿清理**：复用 `agent-pids.json` 模式（`src/agent.rs:448`）——落 pid 账本，启动时清理上次残留；再叠加 `killpg` / Job Object 兜底（`src/buzz/acp.rs:1947`）。
- **看门狗**：keepalive 退出 → 按退避重启；连续失败 N 次 → 熔断并投递告警到创建者会话。

**必须规避的历史坑**（#164）：agent 曾经能 `taskkill` 掉宿主 ABB 导致 pending 恢复死循环。托管任务必须有**防自杀**边界：不许 kill ABB 自身 / 不许杀 pid ≤ 自身会话的进程组。

#### D6 可观测与日志

- `task list`：id / name / payload / trigger / state / 下次触发时间
- `task status <id>`：pid / uptime / 重启次数 / 最近退出码 / 最近产出时间
- `task logs <id> [-n N] [-f]`：日志 tail
- 日志轮转：单文件上限 + 保留 N 份（D6 具体数值待定，见 §4 Q4）

#### D7 与现有 `job` 的关系

| | `job`（保留别名） | `task` |
|---|---|---|
| 语义 | 定时唤起 agent 回合 | 统一任务域（agent/proc × 各类触发） |
| 存储 | `jobs.json`（迁移后只读） | `tasks.json` |
| 兼容期 | 至少一个大版本 | — |

迁移：`jobs.json` → `tasks.json` 一次性转换（保留原 job id 作为 task id，保证 `job del <id>` 仍可用）。

### 2.4 分阶段落地（每阶段一个可独立验收的 PR）

| 阶段 | 内容 | 依赖 | 风险 |
|---|---|---|---|
| **P0** | 本设计文档定稿（#307） | — | 低 |
| **P1** | 统一默认投递目标（`deliver` / `job`，`spawn` 随后复用） | D2 | **低**，建议立刻做 |
| **P2** | 后台 agent 任务（`task add --agent`，独立 session） | D1、D4 | 中（会话隔离是新机制） |
| **P3** | `proc` 托管（#305） | D3（**Step 0 先验**）、D5 | 中高 |
| **P4** | 可观测 / 日志轮转 / 熔断告警 | D6 | 低 |
| **P5** | `job` → `task` 迁移（别名保留） | — | 中（兼容面广） |

> 建议顺序：**P1 立刻做**（低风险高体感）；**P3 的 TCC Step 0 与 P1 并行**（一个脚本 + 一次真机验证）；P2/P3 共用的「后台运行器底座」一次实现，不要做两套。

---

## 3. 风险

| 风险 | 说明 | 缓解 |
|---|---|---|
| TCC 继承不成立 | D3 是 #305 的地基，未验证前实现可能白做 | Step 0 最小实验先行 |
| 会话隔离带来的体验落差 | 后台任务看不到聊天上下文，用户可能预期「接着刚才说」 | 文档 + prompt 显式带上下文；`--with-context` 可选（待定） |
| 迁移破坏 `job` 使用者 | skill `schedule` 与用户脚本依赖 `job` CLI | 别名保留一个大版本 + 迁移测试 |
| 托管任务的安全面 | `proc` 是任意命令执行入口 | D4 白名单 + 防自杀边界（#164） |
| 日志吃满磁盘 | 常驻任务日志无上限 | D6 轮转上限 + 默认值 |

---

## 4. 待拍板（不埋在方案里）

| # | 问题 | 选项 | 建议 |
|---|---|---|---|
| Q1 | CLI 命名：`task` 还是 `proc`/`daemon`？ | A. `abb task`（+ 内部模块改名） B. `abb proc` | **A**（统一域，代价是改内部模块名） |
| Q2 | 后台 agent 任务是否需要「带聊天上下文」模式？ | A. 一律隔离 B. 提供 `--with-context`（只读快照，不写回） | **A 先做，B 观察需求** |
| Q3 | 后台任务能否被会话内停止词打断？ | A. 不能（须 `task cancel`） B. 能（会话内 `/task cancel <id>`） | **B**（体验更好，但要显式指定 id，避免误杀） |
| Q4 | 日志上限与保留份数 | 具体数值 | 单文件 10MB / 保留 3 份（待定） |
| Q5 | 是否支持「ABB 重启后恢复常驻任务」 | A. 支持（默认） B. 不恢复 | **A**（常驻语义的自然预期） |
| Q6 | 是否允许跨 bot 建 `proc` 任务 | A. 不允许（bot 归属即权限面） B. 允许 owner 跨 bot | **A**（权限面清晰） |

---

## 5. 附：本次复盘用到的代码索引

| 主题 | 位置 |
|---|---|
| per-chat 串行锁 | `src/bridge/mod.rs:81`、`:326` |
| cancel 标志（聊天/定时共用） | `src/bridge/mod.rs:194`、`src/service.rs:1316` |
| 定时任务定义与存储 | `src/schedule.rs:17`、`:24`、`:53` |
| 定时任务执行 | `src/service.rs:1285 run_job` |
| `job` CLI | `src/main.rs:345`、`:559` |
| 会话单槽 | `src/sessions.rs:3` |
| 投递目标 / `--to-current` / 自环豁免 | `src/deliver.rs:531`、`:542`、`:253` |
| `$ABB_BIN` 白名单 | `src/guard.rs:618` |
| service 后台任务治理 | `src/tasks.rs:1` |
| 待处理恢复 | `src/pending.rs:1` |
| 进程树终止 | `src/buzz/acp.rs:1947` |
| 孤儿 pid 账本 | `src/agent.rs:448` |
