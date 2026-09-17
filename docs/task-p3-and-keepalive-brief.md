# P3 `proc` supervisor + keepalive 触发档：决策简报

> 基线：`origin/main@0967c90`（2026-09-17）。本文只冻结决策，不改源码；下文行号以该 commit 为准，后续 rebase 时需重新核对。
> 关联：`docs/task-model.md` Q4/Q5/Q11/Q14、#326、#305，以及 walgit `abb-p2bc-orchestrator-20260916`、`abb-task-botkey-dir-1`。

## 1. 现状

### 1.1 P3（`proc` supervisor）已有什么、缺什么

**已有**

- 数据模型已给 `proc` 留位：`PayloadKind::{Agent, Proc}`（`src/task_store.rs:40-48`），载荷有 argv `cmd` 和 `env`（`src/task_store.rs:222-238`），并校验非空 argv（`src/task_store.rs:427-434`）。
- 运行态已有 `pid`、`started_at`、`finished_at`、`last_exit_code`、`restarts`，但还没有进程代际/身份字段（`src/task_store.rs:512-537`）。
- 每 bot 的 task worker 已由 service 拉起并可随 service 关停（`src/service.rs:512-523`）。
- ACP 侧有可参考的进程组行为：Unix 子进程 `process_group(0)`，退出/丢弃时 `killpg(SIGKILL)`；非 Unix 直接返回 `false`（`src/buzz/acp.rs:378-382`、`:1941-1963`）。
- 服务升级/停止已有“SIGTERM → grace → SIGKILL”的直接 pid 先例，但它是单 pid、Windows 无宽限期（`src/install.rs:181-212`）。
- 基础关闭治理已有 `TaskGovernance`（追踪、广播取消、等待收尾），但它只管理 Tokio 任务，不管理 OS 进程树（`src/tasks.rs:48-78`、`:135-163`）。
- #305 已确认 TCC 继承成立：生产链路由 `ABB.app → service → 子进程` 派生，camera 真实可用；P3 不再需要重做 TCC Step 0。

**缺什么**

- 没有 `proc` 启动、drain stdout/stderr、等待退出、回收退出码的 supervisor；当前 `run_one` 对非 `Agent` 直接记 `Failed`，日志写明“P3 进程超管落地后可用”（`src/task_run.rs:141-159`，另见模块声明 `:20-22`）。
- 没有供人类/CLI 使用的 `--proc/--cmd/--env` 路径；当前 `task add` 固定构造 `PayloadKind::Agent`（`src/main.rs:1174-1200`）。guard 已预埋拒绝 `--proc/--cmd`，防止以后新增 CLI 时 agent 绕过（`src/guard.rs:650-668`；fork 同步策略 `crates/buzz-agent/src/shell_policy.rs:126-155`）。
- **Windows Job Object 没有实现**：全仓 `grep -rn 'JobObject' src/` 无命中；`src/buzz/acp.rs:1949-1963` 只给出 Unix `killpg`，非 Unix 明确退回只杀直接子进程。`taskkill /PID /F` 只能处理单 pid（`src/agent.rs:567-574`），不能替代进程树归属。
- `agent-pids.json` 不可复用：它只清理 legacy `claude/codex/pi` pid，写端已消失，Windows 甚至直接信任 pid 文件（`src/agent.rs:479-582`）；没有启动时间/命令行/代际校验，PID 复用风险不能接受为 proc 的账本。
- 没有 pid 归属、退出回收、崩溃后孤儿发现、Windows 进程树 kill 的契约；现有 `requeue_orphans` 对所有残留 `Running` 一律走“归位 Pending 重跑”（`src/task_run.rs:747-793`），这是 agent 语义，不足以定义 proc。

**今天不能直接开工的决策点**

- **Q14（信号/退出/补跑）是 P3 的硬契约**：不先定 SIGTERM/SIGKILL、进程组/Job Object、退出码、崩溃清理和 once/cron 补跑，代码只能在临时语义上堆叠。
- **Q5（重启恢复）是 keepalive 的硬契约**：ABB 正常退出、崩溃、升级后有旧进程时，是清理后重启、只清理不重启，还是允许用户选择，必须先定。
- Q4、Q11 不一定阻塞“最小 P3a”，但会决定日志 sink 和“完成结果可靠投递”的接口边界；若先实现再改，会造成 `TaskRuntime`/状态机返工。

### 1.2 keepalive 已有什么、缺什么

**已有**

- `TriggerKind::Keepalive` 已在 schema 中（`src/task_store.rs:50-60`），但 `is_repeating()` 明确不包含它（`src/task_store.rs:71-75`）。
- 登记期已显式拒绝 keepalive，避免“列表显示支持但永远不触发”（`src/task_store.rs:415-419`）。
- 调度认领中 keepalive 恒为 `false`，并注明 Q5/Q14 未定（`src/task_run.rs:624-675`）。
- worker 已有 2 秒轮询、单 bot 串行认领和重复档的 `last_fired_at` 记账基础（`src/task_run.rs:43-45`、`:624-675`）。

**缺什么**

- 没有 keepalive 的“启动、保持 Running、退出、backoff、重启、熔断、用户停止后不再拉起”状态机。
- `TaskStateKind` 只有 `Pending/Running/Succeeded/Failed/Cancelled`，没有 `Backoff/Interrupted/Stopping` 这类可观测状态（`src/task_store.rs:499-510`）。
- `TaskLimits` 只有 `timeout_secs/max_restarts/log_max_bytes`，没有 backoff/重启窗口；`docs/task-model.md:169` 的 `backoff` 只是设计示例，代码中不存在（`src/task_store.rs:296-323`）。
- 没有持续 stdout/stderr drain；当前日志是回合结束后整段写入（`src/task_run.rs:587-621`），而 keepalive 必须边跑边落盘并在写盘时轮转。

### 1.3 协作上下文

- walgit `abb-p2bc-orchestrator-20260916` 已 closed，P2b-C/D 已合入 `2e81418`；该线程最后明确把 keepalive/Q5/Q14 留作下一轮拍板。
- walgit `abb-task-botkey-dir-1` 仍在 in-progress，当前修改 `src/main.rs` 与 `src/service.rs`（bot key alias + 幽灵目录告警）。P3 若同时改 `main.rs`/`service.rs`，应先合并或明确 rebase owner，避免 CLI 装配冲突。

## 2. 四个决策

### Q5：ABB 重启后是否恢复常驻任务？

**选项**

- **A. 全量自动恢复**：所有 `proc + keepalive` 在 startup 都清理旧代际并重新拉起。
  - 代价：需要 pid + 启动时间/代际账本；无法安全 adopt 旧进程时只能先杀后起；若旧的孙进程未清干净，会短暂出现双实例。Windows 在无 Job Object 时无法保证清树，不能声称“已恢复干净”。
- **B. 一律不恢复**：启动时把旧 `Running` 标成 `Interrupted/Failed`，只清孤儿，等人手动重启。
  - 代价：实现最简单、最安全，但 ABB 升级/崩溃后常驻能力实际中断，和 keepalive 的体验目标冲突。
- **C. 默认恢复 + 每任务 opt-out**：新增 `resume_on_boot`（例如默认 `true`，可显式 `false`），只对 `proc + keepalive` 生效；启动时先按 Q14 清理旧代际，再按配置决定是否拉起。
  - 代价：多一个 schema/CLI 字段、多一组恢复测试；仍需 Q14 的进程身份和清理能力，不能绕过。

**推荐：C。** 与文档当前“建议恢复”一致，同时照顾有副作用、只能人工确认的进程；比“全量恢复”更容易给出可审计的关闭开关。

### Q14：信号、退出、进程树与补跑语义

**选项**

- **A. Unix-only 严格实现**：Unix 用独立进程组 + SIGTERM grace → SIGKILL；Windows 明确标记 `proc` 不支持。
  - 代价：实现和验证最快，但直接违背 P3 的分平台交付目标，Windows 用户会得到功能缺口；不能把“Windows 可编译”当成支持。
- **B. 跨平台强制**：Unix 用独立进程组；Windows 新增 Job Object 归属和终止；两平台都落 pid、退出码、日志、取消和崩溃清理。
  - 代价：最高；需要直接引入/封装 Windows API、补 Windows 真机或受控 runner 测试；Job Object 的嵌套、`KILL_ON_JOB_CLOSE`/终止标志等具体语义实现前必须查官方文档并做阳性/反例验证（**待核实**，本文不把未验证 flag 当事实）。
- **C. 只杀直接子进程**：两平台都沿用 `taskkill /PID /F`/`start_kill()`。
  - 代价：实现最小，但孙进程会遗留，违反 `docs/task-model.md:289-301` 对“真正 supervisor”的要求，也满足不了取消/崩溃清理验收。**不推荐。**

**推荐：B。** 若暂时无法完成 Windows 真机闭环，则 P3a 只能作为 Unix 预演且显式禁用 Windows keepalive，不能对外宣称 P3 完成。

同时建议钉死以下契约：

1. 正常 `cancel`/service 停止：先停止新重启，发 SIGTERM/Job Object 终止请求，默认宽限 5 秒，再 SIGKILL/强制终止；宽限可注入测试。窗口树清理失败必须记 `last_error`，不能假装成功。
2. 退出：持久化退出码、退出时间、是否由用户取消；keepalive 的正常/异常退出都记录，只有用户取消/显式停用才进入终态，其他退出进入 backoff。
3. 重启：采用有界指数退避（建议起步 1s、翻倍、封顶 60s，连续快速失败到 `max_restarts` 后熔断；稳定运行一段时间后重置计数；具体阈值作为独立可测常量）。不要无界 `while true` 拉起。
4. 补跑：保留现状 `once` 的一次补跑（`src/task_run.rs:648-651`）；`cron` 错过分钟不补跑，只记录/告警；`interval` 停机跨过多个周期只补一次，避免重启风暴。keepalive 不补历史轮次，只恢复当前实例。
5. 崩溃恢复：不设计安全 adopt/重挂接；启动时先按账本确认旧代际并清理，再启动新代际。PID 复用防护所需的启动时间/进程身份 API 及 Windows 对应实现，**待核实**。

### Q4：日志上限与保留份数

**选项**

- **A. 封板当前默认**：单文件 10 MiB、含当前共 3 份、终态日志保留 30 天（`src/task_store.rs:31-32`、`src/task_run.rs:62-68`）。
  - 代价：零 schema 迁移，已有轮转/GC 测试；缺点是持续输出的 keepalive 可能快速覆盖历史，且没有 per-bot 全局磁盘上限。
- **B. 放大固定默认**：例如单文件 50 MiB、保留 5 份、仍无全局上限。
  - 代价：单任务最多约占 250 MiB；需要改阈值和现有尺寸测试，没有实际日志速率数据时属于拍脑袋放量，且会放大磁盘风险。
- **C. 每任务可配 + per-bot 总上限**：增加 `log_max_files`/`log_total_max_bytes`，CLI 可覆盖，GC 超总上限时按最老日志回收。
  - 代价：schema、CLI、轮转和并发 GC 都要改；需要明确“总上限时删哪份、删日志是否影响定义/状态”的安全边界，适合 P4 而不是 P3 首发。

**推荐：A，P3 只补流式写盘。** 先把已实现的 10 MiB × 3、30 天封板；P3 的日志 sink 必须按块追加、按块轮转，避免一次大输出绕过上限。P4 增加“轮转次数/日志总量”观测后，再用真实数据决定 B/C。任何调整按 `RULE_阈值变更` 独立提交。

### Q11：任务完成但投递前崩溃，如何不重跑副作用又可重试投递？

**现状证据**

- 任务结束态在投递前已经写入运行态（`src/task_run.rs:300-304`），结果文本还只在内存中；随后构造的是一个临时 `DeliveryItem`（`src/task_run.rs:370-384`）。
- 普通队列具备“未 ack 下次重投”的 at-least-once 语义（`src/service.rs:650-655`），`DeliveryItem.id` 同 id 可幂等入队（`src/deliver.rs:22-24`、`:145-155`）；但 task 结果当前直接调用 `Router::deliver`，没有在投递前持久化结果，也没有 `delivery_pending` 状态。
- `job_id` 非空会跳过防循环去重（`src/deliver.rs:246-247`、`:454-456`），且 P1b 的 `DeliveryOrigin` 尚未落地；不能把“消息平台会替我们去重”当成既定事实。

**选项**

- **A. 维持现状（结果可能丢）**：崩溃后不重跑、不重投，终端状态只保留在 `tasks-state.json`/日志。
  - 代价：实现最小，但“任务完成但崩溃在投递前”对用户静默丢结果，和 #306/Q11 的目标冲突。
- **B. 持久化完成结果 + 稳定 run key + 恢复重投**：为每次运行生成不可变 `run_id`；在进入终态前写入结果记录和 `delivery_pending`，投递成功后 ack；startup 扫描 pending 重投。
  - 代价：新增结果存储/清理、状态迁移和并发协议；结果文本不能直接塞入高频 `tasks-state.json`（体积/写放大）。如果仍要端到端 exactly-once，必须证明各 IM API 有幂等能力——当前**待核实**，不应作为验收前提。
- **C. 崩溃后重跑任务以再生成结果**：恢复时把非终态都归位重跑。
  - 代价：重复发消息、重复写文件、重复调用外部服务；只能做到副作用自身幂等才行。**不推荐。**

**推荐：B，且与 P3 解耦。** 以 `task_id + run_id` 作为稳定本地幂等键，先持久化“已完成待投递”，再投递、再 ack；启动恢复只重投，不重跑已终态副作用。目标是本地 at-least-once，不宣称跨 IM exactly-once。P3a/P3b 可以先保证进程退出码和日志，Q11 批次随后补结果投递可靠性。

## 3. 建议落地批次

### B1：P3a `proc` supervisor 核心（建议先做）

- **范围**：新增 `src/proc_supervisor.rs`；接入 `src/task_run.rs` 的 `Proc` 分支；扩展 `src/task_store.rs` 的运行态/limits（仅 P3 必需字段）；`src/main.rs` 增加仅人工路径可用的 `task add --proc --cmd ...`；同步 `src/guard.rs` 与 `crates/buzz-agent/src/shell_policy.rs` 的拒绝测试。
- **行为边界**：支持 `now/once/cron/interval` 的有限 `proc`，不启用 keepalive；argv 直接 spawn，不经 shell；stdout/stderr 流式写同一日志 sink；取消/service 停止走 supervisor；Windows 必须带 Job Object，若尚未完成则显式拒绝 Windows proc，不静默降级。
- **机器验收**：
  - 测试探针进程写 stdout、stderr、退出码 7；断言两路日志可读、`TaskRuntime.last_exit_code == 7`、pid 在退出后清空、状态为 Failed。
  - 测试孙进程持有文件描述符；取消后 Unix 进程组和 Windows Job Object 中进程均消失（Windows 无 Job Object 时该用例必须红，不能 skip 成绿）。
  - agent 会话调用 `task add --proc/--cmd` 在两份 policy 测试中均被拒；`main.rs` 参数解析测试能把 `--proc --cmd` 组装成 `PayloadKind::Proc`，且空 argv 在 `Task::validate` 被拒。
  - `cargo fmt --check`、`cargo clippy --all-targets -- -D warnings`、`cargo build --locked`、`tools/check_test_isolation.sh` 全绿；walgit CI 跑实际结果而非仅声明。
- **规模**：约 700–1200 LOC（含测试和平台适配），中高风险；主要风险是 Windows Job Object/unsafe 和 pid 身份。
- **依赖**：Q14 先拍板；Q4 按 A 封板；与 `abb-task-botkey-dir-1` 的 `main.rs/service.rs` 改动先收口。

### B2：keepalive 状态机 + Q5 recovery

- **范围**：`src/task_store.rs`（`Backoff/Interrupted`、`resume_on_boot`、backoff/重启窗口）、`src/task_run.rs`/`proc_supervisor`（保持 Running、退出重启、熔断、停止后不拉起）、`src/main.rs`（`--keepalive` 和状态展示）、对应迁移/单测。
- **机器验收**：
  - 子进程退出非零后进入 Backoff，按可注入时钟重拉；连续失败达上限后进入 Failed，不再重启。
  - `task cancel`/service 正常停止后进程退出且不再拉起；重复取消幂等。
  - 用测试钩子模拟 service 重启：`resume_on_boot=true` 时先清旧代际再且仅拉起一个新实例；`false` 时保持 Interrupted，不启动。
  - once 过期只补一次；cron 错过分钟不补；interval 跨多周期只补一次；日志轮转仍小于 Q4 上限。
- **规模**：约 500–900 LOC，中高风险；依赖 B1、Q5、Q14、Q4。
- **风险**：进程身份/PID 复用、快速退出风暴、Windows Job Object 生命周期；必须用真实子进程测试，不接受只测纯函数。

### B3：Q11 结果持久化与恢复重投

- **范围**：`src/task_store.rs`/结果存储、`src/task_run.rs`、`src/deliver.rs`、`src/service.rs` 的 pending 扫描与 ack；P1b `DeliveryOrigin` 可并行或先落。
- **机器验收**：
  - 在“终态已写、结果未投递”处注入崩溃；重启后任务**不重跑**，同一 `task_id+run_id` 只重投一次并最终 ack。
  - 重复恢复/并发 consumer 不产生第二次本地入队；无 pending 时不重复发送。
  - 清理策略保证结果记录与日志不会成为永久孤儿。
- **规模**：约 300–600 LOC，配合 P1b；不阻塞 B1/B2，但必须在宣称“完成结果可靠送达”前完成。
- **风险**：结果体积、ack 原子性、与 P1b 的 `job_id` 替换同步；跨平台消息 API 不假设端到端幂等。

## 4. 明确不建议现在做

- 不做跨 IM 的 exactly-once 承诺。没有已验证的平台幂等 API 时，最多做到本地持久化 at-least-once。
- 不把 `agent-pids.json`、`TaskGovernance` 或 ACP 的 `killpg` 直接包装成“proc supervisor”；前者是 legacy 单 pid，后两者分别只管 Tokio 任务和 ACP 子进程。
- 不在 Windows Job Object 完成真实验收前开启 Windows keepalive，也不以 `taskkill /PID /F` 冒充进程树终止。
- 不先做 per-bot 日志总量、告警、熔断面板等 P4 能力；Q4 先封板 10 MiB × 3 / 30 天，并用观测结果驱动后续调整。
- 不把 Q13（agent backend/模型供应商参数）混进 P3；proc 不依赖 ACP agent 配置，Q10 thread/topic 也不属于本批。
- 不做重启后 adopt/重挂接旧进程；没有可靠进程身份时，清理后启动新代际比“接管未知进程”安全。
- 不为 cron 回放所有错过周期，也不为 interval 补跑多个周期；避免 ABB 重启变成任务风暴。

## 5. 需要拍板的摘要

1. **Q5**：选 C（默认恢复 + `resume_on_boot` opt-out）。
2. **Q14**：选 B（Unix 进程组 + Windows Job Object；SIGTERM grace 5s → SIGKILL；once 补一次、cron/interval 不风暴式补跑）。
3. **Q4**：选 A（10 MiB × 3，30 天），P3 仅补流式写盘与上限断言。
4. **Q11**：选 B（持久化完成结果 + `task_id+run_id` 稳定键 + 本地恢复重投），但排在 B3，不阻塞 B1/B2。
