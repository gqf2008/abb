# Repository Guidelines

Contributor guide for **ABB (agent-bridge)**, a Rust + Slint menu-bar app that bridges Feishu/WeChat/DingTalk messages to local Claude/Codex agents. It runs as a tray controller (default) or a headless bridge daemon (`--service`).

## 0. 参数表

> 本文件是**协作章程**，不是入门教程。项目无关的模板来自 `team-charter` skill；
> 下表把模板里的占位符换成本仓真实值。**与全局规则（`~/.ki/agent/AGENTS.md`、
> `~/.agents/rules/`）冲突时以全局规则为准**，尤其是安全、授权、删除类规则。

| 参数 | 本仓的值 |
| --- | --- |
| 项目名 | **ABB (agent-bridge)** — Rust + Slint 菜单栏应用 |
| 仓库根 | `/Volumes/DataExt/GitHub/abb` |
| 主分支 | `main` |
| 成员 worktree 根 | `/Volumes/DataExt/GitHub/abb-<role>`（**同级目录**） |
| 成员分支前缀 | `feat/` 或 `docs/`（本仓无统一 `team/` 前缀） |
| 构建命令 | `bash tools/gates.sh`（内含 `cargo build --locked`） |
| 检查命令 | `bash tools/gates.sh`（内含 `fmt --check` + `clippy -D warnings`） |
| 测试命令 | `bash tools/gates.sh`（内含 `tools/check_test_isolation.sh`） |
| 集成测试 | 同上门禁；真渠道 e2e 靠 `gates.sh` 传 `ABB_CHANNEL_BIN` 才真跑 |
| 评审方式 | walgit `review` 条目，`abb-reviewer-N`（**必须与作者不同 principal**） |
| 定时推进 | **无**（本仓明确不起常驻 CI runner，见下） |
| 决策辅助 | Jev（`jev_ask`），常设口径见 `docs/decision-defaults.md` |
| 跨团队 tracker | **walgit collab threads**（`refs/collab/*`）；GitHub 只是镜像 |

### 模板与本仓实际不符的三处（照事实写，不照模板写）

1. **worktree 不在 `<REPO_ROOT>/.worktree`**，而在 `/Volumes/DataExt/GitHub/abb-<role>`
   同级目录。理由：构建中间产物必须外置到数据卷（见「Build, Test」），把 worktree
   也放同级更省心；`.worktree` 嵌在检出内会被 `tools/gates.sh` 的「base 不得在
   检出内」判据挡下。
2. **分支前缀没有统一 `team/`**，历史上是 `feat/` 与 `docs/`。
3. **§7「以 Issue 为准」在本仓指 walgit collab thread，不是 GitHub Issues**。
   本仓明令「Do not open new GitHub issues or PRs for normal development」。
   GitHub 只是镜像/发布远端。

## 1. 角色

### Leader（架构师 + 项目负责人）

- 架构设计与技术方案决策、任务拆解与派工、代码审查、验收与合并。
- owner 已明确自己的角色：**只验收，不当决策关口**。可执行含义见
  「Decision Making → owner 已明确自己的角色」一节。
- **原则上不写代码**：代码/测试/配置/脚本变更交给子代理；Leader 做设计、
  审查、验收、合并。人类明确要求时 Leader 可直接改非代码文件（本文档、`docs/`），
  但仍应优先派工。

### 团队成员（子代理）

- 以**子代理**形式工作，每人独立 worktree（`/Volumes/DataExt/GitHub/abb-<role>`）
  与分支（`feat/<role>` 或 `docs/<role>`）。
- **只在自身 worktree 内工作**，不碰主工作区与其他成员工作区。
- **不得再派生下级子代理**（避免任务失控与资源争抢）。
- 交付时必须给出：分支名、commit SHA、改动摘要、**实际跑过的命令与结果**、
  剩余风险/未验证项。只报「已完成」不算交付。

## Project Structure & Module Organization

- `src/` — Rust source; one module per concern (`agent`, `bridge`, `wechat`, `feishu`, `dingtalk`, `schedule`, `ws`, …). Unit tests live inline at the bottom of each file.
- `ui/app.slint` — Slint UI definition, compiled at build time via `build.rs`.
- `app-assets/` — macOS bundle assets (`Info.plist`, `AppIcon.icns`, tray icons).
- `scripts/` — macOS helpers: `build.sh` (bundle + sign + install), `sign.sh` (re-sign), `make-certs.sh` (dev cert).
- `reference/` — protocol references (e.g., `feishu_ws_protocol.py`).
- `crates/abb-agent/` — **随包执行层的源码（待退役）**。**2026-10-23 起分发的 `abb-agent` 二进制是 ki**（pi-rust 构建后改名，钉 SHA 见 `release.yml`；见 `tools/licenses/ki-NOTICE.txt`）：解析链/签名/安装路径零改动，`resolve_agent_command` 随包优先、PATH `ki` 回落，命令形状恒 `<program> acp serve`（旧 abb-agent 忽略 argv，两代兼容）。**供应商接线**：ki 不收参数、也不读旧的 `ANTHROPIC_*` / `OPENAI_COMPAT_*` env（那是旧 abb-agent 的约定）；ABB 把当前供应商落成一份只含它的 `models.json`（`src/ki_config.rs`，目录经 `KI_CODING_AGENT_DIR` 下发，spawn 前在 `runtime/harness.rs` 落盘）。**不要**再给 ki 注入 `ANTHROPIC_API_KEY` / `OPENAI_API_KEY`：实测会让 ki 内置身份先命中、用户选的模型被静默换掉。本目录源码保留待退役（阶段 3，单独批次）；独立包（自带 manifest + lock，root 的 fmt/clippy/test 不碰它）；**GitHub Actions** 里有 `abb-agent-lint`（macOS）与 `abb-agent-windows` 两个 job；命令用 `--manifest-path crates/abb-agent/Cargo.toml`。**granted（授权者）机制已移除**（2026-10-23，`23e2c91`）：准入 owner-only、单执行实例；owner 自定义的 `sandbox_mode` 仍经 `_meta.abbSandbox` 下发，执行层不声明该能力时受限档会话在建会话处 fail-closed 拒建（ki 当前不声明；owner 全权限会话不受影响）。
- `crates/abb-chart/` — **图形生成 CLI**（`abb-chart`）：JSON 图规格 + CSV/内联数据 → PNG，供 agent 用 `"$ABB_BIN" chart render …` 出图、再 `deliver --to-current --file` 发回聊天。同样是独立包（自带 manifest + lock，root 的 fmt/clippy/test 不碰它），**自带 MSRV 1.85**（charton 0.8 是 edition 2024，而 abb-agent 锁 1.78——这是它不放进 abb-agent 当内置工具的原因之一；三条候选路的对比见 `crates/abb-chart/README.md`）。为什么走 `$ABB_BIN` 子命令转发：受限（授权者）会话的白名单只放行 `$ABB_BIN` 的子命令（`src/guard.rs::check_abb_bin`），且 charton 的 raster 依赖不该进托盘进程。**上游 raster 字体路径有 4 个静默缺陷**（CSS 栈族名当单体名 / 未知族名静默回落 / 无逐字回退 / PingFang 吞小数点），本包在应用层用逐字符覆盖校验绕开；上游 issue <https://github.com/wangjiawen2013/charton/issues/126>，仓库内长版归档 `docs/upstream-charton-raster-font-findings.md`。**安全边界（改动前必读）**：规格内容里的 `data` 只能读规格文件**同目录**内的文件（`spec.rs::resolve_in_dir`——闸门看不到文件内容，只能由解析方收口；相对与绝对都受限）；命令行 `--data` 相对限 CWD、绝对放行（命令行在闸门可见范围内）。**GitHub Actions** 里 `abb-chart-lint`（macOS）与 `abb-chart-windows` 两个 job；命令用 `--manifest-path crates/abb-chart/Cargo.toml`。
- `third_party/i-slint-core/` — vendored `i-slint-core` wired via `[patch.crates-io]`. The only local patch is the Windows tray window (message-only → top-level hidden; see `Cargo.toml` comment). Its published tree does not ship everything upstream's repo has — `benches/string.rs` and a font its lib tests `include_bytes!` are missing, so standalone `--all-targets` / `--lib --tests` builds fail for reasons unrelated to the patch; compile coverage of the patch comes from root CI building it as a dependency on windows-latest.
- `src/runtime/**` — 从上游 harness 移植的桥/会话层。移植已完成：当年的自维护 fork（独立包）与它的处置台账已在 0685e66 一并删除；`src/runtime/mod.rs` 头部保留的上游 pin 只是**历史出处标注**（不要据此把 ABB 定制反推为上游行为，也不要重开 rsync 流程）。此处改动按普通代码改动对待——在提交信息与 walgit patch 说明里写清动机与影响面即可，没有额外的台账文件。

Runtime data lives in `~/.agent-bridge/` (the non-empty `AGENT_BRIDGE_HOME` env var overrides the
entire runtime directory; empty means default); per-bot workspaces under
`~/.agent-bridge/workspaces/<bot_key>/`.

## Walgit Collaboration

- **Canonical development remote**: `origin` is the walgit host; `github` is a mirror/release remote only.
  **本机 walgit 服务正在退役**（只能远程访问）⇒ `origin` 迟早要改成远程地址。下面所有默认值都
  **不写死主机**：MCP 端点从 `origin` 推导（`<scheme>://<host>/mcp`），推不出来就 fail-closed
  并要求显式设 `WALGIT_MCP_URL`。所以迁移时**只需改 `origin`**，其余自动跟随。
- **All work tracking lives in walgit**: create/update issues, patches, reviews,
  merge results and status transitions with collab entries under
  `refs/collab/*`. Do not open new GitHub issues or PRs for normal development.
- **协作面 = 纯 git + MCP（D68）**。walgit **v0.13.0** 起 `walgit collab` / `walgit ci` /
  `walgit principal` 本地子命令**默认拒执行**（`hide = true`，报错直接指路到对应 MCP 工具），
  `/{o}/{r}/api/…` 车道也全 404（实测；只剩 `/api-browser`、`/api/v1`、`/mcp`）。
  **协作的读写全部走 MCP**，工具面实测共 23 个，协作相关就这六个：
  `collab_board` / `collab_thread` / `collab_report` / `collab_publish` / `collab_register` / `ci_status`
  （+ `repo_*` 与 `wal_ls`）。claim / status / patch / review / merge_result **都不是独立工具**，
  而是发一条对应 `kind` 的条目走 `collab_publish`。
- **签名在客户端，不是「MCP 做不到」**。`collab_publish` 从不代签（hold 不到 key），条目得先签好；
  要签的字节是 `entry_canonical`：**把 `sig` 的值置成空串（键必须保留，别删掉这个键）**，
  再把对象键按字节排序、去掉全部空白。**删掉 `sig` 键会签出 `verified: false` 的条目**——
  实测过：拿真账本里 399 条已验证的条目，用 `"sig":""` 规则逐条验签全部通过，用「去键」规则全部失败。
  规则就这一句，**任何 ed25519 实现都行**，不必是 walgit CLI
  （`openssl pkeyutl -sign -rawin` + 32 字节 seed → PKCS8 DER 即可）。
  密钥永不上行是**安全边界**，不是能力缺口。
- **本地二进制正在退场（重要）**：主机会部署到服务器，**本地不再有 `walgit` 命令**。
  所以本仓的脚本与提示词一律**不把本地二进制当唯一路径**。现状：
  `tools/check_review_cadence.sh` 的数据源顺序是 **MCP → CLI（过渡回落）→ fail-closed**，
  MCP 走 `tools/review-cadence/mcp_thread.py`（凭据 `WALGIT_MCP_TOKEN` / `WALGIT_MCP_URL`）；
  `tools/check_collab_parents.sh` 读的是**纯 git** 本地 refs（`for-each-ref` + `cat-file`），
  因此它自己也**默认先 `git fetch refs/collab/*`**、同步失败 fail-closed
  （`--no-fetch` 可显式降级）——「纯 git」不等于「天然无陈旧问题」；
  `src/agent.rs` 的提示词改为「构造 JSON + 客户端签名（openssl 即可，canonical = `sig` 置空串
  后键排序无空白）+ MCP `collab_publish`」，并**反向断言**不许再把 CLI 写成唯一命令形态。
- **CLI 只剩本地特权动作**（不是协作，也不是长期依赖）：`collab gc`（inbox fold）、repo ops
  （fsck/gc/repair/compact/rev-index/checkpoint）、`ci run`（runner 本体）、key 生成。
  本机还有二进制时这些仍可用，需显式 **`WALGIT_LOCAL_COMMANDS=1`**。
  **注意参数校验在门控之前**：`walgit collab thread`（缺 id）报的是 clap 的参数错，不是门被开。
- **陈旧即假绿：MCP 发布不写本地 refs**。`collab_publish` 只落在**服务端** `refs/collab/inbox/*`，
  本地检出不会自动更新（实证：MCP 发一条 comment 后本地 `collab thread` 只见 4 条、服务端 5 条；
  `git fetch origin 'refs/collab/inbox/*:refs/collab/inbox/*'` 后变 5）。而本仓合并门禁读**本地**
  refs ⇒ 评审用 MCP 发 `request_changes`、协调者本地跑门禁会**看不到它而判绿**。两道防线：
  `check_review_cadence.sh` 默认先同步、同步不成 **fail-closed**（`--no-fetch` 才降级为
  best-effort）；人手核对时也先 fetch。同步用 `git fetch` 即可，**不需要 walgit CLI**。
- **The host standard is normative**: agents collaborate by this walgit host's `SKILL.md` (`<host>/SKILL.md`, §0–§7). Where this file and that standard disagree, **the standard wins**; this section is a pointer, not a second source of truth.
- **Identity first (§0/§0a)**: one principal per collaborator, one Ed25519 key, materialized **per worktree** by
  generating the seed with any ed25519 tool (`openssl genpkey -algorithm ed25519`) and registering the
  public half with MCP `collab_register` (the seed lands in that worktree's git dir;
  never share a key, never sign an entry for someone else).
  Where a `walgit` binary happens to exist, `WALGIT_LOCAL_COMMANDS=1 walgit collab join --principal <proj>-<role>-N --push origin`
  is the one-shot equivalent — but it is a convenience, never the required path (the host moves to a
  server; the local binary goes away). This project's convention is
  `abb-worker-N` (author) / `abb-reviewer-N` (acceptance) / `abb-coordinator` (merge & close) — take the
  next free index after the highest registered one. Do the §0a routine before filing or claiming: fetch
  `refs/collab/inbox/*` + `refs/collab/meta/*`, read `WALGIT_LOCAL_COMMANDS=1 walgit collab board`, then claim.
- **Entry contract**: `issue` opens a thread (objective, owner, **machine-checkable acceptance**);
  `comment` + `status: in-progress` records the claim (`owner` / `worktree` / `branch` / `work`);
  `status` values are `in-progress` / `needs-review` / `blocked` / `needs-human` / `done` / `closed`
  (`needs-human` only for what genuinely needs the human); `patch` uses `--base refs/heads/main
  --head refs/heads/<branch>`; `review` uses `decision` (**`approve`** / **`request_changes`**),
  `agent` and `note` (full findings: location / problem / suggestion / evidence). Every entry's
  `--parent` is the previous entry's **real oid**. Reference other threads with
  `--related <oid>` / `--depends-on <oid>`.
- **Independent review is structural (§0b/§4)**: the reviewer must be a **different principal** from
  the patch author (and, for the final review, from the merger). An author's own `approve` never
  satisfies the gate; several roles signing with one shared key destroys the audit trail.
- **Merge & close (§5)**: the coordinator merges locally (**fast-forward preferred**), pushes to
  `origin`, records **one** `merge_result {"merged":true,"oid":"<sha>","result":"merged"}` (that entry
  moves the card to `merged`), then `status: closed`; afterwards remove the card's worktree and branch
  and verify with `git worktree list`. **Before writing `merge_result`, run
  `tools/check_review_cadence.sh <thread-id>` — red means do not merge** (契约行 / blocking 轮数 /
  终审一致性 / 未验签 / **零评审合并** 判据 (a)–(g)，见 `docs/review-findings-contract.md`；
  样本自测在 `.walgit/ci.toml` 里以 `review-cadence-selftest` 执行)。两个时间阈值可用
  `--contract-ts <epoch>` / `--no-review-ts <epoch>`（或 env `ABB_REVIEW_CONTRACT_TS` /
  `ABB_NO_REVIEW_TS`）覆盖，缺省取脚本内常量——**判据 (g) 只对 `ts >= no_review_ts` 的每一条
  `merge_result{merged:true}` 生效**，所以往新规前回溯 `ts` 不能绕过它。零评审合并的降档只认
  结构化字段：由**独立第三方**（不是该 thread 任何 patch 的作者、也不是该次合并的执行者）
  签一条 `comment`，其 `body.waiver` 等于字符串 `"no-independent-review"`，且该 comment 的
  `ts` 在 `no_review_ts` 与该次合并之间、`verified` 不为 false；**正文里提到这句话不算数**。
- **Board/CI declarations**: `.walgit/board.toml` and `.walgit/ci.toml` are part of
  the tested tree. Move cards only by appending a signed `status` entry; never edit
  the board to represent a state change.
- **MCP lane (自动协作，本机 ki 会话)**: ki 的 `mcpServers` 读 `<cwd>/.ki/settings.json`
  （本机配置，含绝对路径与 token，已 gitignore）。本项目接入的是**服务端 `/mcp`**（HTTP）：
  `{"url": "<host>/mcp", "headers": {"Authorization": "Bearer wgt_…"}}`
  （本机服务退役后就把 url 换成远程；门禁脚本自己会从 `origin` 推导，不依赖这条配置）。
  ⚠️ **`walgit mcp` 子命令已不存在**（D68 后 `walgit mcp --allow-write --key …` 是失效指令，
  `walgit mcp --help` 直接 `unrecognized subcommand`）——不要再照旧文档启 stdio server。
  `/api` 车道也已全 404（只剩 `/api-browser`、`/api/v1`、`/mcp`）。
  **控制台只订 `walgit://collab/board/<owner>/<repo>` 一条**，`signals.wake=auto` + `minIntervalSeconds`
  限流。每条通知只带 URI，读到的 `_meta.version` 就是游标：**变了才处理，自己写下的那条按 head
  entry 的 `actor` 过滤掉**；重启后先读一次（订阅是进程内轮询、best-effort，不能把重启读成
  「什么都没发生」）。写走 MCP 的 **`collab_publish`**（`actor` 必须是认证调用者；工具**不代签**，
  条目得先在客户端签好），claim 不是独立工具而是发一条 `kind: card_claim` 条目；
  公钥注册/轮换用 `collab_register`。重活（构建、测试、push）仍在工具调用之外跑，收尾只写一条 entry。
  （曾误写作 `collab_entry` / `collab_claim`——实测 `tools/list` 无此二工具。）
  **看板内容不要走 `read_resource`**：本项目 board 约 580 KB，而 ki 的读工具硬截断 20 KiB
  （拿到的是断掉的 JSON 残片）；唤醒后用 CLI 取全文 —— `WALGIT_LOCAL_COMMANDS=1 walgit collab board` / `… --format json`
  （或 MCP `collab_board` 工具本身，它不走 20 KiB 截断）。
  同理 `walgit://refs/<owner>/<repo>` **不要订**：实测该仓库 4514 条 ref（`inbox` 3625、
  `ci-artifacts` 516、tag 166、`refs/heads` 1），摘要按字典序预算 464 条**全部**是
  `refs/collab/ci-artifacts/*`，分支/inbox 一条不回；而它的 `_meta.version` 是全量 ref 的流式
  hash，等于每次 CI 产物落地都唤醒一次、却拿不到可读信息。worker 要盯自己的卡就订
  `walgit://collab/thread/<owner>/<repo>/<thread-id>`（version = head oid，payload 小）。
- **MCP 不改角色拓扑**：看板唤醒的是**控制台/orchestrator**，`--allow-write` 的 key 是
  `abb-coordinator`；实现仍派给各 worktree 自己的 `abb-worker-N`（各自 `collab join` 生成 key），
  验收仍是与作者不同的 `abb-reviewer-N`。一卡一 owner，改同一文件的卡串行。
- **Build intermediates go to `/Volumes/DataExt/tmp`** (user policy, 2026-09-17): the
  228 GiB boot volume must not hold build output. `export TMPDIR=/Volumes/DataExt/tmp`
  for anything that makes temp trees, and put `CARGO_TARGET_DIR` on the data volume too.
  Falling back to the default `target/` or `$TMPDIR` fills the boot volume and manifests
  as an `errno=28 (No space left on device)` link failure — not as a code failure.
- **`CARGO_TARGET_DIR` must be per-worktree**, not the shared `abb-target`
  (card `abb-target-dir-per-worktree-20261022`). `tools/gates.sh` now derives it as
  `$base/abb-target-$(basename <worktree>)`; if you set it by hand, do the same. A shared
  directory makes concurrent worktrees starve each other on cargo's file lock — observed:
  a single `cargo build` stuck 25+ min, load 12.4, and `check_test_isolation.sh` timing
  out repeatedly (the tests themselves take 15 s) while *looking like test failures*.
  `ABB_TARGET_SHARED=1 tools/gates.sh` opts back into the shared dir for single-worktree use.
- **CI**: merge gate = the local gate set below (fmt / clippy / build / `tools/check_test_isolation.sh`)
  + independent review — `merge_rule_eval` on this repo requires no CI run (`base is not protected`).
  This repo keeps **no resident local walgit CI runner**: do not start `tools/run_ci_runner.sh`. Historical
  runs do exist in the collab log (the last one is from 2026-10-09, produced by the `ci-runner` principal),
  so `WALGIT_LOCAL_COMMANDS=1 walgit ci status` may answer with old runs — treat them as **history, not verification of your head**.
  GitHub Actions covers `ci.yml` at PR/mirror level (six jobs) and `release.yml` at tag/release time
  (`anti-spam.yml` / `release-on-acceptance.yml` cover their own narrow paths); the full
  build/sign/notarize path lives in `release.yml`.
  `ci.toml` 的任务**假定 POSIX shell**：每条 `command` 都套了 `sh -c '...'`，目的是把
  "Windows runner 经 `cmd /C` 执行 ⇒ 记 exit 0 且日志为空（**签假绿**）"变成"**响亮的红**"
  （cmd 不认 POSIX 语义 ⇒ `sh` 不存在、**或**收到带单引号的畸形 argv，两条路径都必然非零退出）。
  因此**不要把 Windows 主机指到这些任务上**——Windows 的门禁覆盖走 GitHub Actions `ci.yml` 的
  windows job。背景与实测见卡 `abb-ci-coverage-win-20261007`；**"cmd 侧会响亮失败"这一句是机制推断，
  未在 Windows 实机验证**（本仓当前无 Windows 主机；将来若真启用 Windows runner 须先实跑确认）。
- **Mirror discipline**: push normal heads/tags to `origin` only. The local
  walgit-to-GitHub mirror syncs `refs/heads/*` and `refs/tags/*`; GitHub Actions is
  used for mirror/release artifacts, not day-to-day collaboration.
  **`release.yml` 跨仓库取 pi-rust 源码时，钉的 SHA 必须是完整 40 位**（`actions/checkout`
  把 `ref` 当分支/标签名 fetch，短 SHA 会直接红在 checkout），且**该 SHA 必须已推到 GitHub
  镜像**——只存在于 walgit 的提交在 CI 里不存在（2026-10-23 首次真跑两个 job 同时踩到）。

## Decision Making（该做哪个选择，问 Jev）

**需要做取舍时，先问 Jev，不要回头问人。** 本机 ki 会话装了 `jev` MCP server
（`~/.agents/skills/jev-decision/scripts/jev_mcp.py`，配置在 `.ki/settings.json`），
暴露 `jev_choice` / `jev_score` / `jev_noul` / `jev_ask` / `jev_health`。

典型该问的场景：合不合 / 先做 A 还是 B / 这个风险大不大 / 优先级排序 / 两种修法选哪个。
一次调用可以混问多个问题（`jev_ask`）——输入只计一次 token，比分开问省。

### 更前置的一条：**技术判断自己拍，别拿它去问人**

**owner 明确要求（2026-11-01）**：「事事都要来问我，太烦人了」。注意这不是让你把问题
转手给 Jev——**该你自己判的，连 Jev 都不用问**。

先分清楚手上是什么：

| 类型 | 谁定 | 例 |
|---|---|---|
| **纯技术判断** | **你自己定，直接做** | 两张卡的改动会不会冲突、样本放哪个目录、变量命名、先跑哪个测试、日志写多详细 |
| **有真实取舍的架构选择** | 问 Jev | 两种修法各有代价、要不要引新依赖、该不该改既有判据语义 |
| **下列三类之外的一律不问** | 你自己定 | — |

判据：**如果答案能从代码、文档、实验或常识推出来，就自己推，别问任何人。**
只有「信息不在你手上」时才开口。

### 触发清单（出现任一项 ⇒ **必须**问 Jev，不再自己归类）

上面那张表要靠「我判断这属于哪类」——而 2026-11-02 的实证是：**我把自己替产品做的决定
归类成技术判断，于是整轮一次都没问。** 所以换成不用归类的硬触发：

| # | 触发 | 为什么它必然不是技术判断 |
|---|---|---|
| T1 | 要**改默认值 / 阈值 / 上限** | 值本身推不出来，只能选 |
| T2 | 要**改用户可见的东西**：文案、导出范围、分享内容、提示措辞 | 「用户看到会怎么想」不在代码里 |
| T3 | **两个方案都能跑通**，差别只在代价/体验 | 能跑通 ⇒ 代码推不出该选哪个 |
| T4 | 我正在写「这是**产品**决定」「按惯例」「一般就这么做」 | 写这几个字本身就是信号 |
| T5 | 要**改既有判据的语义**（不是补用例、不是修 bug） | 改的是规则本身，影响面超出本卡 |

**问的时候一次问完**（`jev_ask` 混问，输入只计一次 token），不要一个问题一次调用。

### 常设口径（问过一次就别再问）

已由 Jev 裁决过的同类问题记在 `docs/decision-defaults.md`；下次遇到**先查它**，
命中就照办、不重复问。新增口径随卡追加一行（含 `request_id`，可回溯）。

### 漏问怎么发现（不靠人守）

`patch` 条目的 body 带 `decisions` 字段：

```json
"decisions": {
  "asked":        [{"rid": "gen-dec-…", "q": "…", "choice": "…", "p": 0.93, "conf": 0.89}],
  "self_decided": [{"what": "渲染放子进程", "why": "Slint 平台每线程一次，代码可推"}]
}
```

`tools/check_review_cadence.sh` 对它判三件事（见 `docs/decision-defaults.md` 的判据节）。
`asked` 为空且 `self_decided` 非空**不判红**（那可能真的是纯技术活）——但**人一眼能看出
「这轮拍了 N 个、问了 0 个」**，异常可见，而不是烂在对话里。

失败案例（不要重犯）：2026-11-01，协调者为「判据 (e) 的洞并卡还是另开卡」去问 owner，
而那是纯粹的文件冲突判断（两张卡改 `check.jq` 同一段 → 必然冲突）——本可自己决定。

**调用顺序**：决策失败时**先跑 `jev_health`**（不发模型请求），它能把「网络/钥匙坏了」
和「模型拒了」分开；本机走代理，节点不可用时表现为 `UNREACHABLE` 而不是模型报错。

**如实转述**：Jev 是概率决策器，返回 `choice` + 全部选项的概率分布 + `confidence`。
转述时必须带上这三个数，**不要**把它的输出当确定性事实；`confidence` 偏低时（如 < 0.6）
要说明「它自己也不确定」，不要替它包装成强结论。

**什么时候仍然找人**（这几类 Jev 替代不了）：

- 需要**授权**的动作：删文件/目录/分支、改外部系统、向他人发消息
- 涉及**人的偏好或现实条件**：产品方向、真机验证、只有人能看到的现场
- Jev **不可用**（`jev_health` 报 `UNREACHABLE` / key 缺失），网络恢复后补问

**除这三类之外，不要问 owner。** 尤其不要把「技术判断」包装成请示——那只是把该做的
工作量转嫁给用户。

### owner 已明确自己的角色：只验收，不当决策关口（2026-11-02）

> 原话：「你是 leader 你决定，我只负责验收，这点记住，给你 mcp 就是需要你无人值守开发，直至达成目标」

可执行的含义：

- **我是 leader**：方案取舍、优先级、拆卡/并卡、要不要重构、用什么手法、先做哪一步，**全部自己定**，
  定了就做，不回报「我准备怎么做」等批准。
- **owner 只验收**：在**交付**时把结论与证据摆给他（做了什么、凭什么说对、残留是什么）。
  验收前不要拿半成品去问。
- **无人值守推进到目标达成**：遇到阻塞先自己解（查代码/文档/实验，必要时问 Jev），
  不要因为「拿不准」就停下来等人。停下来只有两个正当理由：需要**授权**、或需要**人的现场**。
- **保持可见**：无人值守不等于安静。多步工作用 `todo` 跟踪，关键结论写进卡片/注释，
  合并前跑完门禁并留证据。

## 2. 交付流程与验收

1. Leader 在 `main` 上维护可构建、可测试的基线。
2. 拆卡并创建 worktree/分支派工；**任务边界必须互不重叠**（改同一文件的卡串行）。
3. 成员在自身 worktree 内实现、自测、提交。
4. Leader 审查 diff 与设计，逐项核对「3. 验收标准」。
5. 独立评审（**不同 principal**）→ `tools/check_review_cadence.sh <thread-id>` 绿 →
   才允许写 `merge_result`。
6. 通过后合并（**优先 fast-forward**）；未通过则附具体失败信息退回返工。

## 3. 验收标准（合并任何分支前，Leader 至少执行）

```sh
bash tools/gates.sh          # 40 项，含 root 四条 + abb-channel 四条 + abb-chart 四条 + 全部守卫
```

外加下列检查（本仓特有，缺一即视为未验收）：

- **测试必须真的咬住**：功能能用 + 测试通过**都不够**，要做变异验证——
  把被守护的行为改坏，断言必须红。`docs/false-green-checklist.md` 列了 9 种假绿形态。
- **判据本身要验「它红得起来吗」**：一条断言如果在沙箱里恒真（例如 `ps` 被禁、
  `output()` 的 `Err` 被 `unwrap_or_default()` 吞成空串），它就是假绿。
- **测试不得依赖全局计数**：并行用例共享全局状态 ⇒ 单跑绿、全跑红。
- 不破坏对外契约（MCP 渠道工具集 18 个、`config.json` 字段、CLI 退出码）；
- 新增配置项同步更新 `docs/mcp-bus-architecture.md` 等文档与示例；
- 日志只写 stderr，**stdout 只有 JSON-RPC**（渠道进程的铁律）；
- UI 改动附改动前后截图；出图类改动必须**亲眼看**产出的图。

## 4. 推进节奏

- 本仓**没有常驻推进任务**，也不要启动 `tools/run_ci_runner.sh`。
  推进靠会话内的 `todo` + walgit 看板，不靠 cron 轮询。
- `cron.json` / 定时任务只在当前会话内有效；跨会话的长期状态**只认看板**，
  `todo` 只装「这一轮要做的几步」，做完即清、跨轮重建。
  （教训：有三个早已完成的事项在 todo 里挂了很久，因为会话压缩丢了勾；
  看板才是权威。）

## Build, Test, and Development Commands

Every cargo command below **must** carry the build-intermediate env prefix (user policy:
build output never lands on the 228 GiB boot volume; on Windows the same rule applies to the
checkout — there `/Volumes` does not exist and intermediates were landing in `target/`):

```sh
# macOS
export TMPDIR=/Volumes/DataExt/tmp
export CARGO_TARGET_DIR=/Volumes/DataExt/tmp/abb-target-$(basename "$PWD")   # 每 worktree 一个

# Windows（Git Bash；E: 是数据卷）
export TMPDIR=/e/tmp
export CARGO_TARGET_DIR=/e/tmp/abb-target-$(basename "$PWD")   # 每 worktree 一个
```

`tools/gates.sh` sets these two for the current platform and then runs the gates below **in two
groups**: the root package's four, plus the same four with `--manifest-path crates/abb-chart`
(that package has its own manifest + lock + MSRV 1.85 and no `[workspace]` covers it, so the root
commands cannot see it; its target dir is `…-chart` so cargo's file lock never shares with root);
it **refuses a base inside the checkout** (`ABB_BUILD_TMP` overrides the platform default,
`--print-env` shows what it resolved, `ABB_GATES_OS=darwin` reproduces the macOS values on any
host). Prefer it over typing the exports by hand — that is what keeps the policy machine-checkable:

```sh
bash tools/gates.sh            # 八条门禁（root 四条 + crates/abb-chart 四条），中间产物外置
bash tools/gates.sh --print-env
```

- `cargo build` — debug build.
- `cargo run` — run the tray app; `cargo run -- --service` runs the headless daemon.
- `cargo test` — run all unit tests (**do not** run this directly on a live machine: it
  writes the real `~/.agent-bridge`; use `tools/check_test_isolation.sh` instead).
- `cargo clippy --all-targets -- -D warnings` — lint（**macOS 与 Windows 两侧都可作门禁**：
  平台门控死代码已按调用点 cfg 对称清掉，见 card `abb-macos-clippy-gate-20261011`；Windows 侧由
  `ci.yml` 的 windows job 覆盖）。
- `cargo fmt --check` — verify formatting.
- `tools/check_test_isolation.sh` — the full-test gate (isolated HOME; never substitute
  a bare `cargo test`).
- `scripts/build.sh` — build the release macOS bundle into `~/Applications/ABB.app`.
- `scripts/sign.sh` — re-sign with the `agent-bridge-dev` certificate so TCC privacy grants survive rebuilds.

With the two exports above in the shell（或直接 `bash tools/gates.sh`）, the root group's four gate commands are verbatim
(`tools/gates.sh` then reruns the same set against `crates/abb-chart` via `--manifest-path`):

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo build --locked
tools/check_test_isolation.sh
```

## Coding Style & Naming Conventions

- Rust: `snake_case` identifiers, `CamelCase` types/enums; follow `rustfmt` (4-space indent) and keep `cargo clippy` clean.
- Use `//!` module docs and `///` doc comments; explain *why* in comments. Existing comments are often in Chinese — match the language of the file you edit.
- String handling must be UTF-8/char aware (e.g., `agent::truncate` truncates by chars, not bytes).
- Keep `.slint` changes in `ui/app.slint`, consistent with existing component naming.
- 面向用户的文案遵循 [`docs/wording-and-naming.md`](docs/wording-and-naming.md)：ABB 里那个 AI 助理的称呼是「怹」。

## Testing Guidelines

- Framework: built-in Rust unit tests in `#[cfg(test)]` modules at the end of each `src/*.rs`; the root package also has one Rust integration test — `tests/cli_botkey_callsites.rs` — alongside Python mock helpers (e.g. `tests/mock_acp_agent.py`). `crates/abb-agent` carries its own Rust tests (`cargo test --manifest-path crates/abb-agent/Cargo.toml`) plus Python probes under `crates/abb-agent/probes/`; it has no `tests/` directory. `crates/abb-chart` also carries its own Rust tests (`cargo test --manifest-path crates/abb-chart/Cargo.toml`): unit tests per `src/*.rs` plus one integration test (`tests/cli_e2e.rs`, drives the real binary and covers the exit-code contract 0/1/2) and a golden fixture under `tests/golden/`.
- Name tests with `snake_case`, behavior-focused names (e.g., `codex_single_message_no_progress`, `strip_user_mentions`).
- Add tests alongside the code you change and run `cargo test` before pushing.

## Commit & Pull Request Guidelines

- History is short; use imperative, concise subjects, optionally prefixed with the affected area (e.g., `feishu: …`).
- Keep commits focused and explain *why* in the body.
- Walgit patch/PR entries: describe what and why, link the issue thread, and run
  `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`,
  `cargo build --locked`, and `tools/check_test_isolation.sh` **with
  `TMPDIR=/Volumes/DataExt/tmp CARGO_TARGET_DIR=/Volumes/DataExt/tmp/abb-target-$(basename "$PWD")`
  exported** (see Build/Test above). The isolated test runner is the full-test gate;
  do not substitute a bare `cargo test` because it can write the real
  `~/.agent-bridge`. Include before/after screenshots for UI
  changes. GitHub PRs are only for mirror/release maintenance.

## Security & Configuration

- **macOS entitlements 是分发的硬前提**（#251）：`app-assets/abb.entitlements` 必须随签名带上
  （bundle 与内部可执行都要），否则 hardened runtime 下相机/麦克风/自动化会被**静默拒绝**
  （连授权弹窗都没有）。`scripts/` 被 `.gitignore` 排除、不在版本控制内，所以本机
  **本机公证统一走仓库内入口 `tools/notarize_app.sh <App.app>`**（`tools/notarize_app.sh --self-test`
  可自测参数转发）：它强制把 `app-assets/abb.entitlements` 传给 `~/scripts/notarize.sh`
  —— 后者用 `--deep --force` 重签，不传就把授权签没 —— 并在重签后跑
  `tools/check_entitlements.sh` 断言最终产物。CI 的 release.yml 同样已接入该守卫。
- `config.json` (contains App Secret) and `*.secret` are gitignored — never commit credentials.
- **`bots[].mcp_servers` 是授权面**：声明一个 MCP server = 授权它以 agent 身份运行——它不受会话档位沙箱约束，
  继承 agent 的白名单 env（含 `$ABB_BIN` = 桥本体绝对路径，agent 用它调 `task`/`job`/`deliver`）并以会话工作区为 cwd，
  因此等于给它在工作区的执行权 + 驱动 CLI 的能力；仍受 Q8（`--proc` 拒）与 `cross_delivery_enabled` 约束。只声明可信 server。
- **`$ABB_BIN` 由 ABB 注入 agent 进程**（`service.rs::abb_bin_env`，两条 spawn 路径都带），并由
  `crates/abb-agent` 的 `child_env::PASSTHROUGH_ENV` 透传给 shell 工具与 MCP 子进程；断言见
  `chat_handles_extra_env_carries_abb_bin`、`public_oneshot_wrapper_*` 与探针 `probes/abb_bin_reaches_agent.py`。
- Don't commit `logs/`, `target/`, or generated `.app` bundles.
- Before touching signing, read `scripts/sign.sh`: usage-description entitlements on the bare binary can cause a startup `SIGKILL`.

## 5. 工程纪律（本仓补充，模板 §8 里本仓还没覆盖的部分）

- **小步提交**：一个提交只做一件事；提交信息写「为什么」，不只是「改了什么」。
  每个提交都必须**独立可构建**——不要交出「后面那个提交才补全」的中间态。
  自查方法：`git worktree add` 到临时目录跑一遍门禁，确认干净检出也是绿的。
  （教训：一次提交漏了未暂存文件，`gates.sh` 还指旧路径，那个 commit 本身是破的。）
- **主分支保护**：显著变更走「卡 + patch + 独立评审 + `merge_result`」。
  **已知偏离**：纯文档/记账类小改与个别工程改动曾直接提交到 `main`
  （如 `06fa240` 渠道合并），未开卡。这不构成惯例——真要再这么做，
  至少在提交信息里写明「为何不走卡」。长期应收紧到全部走卡。
- **契约与兼容**：MCP 渠道工具集、`config.json` 字段、CLI 退出码约定属对外契约；
  变更必须同步文档、示例与迁移说明；破坏性变更先征得人类同意。
- **缺陷处理**：先复现 → 写失败测试 → 修复 → 全量验证 → 记录根因。
  **不得用重试、忽略或放宽断言掩盖可复现缺陷**，也不得删/跳过测试让门禁变绿。
- **依赖与体积**：默认最小依赖；新增依赖需说明理由并固定版本。
  **合并二进制以摊薄固定成本**是既定策略（三渠道合一省 55%），
  由 `tools/check_channel_layout.sh` 盯住回退。
- **技术债**：临时方案必须关联卡并写明移除条件，不允许无记录的「临时」长期存在。

## 6. 派工模板（Leader 复制使用）

```markdown
你是本项目团队成员。仓库：/Volumes/DataExt/GitHub/abb（Rust，MSRV 见各 crate）。
工作区/分支（已建好）：/Volumes/DataExt/GitHub/abb-<role>，分支 feat/<role>。
只在该 worktree 内改动；不要 push、不要合并、不要派生下级代理。

## 任务
<背景、目标、边界、明确不做的事>

## 环境（必须照做）
export TMPDIR=/Volumes/DataExt/tmp
export CARGO_TARGET_DIR=/Volumes/DataExt/tmp/abb-target-abb-<role>   # 每 worktree 一个

**绝对不要跑 `bash tools/gates.sh`、也不要裸跑 `cargo test`**：
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

## 7. Definition of Done

一个分支可以合并，当且仅当：

- [ ] 目标行为已实现，边界外未扩散
- [ ] `bash tools/gates.sh` 全绿（40 项），且在**干净检出**里复跑过
- [ ] 新增/修改行为有测试，缺陷有回归测试，**且做过变异验证**
- [ ] 文档、示例、`docs/architecture.md` 行数台账同步（`tools/arch-check.sh`）
- [ ] 无硬编码密钥、无调试输出污染 stdout / 协议通道
- [ ] 独立评审完成（**不同 principal**），P0/P1 已处置或有不合并的明确结论
- [ ] `tools/check_review_cadence.sh <thread-id>` 绿之后才写 `merge_result`
- [ ] 提交信息清晰、范围单一、独立可构建
