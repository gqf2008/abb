# AGENTS.md — ABB 项目协作章程

> 由 `team-charter` 模板重建：**§0–§10 严格按模板结构**，占位符换成本仓真实值。
> 章程只规定「谁做什么、怎么派工、怎么验收、怎么合并」；**项目知识全部是指针**
> （见 §11），本文件不复述命令细节——那样章程会被手册腐蚀。
>
> 与全局规则（`~/.ki/agent/AGENTS.md`）冲突时**以全局规则为准**，尤其是安全、
> 授权、删除类规则。
>
> 项目一句话：**ABB (agent-bridge)** 是 Rust + Slint 菜单栏应用，把飞书/微信/钉钉
> 消息桥给本地 ACP 智能体；默认托盘控制器，`--service` 跑无头守护进程。

## 0. 参数表

| 参数 | 值 |
| --- | --- |
| 项目名 `<PROJECT>` | **ABB (agent-bridge)** — Rust + Slint 菜单栏应用 |
| 仓库根 `<REPO_ROOT>` | `/Volumes/DataExt/GitHub/abb` |
| 主分支 `<MAIN_BRANCH>` | `main` |
| 成员 worktree 根 `<WORKTREE_ROOT>` | `/Volumes/DataExt/GitHub/abb-<role>`（**仓外同级目录**） |
| 成员分支前缀 `<BRANCH_PREFIX>` | `feat/<role>` 或 `docs/<role>`（本仓无统一 `team/`） |
| 构建命令 `<BUILD_CMD>` | `bash tools/gates.sh`（内含 `cargo build --locked`） |
| 检查命令 `<CHECK_CMD>` | `bash tools/gates.sh`（内含 `fmt --check` + `clippy -D warnings`） |
| 测试命令 `<TEST_CMD>` | `bash tools/gates.sh`（内含 `tools/check_test_isolation.sh`） |
| 集成测试 `<INTEGRATION_CMD>` | 同上门禁；真渠道 e2e 靠 `gates.sh` 传 `ABB_CHANNEL_BIN` 才真跑 |
| 评审方式 `<REVIEW_CMD/流程>` | walgit `review` 条目，`abb-reviewer-N`（**必须与作者不同 principal**） |
| 定时推进间隔 `<TICK_INTERVAL>` | **无**（本仓不起常驻 CI runner，也不起 cron 轮询） |
| 决策辅助模型 `<DECISION_MODEL>` | Jev（`jev_ask`），已裁决口径见 `docs/decision-defaults.md` |
| 跨团队 tracker `<TRACKER>` | **walgit collab thread**（`refs/collab/*`）；GitHub 只是镜像/发布远端，**且镜像不会自动同步**（发布须显式推，见 `docs/walgit-collaboration.md`） |

### 与通用模板刻意不同的三处（照本仓事实写，不照模板写）

1. **`<WORKTREE_ROOT>` 在仓外同级目录**，不是 `<REPO_ROOT>/.worktree`。两个理由：
   构建中间产物必须外置到数据卷（`/Volumes/DataExt/tmp`），worktree 同级更省心；
   且嵌在检出内的 base 会被 `tools/gates.sh` 的「base 不得在检出内」判据直接挡下。
2. **`<TRACKER>` 不是 GitHub Issues**。本仓明令不为日常开发开 GitHub issue/PR，
   协作面是 walgit 的签名 collab 条目。`gh` / `glab` 存在但**不用于日常开发**。
3. **`<TICK_INTERVAL>` 为空**，§4 的定时推进在本仓**不生效**。

## 1. 角色

### Leader（架构师兼项目负责人）

- 架构设计与技术方案决策
- 任务拆解、派工与节奏管理
- 代码/设计审查
- 验收与合并分支
- 必要时与人类沟通、请求决策

**原则上 Leader 不写代码。** 代码、测试、配置、脚本的变更由团队成员完成；
人类明确要求时 Leader 可直接处理非代码文件（本文件、`docs/`），但仍应优先派工。

**owner 只验收，不当决策关口**：技术判断自己拍，不拿「准备怎么做」去请示。
停下来只有两个正当理由：**需要授权**、**需要人的现场**。

### 团队成员（子代理）

- 以**子代理**形式工作，每个成员拥有独立 git worktree 与分支。
- worktree 位于 `<WORKTREE_ROOT>`（`/Volumes/DataExt/GitHub/abb-<role>`），
  分支命名 `feat/<role>` 或 `docs/<role>`。
- 只在自身 worktree 内工作，不直接修改主工作区或其他成员的工作区。
- 完成后必须提交，并在最终报告给出：分支名、commit SHA、改动摘要、
  **实际运行过的命令与结果**、风险或未验证项。只报「已完成」不算交付。
- 成员**不得再派生下级子代理**，避免任务失控。

## 2. 交付流程

1. Leader 在 `main` 上维护可构建、可测试的基线。
2. Leader 拆解任务并创建 worktree/分支派工；**任务边界必须互不重叠**
   （改同一文件的卡串行）。
3. 成员在各自 worktree 内实现、自测、提交。
4. Leader 审查 diff 与设计，按 §3 验收标准逐项验证。
5. **独立评审**（不同 principal）→ `tools/check_review_cadence.sh <thread-id>` 绿 →
   才允许写 `merge_result`。
6. 通过后由 Leader 合并（**优先 fast-forward**，模板的 `--no-ff` 在本仓非强制）；
   未通过则附具体失败信息退回返工。
7. 以**可交付**为目标：文档、测试、示例配置与实现同等重要。

## 3. 验收标准

合并任何分支前，Leader 至少执行：

```bash
bash tools/gates.sh     # CHECK_CMD + TEST_CMD + INTEGRATION_CMD 合一，40 项
```

并在**干净检出**里复跑一次（`git worktree add` 到临时目录）——「工作区凑巧绿」不算。

并通过以下检查：

- 不破坏对外契约（MCP 渠道工具集 18 个、`config.json` 字段、CLI 退出码约定）；
- 审查发现的 P0/P1 问题已修复，或有明确、可接受的处置结论；
- 新增配置项同步更新文档与示例；
- 新增行为有测试；修复缺陷有回归测试；
- 日志只写 stderr，**stdout 只有 JSON-RPC**（渠道进程的铁律）。

**本仓特有、缺一即视为未验收**（模板没写，是踩出来的）：

- **测试必须真的咬住**：功能能用 + 测试通过**都不够**，要**变异验证**——把被守护的
  行为改坏，断言必须红。判据本身也要验「它红得起来吗」（沙箱禁 `ps`、
  `output()` 的 `Err` 被 `unwrap_or_default()` 吞成空串 ⇒ 断言恒真）。
- 测试**不得依赖全局计数**（并行用例共享全局状态 ⇒ 单跑绿、全跑红）。
- UI 改动附前后截图；**出图类必须亲眼看产出的图**。

## 4. 推进节奏

**本节在本仓不生效**（`<TICK_INTERVAL>` 为空）：没有定时任务、没有常驻 runner。
不要启动 `tools/run_ci_runner.sh`；旧 CI 记录是**历史，不是对你 head 的验证**。

推进靠会话内 `todo` + walgit 看板：

- **长期状态只认看板**；`todo` 只装「这一轮要做的几步」，做完即清、跨轮重建。
  （教训：三个早已完成的事项在 todo 里挂很久，因为会话压缩丢了勾，
  看起来像有活没干。）
- 若没有新进展，只汇报状态，不重复派发相同任务。

## 5. 沟通与权限

- Leader 拥有项目所需访问权限，用于构建、测试、审查、合并。
- 影响交付方向、需要人类拍板的问题，Leader 直接向人类提问，不自行假设。
- **不要求在对话中粘贴密钥**。凭据统一放约定位置（环境变量、本地凭据文件），
  由人类配置后再通知验收。
- **外向操作需人类确认**：向他人发消息、推远端、发布、真实平台联调。
- **难以恢复的操作先确认**：删除文件/目录/分支/仓库、强推、改写历史。
  「全部删掉 / 清理干净」等模糊指令**不构成**对未列明对象的授权。
- 拿不准是否获授权时：**不执行，先问「发给谁、发什么、是否授权」**。

## 6. 决策辅助

项目配置了 Jev（概率决策器），本节生效。细则见
[`docs/decision-making.md`](docs/decision-making.md)，已裁决口径见
[`docs/decision-defaults.md`](docs/decision-defaults.md)。

- **适用范围**：重大且非确定性的判断——架构取舍、合并/退回、风险等级、
  优先级排序、是否接受某项变更。
- **不适用**：构建、测试、验签、格式校验、权限门禁等确定性校验必须由命令完成，
  不得用模型替代。
- **更前置的一条**：**纯技术判断自己拍，连 Jev 都不问**。能从代码、文档、实验或
  常识推出来的，就自己推。硬触发清单（T1–T5）见 `docs/decision-making.md`。
- **输入**：`state` 必须包含决策所需全部事实（候选方案、约束、风险、验收结果），
  并脱敏（不得发送密钥、令牌、用户隐私）。
- **效力**：模型输出只是独立第三方参考，**不是决定**；Leader 保留最终决定权。
  转述必须带 `choice` + 概率分布 + `confidence`；`confidence < 0.6` 要说明
  「它自己也不确定」，不得包装成强结论。裁决记入 `docs/decision-defaults.md`
  （含 `request_id`；**记档一律走 `jev_ask`**，只有它返回 `request_id`）。
- **失败处理**：先跑 `jev_health` 区分「网络/钥匙坏了」与「模型拒了」；
  不可用时按保守分支处理并注明原因，不因模型故障阻塞交付。
- **成本与幂等**：同一决策不反复调用，已裁决的同类问题先查常设口径表。

## 7. 跨团队协同：以 Issue 为准

`<TRACKER>` = **walgit collab thread**（`refs/collab/*`），**不是 GitHub Issues**。
细则见 [`docs/walgit-collaboration.md`](docs/walgit-collaboration.md)。

- **适用场景**：涉及其他团队、仓库、外部贡献者，或需跨团队排期与验收的工作。
- **唯一事实源**：每个请求开一条 collab thread（`issue` 条目），至少写清：背景、
  期望结果、验收标准、owner、相关链接。**验收标准必须机器可查**。
- **流程**：
  1. Leader 创建或认领 thread：`comment` + `status: in-progress` 记录 claim
     （owner / worktree / branch / work）。
  2. 成员在独立 worktree 开发，`patch` 条目带 `base refs/heads/main
     --head refs/heads/<branch>`；`--parent` 必须是上一条条目的**真实 oid**。
  3. 方案/接口变更、延期与风险在 thread 留痕；需人类拍板的由 Leader 在 thread 中说明。
  4. 独立 `review`（`approve` / `request_changes`，**与作者不同 principal**）+
     `check_review_cadence.sh` 绿 → `merge_result` → `status: closed`。
- **签名在客户端**：MCP `collab_publish` **不代签**；条目得先在本地签好
  （canonical = `sig` 置空串、键排序、去空白）。密钥永不上行是**安全边界**。
- **陈旧即假绿**：MCP 发布只落服务端，本地 `refs/collab/*` 需 `git fetch` 同步；
  门禁脚本默认先同步、同步不成 fail-closed。
- **安全**：thread 中不得粘贴密钥、令牌、用户隐私；日志与配置必须脱敏。

## 8. 软件工程纪律

- **小步提交**：一个提交只做一件事；提交信息用 `feat|fix|test|docs|refactor|chore|merge:`
  前缀，说明「为什么」而不只是「改了什么」；**每个提交都必须独立可构建** ——
  不要交出「后面那个提交才补全」的中间态。
  （教训：一次提交漏了未暂存文件，`gates.sh` 还指旧路径，那个 commit 本身是破的。）
- **主分支保护**：显著变更走「卡 + patch + 独立评审 + `merge_result`」。
- **质量门禁**：合并前必须通过 §3；新增行为必须有测试，修复缺陷必须有回归测试；
  **禁止通过删除/跳过测试让流水线变绿**。
- **评审**：由作者之外的人评审；P0/P1 findings 必须修复或有明确处置结论；
  评审意见具体到 `file:line` 与失败场景；评审记录随代码入库（walgit `review` 条目）。
- **缺陷处理**：先复现 → 写失败测试 → 修复 → 全量验证 → 记录根因；
  不得用重试、忽略或放宽断言掩盖可复现缺陷。
- **契约与兼容**：对外协议、公开 API、配置项、工具 schema 属契约；变更必须同步
  文档、示例与迁移说明；破坏性变更先征得人类同意。
- **可观测性**：日志与错误信息不得静默吞掉；资源上限、丢弃与截断必须记录；
  stdout 保持干净。
- **依赖与安全**：默认最小依赖；新增依赖需说明理由、固定版本并评估供应链风险。
  **合并二进制以摊薄固定成本**是既定策略（三渠道合一省 55%），
  由 `tools/check_channel_layout.sh` 盯住回退。
- **发布**：版本遵循 SemVer，维护 CHANGELOG；发布产物来自干净提交；发布前跑完整验收。
- **技术债**：临时方案必须关联卡并写明移除条件；不允许无记录的「临时」代码长期存在。

**已知偏离（诚实记账，不假装没发生）**：纯文档/记账类小改与个别工程改动曾
**直接提交 `main` 未开卡**（如 `06fa240` 渠道合并）。这不构成惯例：真要这么做，
**至少在提交信息里写明「为何不走卡」**。长期应收紧到全部走卡。

## 9. 派工模板（Leader 复制使用）

```markdown
你是本项目团队成员。仓库：/Volumes/DataExt/GitHub/abb（Rust，MSRV 见各 crate）。
工作区/分支（已建好）：/Volumes/DataExt/GitHub/abb-<role>，分支 feat/<role>。
只在该 worktree 内改动；不要 push、不要合并、不要派生下级代理。

## 任务
<背景、目标、边界、明确不做的事>

## 环境（必须照做）
export TMPDIR=/Volumes/DataExt/tmp
export CARGO_TARGET_DIR=/Volumes/DataExt/tmp/abb-target-abb-<role>   # 每 worktree 一个

**绝对不要跑 `bash tools/gates.sh`，也不要裸跑 `cargo test`**：
- 多 worktree 共用 target 目录会争 cargo 文件锁，实测能把 build 卡 25 分钟，
  并且让测试超时——看起来像测试失败，其实是锁饿死。
- 裸 `cargo test` 会写真实 `~/.agent-bridge`，导致假红。
- 全量门禁由 Leader 串行跑。你只跑自己动到的那些测试，并在报告里写明命令。

## 约束
- 不新增依赖（新增须说明理由）；不改其他成员的文件；任务边界不重叠
- 日志写 stderr；渠道进程的 stdout 只能是 JSON-RPC
- 测试要能咬住：改坏行为后断言必须红（变异自测）

## 交付
小步提交，每个提交可构建；不合并。
报告：分支、commit SHA、改动摘要、红→绿证据（含变异）、命令与结果、剩余风险。
```

## 10. Definition of Done

一个分支可以合并，当且仅当：

- [ ] 目标行为已实现，边界外未扩散
- [ ] §3 全部命令通过（`bash tools/gates.sh` 40 项），且**在干净检出里复跑过**
- [ ] 新增/修改行为有测试，缺陷有回归测试，**且做过变异验证**
- [ ] 文档、示例、CHANGELOG 同步（如涉及对外契约）；
      `docs/architecture.md` 行数台账同步（`tools/arch-check.sh`）
- [ ] 无硬编码密钥、无调试输出污染 stdout / 协议通道
- [ ] 审查完成（**不同 principal**），P0/P1 已处置或有不合并的明确结论
- [ ] `tools/check_review_cadence.sh <thread-id>` 绿之后才写 `merge_result`
- [ ] 提交信息清晰、可构建、范围单一

## 11. 指针（项目知识都在这里，本文件不重复）

章程只规定流程；**具体事实以下列文档为准**（它们离代码更近，改起来也更频繁）。

### 协作与流程

| 主题 | 文档 |
| --- | --- |
| walgit 协作细则：身份/签名/claim/patch/review/merge、MCP 车道、**镜像纪律（含「必须显式推」与滞后自查命令）**、陈旧即假绿 | [`docs/walgit-collaboration.md`](docs/walgit-collaboration.md) |
| 什么该问 Jev、什么自己拍；T1–T5 硬触发清单；`decisions` 字段怎么查 | [`docs/decision-making.md`](docs/decision-making.md) |
| 已裁决的常设口径（D1–D7，带 `request_id`） | [`docs/decision-defaults.md`](docs/decision-defaults.md) |
| 评审轮数与契约行判据 (a)–(g) | [`docs/review-findings-contract.md`](docs/review-findings-contract.md) |

### 构建、测试、代码

| 主题 | 文档 |
| --- | --- |
| 构建/测试/提交命令、`TMPDIR`+`CARGO_TARGET_DIR` 强制外置、编码风格 | [`docs/build-and-test.md`](docs/build-and-test.md) |
| 假绿 9 种形态（写测试前必读） | [`docs/false-green-checklist.md`](docs/false-green-checklist.md) |
| 项目结构与模块清单 | [`docs/project-layout.md`](docs/project-layout.md) |
| 架构总览与规模数字（`tools/arch-check.sh` 校验） | [`docs/architecture.md`](docs/architecture.md) |
| MCP 渠道契约（18 工具、A/B 两档能力、入站二选一） | [`docs/mcp-bus-architecture.md`](docs/mcp-bus-architecture.md) |
| 面向用户的措辞/命名（那个 AI 助理叫「怹」） | [`docs/wording-and-naming.md`](docs/wording-and-naming.md) |

### 安全与配置

| 主题 | 文档 |
| --- | --- |
| entitlements / 公证 / 授权面 / 凭据红线 | [`docs/security.md`](docs/security.md) |
| 渠道二进制布局与体积守卫 | [`crates/abb-channel/README.md`](crates/abb-channel/README.md) |

### 机制自检

```sh
bash tools/gates.sh                        # 40 项全绿
bash tools/arch-check.sh                   # 架构数字没漂
bash tools/check-prose-claims.sh            # 注释/文档里的断言属实
bash tools/check_channel_layout.sh          # 渠道仍是单个二进制
tools/review-cadence/mutation-selftest.sh   # 上面这些守卫真的咬得住
```
