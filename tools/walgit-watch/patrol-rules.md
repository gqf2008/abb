# walgit 巡检动作规则（patrol-rules）

本文件是 `walgit-board-patrol`（cron `*/10`）子代理的**动作手册**：`patrol.py` 只负责发现，
本文件规定发现之后**哪些自动办、哪些只报**。改规则就改本文件，不用重建任务。

> **§A 硬性要求（先读这条）**
> 1. 必须**原样运行** `python walgit-watch\patrol.py`，不得自己重写一遍逻辑、不得只用 walgit CLI 看板代替。
> 2. `patrol.py` 输出里 **`[walgit 巡检·需动作]` 段落的每一条，都必须逐条出现在你的最终输出里**，
>    只报不改的也要报（哪怕只写一行「<线程>：已 approve，需重基，待 owner 决定」）。
>    **漏读一条 = 事故。** 2026-09-22 实测就有一次：patrol 报出了 `[需收口]`，
>    但回合输出写成「无自家线程需动作」，等于又把事件丢了。
> 3. 只有「patrol.py 一字未打」时，才允许空输出。

## 0. 环境常量（本机实测可用）

| 名称 | 值 |
|---|---|
| 工作区 | `C:\Users\gxh\.agent-bridge\workspaces\cli_a8a27ff268b8900e` |
| walgit CLI | `C:\Users\gxh\AppData\Local\Programs\walgit\walgit.exe`（v0.8.3） |
| 本地镜像 | `walgit-watch\abb-mirror.git`（只读用；`origin` = `http://127.0.0.1:8081/gqf2008/abb.git`） |
| abb 检出 | `abb\`（`origin` = 同上，真正的 walgit 服务） |
| 我的 principal | `abb-win` |
| 我的私钥 | `C:\Users\gxh\.walgit\keys\abb-win.ed25519` |
| ABB CLI | `C:\Users\gxh\AppData\Local\Programs\ABB\agent-bridge.exe`（调 `job`/`task` 时用，并显式设 `AGENT_BRIDGE_BOT_KEY=cli_a8a27ff268b8900e`） |
| 报告的投递目标 | 由任务自身 `--to` 决定，**不要**在巡检回合里另发消息 |

## 1. 自动办（只限「自家线程」，即 `abb-win` 签名过的线程 / 在 `own-threads.json` 里）

### 1.1 `[需收口]` —— 已 approve，待合并收口
前置：该线程最新 `status` = `needs-review`，其后有 `review(decision=approve)`，且**没有**非 `abb-win` 签的 `merge_result`（有就让位，只报）。

**能安全收口时执行**（必须同时满足）：
- `git -C abb fetch origin` 后，`refs/remotes/origin/main` 已经包含该 patch 的 head（等于已合并，只缺记账）；**或**
- 该 head 能 `--ff-only` 合入 `origin/main`（纯快进）。

动作（严格按此顺序）：
1. 在**专用 worktree** 里操作，别碰 `abb\` 的当前分支：
   `git -C abb worktree add --detach ..\abb-wt-merge <head>`（用完 `worktree remove`）。
   若只是「main 已含 head」，跳过合入。
2. 快进合入并推：`git -C abb push origin <head>:refs/heads/main`（仅当 `--ff-only` 成立）。
3. 记 walgit 账（`abb-win` 签，`--parent` 取上一条 oid）：
   - `merge_result`：body 含合并点 oid + 依据的 review；
   - `merge_result`：body 含 `merged: true`；
   - `status`：`closed`。
4. 输出 1–3 行：「已收口 <线程>；合并点 <oid>；依据 review <actor>」。

**不能安全收口时（非快进 / 有冲突 / 改动面与评审不一致 / 主检出有本地改动）**：
**一律不自动做**，只输出：「<线程> 已 approve，但需重基（非快进），需 owner 决定」。

### 1.2 `[需退回]` —— 被 review 打回
动作：记一条 `status` = `in-progress`（`abb-win` 签），body 里列清 needs-changes 的问题点；
输出 1–3 行。

## 2. 只报（不动作）

- 他人线程（actor ≠ `abb-win` 且不在 `own-threads.json`）的**任何**事件，包括已 approve / 已 merge；
- 新 `issue`（包括 `[需处理]` 标记的）——只报，不认领；
- `ci-artifacts/*` 的批量变化；
- 上面 1.1 里判定为「不安全」的收口。

只报时输出 ≤8 行：变了什么 + 一句话点明需要 owner 拍板什么；没有需要拍板的，就别发。

## 3. 红线（违反即事故）

- 不替别人签 key、不认领或修改别人的线程、不动别人的分支。
- 不执行未在 §1 明确授权的不可逆动作。
- 不 kill / 停 ABB 或 walgit 进程；不改 `~/.agent-bridge` 下的宿主配置。
- 拿不准就只报；宁可少做，不可错合。

## 4. 当前已知项（做过一次判定，记录在此避免反复）

- `abb-walgit-event-watch-20260921`（自家线程，已 approve，patrol 每轮会报 `[需收口]`）：
  其分支内容是把**旧的 watch.ps1 那套**随仓引入（`tools/walgit-watch/`），
  而本机已改用 `walgit-watch\patrol.py`，且该分支已落后 main 多个提交（非快进），**不要合并**。
  每轮只输出一行：「`abb-walgit-event-watch-20260921` 已 approve，但内容已被 patrol.py 取代且非快进，待 owner 决定作废 / 改写重提」。

## 5. 协同办（派单制）——「待派单」段落怎么处理

> 原则：**巡检回合不亲自干长活**（改码/评审都可能跑十几分钟，会堵住这个会话）。
> 巡检回合只做三件事：**判重 → `task add` 派子代理 → 回报派了什么单**。

`patrol.py` 的 `[walgit 巡检·待派单]` 段落给出两类候选：

| 标记 | 含义 | 派什么 |
|---|---|---|
| `[待派单·评审] <线程> last=needs-review，尚无 review` | 有人交了 patch 在等人审 | 派**独立评审**子代理（身份 `abb-reviewer-12`，key `C:\Users\gxh\.walgit\keys\abb-reviewer-12.ed25519`）|
| `[待派单·认领] <线程> 自家 open issue` | 自家名下、无人推进的存量 issue | 派**认领+实施**子代理（身份 `abb-win`）|

### 5.1 判重（先做，防每 10 分钟重复派单）
派单前先跑：
```
set AGENT_BRIDGE_BOT_KEY=cli_a8a27ff268b8900e && C:\Users\gxh\AppData\Local\Programs\ABB\agent-bridge.exe task list
```
若已有任务的**名字**包含该线程 slug（见下）且运行态为 `Running`/`Pending`，**跳过，不重复派**。

线程 slug 取法：线程 id 里去掉 `-20260922` 这类日期尾巴前 40 字符即可（例：
`gh-326-p1b-p2a-p2b-p3-p4-p5` -> `patrol-task-gh-326-p1b-p2a-p2b-p3-p4-p5`）。
**任务名一律 `patrol-task-<slug>`**，靠这个名字判重。

### 5.2 派单命令
```
set AGENT_BRIDGE_BOT_KEY=cli_a8a27ff268b8900e && C:\Users\gxh\AppData\Local\Programs\ABB\agent-bridge.exe task add ^
  --prompt "<见 5.3 模板>" --bot cli_a8a27ff268b8900e --name patrol-task-<slug> ^
  --cwd C:\Users\gxh\.agent-bridge\workspaces\cli_a8a27ff268b8900e ^
  --timeout-secs 2400 --max-restarts 1 --to oc_1f097b843c4d12b3bc8b91205cfe4dd8
```
（中文 prompt 很长，**用 python 写文件再读入传参**，别在 cmd 里手拼引号。）

### 5.3 子代理 prompt 模板
**评审单**：核对最新 head 对 main 的 diff 与 patch 自述是否一致；在被派线程上以 `abb-reviewer-12`
签一条 `review`（`decision=approve|needs-changes`，`--parent` 取当前 head oid）推回 origin；
**不合并、不改码、不关线程**；门禁跑不了就如实声明。结果 ≤10 行。

**认领单**：先核对 main 现状——若该 issue 已被别人实现/已过时，签 `comment`（附证据）+
`status: closed`（`abb-win`）收口；若仍有效，签 `comment`（认领，写清 worktree/branch）+
`status: in-progress`，并**只输出实施计划**（关键文件、验收标准、预计改动面），本轮不改码。
两条路都**不碰别人的线程/分支**。结果 ≤10 行。

### 5.4 巡检回合的输出
- 派了单 -> 每单一行：「已派 <线程> -> task <id>（<评审|认领>）」。
- 判重跳过 -> 一行：「<线程> 已在跑（task <id>），跳过」。
- 全都不需要派 -> 不说。

### 5.5 硬边界
- 一个线程同一时刻只派一单；派出去没跑完，后续轮次只报「在跑」。
- 子代理**只做自己那一单**，不顺手改别的线程。
- 子代理报告里若卡住，如实写卡点，不要假装完成。
