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

| 上游文件 | 处置 | 说明 |
|---|---|---|
| `queue.rs` / `pool.rs`（同步区） | ABB 扩展字段 | P0.B：`PromptChannelInfo.workspace` + `NewSessionChannelContext.workspace`——session/new 的 cwd 与 `<workspace>` 段按频道工作区（vb 群=vb/<uuid>、普通=bot 工作区）取真值，None 回落 handle cwd；上游同步时保留该字段与其透传 |
| acp.rs | 保留并裁剪 | ACP 客户端全协议面；删 usage/observer 引用；增 `turn_text` 文本捕获。P2.2/P2.3：initialize 解析 `_meta.abbSandbox` 能力位（`abb_sandbox_supported`，与既有 steering_supported 同「就地解析防调用方遗漏」模式）；新增 `SessionSandboxMeta`（sandbox/writableRoots/shell/abbBin，camelCase）+ `session_new_full_with_meta`（旧 `session_new_full` 保留为 None-meta 包装，None ⇒ 字节级不变回归锁） |
| pool.rs | 保留并裁剪 | AgentPool/SessionState/run_prompt_task；删 fetch_* REST 面/reaction/用量/失败告示/guard REST 侧。P2.2：`OwnedAgent.session_sandbox` 透传，`create_session_and_apply_model` 改喂 `session_new_full_with_meta` |
| queue.rs | 保留并裁剪 | EventQueue/format_prompt；nostr::Event → InboundMsg；删 buzz CLI 发布指令 |
| harness.rs | ABB 扩展字段 | 单后端化 P2.2/P2.3：`AgentConfig.session_sandbox`（本 handle 全部会话的 `_meta` 档位载荷）；`SandboxSupport` 三态（Unknown/Supported/Unsupported）+ `BuzzHandle.sandbox_supported: AtomicU8`（`handle_spawn_outcome` Ok 臂写、`schedule_agent_start` 复位 Unknown，与 `dead` 共享态同模式）；`spawn_and_init_agent` 传播 `cfg.session_sandbox` 入 OwnedAgent |
| prompt_framing.rs | 保留 | 上下文片段渲染（可能小裁） |
| lib.rs | 裁为壳 | 只留 dispatch_pending/handle_prompt_result/重拉退避切片；删 run()/clap/子命令/装配 |
| pool_lifecycle.rs | 保留 | 懒池状态机（零外部依赖） |
| base_prompt.md | 重写 | ABB 交付语义：「回合结束文本由桥直接投递，禁止发布命令/假装工具」 |
| relay.rs | 删除 | WS/事件层——ABB 全进程内，无外部消费者 |
| config.rs | 删除 | clap/env 装配——改由 ABB config 侧注入 |
| filter.rs / engram_fetch.rs / setup_mode.rs / observer.rs / usage.rs / prompt_project.rs | 删除 | 门控/抓取/装配面被裁剪或由 ABB 侧替代 |

## 升级流程（sync 规则）

1. 上游新基线改 `docs/` 内 pin 记录 + 本表；`rsync -a` 上游 `crates/buzz-acp/src/{acp,pool,queue,prompt_framing,lib,pool_lifecycle}.rs + base_prompt.md` 到 src/buzz/（其余文件按上表不迁）。
2. **逐文件 `git diff` 人工合并**：ABB 定制点（文本捕获、InboundMsg、同步投递）与上游新逻辑冲突时以 ABB 语义为准，merge 后跑全门禁 + fake_wx e2e。
3. 禁止整体覆盖回滚 ABB 定制；上游行为变更须在 commit body 注明上游 commit。

## 验收相关

- 真机 e2e 只允许在隔离 HOME 跑（`/tmp/abb-e2e-*`），禁碰真实 `~/.agent-bridge`。
- 门禁红基线：`detect_permissions_shape`（macOS 权限探测恒红）环境性忽略；abb-helper ×3 / lockctl ×2 clippy 告警为基线。

# crates/buzz-agent 分叉（自维护 fork，不再跟上游）

## 来源与差异

- 上游：github.com/block/buzz `crates/buzz-agent/`，基线 **eed74bde2**（2026-09-03 搬运时的最新触碰该 crate 的提交）。**2026-09-03 起自维护**：上游改动一律经 `git diff` 人工合入本 fork，禁止反向污染上游仓库。
- 差异清单维护在 fork `Cargo.toml` 头部注释（当前：agent.rs 文本直答回合 EndTurn 前补最终 steer drain；workspace 继承展开为直接值；scripts/ 两 JSON 随包 vendor 并改指包内 include 路径）。
- 许可：Apache-2.0，全文见 fork 内 `LICENSE`（随上游 LICENSE 原样复制）。再分发须附文本（release.yml/ABB.iss 拷为 `buzz-LICENSE.txt`）。

## 构建与门禁

- 独立 manifest、独立 `Cargo.lock`：`cargo +1.98.0 build --release --manifest-path crates/buzz-agent/Cargo.toml`（产物在 `crates/buzz-agent/target/`，不污染仓库根 target）。
- 测试：`cargo +1.98.0 test --manifest-path crates/buzz-agent/Cargo.toml`（642+ 全绿含 corpus drift gate）。
- 分发：release.yml（macOS/Windows）+ ABB.iss 构建 fork 随包；运行时执行层解析（`service.rs::resolve_buzz_agent`）：`buzz_agent_exe` 覆盖（绝对路径或 PATH 名，指错告警并回落）→ 主程序同目录 `buzz-agent`/`buzz-agent.exe`（ABB.app/Contents/MacOS/）→ PATH `pi-acp` 兜底（开发/自签构建无随包时）。

## 已知 flaky（fork 测试）

CI 因此只 `--no-run` 编译不执行（ci.yml fork-lint），逻辑回归归本地全量门禁。处置纪律：**禁止重跑至绿**（幸存者偏差），逐测试归因裁决。现存两条（出处：093451a 提交信息，均自移植早期存在、与本 fork 后续改动零交集）：

1. `cancelled_turn_with_usage_emits_notification_before_response`（断言 `tests/fake_llm.rs:1376`，Null vs "cancelled"）——cancel 写 stdin 后 gate 立即放开、agent reader 未及处理，第 2 轮 fallback 错误臂在 biased select 中抢先（`agent.rs:406-410`）。修它要动同步区 cancel 优先级，**另案评估**。发生率：20 轮口径 1 轮。
2. `steer_rejected_on_empty_prompt`（断言 `tests/fake_llm.rs:1459`）——空 prompt 的 -32602 拒绝帧在争用下落后于 prompt 响应帧，先 break → `saw_reject=false`。自移植初始提交 be46634 存在、从未改动；仅全量并发下偶发（整文件 20 轮 0 出现）。

已修复案例（修法口径参考）：`steer_folds_into_active_turn_without_cancelling` 于 **093451a** 修复——根因是 fixture 容量（2 条 canned）与合法时序（end_turn 后收尾 drain `agent.rs:777` 合法多跑第 3 轮 → 队列空 → 500 → wire::err 无 `result`）不匹配，修法仅补第 3 条 canned，未动任何 timeout/sleep/断言；修后 20/20 轮 0 失败。

