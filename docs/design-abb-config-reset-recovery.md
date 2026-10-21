# ABB 崩溃自救：一键重置与数据恢复 —— 可落地设计

- 线程：`abb-config-reset-recovery-20261010`；principal：`abb-win`；本轮**只出设计，不改业务代码**。
- 版本：**v1.1**——在 v1.0 基础上补齐 owner 追加的**硬约束**「Windows 下整目录重命名会被句柄占用阻塞」，
  新增 **§2.7（进程外 / 冷启动自举）**，并据此修正 §0.2 / §0.3 / §2.0 / §2.1 / §2.6 / §4.2 / §7 的**执行者归属**。
- 代码基线：本地检出 `C:\Users\gxh\.agent-bridge\workspaces\cli_a8a27ff268b8900e\abb`
  （HEAD = `90970c1` @ `refactor/win-no-console-centralize`，2026-09-28）。`origin/main` = `640e1d0`（2026-10-10）。
  下文 `file:line` 取自**本地检出**；已抽查 `origin/main` 的关键锚点会随版本漂移（见附录 A），实施时须以当次 main 为准。
- 所有结论凡未读到证据者一律标「未确认」。实测对象：宿主数据目录 `C:\Users\gxh\.agent-bridge`
  （该路径是**符号链接**，真实目录 = `F:\agent-bridge`，见 §4.4）。

---

## 0. 结论速览

需求：配置/运行状态被误操作或未知原因破坏、常规手段修不回来时，提供受控的「重置与恢复」通道：
① 把 `~/.agent-bridge` 整体**重命名**为带时间戳备份（**绝不原地删除**）；② 在空目录上重新初始化；
③ 引导用户重新配置 LLM 与 bot（复用已有设计 `abb-first-run-onboarding-20260922`）；
④ 从备份按需恢复历史数据。目标是**救得回来、可回滚、不丢数据**。

### 0.1 六个开放判断全部由 Jev 判掉（无 escalate）

生产锁 `typesafe/jev-1.13-20260917`（`walgit-watch/accept_check.py decisions`）。

| # | 问题 | Jev 结论 | 分布（argmax 已加粗） | spec / raw |
|---|---|---|---|---|
| D1 | 入口形态 | **gui_primary_multi**（GUI 设置页为主 + CLI 等价通道；IM 只提示不承载） | **0.99** · gui_settings_only 0.01 | `_reset_dec/d1_entry.spec.json` / `.spec.raw.json` |
| D2 | 判定与发起方式 | **auto_diagnose_then_prompt**（程序健康检查，判定不可恢复才提示，用户确认后才进流程） | **0.98** · user_initiated_only 0.02 · auto_execute 0 | `_reset_dec/d2_trigger.spec.json` / `.raw.json` |
| D3 | 四步执行者 | **program_assisted_confirm**（用户显式二次确认后程序代劳；agent 永不自行执行） | **0.99** · manual_with_scaffold 0.01 | `_reset_dec/d3_executor.spec.json` / `.raw.json` |
| D4 | 恢复合并策略 | **merge_import**（分类合并导入：配置以新为准、历史以备份为准） | **0.95** · selective_manual 0.05 · full_overwrite 0 | `_reset_dec/d4_merge.spec.json` / `.raw.json` |
| D5 | 本期重置范围 | **本期只做整目录级**（whole_only 0.53 vs whole_now_per_item_later 0.47，**灰区平局**） | 0.53 / 0.47，置信度 0.41 | `_reset_dec/d5_scope.spec.json` / `.raw.json` |
| D5b | D5 交叉校验：是否为「单项重置」预留扩展点 | **应预留**（p(是)=0.60） | noul 0.600 | `_reset_dec/d5b_extpoint.spec.json` / `.raw.json` |

> D5 的领先两项**共同点** = 本期都不实现单项重置，**唯一分歧** = 要不要预留扩展点；D5b 单独判该分歧 → 预留。
> 合并结论：**本期只做「整目录一键重置」，但在接口/数据结构上为单项重置留口，单项实现留二期。**
> 无命中兜底类目、无两问矛盾；D5 置信度低（0.41）故做了交叉校验，已按 D5b 收敛，不上报 owner。
> 原始候选与全量分布见附录 B。

### 0.2 四步一句话

```
[P0 登记意图+退出] → [1 树外引导程序 rename 备份] → [2 新建空目录] → [3 重新引导 LLM/bot] → [4 分类恢复数据] → [5 校验收尾]
                       §2.7（树外·句柄排空）        create_dir_all       复用 onboarding 线程      merge_import          可整体回滚
```

### 0.3 关键落点（本设计将新增/改动的位置）

| 面 | 位置 |
|---|---|
| 新增模块 | `src/recovery.rs`（备份/新建/恢复/回滚/状态；纯 rename + 复制 + 元数据） |
| CLI 挂载 | `src/main.rs` 顶部分发处新增 `args[1] == "recovery"`（与既有 `job/task/deliver/session/trash/wsver/team/…` 并列，**不与 `session reset` 冲突**，src/main.rs:403-476） |
| GUI | `ui/app.slint` SettingsWindow「危险区」卡片 + 确认弹窗；`src/ui.rs` 诊断信号推导 + 确认流程 + 调 `recovery` |
| 新增可执行（Windows 重命名硬约束，§2.7） | `abb-reset`（`src/bin/abb-reset.rs`）——**树外、短命、干完即退**的整目录 rename 执行体；范式同既有 `src/bin/abb-elev-helper.rs`（`ShellExecuteExW "runas"` 拉起 + 命名管道 DACL + 令牌 + 固定白名单 + 120s 看门，**绝不常驻**） |
| 意图文件（树外） | `<LOCALAPPDATA>/ABB/reset/reset.requested`——**必须落在被重命名树之外**，否则会随 rename 一起被搬走（§2.7.1） |
| 复用 | `Config::save/load`（src/config.rs:1194/1700）、`atomic_write_text`（src/main.rs:111）、`rename` 语义（src/main.rs:86 `rename_replace_retry`）、`platform::migrate_to_agent_bridge`（src/platform.rs:1037）、`task_migrate`（src/task_migrate.rs）、`session_import`（src/session_import.rs）、onboarding 设计 |

---

## 1. 触发入口与「不可恢复」判定

### 1.1 入口形态（Jev D1 = gui_primary_multi）

三通道分工，**只有 GUI 承载不可逆动作**：

1. **GUI 设置页「危险区」卡片（主入口）**——落点 `ui/app.slint` SettingsWindow（1128 起）新增一个默认折叠的
   「重置与恢复（危险）」卡片；逻辑在 `src/ui.rs`（复用 `push_settings_status` src/ui.rs:354-398 的 2s tick
   推导诊断态）。理由：重置要用户看得懂上下文、要二次确认、要能中途取消——GUI 是唯一合适的载体；
   且 onboarding 设计的结论也是「引导归 GUI 首页清单卡承载」（`abb-first-run-onboarding-20260922`）。
2. **CLI 等价子命令**（无 GUI / 脚本 / 远程排障）：
   `agent-bridge recovery <status|diagnose|backup|new-home|restore|rollback|purge> [选项]`。
   与 GUI 走**同一份** `src/recovery.rs`，CLI 只是无头入口；**命令本身不代替确认**——不可逆动作仍要
   `--confirm <确认词>` 且仅在交互式终端或用户显式给出时执行。
3. **IM 侧只提示、不承载动作**——故障时（见 §1.2）向 owner 私聊推一条「检测到疑似不可恢复故障，可在
   ABB 设置页「重置与恢复」查看」；**IM 里不提供任何执行重置的指令**。理由：宿主护栏要求不可逆动作由
   用户显式确认；IM 消息是异步、易误发、无上下文确认的通道，不能作为不可逆动作的入口（D1 判 argv 0.99 也指向此）。

> 为什么不允许 agent 自己触发：宿主护栏（§5）明令 agent 不得迁移/删除该目录；本设计的 CLI/GUI 入口也
> 只对「人类用户显式确认」开放，agent 会话里的 `$ABB_BIN` 白名单**不应**放行 `recovery backup/new-home/restore/rollback`
> （见 §5.3）。

### 1.2 「不可恢复」判定 —— 健康检查信号清单（Jev D2 = auto_diagnose_then_prompt）

**原则**：程序做检查，**只在「硬故障且自动修复失败」时**把「建议重置」呈现给用户；
正常运行的用户**完全无感**（不弹窗、不打扰）。

**一级信号（硬故障，任一命中且持续即视为「常规手段修不回来」）**

| 信号 | 来源 | 判据 | 说明 |
|---|---|---|---|
| S1 配置无法解析 | `Config::load()`（src/config.rs:1194）返回 `Err` | 连续 2 次 load 失败 | config.json 被写坏（半截 JSON / 非法编码）——修不回来的典型 |
| S2 关键配置缺失且长期未补 | `Config::missing()`（src/config.rs:1590）非空 | 存在 ≥30 分钟未变化 | 用户填一半卡住；不强制重置，只提示「去引导页补」 |
| S3 service 起不来 | `logs/service.pid`（src/install.rs:17）+ 存活探测 | 看门连续 K 次拉起失败 | service 反复崩 |
| S4 所有 bot 均未就绪 | service 启动门槛（src/service.rs 就绪判定）+ `bot-status.json` | 没有任何 `enabled` bot 通过凭证校验 | 与 S2 同源但更硬 |
| S5 消息长期无回复 | 入站消息落库（messages.sqlite）vs 出站/回复 | 最近 ≥15 分钟有入站、且**零出站**、且 pending 积压 | 「死局」的用户体感 |

> S1/S3/S5 是「硬」的（结构性坏）；S2/S4 更像「配置不全」——**S2/S4 优先引导去 onboarding 页，不推重置**。
> 只有 **S1 / S3 / S5 且连续命中 N 次**才升级为「建议重置」。阈值（2 / 30min / 15min / K / N）在实现时
> 做成常量并有单测（对齐 `bridge/recover.rs` `RESUME_MAX_ATTEMPTS` 的护栏注释风格）。

**发起方式**：诊断命中 → GUI 托盘红点 + 设置页危险区卡片显高亮 + （可选）IM 私聊一条提示。
用户点进卡片 → 看到命中的信号原文与「重置」按钮（不可逆，见 §1.3 防误触）→ 进入 §2 四步流程。
「用户主动发起」同样支持：任何时候打开设置页危险区就能手动发起（D2 次选项 0.02，保留为兜底入口）。

### 1.3 防误触（正常用户不被骚扰 + 不可逆动作要强确认）

1. **可用性**：入口默认折叠在「危险区」；无故障时卡片不显示红色诊断态，普通用户看不到「重置」按钮。
2. **不可逆动作三级确认**：点击「重置」→ ① 弹窗列出「将被重命名的目录 + 备份名 + 后续步骤」；
   ② 要求**输入确认词**（如 `RESET`，防手滑）；③ 再点「确认执行」。**确认词只在 GUI 手输 / CLI `--confirm` 提供，
   不接受 IM/脚本代传。**
3. **绝不删除**：全程只 `rename`，不 `remove_dir_all`（§4.1）。这就是最根本的防误触——错了能回滚。
4. **审计**：每次动作往 `<bridge_dir>/logs/reset-audit.log` 追加一行（时间 / 动作 / actor=gui|cli / 备份名 / 结果）。
5. **节流**：诊断提示对同一故障 24h 内最多提示 1 次（`logs/reset-prompt.pos`，复用 `agentfail-watch.pos` 的先例）。
6. **默认行为不变**：未命中故障时，ABB 启动/运行路径与本设计前**完全一致**（不新增启动门槛）。

---

## 2. 四步流程（Jev D3 = program_assisted_confirm）

**总原则**：程序提供**可审计的脚手架**并在用户**显式二次确认**后代劳；agent 永不自行执行；
每一步都有明确失败回退，且**任何一步失败都不破坏备份**。

### 2.0 前置 P0：登记意图 + 停服 + 优雅退出（执行者：运行中的 ABB）

> **重要修正（v1.1）**：运行中的 ABB **不自己搬目录**——它的 CWD 就在被搬的树里，由它 `fs::rename` **必然失败**。
> 整目录 rename 交给**树外、CWD 也在树外的外部引导程序**在冷启动阶段执行，机制见 **§2.7**。本步只做「登记 + 退出」。

- **操作**（运行中的 ABB，只做这三件事，**绝不 rename**）：
  1. 在**数据目录之外**写意图文件 `reset.requested`（内容 = 原路径 canonical / 符号链接路径 / 备份目标名 /
     恢复分类清单（§3）/ 触发信号 / 版本 / 时间 / actor）——按 `atomic_write_text`（src/main.rs:111）语义，落盘成功才算数。
  2. 记录**自家进程树 PID 集合**（GUI / service / 它 spawn 的 agent 与 codex/claude 子进程），供阶段 B 排空与自收。
  3. 停 service（`install.rs` 看门的 svc_stop 路径）→ 释放 GUI 自身持有的 `.gui.lock/.service.lock`
     （flock fd 锚点，src/platform.rs:1036）→ **优雅退出**（GUI + service 全退）。
- **为什么必须退出**：Windows 上目录树里只要有**未以 `FILE_SHARE_DELETE` 打开的句柄**（尤其**把树内目录当
  CWD 的进程**、sqlite 的 `-wal/-shm` mmap、锁文件、杀软/索引器瞬时锁），`MoveFileExW` 就报
  `ERROR_ACCESS_DENIED(5)` / `ERROR_SHARING_VIOLATION(32)`；而 `rename_replace_retry`（src/main.rs:86）只退避
  3×50ms、只管**单文件**覆盖，**撑不住**搬整棵树。messages.sqlite 在 WAL 下运行中改名还会破坏一致性。
- **失败回退**：意图文件写不下/写失败 → **不退出、不继续**、报错；停不掉 service/GUI → 提示用户手动退出后重试。
  **严禁**在未登记意图、或目录仍被占用时强行 rename（错误做法 → 见 §2.7 开头）。

### 2.1 步 1：重命名备份（执行者：**树外外部引导程序**（§2.7.2）；**不可逆**，需二次确认）

- **操作**：
  1. 解析真实目录 `real = canonicalize(bridge_dir())`（见 §4.4 符号链接）。
  2. `backup = real.parent()/<name>.bak-YYYYMMDD-HHMMSS`（**同卷同级**，保证 rename 原子）。
  3. `MoveFileExW(real, backup, 0)`——**由树外、CWD 也在树外的外部引导程序执行**（§2.7.2），
     带放宽的退避重试；**不由运行中的 ABB 执行**（会 os error 5/32）。
  4. 往 `backup/.reset-meta.json` 写元数据（原路径 / 时间 / 版本 / 触发信号 / actor）——**只写在备份里**。
- **幂等**：备份名含时间戳；若同名已存在（同秒重跑）则拒绝并提示。
- **失败回退**：rename 失败 → 原目录**未动**，直接报错退出；**严禁退化成 copy+delete**（copy 中断会两半都不完整）。
- **红线**：此步是唯一「移动原目录」的动作，且它是**可逆的**（反向 rename 即回滚，§4.2）。

### 2.2 步 2：新建干净目录 + 重新初始化（执行者：程序）

- **操作**：`create_dir_all(bridge_dir())`（若是符号链接则重建真实目标，§4.4）。**不预置** config.json，
  让 `Config::load()` 返回 `Config::default()`（src/config.rs:1194 的既有语义）。
- **失败回退**：建目录失败 → 提示「可执行回滚把备份改回来」（§4.2）。**注意**：只有「本步刚建的空目录」
  才允许被删除；已有内容的目录一律不删。
- **不做**：不从备份 copy 任何东西进新目录（恢复统一在步 4，按分类策略做）。

### 2.3 步 3：重新引导 LLM/bot（执行者：用户 + 程序脚手架）——**复用 onboarding 设计**

- 直接复用线程 `abb-first-run-onboarding-20260922` 的成果：首页引导清单卡（渠道未接 / 模型未配 /
  飞书未设管理员三条）→ 机器人配置页 → 模型供应商页 → owner 授权码。
- 差异点：本流程进入时数据目录是**空的**，因此必然处于 `need_onboarding()==true` 态；引导完成后
  写 `Config.onboarded = true` 走 `Config::save()`（src/config.rs:1700）。
- **只读辅助（可选，安全增强）**：为免用户忘了原来的 app_id/base_url/model，可在引导页提供
  「从备份只读查看旧配置」——**只回显非密字段**（bot kind/name/app_id、provider kind/base_url/model、
  default_provider），**密钥类字段（app_secret / wx_token / api_key / ding app_secret）一律不回显、不导入**。
  实现上是**只读解析备份里的 config.json**，绝不写回新目录。
- **失败回退**：可跳过、可随时重进；引导未完成不阻塞步 4（恢复数据与是否配好 LLM 无关）。
- IM 侧兜底（已存在、无须新做）：模型未配 → `NoProvider` 预检文案；渠道未接 → 无 bot 可对话。

### 2.4 步 4：从备份恢复数据（执行者：程序；用户逐类确认）——**策略 merge_import（Jev D4）**

- **总策略**：**分类合并导入**，不是整目录覆盖（D4 argmax 0.95，full_overwrite 0）。
  理由：备份里的 `config.json` 恰恰是「被怀疑损坏的东西」，全量覆盖会把坏配置带回来；
  而 `messages.sqlite / tasks / workspaces` 是宝贵历史且与新配置无关，必须以备份为准。
- **恢复单元 = 分类表（§3）里的每一类**。默认勾选「必须恢复」类，用户可取消；「禁止沿用」类灰掉不可选。
- **执行**：
  1. 先写恢复状态文件 `<bridge_dir>/.recovery-state.json`（记录 源备份名 + 每类 {状态, sha256, 时间}）。
  2. 逐类：**先把新目录里的同名现有文件/目录改名备份为 `<name>.pre-recovery`**（不删）→ 复制/导入 →
     校验（存在 + 可解析/计数）→ 落标记。
  3. **幂等**：`(源备份 id, 类别)` 已标记完成则跳过（对齐 `task_migrate` 的幂等标记与 `session_import` 的
     `imported.json` 先例，src/task_migrate.rs / src/session_import.rs）。
  4. **bot_key 重映射**：新配置算出的 bot_key 集合可能与备份不同（同名 bots、启用顺序都可能变）。
     复用仓内既有 `bot_key` 替换先例（src/config.rs:1322/1440-1521 已对 `virtual-bots.json / teams.json /
     deliveries.json` 做 bot_key 替换）对恢复文件做一次映射；无法映射的引用**保留并标 orphan**，不静默丢。
  5. `messages.sqlite` 二进制恢复走「停 service → 复制 db → 清掉新目录的 `-wal/-shm` → 重启」，
     或（更稳）用 SQLite backup API；**必须在 service 停止态做**。
- **失败回退**：单类失败**不阻塞其它类**（各自独立事务 + 标记）；整批可重跑（幂等）；
  最坏情况整体回滚（§4.2）。恢复失败**不损坏备份**（只读备份，写入都在新目录）。

### 2.5 步 5：校验与收尾（执行者：程序）

- 校验：config.json 可解析（`Config::load()` Ok）、`enabled` bot 至少一个就绪、消息历史可读且条数 > 0、
  `tasks list` 可枚举、`recovery status` 显示各类已完成。
- 收尾：提示「备份目录 `<backup>` 仍在，确认无误后可保留或删除」；**默认不自动删备份**（保留策略见 §4.3）。

### 2.6 失败矩阵（汇总）

| 步骤 | 失败 | 回退 |
|---|---|---|
| P0 停服 | 停不掉 service | 拒绝继续，提示手动退出 ABB |
| 1 重命名 | rename 失败/被占用 | 原目录未动，报错退出（不退化成 copy+delete）；外部引导程序按 §2.7.3 排空/报占用清单/重试/指针切换 |
| 2 新建 | 建目录失败 | 提示执行回滚（§4.2） |
| 3 引导 | 用户中断/配置错 | 可跳过、可重进；不阻塞步 4 |
| 4 恢复 | 某类失败 | 该类跳过，其它继续；重跑幂等；最坏整体回滚 |
| 5 校验 | 校验不过 | 保留备份，提示重跑恢复或回滚 |

---

## 2.7 硬约束：Windows 下「整目录重命名」会被句柄占用阻塞 —— 进程外 / 冷启动自举（核心机制）

> 本节正面解决 owner 提出的硬约束。**结论先行**：**运行中的 ABB 与任何「CWD 在 ~/.agent-bridge 树内」的进程
> 都不能重命名这棵树**；重命名必须交给**不在这棵树里、且 CWD 也不在这棵树里**的外部引导程序，在**句柄排空后**执行。

### 2.7.0 事实边界（本设计的地基）

- `MoveFile`/`MoveFileExW` 对目录树内**存在未以 `FILE_SHARE_DELETE` 打开的句柄**时失败，返回
  `ERROR_ACCESS_DENIED(5)` / `ERROR_SHARING_VIOLATION(32)`。仓内 `rename_replace_retry`（src/main.rs:86-98，注释
  明写 os error 5/32）就是为这个坑写的，但它**只退避 3×50ms、只管单文件覆盖**，不足以搬整棵树。
- **最坏情形（本机就是）**：数据目录同时是工作区根（`~/.agent-bridge/workspaces/cli_...`）。跑 agent 的进程本身、
  以及它 spawn 的 codex/claude 子进程，**CWD 就在这棵树里**；而 CWD 是一个**不带 share-delete 的目录句柄**。
  因此「由这棵树里、CWD 在树里的进程去重命名 `~/.agent-bridge`」**必然失败**。
- 其余占用源：sqlite `-wal/-shm`（可能 mmap）、`.gui.lock/.service.lock`（flock）、日志句柄、
  杀软/Windows Search 索引器的**瞬时**锁。

**错误做法（必须避免）**：运行中的 ABB 直接 `fs::rename(real, backup)`；或失败后退化成 `copy + delete`
（copy 中断 = 新旧两半都不完整，且丢掉「原子可回滚」这一唯一优点）。

### 2.7.1 阶段 A：登记意图 + 优雅退出（执行者：运行中的 ABB；= §2.0）

- ABB **不搬目录**，只：① 往**树外**写 `reset.requested`（原路径 canonical / 符号链接路径 / 备份目标名 /
  恢复分类清单 / 触发信号 / 版本 / 时间 / actor）；② 记录自家进程树 PID 集；③ 停 service → 释放自己持有的锁 → 优雅退出。
- **关键**：意图文件必须落在**被重命名的树之外**（如 `%LOCALAPPDATA%\ABB\reset\`）——否则它自己也随 rename 被搬进备份，
  阶段 B 就再也读不到。
- **失败回退**：意图写失败 ⇒ 不退出、不继续；停 service/GUI 停不掉 ⇒ 提示用户手动退出后重试。

### 2.7.2 阶段 B：进程外自举执行 rename（执行者：外部轻量引导程序）

**三个候选载体（择一，推荐 ①）**：

1. **独立 `abb-reset`（推荐）**：与既有 `src/bin/abb-elev-helper.rs` **同款范式**的独立短命可执行——
   `ShellExecuteExW` 拉起、**干完一件即退、绝不常驻**、命名管道 + 令牌鉴权、固定白名单、看门超时兜底。
   由 GUI/CLI 拉起时**显式把 `lpCurrentDirectory` 设为树外目录**（如 `%SystemRoot%\System32` 或自身安装目录），
   **确保其自身 CWD 不在 `~/.agent-bridge` 里**——这是本机制成立的充要条件。
2. **下次冷启动的 bootstrap 阶段**（兜底）：ABB 主程序在**读 config 之前**的最早期先检查 `reset.requested`。
   **但**主程序自身若被从树内拉起、CWD 在树里则同样失败 ⇒ 仅当「用户从开始菜单/桌面快捷方式冷启动
   （其 WorkingDirectory 通常 = 安装目录，已出树）」时才可用；否则落回 ①。
3. **安装器（Inno Setup，`app-assets/ABB.iss`）同款**入口：更新/修复安装包里内嵌一步「若存在 `reset.requested`
   则执行重命名」。缺点：要跑安装器，重；仅作最后兜底。

**阶段 B 执行序列**：

1. 读树外的 `reset.requested`。
2. **句柄排空**：等待阶段 A 记录的 PID **全部退净**（带超时，建议 60s）；到点仍有存活 → 进入第 3 步。
3. **占用枚举（Restart Manager API）**：`RmStartSession` → `RmRegisterResources`（注册真实目录路径）→
   `RmGetList` 得到 `RM_PROCESS_INFO[]`（进程名 / pid / 应用类型）。**分两类处理**：
   - **自家残留**（进程名属 ABB/agent-bridge，或命中阶段 A 的 PID 集）→ **自己收**（`TerminateProcess` / 等其退）；
   - **外部程序**（编辑器、杀软、索引器、资源管理器预览等）→ **绝不 kill**，把**进程名 + pid 清单报给用户**，
     让用户关闭后重试。
   > 局限（如实声明）：Restart Manager 报告「已打开文件」，对「目录被当 CWD 持有」不一定列出 ⇒ 它是**辅助**
   > 而非充分；真正的保证仍是「**自身 CWD 不在树里 + PID 退净**」。
4. **执行重命名**：`MoveFileExW(real, backup, 0)`（同卷、不覆盖），包一层**放宽的退避重试**
   （如 5 次 × 200ms→1s 指数退避）——语义对齐 `rename_replace_retry` 但次数/间隔更宽。
5. **成功** → 在备份里写 `.reset-meta.json`（§4.1）→ `create_dir_all(bridge_dir())` 重建（符号链接场景 §4.4）
   → 清除 `reset.requested`（或标 done）→ 提示/拉起主程序进入引导（步 3）。
6. **失败**（仍被占用）→ 走 §2.7.3 兜底；**绝不**退化成 copy+delete。

### 2.7.3 兜底与回滚（rename 死活做不成时）

1. **指针切换兜底**（最后手段，可选）：**不改名**旧目录，把**新目录放到别处**（如 `%LOCALAPPDATA%\ABB\home-new`），
   用 `AGENT_BRIDGE_HOME` 环境变量 / 一个「活动目录指针」把 ABB 指到新目录——§4.4 已确认 `bridge_dir()`
   （src/main.rs:62/70）尊重 `AGENT_BRIDGE_HOME`。**旧目录原地保留**（删指针即回旧，天然可回滚）。
   代价：默认路径 `~/.agent-bridge` 仍是旧的，须向用户说清。
2. **报清单让用户关进程后重试**：把 Restart Manager 列出的外部占用者（进程名 + pid）原样报给用户，用户关掉后点「重试」。
3. **回滚**：任何失败路径下，只要**尚未成功 rename**，旧目录即未动、原状即回滚；若已 rename 但后续
   （新建/引导/恢复）失败，走 §4.2 反向 rename——**同样由树外引导程序执行、同样先排空句柄**。

### 2.7.4 两个「不能依赖」的旁证（可选优化，**不作替代**）

- Rust `std::fs::File` 在 Windows 默认以 `FILE_SHARE_READ|WRITE|DELETE` 打开 ⇒ ABB 自己用 std 打开的**普通文件**
  一般**不**阻塞目录 rename；真正卡人的是 **CWD 在树里的进程**、sqlite 的 mmap、以及**外部程序**。
- Win10 1709+ 对「目录内有打开文件时的重命名」放宽，允许按句柄 `SetFileInformationByHandle(FILE_RENAME_INFORMATION)`
  重命名目录——但**不能依赖**（旧系统/权限/句柄类型都不保证）。**正解仍是**：在**没人占用（或句柄可控）时**做。

---

## 3. 数据分类表

实测 `~/.agent-bridge` 全部条目（真实目录 `F:\agent-bridge`）。处置四档：
**必须恢复**（用户数据/运行状态，与 config 无关，默认回流）/ **可恢复**（低风险，默认回流但可取消）/
**禁止沿用**（被怀疑损坏或与新环境冲突，绝不回流）/ **忽略**（日志/锁/临时/残档，留在备份里不回流）。

### 3.1 顶层条目

| 路径 | 类别 | 处置 | 理由 | 冲突合并策略 |
|---|---|---|---|---|
| `config.json` | 配置（含所有 bot 凭据 + provider key） | **禁止沿用** | 正是「被怀疑损坏」的对象；密钥应随新环境重配 | 一律以新目录为准；仅在步 3 引导期**只读**回显非密字段帮助重填，密钥不回显不导入 |
| `config.json.bak-rename` / `config.json.pre-acp-fix` / `virtual-bots.json.bak-rename` | 历史残档 | **忽略** | 旧迁移残留，无当前语义 | 不回流 |
| `teams.json` | 团队登记表（#147，src/teamreg.rs:51） | **必须恢复** | 用户创建的团队/成员数据，与 config 无关 | 按 team id 合并；冲突以**备份为准**（用户数据）；引用已删 bot_key 的保留并标 orphan |
| `virtual-bots.json` | 虚拟 Bot 登记（角色↔群映射，src/bridge/mod.rs:453） | **必须恢复** | 用户创建数据 | 按 id 合并；备份为准；做 bot_key 重映射 |
| `messages.sqlite`（+ `-wal` / `-shm`） | 消息历史库（src/msgstore.rs:47，WAL） | **必须恢复** | 用户对话历史，最宝贵 | 备份为准（新目录初始为空）；**停 service 后**整库恢复并丢弃新 `-wal/-shm` |
| `tasks/` | 任务定义/状态/日志（`tasks/<bot_key>/tasks.json`、`tasks-state.json`、`task-logs/`） | **必须恢复** | 用户任务与运行记录 | 按 bot_key 目录合并；按 task id 去重（幂等）；新目录无同名冲突 |
| `deliveries.json` | 跨会话投递队列（src/deliver.rs:127，「存在即消费」） | **必须恢复**（仅未消费条目） | 未消费条目是用户待办投递 | 只回流**未消费**条目；按「新鲜度」过滤（默认丢弃超过 24h 的陈旧条目，可开关）；按 (target,text,created_at) 去重 |
| `session_state.json` | 会话暂停/恢复状态（src/session_state.rs:44） | **可恢复** | 低风险运行态 | 备份为准；bot_key 重映射 |
| `subs.json` | MCP 事件订阅游标（src/mcp_events.rs:43） | **可恢复** | 订阅配置；旧游标可能已失效 | 备份为准；恢复后标记「可能需刷新」 |
| `events.ndjson` + `events.seen` | MCP 事件流（append-only，src/mcp_events.rs:42） | **可恢复** | 事件溯源历史，不致命 | 备份为准；不追加合并（append-only 语义按源覆盖） |
| `events.lock` / `deliveries.lock` | 文件锁 | **忽略** | flock/LockFileEx 的 fd 锚点，复制无意义 | 新位置自建 |
| `.gui.lock` / `.service.lock` | 单实例 flock | **禁止沿用** | 复制锁文件无意义且可能误判 | 新位置由 single_instance 自建（src/platform.rs:1036 同款结论） |
| `wsids.txt` | **未知**（现状 0 字节，源码无引用） | **忽略** | 无法确认语义 | 不回流（在备份里留存）；实现前再 grep 一次确认 |
| `guard/` | 受限会话 guard hook 文件（src/guard.rs:23，`guard/<bot_key>/settings.json`） | **禁止沿用** | 由当前 config 派生，且是防篡改锚点 | 保存后用 `ensure_guard_files` 按新配置重建 |
| `logs/` | 日志目录（bridge.out、boot.log、service.pid、service.desired、update.log、`backup-*` 更新备份…） | **忽略** | 纯日志/看门状态 | 不回流；`service.pid` 是 stale pid **必删**（src/platform.rs:1065 同款教训），`service.desired` 视用户是否需要自启决定 |
| `_abb_tmp/` | 临时文件 | **忽略** | 临时 | 不回流 |
| `workspaces/` | 每 bot 工作目录（见 3.2） | **必须恢复** | 用户工作成果与历史 | 见 3.2 |

### 3.2 `workspaces/<bot_key>/`（bot 工作区，实测每个工作区约 1GB+）

| 路径 | 处置 | 理由 / 冲突策略 |
|---|---|---|
| `sessions.json`（+ `sessions.json.legacy.bak`） | **必须恢复** | 会话槽位（src/sessions.rs:1）；备份为准；bot_key 重映射 |
| `jobs.json` + `jobs.json.migrated` + `jobs.json.migrated.bak` | **可恢复** | 旧 job 迁移源（src/task_migrate.rs）；迁移已由 tasks/ 承接，仅保底保留 |
| `history/` | **必须恢复** | 每 chat 的历史条目（src/history.rs） |
| `summaries/` | **可恢复** | 会话归纳存档 |
| `.trash/`（含 manifest + items） | **可恢复** | 回收站（src/trash.rs）；备份为准 |
| `.git/`（工作区 git 快照，内置 libgit2） | **必须恢复** | 删除保护/版本快照（#209）；随工作区整体恢复 |
| `AGENTS.md` / `CLAUDE.md` / 引导文档 | **忽略** | 由程序按新版本重写（`ensure_workspace_guide` / `rewrite_workspace_guides` 先例，src/platform.rs:1072）；**用户手改内容会被覆盖**，故恢复时把备份版另存为 `*.recovered` 供比对 |
| 用户放在工作区里的业务文件（代码/文档/产出） | **必须恢复** | 这是工作区存在的意义 |
| `.pi-sessions/` / 后端私有会话文件 | **可恢复** | 与后端 session 相关，可能失效；保留不致命 |
| `.abb-session-gc-last` / `.abb-trash-gc-last` | **忽略** | GC 时间戳，重建即可 |

> **冲突总原则**：历史/用户数据 → **以备份为准**；配置/派生/由程序生成的 → **以新目录为准**。
> 每条恢复前先把新目录同名项改名为 `.pre-recovery`（不删），便于事后核对与二次回滚。

---

## 4. 备份命名 / 保留策略 / 回滚

### 4.1 命名与位置

- 命名：`<真实目录名>.bak-YYYYMMDD-HHMMSS`（**本地时间**，对齐仓内既有 `*.bak-20260917`、`config.json.bak-rename` 约定）。
  例：`F:\.agent-bridge.bak-20261010-200500`。
- 位置：与真实目录**同卷同级**（`real.parent()`）。**必须同卷**——跨卷 `rename` 会 `EXDEV` 失败，
  只能退化成 copy+delete（破坏原子性与可回滚性），本设计**拒绝跨卷**，检测到则明确报错。
- 元数据：备份里写 `.reset-meta.json`（原路径 / 触发信号 / 版本 / actor / 时间 / 本机标识），供回滚与审计。

### 4.2 回滚步骤（认错了 / 恢复失败时退回原状）

**整体回滚（把备份改回来）**：
1. 停 GUI + service（同 §2.0）。
2. （可选）把当前新目录改名为 `<dir>.failed-YYYYMMDD-HHMMSS`（**不删**，留证）。
3. `MoveFileExW(backup, bridge_dir(), 0)`——反向 rename，**同样由树外引导程序执行**（先排空句柄，§2.7.2）。
4. 删除 stale `logs/service.pid`（防看门误杀，src/platform.rs:1065）。
5. 重启 ABB → 校验 config.json 可解析、bot 在线。
6. 若第 3–5 步中反悔：再反向 rename 回来即可（两次 rename 都无损）。

**局部回滚（恢复出错但新目录已可用）**：把该类的 `<name>.pre-recovery` 改回原名，或重跑恢复（幂等）。

### 4.3 保留策略

- 自动备份默认**保留最近 3 份**；**「本次重置创建」的备份不参与自动清理**（`backup` 只自增，清理只在
  用户显式 `recovery purge` 或确认「重置已完成且验证通过」后按策略淘汰）。
- 备份目录置只读标记（`.reset-meta.json` 里 `pinned: true` 表示用户要求永留）。
- 磁盘告警：备份与新目录同卷，重置瞬间占用不变（rename 不复制）；但**恢复**会复制历史数据，
  空间不足时**先报错后不恢复**（不半途留残）。
- 绝不自动删除任何备份；删除只经 `recovery purge <name> --confirm`。

### 4.4 符号链接 / `AGENT_BRIDGE_HOME`（本机实测必踩）

- 本机 `C:\Users\gxh\.agent-bridge` 是**符号链接** → `F:\agent-bridge`。设计上：
  以 `canonicalize(bridge_dir())` 得到的**真实目录**为操作对象，`rename` 真实目录后**重建真实目录**，
  **符号链接本身不动**（否则会出现悬空链接）。
- `AGENT_BRIDGE_HOME` 非空时 `bridge_dir()` 直接取该值（src/main.rs:62/70）：重置应作用于该目录，
  但**不探测也不搬动**默认 `~/.agent-bridge`（对齐 `migrate_to_agent_bridge` 在 override 时的 early-return，src/platform.rs:1037 附近）。
- 备份目录名以**真实目录名**为准，避免链接名与目标名混淆。

---

## 5. 与宿主护栏的关系

### 5.1 护栏原文（`src/agents_md.rs:290` const `HOST_GUARD`，无条件注入 agent）

```
── 宿主护栏（ABB 无条件）──
- 禁止 kill / taskkill / Stop-Process 本机 agent-bridge / ABB 进程（含 /IM、/PID 方式）
- 禁止修改 ~/.agent-bridge 下的宿主配置文件（config.json、teams.json 等），禁止迁移/删除该数据目录
- 需要重启服务或修改宿主配置时，请直接告知用户操作，不要自行执行
```

### 5.2 本设计如何服从护栏

1. **agent 永不自行重命名/删除 `~/.agent-bridge`**：本流程的所有不可逆动作只能由**人类用户**在 GUI 手输确认词
   或 CLI `--confirm` 后，由**程序**执行；agent 只提供诊断信息与指引。
2. **不可逆动作只由用户显式确认或手动执行**：程序提供脚手架（诊断、备份、恢复、回滚命令），
   但每一步的「进入下一步」都由用户点确认。
3. **绝不原地删除**：全程 `rename`；连恢复时覆盖同名文件也先改名 `.pre-recovery` 而非删除。
4. **与既有删除保护的关系**：`guard-check`（src/guard.rs）对**工作区外删除不拦截**，所以护栏不能
   自动兜住 `~/.agent-bridge` 的误删——本设计用「只 rename + 三级确认 + 审计」补上这道人工闸，
   而不是扩 guard 去拦 owner 用户（owner 的显式操作本就允许）。

### 5.3 建议（交给实现阶段定）：`$ABB_BIN` 白名单不放行 `recovery` 写操作

受限/授权者会话的 CLI 白名单目前只放行 `job add` / `task add` / `session reset` / `deliver` 等。
本设计的 `recovery backup|new-home|restore|rollback` **属不可逆**，建议**只对 owner 人类入口开放**，
不加入 agent 可调白名单；`recovery status|diagnose` 只读，可考虑放行供 agent 排障（但须只读、不落盘改动）。

---

## 6. 开放决策点（已按 §0.1 用 Jev 判掉，留作审计）

| 决策点 | 候选（全量） | Jev 结论 | 说明 |
|---|---|---|---|
| D1 入口形态 | gui_primary_multi / gui_settings_only / tray_only / cli_only / im_command / other / clarify | **gui_primary_multi 0.99** | GUI 为主 + CLI 等价；IM 只提示 |
| D2 判定与发起 | auto_diagnose_then_prompt / user_initiated_only / auto_execute / other / clarify | **auto_diagnose_then_prompt 0.98** | 自动诊断 + 用户确认；不自动执行 |
| D3 执行者 | program_assisted_confirm / manual_with_scaffold / fully_automatic / other / clarify | **program_assisted_confirm 0.99** | 程序在用户确认后代劳；agent 永不执行 |
| D4 合并策略 | merge_import / full_overwrite / selective_manual / other / clarify | **merge_import 0.95** | 配置以新为准、历史以备份为准 |
| D5 本期范围 | whole_now_per_item_later / whole_only / per_item / other / clarify | **whole_only 0.53**（灰区，置信度 0.41） | 本期只做整目录 |
| D5b 扩展点交叉校验 | true / false（是否为单项重置预留扩展点） | **true（p=0.60）** | 预留扩展点，单项留二期 |

- 无命中兜底类目（other/clarify 均为 0）；无两问矛盾；D5 灰区已由 D5b 收敛。
- 原始 spec 与 raw 响应：`walgit-watch/_reset_dec/{d1_entry,d2_trigger,d3_executor,d4_merge,d5_scope,d5b_extpoint}.spec{,.raw}.json`。

---

## 7. 分阶段实施计划 + 验收标准 + 风险

> 本轮（P0）只交付本设计文档；P1+ 为实施建议。开发/验收流程按 `walgit-watch/patrol-rules.md` §6：**开发派 codex**，
> **验收 = 硬门禁（fmt/clippy/build/test）+ Jev `accept_check.py --evidence`**，两方结论须一致。

### P0 设计（本轮）
- 交付：本文件。验收：acceptance 7 条逐条覆盖（§9）。
- 风险：无（文档）。

### P1 备份/新建/回滚骨架（无 GUI）
- 改：新增 `src/recovery.rs`（`backup` / `new_home` / `rollback` / `status` / `purge`；纯 rename + 元数据 + 审计）；
  `src/main.rs` 挂 `recovery` 子命令；单测。
- 验收：单测覆盖——① 备份名格式与同秒冲突拒绝；② 同卷 rename 成功、EXDEV 明确报错；
  ③ 回滚反向 rename 恢复原状；④ 符号链接场景重建真实目录；⑤ stale `service.pid` 删除；
  ⑥ **句柄占用语义（§2.7，硬约束）**：造一个「CWD 在树里」的进程 + 一个非 share-delete 打开的文件，
     断言**树内进程** rename 失败（os error 5/32）、**树外 `abb-reset`（CWD 在树外）** rename 成功；
     断言 Restart Manager 能列出占用者、自家残留被自收、外部进程只报**不**杀。
  真机演练**只对 `AGENT_BRIDGE_HOME` 指向的临时目录**做（**绝不动真实 `~/.agent-bridge`**），
  并覆盖「冷启动 bootstrap 检查 `reset.requested`」这条路径。
- 风险：**Windows 句柄占用**（正解 = 树外引导程序 + 句柄排空，§2.7；**不能**靠 `rename_replace_retry`）；
  跨卷（拒绝并报错）；符号链接误处理；**意图文件误落在树内**（会随 rename 一起被搬走）。

### P2 恢复引擎（merge_import + §3 分类 + 幂等）
- 改：`src/recovery.rs` 增 `restore`；复用 bot_key 重映射（src/config.rs:1322 一带）与幂等标记
  （`task_migrate` / `session_import` 先例）。
- 验收：逐类用例（必须/可恢复/禁止/忽略 四档各覆盖）；**幂等**（重跑零副作用）；冲突用例
  （同名 bot、bot_key 变化、deliveries 陈旧条目过滤、messages.sqlite WAL）；失败单类不阻塞其它。
- 风险：messages.sqlite WAL 一致性（必须停服态）；大数据量复制耗时（进度反馈 + 失败不留半截）。

### P3 GUI 入口 + 诊断 + 防误触
- 改：`ui/app.slint`（危险区卡片 + 确认弹窗 + 确认词输入）、`src/ui.rs`（诊断信号推导、确认流程、调 `recovery`）。
- 验收：坏 config 下诊断触发并显示命中信号；确认词错误时拒绝执行；正常配置用户无感（卡片不显红）；
  IM 侧只收到提示、无执行入口。
- 风险：UI 跨平台时序（必须走 `show_window_and_focus`，src/ui.rs:405-415）；诊断误报（阈值需真机标定）。

### P4 IM 提示 + 文档
- 改：IM 提示文案（复用 `NoProvider` 一类的现成话术风格）；README / AGENTS 文档补「重置与恢复」章节。
- 验收：故障时 owner 收到一条提示且 24h 节流生效；文档可照做。
- 风险：误骚扰（节流阈值）；跨平台（Windows/macOS）文案一致。

### 跨平台风险（Windows / macOS）
1. rename 占用（Windows os error 5/32 → `rename_replace_retry`）；
2. `show_window_and_focus` 的 macOS 专有时序坑；
3. 符号链接（本机实测，§4.4）；
4. 大小写/路径分隔差异（备份名与元数据统一用 `/` 存储、展示时本地化）。

---

## 8. 与 `abb-first-run-onboarding-20260922` 的复用关系

| 复用点 | 来源 | 本设计用法 |
|---|---|---|
| 首页引导清单卡（渠道/模型/管理员三条） | onboarding §4 | 步骤 3 的载体；重置后数据目录为空，必然进入该引导 |
| `Config.onboarded` 字段 + `migrate_onboarded()` | onboarding §1 | 引导完成后落 `onboarded=true` |
| 模型供应商页（provider_key_only） | onboarding §3 | 步骤 3 的 LLM 配置 |
| owner 授权码 / 飞书 fail-closed 提示 | onboarding §2.1 S6 | 步骤 3 的管理员配置 |
| 复用关系性质 | — | 本线程**主要新增**「重命名备份 + 分类恢复 + 回滚 + 诊断触发」四段；引导段**原地复用**，不重复设计 |

---

## 9. 与 acceptance 的逐条对应

| # | acceptance 条目 | 覆盖位置 |
|---|---|---|
| 1 | 入口何时出现、由谁触发、如何判定不可恢复（健康信号）、防误触 | §1（D1 入口 + §1.2 五信号 + §1.3 防误触三级确认/节流）；正常用户无感见 §1.3.6 |
| 2 | 四步各步执行者/输入输出/失败处理；哪些不可逆、如何二次确认 | §2（P0+五步；失败矩阵 §2.6；不可逆=步 1 重命名，§2.1 + §1.3.2；标 D3） |
| 3 | 恢复范围与合并策略；必须回流 / 禁止沿用；推荐全量覆盖还是合并导入 | §3 分类表（必须/可恢复/禁止/忽略四档）；§2.4 + D4 = **merge_import**（附理由） |
| 4 | 备份命名/位置、保留策略、回滚步骤 | §4（命名 §4.1、保留 §4.3、回滚 §4.2） |
| 5 | 与宿主护栏的关系 | §5（护栏原文 + 服从方式 + 白名单建议） |
| 6 | 关键决策点显式列出（含 3 个点名项） | §6（含 GUI vs IM、程序 vs 手动、单项重置三项）+ §0.1 Jev 结论 |
| 7 | 不写业务代码（本轮只出设计） | 本文件为唯一交付物；未改任何 `crates/`/`src/` 代码 |

> 附：本轮 owner 追加的**硬约束**（Windows 下运行中进程无法重命名被句柄占用的目录树）在 **§2.7** 正面解决
> （进程外 / 冷启动自举 + Restart Manager 排空 + 兜底与回滚）；该条不是 acceptance 原文条目，故不占上表行。

---

## 附录 A：基线 `file:line` 抽查（本地检出 `90970c1` vs `origin/main` `640e1d0`）

| 锚点 | 本地检出 | origin/main |
|---|---|---|
| `bridge_dir()` | src/main.rs:70 | src/main.rs:81 |
| `workspace_dir()` | src/main.rs:77 | — |
| `rename_replace_retry` | src/main.rs:86/102 | src/main.rs:97/113 |
| `atomic_write_text` | src/main.rs:111 | — |
| `HOST_GUARD` | src/agents_md.rs:290 | src/agents_md.rs:111 |
| `migrate_to_agent_bridge` | src/platform.rs:1037 | src/platform.rs:1678 |
| `Config::save` | src/config.rs:1700 | src/config.rs:1708 |
| `Config::missing` | src/config.rs:1590 | src/config.rs:1598 |
| `Config::is_configured` | src/config.rs:1695 | src/config.rs:1703 |

`origin/main` 上仍**无任何 reset/recovery 实现**（`git grep -i "self_reset|reset_home|backup_dir"` 仅命中一个测试函数名），
与 issue 背景一致。实施时以当次 main 为准复核行号。

## 附录 B：Jev 候选全量分布

```
D1 entry_form     : gui_primary_multi .99 | gui_settings_only .01 | tray_only/cli_only/im_command/other/clarify 0
D2 trigger_and_detect: auto_diagnose_then_prompt .98 | user_initiated_only .02 | auto_execute/other/clarify 0
D3 who_executes   : program_assisted_confirm .99 | manual_with_scaffold .01 | fully_automatic/other/clarify 0
D4 restore_strategy: merge_import .95 | selective_manual .05 | full_overwrite/other/clarify 0
D5 reset_scope    : whole_only .53 | whole_now_per_item_later .47 | per_item/other/clarify 0   (confidence .41，灰区)
D5b reserve_ext_point: p(是)=.600
```

## 附录 C：未确认 / 未做（如实声明）

- 未读 `origin/main` 全部相关文件（仅抽查关键锚点）；行号以本地检出为准，已在附录 A 标注漂移。
- 未真机演练备份/回滚（红线禁止动真实 `~/.agent-bridge`）；P1 起在 `AGENT_BRIDGE_HOME` 临时目录演练。
- `wsids.txt` 语义未确认（0 字节、源码无引用）→ 暂列「忽略」，实施前再核实。
- 诊断阈值（2 次 / 30min / 15min / K / N）为初始建议值，需真机标定（P3 验收内测）。
- 未改任何业务代码，未碰他人线程/分支，未合并/关线程。
