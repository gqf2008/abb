# P3 `proc` supervisor 与 keepalive：拍板决策简报

> **基线**：`origin/main@1850e38`（2026-09-17；复审时已按独立审查意见把引用行号校到该 commit）。本文只冻结决策，不改源码。下文所有 `文件:行号` 都以该 commit 为准；rebase 后必须重新核对行号。
> **关联**：`docs/task-model.md`（权威任务模型，§2.3 D5/D6、§2.4 阶段表、§4 决策表）、#326（批次 issue）、#305（常驻进程）、#306（后台子任务）。
> **阅读方式**：每个 Q 都是「问题 → 现状（可指到行）→ 选项（含代价）→ 推荐 → 验收标准」。验收标准全部是命令/断言级，不接受「体验更好」这类措辞。

## 0. Q 编号说明（先看这一条，避免拍错题）

本简报覆盖的 **Q4 / Q5 / Q11 / Q14** 与 `docs/task-model.md:398-399`、`:402`、`:405` 的同名条目一致。

本简报的 **Q13 指「Windows 无 Job Object 的进程树管理缺口」**；而 `docs/task-model.md:404` 当前把 Q13 记为「`payload.agent` 如何承接 #306 的 backend/模型供应商参数」。**这是两个不同的问题**，本文不覆盖后者。

> 拍板后必须回写 `docs/task-model.md`：把「Windows 进程树缺口」补一个独立 Q 号（或明确它归属 Q14），否则编号会漂移成「同一个 Q13 指两件事」。

---

## 1. Q4 日志上限与保留份数

### 问题

单任务日志的单文件上限、保留份数、终态保留期各取多少？以及 `proc`/keepalive 这类**长跑**载荷能否复用现状？

### 现状（文件:行号）

| 事实 | 位置 |
|---|---|
| 单文件上限默认 10 MiB | `src/task_store.rs:32`（`DEFAULT_LOG_MAX_BYTES = 10 * 1024 * 1024`） |
| 上限是 per-task 字段（可被定义覆盖） | `src/task_store.rs:296-303`（`TaskLimits.log_max_bytes`） |
| 保留 **3 份**（含当前 `.log`） | `src/task_run.rs:64`（`LOG_KEEP_FILES: usize = 3`） |
| 轮转链 `.log → .log.1 → .log.2`，最老丢弃 | `src/task_run.rs:678-694`（`rotate_logs`） |
| 触发条件：写前 `meta.len() >= log_max_bytes` | `src/task_run.rs:604-606` |
| 终态保留期 30 天 | `src/task_run.rs:68`（`LOG_RETENTION_DAYS: u64 = 30`） |
| 30 天后只删日志，保留定义与运行态 | `src/task_run.rs:696-736`（`gc_logs`），「只删日志」注释 `:697-699` |
| GC 每小时扫一次 | `src/task_run.rs:71`（`LOG_GC_INTERVAL_SECS`）、调用点 `:112-116` |
| **日志是「回合结束后整段写一次」** | 唯一生产调用点 `src/task_run.rs:307`，实现 `src/task_run.rs:592-621`（`write_log`） |

**关键缺口**：`write_log` 只在一次运行**结束后**被调用一次（`src/task_run.rs:307`）。keepalive/proc 是「进程活着就一直在写 stdout/stderr」，没有「结束」这个点 —— 现状的日志实现**不可能**给 keepalive 落任何日志。这不是调参问题，是 P3 必须补的能力（D6 已写「基础能力必须与 supervisor 同期落地」，`docs/task-model.md:305`）。

### 选项

| 档 | 单文件 × 份数 | 终态保留 | 单 bot 最坏占用 | 代价 |
|---|---|---|---|---|
| **A（现状封板）** | 10 MiB × 3 | 30 天 | ~30 MiB | 排障窗口 30 天；proc 长跑会在几分钟内轮转掉历史，只能看最近 3 段 |
| **B（收紧）** | 5 MiB × 3 | 14 天 | ~15 MiB | 磁盘省一半，但 proc 排障窗口更短；用户看不到昨天的失败原因 |
| **C（放宽）** | 50 MiB × 5 | 30 天 | ~250 MiB | 多 bot（当前现场 ≥2 bot）时磁盘压力线性放大，且大概率是垃圾堆积 |
| **D（分档）** | agent 10 MiB × 3；proc/keepalive 50 MiB × 5 | 30 天 | 取决于任务数 | 配置面变大；但 `log_max_bytes` 已是 per-task 字段，档位可在登记期写入 `TaskLimits`，无需新机制 |

### 推荐

**Q4 选 A（10 MiB × 3 / 30 天）作为封板默认值，但必须绑定两条 P3 硬要求：**

1. `proc`/keepalive 的日志改为**流式写盘**（spawn 后持续 drain stdout/stderr），每次写入前按字节累计判轮转，而不是只在结束写一次。这是 P3 的准入项，不是 P4。
2. `rotate_logs` 的**份数与上限抽成一处常量**（现已是 `src/task_run.rs:64`），将来若按 D 分档，只改这一处 + `TaskLimits` 默认值。

理由：现状值已经在 `docs/task-model.md:360` 被记为「Q4 拟定默认值」，且轮转/GC 已有实测回归测试锚点（`src/task_run.rs:1307`、`:1334`、`:1260`、`:1355`），先封板能解锁 P3，磁盘观测数据不足时再按 D 分档，避免在没有观测前拍一个拍脑袋的大值。

### 验收标准

- [ ] **轮转**：单任务连续写超过 `log_max_bytes` 后，断言 `task-logs/<id>.log` 大小 `< log_max_bytes` 且 `.log.1`、`.log.2` 存在（复用 `src/task_run.rs:1307` 的写放大测试思路）。
- [ ] **GC**：终态任务 `finished_at` 超 30 天后，断言 `.log` / `.log.1` / `.log.2` 三者都被清掉，而 `tasks.json` 与 `tasks-state.json` 中该任务仍在（复用 `src/task_run.rs:1355`）。
- [ ] **流式（新增，当前不可达）**：proc 任务跑满 60s 仍未退出时，`agent-bridge task logs <id> --tail 20` 能读到这 60s 内的输出。**现状必红**（日志只在 `:307` 结束写），这条是 P3 的准入断言。
- [ ] **上限生效不静默**：轮转发生时日志里能看出发生过轮转（用户不会以为"日志断了"）。

---

## 2. Q5 重启后是否恢复常驻任务（keepalive）

### 问题

ABB 因升级、看门狗重启、崩溃而重启后，登记过的 keepalive 常驻任务（如相机、长跑服务）该不该自动恢复？恢复了要不要补跑错过的周期？

### 现状（文件:行号）

| 事实 | 位置 |
|---|---|
| `TriggerKind::Keepalive` 已在 schema 中 | `src/task_store.rs:53-60` |
| 但 `is_repeating()` **不含** keepalive | `src/task_store.rs:71-75` |
| 登记期**显式拒绝** keepalive（避免"列表显示支持却永不触发"） | `src/task_store.rs:415-419`（`bail!("keepalive 触发档尚未支持…")`） |
| 认领逻辑对 keepalive **恒返回 false** | `src/task_run.rs:673`（`TriggerKind::Keepalive => false`），注释 `:632-634` |
| 启动清理：残留 `Running` **一律归位 Pending 重跑整条 prompt** | `src/task_run.rs:756-793`（`requeue_orphans`） |
| 重跑有上界 `max_restarts`，默认 **1** | `src/task_store.rs:38`（`DEFAULT_MAX_RESTARTS: u32 = 1`），用于 `:764-790` |
| 运行态无"代际/进程身份"字段 | `src/task_store.rs:514-537`（`TaskRuntime` 只有 `pid`/`started_at`/`finished_at`/`last_exit_code`/`restarts`/`last_error`/`last_fired_at`） |
| 无 `Backoff`/`Interrupted`/`Stopping` 状态 | `src/task_store.rs:502-510`（`TaskStateKind` 只有 Pending/Running/Succeeded/Failed/Cancelled） |

**注意 `requeue_orphans` 的语义与 keepalive 直接冲突**：它现在对所有残留 `Running` 一律"归位 Pending 重跑"（`src/task_run.rs:756-793`）。对 agent 载荷这是对的（重跑一轮便宜且幂等由 prompt 自负）；对 keepalive 是**错**的——keepalive 的旧进程可能还活着（ABB 被 SIGKILL 时 Unix 上独立进程组不会自动被带走，见 `docs/task-model.md:293`），盲目归位 Pending 会拉起**第二个实例**，变成双写。

### 选项

| 档 | 重启后语义 | 代价 |
|---|---|---|
| **A 不恢复** | 重启后保持停止态，需人工 `task resume` 拉起 | 常驻能力变半成品：ABB 每天升级/看门狗重启时相机静默停摆，用户以为还在跑（正是本次要消灭的"静默失效"模式） |
| **B 恢复并补跑** | 无条件拉起；once 补跑、cron/interval 按错过的周期逐个补 | 崩溃放大：ABB 自杀/被 OOM 杀 → 重启 → 全部任务并发拉起 → 再崩；cron 错过 10 分钟补 10 次 = 任务风暴与重复投递 |
| **C 恢复但不补跑** | 默认恢复；keepalive 只新起**一个**代际；once 若从未跑过补 1 次；cron/interval **不补历史周期**，从当前时刻重新计时；增加 per-task `resume_on_boot` 允许逐任务 opt-out | 需要新增进程代际字段与"上次是否正常停止"的持久化判据；仍需要 backoff/熔断才能挡住崩溃循环（B 的风险在 C 里只是被削弱，不是消失） |

### 推荐

**Q5 选 C（默认恢复 + `resume_on_boot` opt-out + 不补历史周期）。**

落地约束（缺一不可）：

1. **keepalive 恢复走独立路径**，不复用 `requeue_orphans`（`src/task_run.rs:756-793`）。理由见上：它假设"残留 Running = 进程已死"，对 keepalive 不成立。
2. 新增**进程代际身份**（记录启动时刻 + 命令行指纹），用于判定旧进程是否真死；无身份时**只清理不 adopt**（不接管未知进程）。
3. 恢复必须过 **backoff + 熔断**：连续拉起失败达上限进入 `Failed`，不再自动拉起。否则 C 与 B 的差别只剩"补跑次数"。
4. `task cancel` / service 正常关停后**不得**自动拉起（用户停止是终态）。

理由：`docs/task-model.md:399` 已经把 Q5 的建议值写成「**恢复**」，C 是该建议的可落地形态；B 在没有 backoff 前不可接受（会把 ABB 重启变成风暴）。

### 验收标准

- [ ] `resume_on_boot=false` + 模拟重启 → 该任务状态可观察到为停止/中断态，且系统进程表中**无**该任务进程（`pgrep`/`tasklist` 断言 0 命中）。
- [ ] `resume_on_boot=true` + 模拟重启 → **恰好 1 个**新实例（进程计数 == 1，不是 2）；`started_at` 更新、`restarts` 语义与 agent 载荷区分开。
- [ ] 连续 3 次模拟"启动即崩"→ 第 4 次**不再拉起**，状态为 `Failed`，`last_error` 写明熔断原因。
- [ ] keepalive 旧进程仍存活时重启 ABB → 不产生第二个实例（用探针进程 pid 文件断言）。
- [ ] `task cancel` 之后模拟重启 → 不拉起（`Cancelled` 是终态）。
- [ ] once 过期只补 **1** 次；cron 错过 10 分钟只产生 **0** 次历史补跑；interval 跨多个周期只补 **≤1** 次。三条都要有可注入时钟的单测。

---

## 3. Q11 幂等键：任务完成但投递前崩溃

### 问题

任务已经跑完、终态已落盘，但结果消息还没投递出去时进程崩了。重启后怎么做到「**不重跑副作用**，但**结果最终能送达**」？需要一个稳定到足以跨重启比对的键。

### 现状（文件:行号）

| 事实 | 位置 |
|---|---|
| 结果投递的 `DeliveryItem.id` 是**每次新生成的 uuid** | `src/task_run.rs:371`（`id: uuid::Uuid::new_v4().to_string()`） |
| 唯一跨重跑稳定的标识是 `job_id = task.id` | `src/task_run.rs:382`（`job_id: task.id.clone()`） |
| `DeliveryItem.id` 的语义是"同 id 不重复入队" | `src/deliver.rs:23-24`（字段文档） |
| `dup_key` 是**内容哈希**，只用于近窗防循环 | `src/deliver.rs:463-475`（`format!("{source_bot}|{source_chat}|{target_bot}|{target_chat}|{text}|{sha}")`） |
| 该近窗去重**只查不记**（避免挡住合法重试） | `src/deliver.rs:477-479`（注释引 #254） |
| 崩溃后恢复对 agent 载荷是**重跑整条 prompt** | `src/task_run.rs:756-793`（`requeue_orphans`），上界 `max_restarts`（`src/task_store.rs:38`） |
| 结果投递失败只写运行态 + 告警，**不重投** | `src/task_run.rs:384-395`（`outcome.is_delivered()` 为假 → 写 `last_error` + 主会话告警） |
| 风险表已记"任务结果重复投递" | `docs/task-model.md:377` |

**当前的实际行为**：`requeue_orphans` 只处理 `Running`（`src/task_run.rs:760-762` 的 `if rt.kind != TaskStateKind::Running { continue; }`）。任务跑完写终态后崩溃 → 状态不是 `Running` → **既不重跑也不重投** → 结果**静默丢失**（只有 `last_error` 留痕，用户要主动 `task status` 才看得到）。这是 Q11 要修的核心。

### 选项

| 档 | 幂等键 | 代价 |
|---|---|---|
| **A 无幂等（现状）** | 无 | 结果静默丢失；跨 bot 重投会重复发送 |
| **B 内容哈希** | 同 `dup_key` 的内容哈希 | text 含时间戳/随机数/进度输出时哈希不稳定，等于没有键；仍需额外持久化"是否已投递" |
| **C 显式业务键** | `task_id + run_seq`（run_seq 单调递增，落在 `TaskRuntime`） | 需要新增 `run_seq` 字段 + 持久化"已投递"水位 + 重启扫描重投；改动面覆盖 task_store / task_run / deliver |
| **D 时间窗去重** | 复用 `deliver.rs` 的近窗 | 窗口过后仍重复；窗口内的合法重试被误挡（#254 已踩过一次，见 `src/deliver.rs:477-479`） |

### 推荐

**Q11 选 C：`task_id + run_seq` 作为稳定幂等键，并把"已投递"做成可恢复的持久状态。**

同时必须**明确不承诺 exactly-once**：三个 IM 平台都没有已验证的端到端幂等 API，能做到的上限是「本地不重复入队 + 对平台 at-least-once 投递」。`docs/task-model.md:377` 的风险表口径保持不动。

落地要点：

1. 投递记录与任务运行态**同一处持久化**（或同事务），避免"状态写了、投递记录没写"的中间态。
2. 启动时扫描「终态 + 未投递」的任务，**只重投不重跑**（与 Q5 的 keepalive 恢复路径分开）。
3. 重投走已有的 `DeliveryStore` + `Router`，沿用 `in_session` 自环判据（`src/task_run.rs:369-383`），不新开投递通道。

### 验收标准

- [ ] 在"终态已写、结果未投递"处注入崩溃 → 重启后断言：任务**不重跑**（`started_at` 与 `restarts` 不变），结果**被重投一次**并最终 ack。
- [ ] 重复恢复 / 并发 consumer 不产生第二次入队（`DeliveryStore.pending()` 长度断言）。
- [ ] 无 pending 记录时，重启不产生任何投递（负例，防"每次重启都重发一遍"）。
- [ ] 结果记录与日志在任务 `task rm` 后一并清理，不留永久孤儿（与 `src/task_run.rs:794-810` 的孤儿清理口径一致）。
- [ ] 断言本地不重复入队后，同一 `task_id+run_seq` 最多在对端出现一次（本地可断言入队次数；对端重复属已知的 at-least-once 边界，写入文档不写进验收）。

---

## 4. Q13 Windows 无 Job Object 的代价

### 问题

`proc` 是"跑一个外部进程"（`src/task_store.rs:44-48`）。Unix 有进程组可以整组发信号；Windows 没有 POSIX 进程组，ABB 当前**完全没有 Job Object**。Windows 上到底缺什么、代价多大、用哪条替代路径、要不要在补齐前直接拒绝？

### 现状（文件:行号）

| 事实 | 位置 |
|---|---|
| Unix：子进程 `process_group(0)`，pid == pgid | `src/buzz/acp.rs:378-382` |
| Unix：整组 `killpg(SIGKILL)` | `src/buzz/acp.rs:1941-1956`（`kill_process_group`，#[cfg(unix)]） |
| **非 Unix：直接返回 `false` → 退回只杀直接子进程** | `src/buzz/acp.rs:1958-1963`（`#[cfg(not(unix))] fn kill_process_group(_pid) -> bool { false }`） |
| 全仓**无** `JobObject` / `CreateJobObject` 命中 | `grep -rn "JobObject\|CreateJobObject" src/ crates/` → 0 命中 |
| Windows 上停止只做单 pid 强杀 | `src/install.rs:204-212`（`taskkill /PID <pid> /F`） |
| Windows 上清理残留 agent 同样是单 pid | `src/agent.rs:570-574`（`taskkill /PID ... /F`） |
| Windows 分支**无宽限期**（`grace` 被丢弃） | `src/install.rs:204-205`（`let _ = grace;` + 注释「无宽限期语义」） |
| doc 已记该缺口 | `docs/task-model.md:291`（「Windows 目前没有 Job Object——要在 proc 落地时新做，否则 Windows 上的 proc 任务停不干净」） |

**四条具体缺口**：

1. **孙进程孤儿**：`proc` spawn 的进程再 spawn 子进程时，杀父进程不会带走孙子进程。Unix 靠 `process_group(0)` + `killpg` 覆盖（`src/buzz/acp.rs:378-382`、`:1941-1956`）；Windows 没有任何归属机制。
2. **超时杀不干净**：`taskkill /PID /F` 只杀一个 pid（`src/install.rs:204-212`），timeout 到期后进程树其余部分继续跑。
3. **无进程组归属**：Windows 无 POSIX pgid。`taskkill /T` 能按 pid 树杀，但依赖 pid 仍存活且树在遍历期间不变；Job Object 是内核级归属，从句柄层面保证"关掉即全杀"。
4. **无优雅退出语义**：`src/install.rs:204-205` 明确丢弃 `grace`。Windows 没有 SIGTERM，只有 `GenerateConsoleCtrlEvent` / `WM_CLOSE` / `TerminateProcess`，语义与 Unix 不等价。

### 选项

| 档 | 做法 | 代价 |
|---|---|---|
| **A 真做 Job Object** | 用 `windows` crate：`CreateJobObjectW` + `SetInformationJobObject(JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE)` + `AssignProcessToJobObject`；终止走 `TerminateJobObject` 或关闭句柄 | 新依赖 `windows` crate 的 Win32 feature；新增 `unsafe` + `#[cfg(windows)]` 分支；需要 Windows 真机/CI 验收；若 ABB 自身已被父 Job 包住，需 `CREATE_BREAKAWAY_FROM_JOB` 才能再建 Job |
| **B 手动遍历 + `taskkill /T /F`** | 用 Toolhelp32 快照枚举 pid 树，再 `taskkill /T /F /PID` | 竞态（pid 复用、树在遍历中变化）；比 Job 弱；胜在无新依赖，但仍是 `unsafe` win32 调用 |
| **C 宽限期尽力而为** | 不做树归属，只对直接子进程发 `GenerateConsoleCtrlEvent`，宽限后强杀 | 孙进程仍孤儿；等于把"停不干净"写成已知行为，不是修复 |
| **D 补齐前显式拒绝** | Windows 上 `task add --proc` 直接报错，不静默降级 | 功能缺失，但**不制造静默失效**；符合 `docs/task-model.md:291` 的「不静默降级」取向 |

### 推荐

**Q13 选 A 作为目标实现，并在 A 完成前用 D 兜底。**

- P3a 在 Unix 上直接开工（pgid 语义已存在，`src/buzz/acp.rs:1941-1956` 可参考）。
- Windows 分支：**Job Object 未通过真机验收前，`task add --proc` 在 Windows 上显式拒绝**（D），而不是退回 C 假装支持。
- B 仅作为 Job Object 不可用（如嵌套 Job 限制无法绕过）时的**降级 fallback**，且必须在驳回时明确告知用户"孙进程可能残留"。

理由：C 会把一个已知缺陷伪装成已支持；D 是诚实的最小面；A 才是 D5（`docs/task-model.md:289-303`）要求的真 supervisor 语义。

### 验收标准

- [ ] **阳性用例（Windows）**：探针进程写自己的 pid、再 spawn 一个孙进程并写孙 pid。取消任务后断言父 pid 与孙 pid **均不存在**（`tasklist /FI "PID eq <pid>"` 命中 0）。
- [ ] **宿主退出**：ABB 被强制结束（模拟看门狗 SIGKILL/ `taskkill /F`）后，因 `KILL_ON_JOB_CLOSE` 子进程树也全部消失。
- [ ] **阴性对照（必红）**：**未接 Job Object 时**，Windows 上的"取消后孙进程消失"用例必须**失败**，不得用 `#[ignore]` / skip 把它写成绿（`RULE_可达性.md` 第 3 条禁 skip 凑绿）。
- [ ] **不静默降级**：Windows 未接 Job Object 时，`agent-bridge task add --proc ...` 返回非 0 并打印明确原因，**不**创建任务。
- [ ] **Unix 回归**：`killpg` 覆盖孙进程的现有用例保持通过（`src/buzz/acp.rs:1941-1956` 附近）。
- [ ] **依赖可达**：`windows` crate 的 Win32 JobObjects feature 在 CI 中有实际编译的 job（`RULE_可达性.md` 第 1 条）。

---

## 5. Q14 停止语义（supervisor 的核心契约）

### 问题

四个必须冻结的常量与语义：① SIGTERM 宽限期多长；② 宽限到期是否 SIGKILL；③ 是否对进程组/Job Object **整体**发信号；④ 重启后错过的触发点是否补跑。

### 现状（文件:行号）

| 事实 | 位置 |
|---|---|
| SIGTERM → **3s** 宽限 → SIGKILL（仅 Unix） | `src/install.rs:178`（`terminate_with_grace(pid, Duration::from_secs(3))`）、`:182-202` |
| SIGKILL 前**复查存活**（防 pid 复用误杀） | `src/install.rs:192-198` |
| Windows 分支**无宽限**，直接 `taskkill /F` | `src/install.rs:204-212` |
| 只对**单个 pid** 发信号，非进程组 | `src/install.rs:178-212`（只有 `pid: u32`） |
| ACP 子进程只发 **SIGKILL 整组**，无 SIGTERM 前置 | `src/buzz/acp.rs:1941-1956` |
| 取消请求方向：CLI 写请求文件、service 消费 | `src/task_run.rs:536-585`（`consume_cancel_requests`；watcher 说明 `:534-535`、Running 留着不删 `:560`） |
| 取消一律**静默收尾、不投递** | `src/task_run.rs:324-330`（`if cancelled { return; }` 及其说明） |
| once：到期即跑，`t <= now` → **会补跑** | `src/task_run.rs:649-651` |
| cron：只匹配当前分钟 + 分钟桶去重 → **不补历史** | `src/task_run.rs:652-660` |
| interval：`last_fired_at + N` → **不补历史**，且对未来记账有回拨护栏 | `src/task_run.rs:661-672` |
| keepalive：恒 `false`（待 Q5/Q14） | `src/task_run.rs:673` |

### 选项

**① SIGTERM 宽限期**

| 值 | 代价 |
|---|---|
| 3s（现状） | 对 agent 够；对 proc（相机/Long-running 服务）往往来不及 flush，可能丢数据 |
| **10s（推荐）** | 多数服务够用；关停时间上限可控（每任务最多多等 10s） |
| 30s | 数据安全更足，但 service 关停会被拖长，看门狗/shutdown 超时窗口要同步放宽 |
| per-task 可配（`limits.grace_secs`，默认 10s） | 配置面 +1 字段；`TaskLimits` 已是 per-task 结构（`src/task_store.rs:296-303`），扩展成本低 |

**② 到期是否 SIGKILL**：现状是（`src/install.rs:198`）。备选"只报错不强杀"会让卡死进程永久占着任务槽位（单 worker 串行 → 整个队列停摆），**不建议**。

**③ 是否整组发信号**：Unix 现状 service 是单 pid、ACP 是整组。proc supervisor 必须**整组**，否则与 Q13 的缺口重复踩一遍。

**④ 补跑口径**

| 档 | 语义 | 代价 |
|---|---|---|
| 全部补 | once/cron/interval 都补 | 重启后任务风暴 + 重复投递，与 Q5-B 同病 |
| **现状口径（推荐保留）** | once 补 1 次；cron/interval 不补历史；keepalive 按 Q5 | 用户可能"错过就不再跑"，但语义可预期、无风暴 |

### 推荐

**Q14 取以下组合：**

1. **宽限期默认 10s**，并用 `TaskLimits.grace_secs` 允许逐任务覆盖（默认值只落一处常量）。
2. **到期 SIGKILL**（等价语义；Windows 走 `TerminateJobObject`），保留 `src/install.rs:192-198` 的"杀前复查存活"。
3. **对进程组 / Job Object 整体发信号**：Unix `SIGTERM → pgid`，宽限后 `SIGKILL → pgid`；Windows 先尽力 `GenerateConsoleCtrlEvent`，宽限后 `TerminateJobObject`。
4. **补跑保留现状口径**：once 补 1 次（`:649-651`）；cron/interval 不补历史周期（`:652-672`）；keepalive 按 Q5-C。
5. **Windows 无优雅退出等价物时，文档如实写明"尽力优雅 / 大概率强杀"**，不留"优雅退出"的假承诺（对齐 `src/install.rs:204-205` 的既有诚实口径）。

### 验收标准

- [ ] **忽略 SIGTERM 探针**：探针进程显式忽略 SIGTERM → 宽限（测试注入 300ms）后必须被杀，断言升级链路有效（复用 `src/install.rs:252-265` 的既有测试形态）。
- [ ] **进程组/Job 覆盖孙进程**：取消后 pgid/Job 内**所有** pid 消失（Unix + Windows 各一条）。
- [ ] **强杀前复查存活**：单测断言"宽限内已退出的 pid"不再收到第二次信号（防 pid 复用误杀）。
- [ ] **优雅路径**：探针进程正常退出码 7 → `TaskRuntime.last_exit_code == 7`，`pid` 清空，状态按退出码落 Succeeded/Failed。
- [ ] **补跑口径可执行**：可注入时钟下，once 过期 1 次 → 恰好 1 次执行；cron 错过 10 分钟 → 0 次历史补跑；interval 跨 5 个周期 → ≤1 次执行。
- [ ] **取消不投递**：取消的任务不产生任何 `DeliveryItem`（现有 `src/task_run.rs:324-330` 的口径保持，加断言）。

---

## 6. 建议的批次拆分

> 依赖关系：`B1 → B2`；`B3` 可与 `B1/B2` 并行；`B4` 在 `B1` 之后。每个批次一个 issue + 一个 PR，独立审查。

### B1（P3a）proc supervisor 最小可用 ⭐ 先拍 Q14 + Q13

- **范围**：新增 `proc` 执行路径（spawn / 流式 stdout+stderr drain / 等你退出 / 回收退出码），`src/task_run.rs:141-159` 的 `PayloadKind::Proc` 显式失败分支改为真实现；`src/main.rs:1201-1202（main@1850e38；该处只硬编码 PayloadKind::Agent，需按锚点而非行号读）` 的硬编码 `PayloadKind::Agent` 增加仅人工可用的 `--proc --cmd` 路径；`src/guard.rs:651-665` 与 `crates/buzz-agent/src/shell_policy.rs:146-157` 的拒绝测试保持（Q8 已拍板：agent 不得创建 proc）。
- **前置**：Q14 的 grace/组信号口径；Q13 的 Windows 策略（A 或 D）。
- **机器验收**：见 Q14 与 Q13 的验收块；外加 `cargo fmt --check`、`cargo clippy --all-targets -- -D warnings`、`cargo build --locked`、`tools/check_test_isolation.sh` 全绿。
- **规模**：约 700–1200 LOC（含测试与平台适配），中高风险。主要风险是 Windows Job Object 的 `unsafe` 与 pid 身份。

### B2（P3b）keepalive 状态机 + Q5 恢复

- **范围**：`src/task_store.rs` 增加 `Backoff`/`Interrupted` 状态、`resume_on_boot`、backoff/重启窗口字段；`src/task_run.rs:756-793` 拆出 keepalive 专属恢复路径（**不复用** `requeue_orphans`）；`src/task_store.rs:415-419` 放开 keepalive 登记；`src/task_run.rs:673` 的认领分支实现真语义。
- **前置**：B1 完成；Q5 拍板。
- **机器验收**：见 Q5 的验收块；必须用真实子进程测，不接受只测纯函数。
- **规模**：约 500–900 LOC，中高风险（进程身份 / pid 复用 / 快速退出风暴）。

### B3（P1b 之后）结果持久化与恢复重投

- **范围**：`TaskRuntime` 增加 `run_seq`；结果投递记录持久化 + 启动扫描；`src/task_run.rs:371` 的随机 uuid 改为 `task_id+run_seq` 派生键或与之关联的稳定键。
- **前置**：Q11 拍板；不阻塞 B1/B2。
- **机器验收**：见 Q11 的验收块。
- **规模**：约 300–600 LOC。

### B4（P4）日志总量 / 告警 / 可观测

- **范围**：Q4 若选 D 才需要分档；per-bot 日志总量统计、告警、面板。
- **前置**：B1（流式日志）落地并跑出真实观测数据。
- **机器验收**：观测脚本能报出每个 bot 的 `task-logs/` 实际占用；超阈值告警有阳性对照。

---

## 7. 必须先用户拍板的问题清单

以下 5 条**不定就不能开工**（B1/B2 会直接依赖它们；B3 依赖第 5 条）：

1. **Q14**：宽限期取 10s（可 `limits.grace_secs` 覆盖）？到期 SIGKILL？对进程组/Job 整体发信号？补跑保留「once 补 1 次、cron/interval 不补历史」？
2. **Q5**：选 C（默认恢复 + `resume_on_boot` opt-out + 不补历史周期）？还是 A（不恢复）/ B（恢复并补跑）？
3. **Q13**：Windows 走 A（Job Object）并在完成前用 D（显式拒绝 proc）兜底？还是接受 C（尽力而为并写明可能残留孤儿）？
4. **Q4**：冻结 10 MiB × 3 / 30 天？还是直接上 D（分档）？（**无论选哪个，proc 流式写盘都是 P3 准入项**，不是可选项。）
5. **Q11**：选 C（`task_id + run_seq` 稳定键 + 持久化重投），并确认对外只承诺 at-least-once？

> 另需顺手拍一条**文档口径**：`docs/task-model.md:404` 的 Q13（`payload.agent` 承接 backend/供应商）与本简报的 Q13（Windows 进程树）撞号，拍板后要回写编号，避免后续引用歧义。
