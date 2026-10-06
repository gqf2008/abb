# buzz-acp 移植区规约（src/buzz/）

## 来源与许可

- 上游：github.com/block/buzz `crates/buzz-acp/`，钉死提交 **c3132c3ee982d194cd0198ad07b57ec8bd726e4e**（2026-08-31 开发基线，下称「上游」）。
- 许可：Apache-2.0。许可证全文见 `src/buzz/UPSTREAM-LICENSE.txt`（随上游 LICENSE 原样复制）。
- 基线 tag：`git tag buzz-acp-port-c3132c3` 锚定原样搬运 commit——该 commit 内 src/buzz/ 与上游逐字节一致（未接线、未裁剪），是同步与归属审计的锚点。

## 目录边界（硬规约）

- `src/buzz/` = 上游 buzz-acp 库裁剪移植区。**禁止把 ABB 特有改动反向理解成上游行为**（移植区已有 ABB 定制：InboundMsg 取代 nostr::Event、同步文本投递、提示词交付语义）。
- 移植代码经 ABB 门禁（`cargo +1.98.0 fmt/clippy/test`），随迁单测只保留与裁剪后语义一致的；断言上游 buzz CLI 发布行为、relay/频道 REST 抓取、heartbeat/gate/observer/usage 的测试一律删除（见下「文件处置」）。
- 编译单元：ABB 单 binary 的同 crate 子模块（`mod buzz;`），不新增 workspace crate。

## 文件处置表（相对 c3132c3）

> 表中每行都是超长单行，后续追加说明统一记在文末「移植区变更追加登记」（同一张表的续页）。

| 上游文件 | 处置 | 说明 |
|---|---|---|
| `queue.rs` / `pool.rs`（同步区） | ABB 扩展字段 | P0.B：`PromptChannelInfo.workspace` + `NewSessionChannelContext.workspace`——session/new 的 cwd 与 `<workspace>` 段按频道工作区（vb 群=vb/<uuid>、普通=bot 工作区）取真值，None 回落 handle cwd；上游同步时保留该字段与其透传 |
| acp.rs | 保留并裁剪 | ACP 客户端全协议面；删 usage/observer 引用；增 `turn_text` 文本捕获。P2.2/P2.3：initialize 顶层 `_meta.abbSandbox` 词表解析（`parse_abb_sandbox_modes` + `abb_sandbox_supported`/`abb_sandbox_supports`，与既有 steering_supported 同「就地解析防调用方遗漏」模式；**只认顶层**，嵌套旧位形刻意不兼容——见函数文档）；新增 `SessionSandboxMeta`（sandbox/writableRoots/shell/abbBin，camelCase）+ `session_new_full_with_meta`（旧 `session_new_full` 保留为 None-meta 包装，None ⇒ 字节级不变回归锁）；`AcpError::SandboxUnsupported` 独立变体（**不得并入 Protocol**：Protocol 属 `is_transport_error`，会把健康 agent 判死重拉）；MCP events：新增 `session_new_payload_includes_abb_events_mcp_server`，在真实 ACP `session/new` 请求 payload 中断言 `abb-events` server 的绝对 command、`mcp-events` args 与 env 形状；仅测试扩展，不改 ACP 客户端生产协议。**Windows CI 修复（本轮）**：`session_new_payload_includes_abb_events_mcp_server` 的夹具仓库路径由写死的 `/tmp/abb-events-repo` 换成 `std::env::temp_dir().join(...)`——生产侧只在 `repo.is_absolute()` 时注入 `ABB_EVENTS_REPO`，unix 风格路径在 Windows 上不是绝对路径，属夹具预期写错而非生产 bug；并补一条「非绝对路径不得注入」的反向断言。**Windows 控制台抑制收口（2026-09-28，批 abb-win-console-centralize-20260928）**：`AcpClient::spawn()` 的 `tokio::process::Command` 由「先 new 再调本文件私有 `configure_no_window`」改为构造期走 `crate::spawn::tokio_command(command)`（统一入口在 `#[cfg(windows)]` 下施加 `CREATE_NO_WINDOW`），并删除该私有 helper——这是 ABB 相对上游的定制点：上游同步时必须保留该改法，回退成上游写法会让 agent 子进程在 Windows 上闪控制台窗口。行为等价已核实（`process_group` 只在 `cfg(unix)` 下调用，与 Windows 的 `creation_flags` 无先后耦合）。**agent 降权（2026-09-28，批 abb-svc-persist-password-gate 的 B2）**：`AcpClient::spawn()` 的命令构造再由 `crate::spawn::tokio_command` 改为 `crate::agent_spawn::tokio_command` —— Windows 上本进程若处于高完整性（bridge 由计划任务以 `HighestAvailable` 拉起），该入口会把命令包一层 `abb-spawner.exe`，由它用桌面 shell（explorer）的令牌 `CreateProcessWithTokenW` 重建 agent，把权限降回普通用户；非 Windows / 未提权时逐字透传（等价）。上游同步时必须保留这层入口（回退成 `spawn::tokio_command` 会让 agent 静默继承管理员权限）。上游同步时保留。 |
| pool.rs | 保留并裁剪 | AgentPool/SessionState/run_prompt_task；删 fetch_* REST 面/reaction/用量/失败告示/guard REST 侧。P2.2/P2.3：`OwnedAgent.session_sandbox` 透传，`create_session_and_apply_model` 改喂 `session_new_full_with_meta`；建会话前档位硬闸（请求档位必须在本 agent initialize 声明的词表内，否则 `AcpError::SandboxUnsupported` 拒建——无懒启动竞态的真闸） |
| queue.rs | 保留并裁剪 | EventQueue/format_prompt；nostr::Event → InboundMsg；删 buzz CLI 发布指令。**#309 PR-B**：新增 `drain_channel_tagged(channel, tag)` —— 只丢该 tag 的排队事件（不碰其它事件与 in-flight；用途：停掉排队中的定时任务而不吞用户消息）；**覆盖三个存放点**：`queues` + `withheld_native_steer`（native steer ack 窗口）+ `cancelled_batches`（Steer 合并待重提示窗口，见 #320）；retry 账分情形处置：该频道仍有在册事件时保留，**被本次丢弃清空**时一并清 `retry_after`/`retry_counts`（否则陈旧退避把后续新消息静默拖到退避到期，最长 300s——同 `requeue()` 死信臂与 `drain_channel()` 口径）+ **八个**单测（含部分清空摘空键/配对清 `cancel_reasons`、无在册事件不误清账） |
| harness.rs | ABB 扩展字段 | 单后端化 P2.2/P2.3：`AgentConfig.session_sandbox`（本 handle 全部会话的 `_meta` 档位载荷）；`SandboxSupport` 三态（Unknown/Supported/Unsupported）+ `BuzzHandle.sandbox_supported: AtomicU8`（`handle_spawn_outcome` Ok 臂写、`schedule_agent_start` 复位 Unknown，与 `dead` 共享态同模式）；`spawn_and_init_agent` 传播 `cfg.session_sandbox` 入 OwnedAgent；`is_sandbox_unsupported` 死信分支（匹配独立变体，**不进** `is_transport_error`）。P3.1：`sync_waiters` 载荷 `String → Result<String,String>`（Err=终态失败原因）+ `notify_channel` 死信时旁路解析等待者（agent 错误对 job/oneshot 不再只表现为挂到超时）；新增 `SyncTurnOutcome`（Ok/Timeout/Closed/Failed）+ `wait_turn_outcome()`，`wait_turn_text` 改为其折叠包装（job 路径语义不变，仅死信提前醒）。P3.2：`SyncTurnOutcome::Cancelled`（外部联动取消）；`BACKEND_SUFFIX_MARK` 常量化（Ok 臂后缀追加处与 oneshot 剥除处同源）；`remove_sync_waiter()`（oneshot 外部取消臂清表防泄漏）。P4.3：`handle_prompt_result` Ok 臂的「── 后端：X」后缀追加与 `BACKEND_SUFFIX_MARK` 常量删除（单后端后路由核验维度消亡）；`AgentConfig.backend` 保留（写多读零的透传字段，移除留给后续清理批次）。#246②：`handle_spawn_outcome` Err 臂改走 `crate::log_to!` 注入 stdout，确保 spawn/initialize 失败明细与退避秒数进入 `bridge.out`；`emit_agent_start_failure(writer, ...)` 提供 writer 注入回归测试。**#309 PR-A1**：`sync_waiters` 载荷 `Result<String,String>` → `SyncWaitMsg{Done,Cancelled}`（Cancelled=取消终态，供 job/oneshot 静默收尾）；`Loop.cancel_requested` 记录「本轮收到过取消」并在 `handle_prompt_result` 的真实结局处统一结算（覆盖 Cancelled/CancelDrainTimeout/AgentExited/Error/Timeout，不再漏成挂 timeout）；纯函数 `cancel_waiter_terminal(cancel_requested, cancel_requeued, &outcome)` 定语义 **自然完成优先**（Ok 不发取消终态、把真实文本交给等待方），批次被 Steer 合并重提示时不发终态；`deliver_sync_wait_msg()` 负责交付。**删除** `wait_turn_text`（job 改用 `wait_turn_outcome` 后已无生产调用方，不留 allow(dead_code) 式假存活）。**#309 PR-B**：新增 `Cmd::DropQueuedJobs` + `BuzzHandle::drop_queued_jobs()` —— 丢弃排队中的 `prompt_tag == crate::schedule::JOB_PROMPT_TAG` 事件（生产者 run_job 与消费者共用该常量，避免字面量漂移），并给该频道同步 waiter 回 `SyncWaitMsg::Cancelled`（被丢的排队 job 没有回合结局，不主动回终态就会挂到超时）。覆盖 `queues` / `withheld_native_steer` / `cancelled_batches` 三处（#320 已修）。**#321**：订正 `SyncTurnOutcome::Closed` 的文档——原文称「job 每 chat 串行、触不到」，但调度按 `job.id` 防重入、同 chat 可并发两条 job 并共用 channel（后登记顶替在跑那条的 waiter）；job 侧对 `Closed` 改为静默收尾（`service::job_outcome_reply`）。**#321 收口（本轮）**：订正同一段「可达」注释——job 侧已改为**同 chat 单飞串行**（`run_job` 全程持 `Bridge::job_gate`），`Ok(None)` 顶替来源不再可达，`Closed` 现只剩「句柄已关闭（服务在关停）」；**仅注释改动，无行为差异**，上游同步时按 ABB 侧新文案保留。MCP events：新增 `events_mcp_server()` / `events_mcp_server_with_repo()`，`BuzzHandle::new()` 默认注入 `abb-events` stdio server；env spec 显式传 `ABB_EVENTS_REPO`（配置非空时）、`AGENT_BRIDGE_HOME`、`ABB_EVENTS_WALGIT_REMOTE=origin`，解决 fork env_clear 与 session workspace 非 git repo 时 walgit 事件源静默为空。**wassette sidecar（2026-09-21；方案 B 2026-09-23）**：新增 `wassette_mcp_server` / `wassette_mcp_server_with`（随包→PATH 解析 + `run --component-dir <dir>` 参数构造，同「就地解析防调用方遗漏」模式）；**2026-09-23 方案 B**：新增 `wassette_components_dir`（应用级组件仓库，与上游 CLI 默认目录逐平台一致：Unix=XDG_DATA_HOME/~/.local/share、Windows=APPDATA/AppData\Roaming），装配侧 component-dir 由 per-bot 目录改为该应用级目录（跨 bot 共享组件与 grant 策略由 owner 拍板接受）；`BuzzHandle::new` 增第 4 参 `extra_mcp: Vec<McpServer>`（events + extra 注入 PromptContext；normal handle 按 bot 开关注入 wassette，granted/oneshot 恒空——上游同步时保留，上游无此概念）。 |
| oneshot.rs | ABB 新增（上游无对应） | 单后端化 P3.1：`oneshot_turn()` 一次性同步回合——自建 CancellationToken + `BuzzHandle::new` + spawn `run_loop` + adhoc 频道 + `wait_turn_outcome`，Timeout 先 cancel 防迟发，`token.cancel()` + 12s 有界 join 拆栈（进程组收尸有界）。P3.2：`external_cancel: Option<CancellationToken>`（只 watch 绝不反 cancel；触发 → Cancelled + Timeout 同款 teardown）；Ok 文本剥后端标识后缀（`strip_backend_suffix`，摘要/prompt/JSON 消费方不带路由标注）。P4.3：harness 追加处已删，`strip_backend_suffix` 恒为无-op——标记字面量就地内联、剥除留作防御（升级前在途/迟发文本仍剥）。供 P3.2 session_gc / P3.3 generate_role_prompt / P3.4 teambuilder 复用；上游同步时保留。**wassette sidecar（2026-09-21）**：`BuzzHandle::new` 调用补 `Vec::new()`（oneshot 不注入 wassette——内部维护任务不需要第三方组件面） |
| prompt_framing.rs | 保留 | 上下文片段渲染（可能小裁） |
| lib.rs | 裁为壳 | 只留 dispatch_pending/handle_prompt_result/重拉退避切片；删 run()/clap/子命令/装配 |
| pool_lifecycle.rs | 保留 | 懒池状态机（零外部依赖） |
| base_prompt.md | 重写 | ABB 交付语义：「回合结束文本由桥直接投递，禁止发布命令/假装工具」；另声明安装包随带 `rg/jq/uv/gh/wassette` 并优先使用，`git/bun/sed/find` 明确不随包；wassette 段（2026-09-21）：沙箱工具宿主语义 + owner 会话注入/授权者会话不注入 |
| relay.rs | 删除 | WS/事件层——ABB 全进程内，无外部消费者 |
| config.rs | 删除 | clap/env 装配——改由 ABB config 侧注入 |
| filter.rs / engram_fetch.rs / setup_mode.rs / observer.rs / usage.rs / prompt_project.rs | 删除 | 门控/抓取/装配面被裁剪或由 ABB 侧替代 |

## 升级流程（sync 规则）

1. 上游新基线改 `docs/` 内 pin 记录 + 本表；`rsync -a` 上游 `crates/buzz-acp/src/{acp,pool,queue,prompt_framing,lib,pool_lifecycle}.rs + base_prompt.md` 到 src/buzz/（其余文件按上表不迁）。
2. **逐文件 `git diff` 人工合并**：ABB 定制点（文本捕获、InboundMsg、同步投递）与上游新逻辑冲突时以 ABB 语义为准，merge 后跑全门禁 + fake_wx e2e。
3. 禁止整体覆盖回滚 ABB 定制；上游行为变更须在 commit body 注明上游 commit。

## 验收相关

- 真机 e2e 只允许在隔离 HOME 跑（`/tmp/abb-e2e-*`），禁碰真实 `~/.agent-bridge`。
（历史上还有 abb-helper / lockctl 两条目标各带 clippy 告警；两者已随 2026-10-04 去提权一并删除，告警面归零。）

# crates/buzz-agent 分叉（自维护 fork，不再跟上游）

## 来源与差异

- 上游：github.com/block/buzz `crates/buzz-agent/`，基线 **eed74bde2**（2026-09-03 搬运时的最新触碰该 crate 的提交）。**2026-09-03 起自维护**：上游改动一律经 `git diff` 人工合入本 fork，禁止反向污染上游仓库。
- 差异清单维护在 fork `Cargo.toml` 头部注释（当前：agent.rs 文本直答回合 EndTurn 前补最终 steer drain；workspace 继承展开为直接值；scripts/ 两 JSON 随包 vendor 并改指包内 include 路径）。
- 许可：Apache-2.0，全文见 fork 内 `LICENSE`（随上游 LICENSE 原样复制）。再分发须附文本（release.yml/ABB.iss 拷为 `buzz-LICENSE.txt`）。
- **2026-09-30 上游增量合入**（区间 `eed74bde2` → `2664d1431`；该区间触及本 crate 的提交 13 个）：
  - 合入 `0cc63fe3e`（#7840）**provider 无关半边**：`llm.rs` 的 `anthropic_body` 按**原始顺序**回放原生 `anthropic_content` 块（签名 thinking 不再被压平成 text+tool_calls）、`stamp_rolling_cache_breakpoint` 跳过 `thinking`/`redacted_thinking`、`openai_body` 只吃 array 形状（防会话中途换模型时原生状态泄漏进 Chat 请求）、`parse_anthropic` 超 `MAX_TOOL_CALLS_PER_TURN`(64) **直接拒绝**而非截断签名内容、返回结构把原生块存进 `reasoning_details`；`types.rs` 文档改为「provider-owned replay state」。**未迁**同提交的 `model_capabilities.rs` 半边（Databricks UC exact record）。
  - 合入 `cae158ce7`（#7185）：`tool_timeout` 默认 660 → **1260**（+ 锁值测试，并钉住 ABB 的排序约束：工具墙 < 回合空闲墙）。
  - **未迁其余 11 个**：8 个 Databricks 专属（`#5545`/`#7606`/`#6407`/`#7358`/`#7829`/`#7844`/`#7213`/`#7135`）——ABB 只发 `BUZZ_AGENT_PROVIDER=anthropic|openai`（`src/agent.rs`），非 Databricks 的 `session/new` 在 fork `lib.rs` 原样回落 ⇒ 这些代码在 ABB 运行时不生效；`#7127`/`#7736` 只动 tests；`#7819` 只从 `PASSTHROUGH_ENV` 删 `NOSTR_PRIVATE_KEY`（ABB 不设该变量，语义空操作）。
  - **依赖告警（将来要搬必须成组、按序）**：`#7358` → `#7829` → `#7840`(capabilities 半边) → `#7606` → `#5545` → `#7844`。`#7606`/`#7844` 的 `lib.rs` 插入点正是 ABB 的 `mod devtools;`（fork `lib.rs:7`）——**整文件覆盖会静默抹掉 devtools/shell_policy 挂载与 `_meta.abbSandbox` 能力广告**。
- **ABB 本地配套（非上游，2026-09-30）**：`src/buzz/harness.rs::IDLE_TIMEOUT` 900 → **1500**（恢复「委派/工具 1260 < 空闲 1500 < 硬上限 3600」排序；空闲墙若先到，整回合被杀且模型拿不到可恢复的工具超时）；fork `devtools.rs` 的 `DELEGATE_TIMEOUT_DEFAULT`/`MAX` 1800/3600 → **1200/1200**、工具 schema `maximum` 同步 1200（原值**不可达**：外层工具墙先杀，模型只会收到 `tool: timeout after 1260s`，看不到「委派预算不够」的真因）。要放宽委派预算必须**同时**抬这两层墙。

## 构建与门禁

- 独立 manifest、独立 `Cargo.lock`：`cargo +1.98.0 build --release --manifest-path crates/buzz-agent/Cargo.toml`（产物在 `crates/buzz-agent/target/`，不污染仓库根 target）。
- 测试：`cargo +1.98.0 test --manifest-path crates/buzz-agent/Cargo.toml`（642+ 全绿含 corpus drift gate）。
- 分发：release.yml（macOS/Windows）+ ABB.iss 构建 fork 随包；运行时执行层解析（`service.rs::resolve_buzz_agent`）：`buzz_agent_exe` 覆盖（绝对路径或 PATH 名，指错告警并回落）→ 主程序同目录 `buzz-agent`/`buzz-agent.exe`（ABB.app/Contents/MacOS/）→ PATH `pi-acp` 兜底（开发/自签构建无随包时）。

## 已知 flaky（fork 测试）

CI 因此只 `--no-run` 编译不执行（ci.yml fork-lint），逻辑回归归本地全量门禁。处置纪律：**禁止重跑至绿**（幸存者偏差），逐测试归因裁决。现存两条（出处：093451a 提交信息，均自移植早期存在、与本 fork 后续改动零交集）：

1. `cancelled_turn_with_usage_emits_notification_before_response`（断言 `tests/fake_llm.rs:1376`，Null vs "cancelled"）——cancel 写 stdin 后 gate 立即放开、agent reader 未及处理，第 2 轮 fallback 错误臂在 biased select 中抢先（`agent.rs:406-410`）。修它要动同步区 cancel 优先级，**另案评估**。发生率：20 轮口径 1 轮。
2. `steer_rejected_on_empty_prompt`（断言 `tests/fake_llm.rs:1459`）——空 prompt 的 -32602 拒绝帧在争用下落后于 prompt 响应帧，先 break → `saw_reject=false`。自移植初始提交 be46634 存在、从未改动；仅全量并发下偶发（整文件 20 轮 0 出现）。

**本机（Windows）环境性红基线（2026-09-30 复核）**：本机跑 fork 全量是 `538 passed / 3 failed`，三条在**干净 `main` worktree** 上同样失败（已复核归因，与本次改动零交集，按纪律不重跑至绿）：`write_confinement_rejects_escape`（Windows 路径语义）、`discover_skills_dedup_by_name`（依赖本机 `~/.agents/skills` 布局）、`corpus_matches_generated_snapshot`（随包 `scripts/normative-corpus.json` 相对基线已漂移，需 `just regen-model-corpus`）。

**GitHub CI（macOS）稳定红基线（2026-10-05 登记）**：`test-macos` 作业稳定失败在两条 oneshot 用例上 ——
`buzz::oneshot::tests::oneshot_external_cancel_returns_cancelled`（断言 `src/buzz/oneshot.rs:353`：
「外部取消后应向在途回合发 cancel: []」）与 `buzz::oneshot::tests::oneshot_timeout_cancels_and_teardown_bounded`。
证据：CI run 37221361140（sha d5227fb）与 37221245958（sha f104237）的 `test-macos` 失败列表**只有**这两条；
同两次 run 的 `fmt` / `clippy -D warnings`（test 作业）/ `fork-lint` / `check-macos` **全部 success**。
归因状态（2026-10-05 复核，读码定论）：**产品行为正确、测试自身有时序竞态** —— `oneshot_external_cancel_returns_cancelled` 的三条断言里，前两条（outcome == Cancelled、started.elapsed() < 30s）在 macOS 上**都通过**，只有第三条『事件里必须有一条 event == cancel』读到**空列表** ⇒ 是 `read_records` 在记录任务落盘之前就读了（macOS 调度更慢/更易被抢占）。**不是 cancel 没生效**。处置建议：让该断言有界等待记录出现，或改为只断言 outcome + 有界耗时；属同步区改动，按纪律另案进行、不重跑至绿。
（原归因保留：未确认是否 pre-existing；本机 Windows 全量截至该轮 886 passed / 0 failed，现 905 passed / 0 failed。）
与本轮（2026-10-05）Windows 进程/安装/自启修复**零交集**。处置：另案评估，按纪律不重跑至绿。

已修复案例（修法口径参考）：`steer_folds_into_active_turn_without_cancelling` 于 **093451a** 修复——根因是 fixture 容量（2 条 canned）与合法时序（end_turn 后收尾 drain `agent.rs:777` 合法多跑第 3 轮 → 队列空 → 500 → wire::err 无 `result`）不匹配，修法仅补第 3 条 canned，未动任何 timeout/sleep/断言；修后 20/20 轮 0 失败。

# 移植区变更追加登记（不逐个改写上表长行）

上表每行都是超长单行，追加说明容易改坏原行；新变更登记在本节，格式与上表同义（文件 / 处置 / 说明）。

| 文件 | 处置 | 说明 |
|---|---|---|
| `harness.rs` | 上游增量（#7538 / `051c3a270`） | **模型缺失不再重试（2026-09-30，批 buzz-acp-sync-20260930）**：`handle_prompt_result` 在 sandbox 分支后、auth 分支前新增终态分支 —— `-32002` 且 message 含 `model not found` ⇒ 当场死信并提示「换用可用模型后重发」；同码的其它 resource-not-found（如 `session no longer exists`）**仍走 requeue**（误判会把本可自愈的批次判死）。告示走 ABB 的 `notify_channel`（旁路同步 waiter，保 job/oneshot 终态），**不**引入上游 `spawn_failure_notice` 的 relay 发布面。判据单测：`model_not_found_tests`。 |
| `pool.rs` / `queue.rs` / `prompt_framing.rs` | 上游增量（#7332 / `ce9decb23`） | **`<system>` → `<agent-instructions>`（2026-09-30）**：7 处字面量（pool 906/932/939、queue 1147、prompt_framing 51/67/59）+ 注释/单测同步改名，零逻辑变化。已核实 fork `crates/buzz-agent` **不解析**该 tag（`wire.rs` 把 `systemPrompt` 当不透明 `Option<String>`、`llm.rs` 原样使用、全仓 0 命中），无 wire 兼容问题。 |
| `base_prompt.md` | 上游增量（#7624 / `deda09c18`） | **「回合的信封」小节（2026-09-30）**：教 agent 从 `<buzz-event>` 的 `Content:` 读当前请求、多条在 `<buzz-events>`、合并回合看 `<new-message-arrived-while-you-were-working>`（先前请求在 `<what-you-were-working-on>`）、`<context>` 只是路由/会话元数据。按 ABB 真实 framing 中文化改写，**删掉**上游的 `<thread-context>` / `<conversation-context>` / `<new-request-supersedes-previous>`（ABB 不产出）。 |
| `acp.rs` | ABB 扩展字段（追加） | **agent 进程 job 守卫（2026-09-30，批 abb-win-mcp-orphan）**：`spawn()` 拿到子进程后经 `agent_spawn::assign_kill_on_close_job` 把它放进 `KILL_ON_JOB_CLOSE` job，守卫句柄存进 `AcpClient.job`，随客户端 Drop 关闭 ⇒ 内核连带杀掉 agent 的**整棵树**（含它为每个 session 起的 MCP 服务 `wassette` / `mcp-events`）。上游无此概念（unix 靠进程组 kill 表达同一语义），同步时保留该字段与调用。 |
| `redact.rs` | 新增（**非**同步区） | **面向用户的错误文本卫生（2026-09-29，复评 F3）**：`mask_secrets`（≥32 连续十六进制 与 api_key/apikey/secret/token/password/Bearer 的**值**掩码，零正则）+ `flatten_line` + `error_with_stderr_tail`。它只服务「失败可见化」，上游无此概念、也不改任何协议行为。 |
| `acp.rs` | ABB 扩展字段（追加） | **子进程 stderr 尾巴（2026-09-29，复评 F3）**：`spawn` 的 stderr 从 `inherit` 改 `piped`，后台任务逐行回显（等价旧 inherit，bridge 侧日志照旧可见）并收进有界环形缓冲 `stderr_tail`（12 行 / 每行 300 字符，见 `STDERR_TAIL_LINES`/`STDERR_LINE_CHARS`）；新增只读访问器 `AcpClient::stderr_tail()`；`shutdown` 给 reader 一个有界（300ms）收尾窗口，保证 kill 之后子进程的**临终输出**也进尾巴。上游同步时保留该差异。 |
| `harness.rs` | ABB 扩展字段（追加） | **启动失败文案并上 stderr 尾巴（2026-09-29，复评 F3）**：`spawn_and_init_agent` 的两条失败臂（initialize 失败 / 60s 超时）经 `redact::error_with_stderr_tail` 把子进程 stderr 尾巴（脱敏 + 折行 + 截断）并进 `SpawnOutcome::Err`，从而进入 `last_start_error` 与用户提示——owner 实报的 elevated 场景里，真正的病句（`BUZZ_AGENT_PROVIDER is required`）正是**只**出现在 stderr。 |
| `harness.rs` | ABB 扩展字段（追加） | **失败原因留存（2026-09-29，批 abb-win-failure-visibility）**：新增 `BuzzHandle.last_start_error`（`Mutex<Option<String>>`）+ `last_start_error()` / `set_last_start_error()`——`handle_spawn_outcome` 的 `SpawnOutcome::Err` 臂与 `schedule_death_respawn`（运行中崩溃）写入真实原因，启动成功臂清空；桥侧预检 `AgentDown` 的回复据此把原因带给用户（旧文案只有「未就绪（启动失败/崩溃退避中）」，用户看不出是缺 `abb-spawner.exe`、取不到桌面令牌、被 EDR 拦还是初始化失败）。上游无此概念，同步时保留。 |
| `harness.rs` | ABB 扩展字段（追加） | **死信原因可见化（2026-10-04，hotfix v2.23.86）**：抽出纯函数 `dead_letter_reason(outcome, stderr_tail)`。`handle_prompt_result` 的重试耗尽分支原先把 `AgentExited` 写成固定串「agent 进程退出」，且**一个字节日志都不落**——owner 实报：机器人只回「⚠️ 多次重试后仍未处理成功（agent 进程退出）」，而 `bridge.out` 查不到任何痕迹，只能靠进程快照倒推（当时两组 `abb-spawner→buzz-agent` 卡在启动握手：0 CPU、零 TCP 连接、零子进程）。现在 `AgentExited` 与 `Timeout(Idle/Hard)` 都经 `redact::error_with_stderr_tail` 把**已有的** `AcpClient::stderr_tail()`（脱敏 + 折行 + 截断，见本表 88 行）带给用户，并在死信分支补一行 `crate::log!`（channel + 原因）落到 `bridge.out`。**不新增采集、不改任何协议行为**；`stderr_tail`/`redact` 均为本表已登记的移植区差异。判据单测：`dead_letter_reason_tests`（4 例：AgentExited 带尾、无尾不许出现悬空「；agent stderr：」、Timeout 带尾、其它结局文案不变）。 |
| `harness.rs` / `acp.rs` | ABB 扩展字段（追加） | **spawn 进程保护（2026-10-04，v2.23.89）**：新增 `src/spawn_guard.rs`（ABB 私有，**非**同步区）——全局限速（`MIN_INTERVAL` 500ms）+ 有界指数退避（`FAIL_BASE` 1s → `FAIL_MAX` 60s）+ 熔断（连续 5 次失败 ⇒ 冷却 60s，之后半开试一次，成功复位）+ 静默衰减（120s）。**唯一入口 `async acquire()` 内部 sleep，调用方绕不过去**（owner 硬要求「不能死循环」）。接线：`AcpClient::spawn()` 在 `cmd.spawn()` 前 `acquire`，拒绝返回新变体 `AcpError::SpawnRefused` —— **刻意不算 transport error**（与 `SandboxUnsupported` 同一理据：agent 根本没起，当传输错误会 `schedule_death_respawn` + 标记 dead，后续消息全被预检拒掉）；`harness.rs` 在 `Ok(_)` 记成功复位、在 `AgentExited\|Timeout` / `CancelDrainTimeout` 记失败，并对 `SpawnRefused` **当场死信**（不进 requeue）+ 日志带守卫快照（连续失败数 / 冷却剩余）。上游无此概念，同步时保留。判据单测：`spawn_guard::tests`（8 例）。 |


| acp.rs | ABB 扩展字段（追加） | **探针可移植化（2026-10-04，v2.23.97）**：probe_real_mock_agent_roundtrip 原硬编码 macOS 解释器路径（/opt/homebrew/bin/python3）与 /tmp 记录文件，在 Windows 上必然失败。改为 crate::deps::find_in_path("python3") 退化到 python + 系统临时目录。动机：会话级隔离（v2.23.96）改了 dispatch_pending 语义，而这条（及同批 25 条）mock-agent 测试是唯一端到端验证手段，必须能在本机跑起来做回归验证。 |
| harness.rs / pool.rs | ABB 扩展字段（追加） | **弹性池 + 空闲回收（2026-10-04，v2.23.98）**：owner 追问「为什么要并发限制」，遂取消 MAX_AGENT_SLOTS 并发上限 —— wanted_slots(pending) = max(pending, 1)（有几个待跑会话就几个 agent，空闲留 1 个热实例）。资源边界改为回收闲置：主循环 select 加 60s 定时器（Evt::Reap）→ reap_idle_agents 回收**闲置超过 5 分钟**的额外槽位（0 号热实例永不回收；在途/启动中不动），回收 = take_slot + AcpClient 优雅关停（job 句柄关闭连带清 MCP 孙进程），槽位保留以维持索引不变式，按需重新拉起 ⇒ 稳态进程数 = 正在跑的会话数。Loop 新增 idle_since 记账（借走删/归还记），策略抽成纯函数 should_reap_slot 并配单测；AgentPool 新增 take_slot。与既有 spawn 守卫（≥500ms 间隔 + 熔断）配合，使「不限并发」不会变成「瞬间起一堆」。 |
| 共享（fork crates/buzz-agent） | ABB 扩展字段（追加） | **回合护栏（2026-10-04，v2.23.103）**：① `config::REPEATED_CALL_LIMIT = 5` + `agent.rs::record_call_signature`（纯函数，单测点）—— 同一回合内同一条工具调用（工具名 + 参数，以 \x1f 分隔防拼接歧义）重复达 5 次即判死循环，返回新变体 `AgentError::LoopGuard`（`catalog.rs::catalog_error_kind` 同步加 loop-guard），**绝不静默重试**；② ABB 侧给每个 ACP agent 注入 `BUZZ_AGENT_MAX_ROUNDS=200`（fork 默认 0 = 无上限），超限由既有 `StopReason::MaxTurnRequests` 收尾。动机：2026-10-04 事故——旧 claude hook `guard-check` 无界读 stdin 挂死 ⇒ 工具永不返回 ⇒ agent 无限重问模型（LLM 风暴）⇒ 定时任务每轮跑满预算超时 + 机器堆一排僵死进程。本机（Windows）fork 测试基线不变：538 passed / 3 failed（三条为已登记平台性红基线，与本次零交集）。 |
### 未迁决策登记（2026-09-30，区间 `c3132c3` → `2664d1431`，**未整体重定基线**）

区间内触及 `crates/buzz-acp/` 的提交 23 个（+12147/−1540）。上面三条按 commit 增量挑入，其余 20 个按下表**不迁**（给出等价能力与重新评估触发条件，避免下次重复分析）：

| 上游提交 | 内容 | 不迁理由 / 等价能力 | 重新评估触发条件 |
|---|---|---|---|
| `674c173eb` #6732 | 每线程独立 session（`scope.rs` + 队列/pool 全量改键） | ABB 的每个飞书话题已由 `keys.rs::topic_channel_uuid` 派生为独立频道、`SessionState.sessions` 按 channel 键控（见 `docs/session-isolation.md`） | 需要在**同一频道内**再分多个 session |
| `b17c0776b` #7337 / `86c189e85` #7340 | busy-owner hold / held deadline 唤醒 / fork generation fence | 前提（同频道多 session owner）在 ABB 不存在 | 引入 #6732 时一并评估 |
| `4d08194ea` #7620 | 按 ACP 会话交付去重 thread context | 读侧随 relay 抓取层一并裁剪（`pool.rs` 仅留写账 `delivered_event_ids`） | 恢复 relay 抓取面 |
| `ea1e97e65` #7851 | `buzz-acp run` 一次性任务（文件/stdin + 机器可读退出码） | ABB 已有等价 `oneshot.rs::oneshot_turn` | 需要外部自动化接口跑单个任务 |
| `5621006bc` #7819 | git 身份/签名 bootstrap 进 ACP harness（`git.rs`） | 全部建立在 nostr Keys/relay 上；ABB 无 nostr，提示词明确 `git` 不随包 | ABB 要给 agent 注入受管 git 身份 |
| `813bbd141` #7552 / `4beffef69` #7335 | Pi 适配器 fork 集成（`pi_launcher.rs`） | ABB 的 agent 是自维护 `crates/buzz-agent` fork，不走 Pi 适配器 | 产品决定支持 Pi |
| `93237b4a7` #6953 / `40220d561` #7154 / `c045321a7` #7325 / `e09f715c9` #7010 / `2f3dd850d` #6961 | relay 作者门控 / 订阅水位 / overflow recovery / ack 频道 | 生产语义全在 `relay.rs`（本表处置 = 删除） | — |
| `cae158ce7` #7185 | dev-mcp shell 超时 1200s + 外层预算对齐 | 无 dev-mcp；ABB idle 900s > fork 工具 660s，排序成立 | **留观**：fork 若采纳上游工具超时 1260s，必须把 `harness.rs` 的 `IDLE_TIMEOUT` 提到 ≥1500s |
| `f463e726d` #6950 / `2af9773d6` #7250 / `e17cdd9d5` #7586 / `42aeb1571` #7208 / `6c35e82bd` #7594 / `47d068e21` #7259 | base_prompt 的 buzz CLI/平台段、desktop/CLI/Pi 装配面 | ABB 的 `base_prompt.md` 是重写的中文交付语义，这些段在 ABB 侧不存在 | — |


### 2026-10-06 测试去竞态：oneshot 外部取消用例（sync 区）

- 文件：`src/buzz/oneshot.rs`
- 改动：`oneshot_external_cancel_returns_cancelled` 第三条断言（事件里必须有 `cancel`）由「立即读记录」改为
  **有界等待（≤5s，50ms 轮询）**。
- 依据：macOS CI 上该断言稳定读到**空列表**；同一用例的另两条断言（`outcome == Cancelled`、
  `elapsed < 30s`）都通过 ⇒ **产品行为正确**，是 `read_records` 与记录任务之间的时序竞态。
- 同步影响：**无行为变更**（仅测试代码），不改变与上游的合并面。


### 2026-10-06 macOS 已知差异：oneshot 两条取消用例拿不到 cancel 记录

- 用例：`oneshot_timeout_cancels_and_teardown_bounded`、`oneshot_external_cancel_returns_cancelled`。
- 现象：macOS 上即使**有界等待 20s**，记录文件里也没有 `event == cancel`；同一用例的
  `outcome == Cancelled/Timeout` 与「拆栈有界」断言**均通过** ⇒ **取消本身生效**。
- Windows 上这两条用例通过 ⇒ 是 **macOS 侧行为差异**（不是落盘竞态，20s 等待已排除）。
- 处置：严格断言加 `#[cfg(not(target_os = "macos"))]`，macOS 只保留行为断言；**不重跑至绿**。
  待查：记录任务在 macOS 取消路径上为何不落 `cancel`。


### 2026-10-06 macOS 已知差异（未修，如实记账）：oneshot 两条取消用例拿不到 cancel 记录

- 用例：`oneshot_timeout_cancels_and_teardown_bounded`、`oneshot_external_cancel_returns_cancelled`。
- 现象：macOS 上即使把读记录改成**有界等待 20s**，记录文件里仍然没有 `event == cancel`；
  而同一用例的 `outcome == Timeout/Cancelled` 与「拆栈有界(<30s)」两条断言**均通过** ⇒ **取消本身是生效的**。
- 对照：Windows 上这两条用例通过 ⇒ 属 **macOS 侧行为差异**（20s 等待已排除「落盘竞态」这一解释）。
- 处置（2026-10-06）：**不改断言、不重跑至绿**；先如实记账。`CI / test-macos` 因此保持红，
  与发布产物无关（`Build & Release` 的 macos 作业在 111/112/113 均成功）。
- 待查：记录任务在该取消路径上为何不落 `cancel`（需要在 macOS 上跑一次带诊断的用例）。
