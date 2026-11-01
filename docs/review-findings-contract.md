# review 严重度契约行（审查收敛门禁）

> 卡：`abb-review-cadence-gate-20261013`。把 `~/.agents/rules/RULE_审查规范.md` 第 7–12 条
> （严重度分档 / 一次一批一审 / 复审只覆盖上轮 blocking / 同一 blocking 第 2 轮未决即升级架构）
> 从人核变成**机器可查**。规则正文不改，本文件只定义可执行契约。

## 契约行

每一条**判决类** `review` 条目（`decision` 为 `approve` 或 `request_changes`）的
`note` **第一个非空行**必须是：

```
findings: blocking=<N> non-blocking=<M>
```

- `blocking`：阻断合并、必须本轮解决的 finding 数（Critical / Important 级）。
- `non-blocking`：不阻断合并、转 follow-up 的 finding 数（Minor 级）。
- 行内两处都是十进制整数，`blocking=` 与 `non-blocking=` 之间至少一个空白。

其余正文照旧写在契约行之后（人读）。

### 可选：逐条标签

如果想逐条标注，用 `- [blocking] …` / `- [non-blocking] …`。**用了就必须自洽**：
标签计数必须与契约行数字相等，否则判红。

### 正确样例

```
findings: blocking=1 non-blocking=2

- [blocking] src/ui.rs:42 断绑后不再更新（B1）
- [non-blocking] 注释里的行号已漂移
- [non-blocking] 变量名 a→authenticated

证据：`bash tools/gates.sh` 全绿；断绑复现脚本见 thread 第 3 条。
```

## 判据（`tools/check_review_cadence.sh` 实现）

守卫本体是 `tools/review-cadence/check.jq`。**下文一律按 jq 程序里的字母 (a)–(g) 引用**，
两者一一对应，以 jq 字母为准。

> 早期版本的本节用 `1./2./3./4.` 编号且只覆盖了四条，于是产生过指向错位的引用
> （如「会被判据 2（自审）拦下」——文档 2 其实是 blocking 轮数，自审是 **(d)**）。
> 新增的交叉引用请直接写字母，不要再造一套编号。

| 字母 | 判据 |
|---|---|
| (a) | **契约与 decision 一致**：`blocking>0 ⇒ request_changes`；`blocking=0 ⇒ approve`；若另有逐条标签，标签计数须等于契约行数字。只看**最后一条**判决类 review；更早的不一致只计数上报（`contract_mismatches_historical`）。 |
| (b) | **blocking 轮数 ≤ 2**：同一 thread 里契约行 `blocking>0` 的 review 条数不超过 2（第 2 轮仍未决 ⇒ 升级为架构问题）。 |
| (c) | **可合并**：thread 内最后一条（`ts` ≥ 契约阈值）判决类 review 必须是 `blocking=0` 的 `approve`；否则判红。在**写 `merge_result` 之前**就能跑——这正是「不得带未决 blocking 合并」的机器判据。 |
| (d) | **独立评审（§0b）**：最后一条判决类 review 的 actor 不得是它所审那份 `patch` 的作者。自审就算签出完全合法的 approve 也不算独立评审。 |
| (e) | **终审不得与合并者同人（§4）**：已合并的 thread，最后一条判决类 review 的 actor 与**每一条** `merge_result` 的 actor 都必须不同（逐条判，理由同 (g) 的「不得只挑一条」）。同一个人**多轮评审**是合法的，不在此拦。历史豁免口径与 (a) 同：只看 `ts` ≥ 契约阈值的 review。 |
| (f) | **未验签不得参与合并**：`verified=false` 的判决性条目（`patch` / `review` / `merge_result`）不得据以合并。`comment` 的未验签不算（通常是「写错身份重签」的记录，污染的是审计叙述而不是代码授权）。 |
| (g) | **不得零评审合并**：合并于无评审新规之后、却没有任何判决类 review ⇒ 判红。细则与豁免契约见下文「判据 (g)」一节。 |

`decision=="comment"` 的 review 不参与上述判定（既不要求契约行，也不计 blocking 轮）。

### 判据 (g)（2026-11-01 新增）：合并于新规之后却零判决类 review ⇒ 红

判据 (c) 的实现是 `( $post | final_judged )[]`——jq 对空数组的 `[]` 迭代是**零次**，
不产生违规，于是**「从没被评审过的卡」与「评审通过的卡」在判据 (c) 眼里一样**。

这不是笔误：`final_judged` 为简化「平局组为空」与「组内多条」两个边界而刻意这么写，
`ok_no_reviews` 样本期望 `rc=0` 也**是对的**——它只含一条 `issue`，是「还没开工的活卡」。
真盲区只在**已合并**这个组合上：**有 `patch` + 有 `merge_result(merged=true)`，却零 `review`**
（2026-11-01 实测：该形状的样本曾判 `✅ 通过`）。

**判据 (g)**：**每一条** `ts >= NO_REVIEW_TS` 的 `merge_result{merged:true}`，若该 thread
没有任何（`ts >= 契约阈值` 的、且**落在该次合并父链上**的）判决类 review ⇒ 判红；
或虽有、但**合并前最后一条不是 `blocking=0` 的 `approve`** ⇒ 也判红（覆盖「合并时仍是
`request_changes`」「合并前那条 approve 自己带了 blocking」两种形态）。

**用父链而不是 `ts`、也不是数组下标**（2026-11-01 第二轮评审的 blocking-1）：
`ts` 由客户端签名时写入、可任意伪造；而数组下标也不可靠 ——
`walgit_wal::collab::thread()` 的聚合顺序是「先按 `(ts, actor, oid)` 排序，再对 `parent`
尚未 emit 的条目**延后**」，只延后不提前 ⇒ 位置主要由**可伪造的 `ts`** 决定。
实测攻击（真二进制 + 真签名）：`issue ← patch ← { review(ts 更早, parent=patch),
merge(parent=patch) }` ⇒ 聚合后 review 排在 merge 之前，被判「合并前已评审」。
**父链不可伪造**：条目 oid 覆盖自身 `ts`/`parent` 的规范字节，`merge` 要引用 `review`
的 oid 就必须在它存在之后才签名 ⇒ 「谁在谁的因果链上」由密码学保证。

**逐条判，不是只看一条**：`ts` 由客户端在签名时写入、不是服务端盖章，所以只要判据挑
「数组里的某一条」merge，构造一条 `ts` 回溯的 merge 就能把整条判据关掉——实测旧实现取
`$merge_items[-1]`，`merge(ts=1000300, 零 review) → merge(ts=999000)` 判 `rc=0`。现在同一
thread 里**多条违规各报一条**（`bad_merged_two_violations` 样本断言 2 条）。

**为什么不复用契约阈值**：本判据落地前的历史合并（含 owner 明确授权降档的那批）
不该被追溯判红，故单列 `NO_REVIEW_TS_DEFAULT`，与「契约前条目豁免」同口径。
回归实测：20 张历史零评审合并卡在默认阈值下**全部仍绿**（不追溯）。

**降档（豁免）机制**：必须由**独立第三方**签一条 `comment`，其 **`body.waiver` 字段**
等于字符串：

```json
"waiver": "no-independent-review"
```

约定与理由：

1. **只看结构化字段，不看散文**。早先的实现是在 `body.body + body.note` 全文里匹配
   `review-waiver: no-independent-review`，没有行锚定 ⇒ 任何**提到**这句话的普通评论都
   成了豁免，连「不接受用 review-waiver: no-independent-review 跳过本轮评审」这种
   **明确拒绝豁免**的评论也能把红卡刷绿（两条均实测 `rc=0`）。豁免是授权动作，必须显式
   编码，不能靠引用。回归样本：`bad_waiver_prose_mention` / `bad_waiver_prose_refusal`。
2. **不能用「补一条 approve review」当豁免**——会被判据 **(d)**（独立评审）当自审拦下：
   `patch(作者) → review(作者, approve) → merge` 实测判红「自审不算独立评审」。
   所以豁免只能用不参与判决的 `comment` 载体。
3. **豁免的效力约束**（每条都有对应的 `samples/bad_waiver_*.json`）：
   - `actor` 不得是本 thread **任何一份** `patch` 的作者（集合判定；只比「合并前最后一份
     patch」时，双 patch 的 thread 里更早那位作者仍可自签豁免）；
   - `actor` 不得是**该次合并的执行者**（合并者不得自签豁免）；
   - `parent` 父链在该次合并之前（不得事后补签）；
   - `ts >= NO_REVIEW_TS`（新规之前写的评论不构成对本次降档的授权）；
   - `verified != false`（未验签的豁免不算数）；
   - `actor` 不得缺失（无主体的豁免无法追责）。
   豁免是**一次一议**：每条 `merge_result` 各自校验，A 次合并的豁免不外溢到 B 次。

这样降档不再能静默跳过，而必须**留痕、且由与该次合并无关的第三方确认**。

**向后兼容性（收紧的追溯影响）**：`body.waiver` 是本轮（评审 `abb-reviewer-171`）新引入的字段，
此前的**散文写法一律不再算数**。实测（2026-11-01）全仓 `ts >= NO_REVIEW_TS` 的
`merge_result{merged:true}` 为 **1** 条 ⇒ 这次收紧**没有追溯影响**，只对新合并生效；
代价是此后的降档必须用结构化字段写（本文件即唯一口径）。

**实测规模（2026-11-01，全仓 1004 个 thread）：**

| 口径 | 数量 |
|---|---|
| 已合并但零 `review` | 47 |
| 其中合并发生在契约阈值之后 | 20 |
| 新规生效后全仓判红 | 4（全部来自既有判据，与本判据无关；改动前后 orig=1/new=1 逐条一致） |

### 判据 (e) 同样逐条判 merge（2026-11-01，同卡并入）

判据 (e)（终审不得与合并者同人）原先取 `$merge_items[-1]`——只比**数组末一条** merge。
这与 (g) 是同一个洞的另一个入口：`ts` 由客户端在签名时写入、不是服务端盖章，所以构造
一条 `ts` 回溯的 merge 就能把 (e) 关掉。实测（阈值 1000000）：

```text
approve(M) → merge(M, ts=1000200) → merge(X, ts=999000)   ⇒ 旧实现 rc=0
删掉尾部那条 merge(X, ts=999000)                          ⇒ 旧实现 rc=1
```

旧实现取到的是末尾那条 `X`（`X ≠ M` ⇒ “不同人”），而真正的终审 `M` 正是第一次合并的
执行者。现在 (e) 对**每一条** `merge_result{merged:true}` 各判一次，同 ts 的多条 merge
会各报一条（多报只更严，不更松）。

- 回归样本：`bad_merged_backdated_merge_tail_hides_final_reviewer`（回溯尾部遮不住，断言
  恰好 1 条违规）、`bad_merged_two_final_reviewer_merges`（两条 merge 同人 ⇒ 各报一条）。
- **历史豁免口径不变**：(e) 仍只看 `ts` ≥ 契约阈值的 review；`ok_merged_historical_review_by_merger`
  样本钉住「契约前的 review 与合并同人不追溯」（把该门槛改成恒真，此样本即变红）。
- **无新增误判**：全仓 1002 个 thread 在改动前后各跑一遍 `tools/check_review_cadence.sh`，
  判红数 **4 → 4**，逐条回报一致（详见提交说明）。

### 常量推导（可复现）

两个阈值都是**客户端 ts 口径**的常量，硬约束写在 `tools/check_review_cadence.sh` 的注释里。
`NO_REVIEW_TS_DEFAULT` 的推导（2026-11-01，扫全仓 4104 条 `refs/collab/inbox` entry）：

- 全仓 `merge_result{merged:true}` 共 **199** 条，**最大 `ts` = 1793356481**；
- 常量取 **1793524600**（= 2026-11-01 09:16:40 UTC，判据 (g) 落地当刻），比最后一条历史
  合并晚 168119 s（≈1.95 天）⇒ 满足「严格大于全仓现存 merge `ts`」这条硬约束，
  落地时 **0 条**历史合并被追溯判红；
- **不得调低到 ≤ 1793356481**（会追溯判红历史合并）；调高只会多豁免，但会放过真实违规。

复现（不需要 walgit 服务，直接读本仓的 collab refs）：

```sh
git for-each-ref refs/collab/inbox --format='%(objectname)' | git cat-file --batch 2>/dev/null \
  | python3 -c '
import sys, json
d = sys.stdin.buffer; rows = []
while True:
    h = d.readline()
    if not h: break
    p = h.split()
    if len(p) < 3: break
    n = int(p[2]); e = json.loads(d.read(n)); d.read(1)
    if e.get("kind") == "merge_result" and (e.get("body") or {}).get("merged") is True:
        rows.append(e.get("ts") or 0)
print("merged:", len(rows), "max ts:", max(rows) if rows else None)
'
```

两个阈值的**邻域**由自测固定（这是「阈值方向」的唯一守卫）：

| 阈值 | 恰好阈值 | 阈值 − 1 |
|---|---|---|
| `--contract-ts`（判据 a–f） | `bad_missing_at_threshold` 必须**判** | `ok_historical_missing` 必须**跳过** |
| `--no-review-ts`（判据 g） | `bad_merged_at_no_review_ts` 必须**判** | `ok_merged_before_no_review_ts` 必须**豁免** |

## 历史条目

契约自本文件落地之日生效。**不改写历史 review 条目**（append-only）：
`ts` 小于契约阈值的 review 视为历史，缺契约行时**跳过并计数、不判红**。

阈值常量在 `tools/check_review_cadence.sh` 的 `CONTRACT_TS_DEFAULT`
（= 落地时「现存 review 最大 `ts`」+ 1，保证不误判历史；**不得调低**），
可用 `--contract-ts <epoch>` 或 `ABB_REVIEW_CONTRACT_TS` 覆盖（自测即用它把样本放在阈值前后）。

## 怎么跑

```sh
# 对一条在飞 thread（合并前由协调者执行）
tools/check_review_cadence.sh <thread-id>

# 离线（自测/样本）
tools/check_review_cadence.sh --json tools/review-cadence/samples/ok_basic.json --contract-ts 1000000
```

退出码：`0` 通过；`1` 违规；`2` 用法/工具错误（缺 jq、thread 取不到、JSON 非法）。

守卫自身的自测（合规 + 违规 + 守卫失效模式，共 83 例）随仓提交，并在
`.walgit/ci.toml` 的 `review-cadence-selftest` 任务里执行：

```sh
tools/review-cadence/selftest.sh
```

## 合并前检查（协调者）

协调者在写 `merge_result` 之前，必须对目标 thread 跑一次
`tools/check_review_cadence.sh <thread-id>`；**红则不得合并**。
本检查需要 thread id，CI 任务拿不到该上下文，故不进 `ci.toml` 的每次提交门禁
（判断依据：`RULE_可达性.md`——无法被自动触发的检查如实登记为流程步骤，并在此写明）。
