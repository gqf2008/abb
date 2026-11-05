# Walgit 协作细则

> 本文是 `AGENTS.md` §0「跨团队 tracker」条目的展开。**协作流程以本文为准**；
> 章程只规定「谁是 Leader、怎么派工、怎么验收」，不重复这里的 git/MCP 细节。


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
- **镜像纪律（2026-11-05 更正：此前把它写成「自动同步」是不实陈述）**：
  日常 push 只推 `origin`（walgit），GitHub 用于镜像与发布产物，不用于日常协作。
  **但镜像不会自己更新。**
  - 2026-11-05 实测：GitHub 的 `main` 停在 `005bff0`（= v3.2.9，2026-10-30），
    本地 `main` 已领先 **152 个提交 / 6 天**；tag 也没同步（`v3.3.0` 只存在于 walgit）。
    仓库内没有任何脚本、cron 或 LaunchAgent 负责这条链路（`~/.walgit/walgit.toml`
    无 `[upstream]`/mirror 段，`crontab -l` 与 `~/Library/LaunchAgents` 均无相关项）。
  - 所以**发布必须显式推镜像**，否则 `release.yml` 不会被触发（它监听的是
    GitHub 上的 `push: tags: v*`）——tag 打在 walgit 上等于没发版。
  - **本机可用凭证是 `aerodesk-io`，对 `gqf2008/abb` 无写权限**（403），SSH 也不通
    （`Permission denied (publickey)`）⇒ 推镜像这一步**当前只能由 owner 执行**：

    ```sh
    git push github main v3.3.0          # 需要 gqf2008 身份的凭证
    # 或：git push https://<token>@github.com/gqf2008/abb.git main v3.3.0
    ```

  - **落后多少，用命令问，别靠印象**：

    ```sh
    git fetch github
    git log --oneline github/main..main | wc -l    # 0 = 已同步
    git ls-remote --tags github | grep "$(git describe --tags --abbrev=0)" || echo "❌ tag 未镜像"
    ```

  - **后果不只是「镜像旧」**：GitHub Actions 的 `ci.yml` 也只在镜像上跑，
    镜像落后多久，那套 windows/ubuntu 覆盖就空窗多久。别把「CI 绿」当成对
    walgit 最新提交的验证。
  **`release.yml` 跨仓库取 pi-rust 源码时，钉的 SHA 必须是完整 40 位**（`actions/checkout`
  把 `ref` 当分支/标签名 fetch，短 SHA 会直接红在 checkout），且**该 SHA 必须已推到 GitHub
  镜像**——只存在于 walgit 的提交在 CI 里不存在（2026-10-23 首次真跑两个 job 同时踩到）。
