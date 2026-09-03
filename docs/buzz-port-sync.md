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
| acp.rs | 保留并裁剪 | ACP 客户端全协议面；删 usage/observer 引用；增 `turn_text` 文本捕获 |
| pool.rs | 保留并裁剪 | AgentPool/SessionState/run_prompt_task；删 fetch_* REST 面/reaction/用量/失败告示/guard REST 侧 |
| queue.rs | 保留并裁剪 | EventQueue/format_prompt；nostr::Event → InboundMsg；删 buzz CLI 发布指令 |
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
