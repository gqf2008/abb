# review cadence guard core — input: `walgit collab thread <id>` JSON array.
# Requires: --argjson contract_ts <epoch seconds>（契约落地时间阈值）。
#
# 判据（与 docs/review-findings-contract.md 一致）：
#   (a) 每条**判决类** review（approve / request_changes）的 decision 必须与其 note 顶部的
#       `findings: blocking=N non-blocking=M` 契约行一致：
#         blocking>0 ⇒ request_changes；blocking=0 ⇒ approve；
#       若 note 里同时用了逐条 `[blocking]` / `[non-blocking]` 标签，标签计数必须与契约行相等；
#       ts < contract_ts 的历史条目缺契约行 ⇒ 跳过并计数（不判红）；ts ≥ contract_ts 缺 ⇒ 判红。
#       同理，`decision` 不在词表里（历史里出现过 `needs-changes` 与 `null`）的条目：
#       ts < contract_ts ⇒ 跳过（不判红，与「缺契约行」同口径）；ts ≥ contract_ts ⇒ 判红。
#       为什么未知 decision 必须一起套阈值：本仓有 26 个历史 thread 用旧词表 needs-changes，
#       不豁免则它们恒红，守卫整体失去信噪比（卡 abb-review-cadence-unknown-decision-ts-20261013）。
#       注意：契约**之后**的未知 decision 仍然判红——否则把 approve/request_changes 判据整个绕开。
#   (b) 同一 thread 的 blocking 轮数（契约行 blocking>0 的**判决类** review 条数）≤ 2。
#   (c) thread 内最后一条（ts ≥ contract_ts）判决类 review 必须是 blocking=0 的 approve；
#       否则判红。该判据在**合并前**即可判定（不依赖是否已有 merge_result）。
#   (d) 独立评审（§0b）：判决类 review 的 actor 不得与本 thread 任何 patch 的 actor 相同。
#       自审可以签出完全合法的 approve（契约行/标签/轮次全部合规），(a)-(c) 一条都拦不住。
#   (e) 终审不得与合并者同人（§4）：已合并的 thread，最后一条判决类 review 的 actor
#       与 merge_result 的 actor 必须不同。**同一个人多轮评审是合法的**，不在此拦。
#   (f) 签名验不过（verified=false）的**判决性**条目（patch/review/merge_result/card_claim）
#       不得参与合并。`comment` 上的未验签不算：那通常是“写错身份后重签”的记录，
#       污染的是审计叙述而不是代码授权，拿它堵整张卡会让事故记录变成永久阻塞。
#   (d)(e)(f) 同样只看 ts ≥ contract_ts 的条目（历史豁免，与 (a) 同口径）。
#   `decision=="comment"` 的 review 不参与上述判定（既不被要求带契约行、也不计 blocking 轮）。
#
# 输出：{violations:[...], summary:{...}, reviews:[...]}

def first_nonempty_line:
  (. // "")
  | split("\n")
  | map(select(test("^[ \t]*$") | not))
  | (.[0] // "");

def contract_of:
  first_nonempty_line as $line
  | if ($line | test("^findings:[ \t]*blocking=[0-9]+[ \t]+non-blocking=[0-9]+[ \t]*$")) then
      ($line | capture("^findings:[ \t]*blocking=(?<b>[0-9]+)[ \t]+non-blocking=(?<nb>[0-9]+)[ \t]*$")
             | {blocking: (.b | tonumber), nonblocking: (.nb | tonumber)})
    else null end;

# 单条 review 的契约问题（空 = 无问题）。抽成函数是为了同时用于「最后一条」与「历史计数」，
# 避免两处各写一遍正则/比较而漂移。
def contract_problems($contract_ts):
  . as $r
  | if ($r.decision == "comment") then empty
    elif ($r.decision != "approve" and $r.decision != "request_changes") then
      (if ($r.ts >= $contract_ts) then
         "\($r.oid): 未知 decision=\($r.decision)（ts=\($r.ts) ≥ 契约阈值 \($contract_ts)）"
       else empty end)
    elif ($r.contract == null) then
      (if ($r.ts >= $contract_ts) then
         "\($r.oid): decision=\($r.decision) 缺 findings 契约行（ts=\($r.ts) ≥ 契约阈值 \($contract_ts)）"
       else empty end)
    elif (($r.labels.blocking + $r.labels.nonblocking) > 0
          and ($r.labels.blocking != $r.contract.blocking
               or $r.labels.nonblocking != $r.contract.nonblocking)) then
      "\($r.oid): 契约行 \($r.contract.blocking)/\($r.contract.nonblocking) 与逐条标签 \($r.labels.blocking)/\($r.labels.nonblocking) 不一致"
    elif ($r.contract.blocking > 0 and $r.decision != "request_changes") then
      "\($r.oid): blocking=\($r.contract.blocking) 但 decision=\($r.decision)（应 request_changes）"
    elif ($r.contract.blocking == 0 and $r.decision != "approve") then
      "\($r.oid): blocking=0 但 decision=\($r.decision)（应 approve）"
    else empty end;

# 「最后一条判决类 review」的判定：返回 ts 最大**那一组**（可能多条）。
#
# 为什么是一组而不是一条（评审 abb-reviewer-142 的 BLOCKING-1）：
# `ts` 是客户端签名时写入的，不是服务端盖章 ⇒ 两条 review 同 ts 是可构造的。而 jq 的
# `max_by(.ts)` 在平局时返回**数组最后一个**最大值元素 ⇒ 「哪条算最后」取决于数组顺序，
# 于是同一份内容换个顺序会得出相反结论（实测：真实 blocking 在前⇒放行、在后⇒拦截）。
#
# 平局本身语义含糊，**含糊时应当保守而非猜测**：所以返回整组，由调用方要求
# 「组内任一条不干净就拦」。这样多条同 ts 只会让判定更严，不会更松。
def final_judged:
  if length == 0 then []
  else (map(.ts) | max) as $max_ts
    | map(select(.ts == $max_ts))
  end;
# 调用约定（评审 abb-reviewer-142 的 NB-1）：各调用方写作 `( … | final_judged )[]`。
# 空数组的 `[]` 迭代是**零次** ⇒ 天然不产生违规，不需要额外判空。**别改成显式
# `if length > 0` 分支**——那样平局组为空与组内多条两个边界都得各写一遍，容易写漏。

# 从某条目的 parent 起沿父链走到 root，返回「祖先 oid 集合」（不含自身）。
# 为什么用父链而不是数组位置（idx）：`walgit_wal::collab::thread()` 的聚合顺序是
# **先按 (ts, actor, oid) 排序，再对 parent 尚未 emit 的条目延后**——它只延后、不提前。
# 所以数组位置主要由**可伪造的 ts** 决定，不是写入顺序。实测攻击（真二进制+真签名）：
#   issue ← patch ← { review(ts 更早, parent=patch),  merge(parent=patch) }
# 聚合后数组是 issue,patch,review,merge ⇒ review 的 idx 小于 merge ⇒ 被判「合并前已评审」。
# 父链则不可伪造：条目 oid 覆盖自身 ts/parent 的规范字节，merge 要引用 review 的 oid，
# 就必须在 review 存在之后才签名 ⇒ 「谁在谁的因果链上」由密码学保证，不由客户端字段决定。
# **环保护是承重的，不是防御性装饰**（第三轮评审 NB-4）：去掉 `.seen[.cur] == true`
# 这一支后，喂一份自环/两元环输入会让 jq **永久挂起**（实测 4s 仍在跑），而门禁脚本
# 本身没有超时 ⇒ 一次误改会把「红」变成「CI 无限挂」。变异 M-C 实测 83 例全绿（零覆盖，
# 因为真路径上 oid 由 blob 内容派生、环不可构造），故单列注释说明它守的是什么。
def ancestor_oids($by_oid; $oid):
  { seen: {}, cur: (($by_oid[$oid].parent) // "") }
  | until(.cur == "" or .seen[.cur] == true;
      .seen[.cur] = true
      | .cur = (($by_oid[.cur].parent) // ""))
  | (.seen | keys);

# 从一条 review 的 note 里提取「位置指纹」（文件路径，**去掉行号**）。
#
# 用途：判据 (b) 要区分两种 blocking 轮次——
#   甲、同一个问题被反复提（代码真的没收敛）⇒ 该拦；
#   乙、每轮都在收敛，只是中途又推了新提交引入**别的**问题 ⇒ 不该按「拉锯」处理。
#
# 判据是「两轮 blocking 是否指向同一批文件」：位置有交集 ⇒ 同一问题链继续；
# 无交集 ⇒ 新问题链重新计数。
#
# 为什么去掉行号：同一处代码改动后行号必然漂移（`src/x.rs:120` → `src/x.rs:135`），
# 带行号比会让「同一个文件里的同一个问题」永远算不重叠 ⇒ 判据形同失效。
def position_fingerprints:
  (. // "")
  | [ scan("[A-Za-z0-9_./-]+\\.(?:rs|sh|py|slint|jq|toml|md|json)") ]
  | unique;

# 判据 (b) 的**未收敛链长**：不是数「总共几轮 blocking」，而是数「连续几轮指向同一处」。
#
# 输入：blocking 轮次的数组（按 ts 升序，每项带 `pos` = position_fingerprints）。
# 输出：最长的那条「同一问题链」的长度。
#
# **解析不出位置时按重叠处理（fail-closed）**：真实账本里相当一部分 blocking 讲的是
# 流程/审计问题（例如「作者与评审未真正分离」），note 里没有任何文件路径。
# 那类问题往往**更严重**，绝不能因为「找不到路径」就被算成新问题、把轮数清零。
def worst_convergence_chain:
  reduce .[] as $b ({seen: [], chain: 0, max: 0};
    ( ($b.pos | length) == 0
      or ([ .seen[] as $s | $b.pos[] | select(. == $s) ] | length) > 0
    ) as $same_problem
    | if $same_problem
      then .chain += 1
      else .chain = 1
      end
    | .seen += $b.pos
    | .max = ([.max, .chain] | max)
  )
  | .max;

def label_counts:
  (. // "")
  | split("\n")
  | reduce .[] as $l ({blocking: 0, nonblocking: 0};
      if   ($l | test("^[ \t]*-[ \t]*\\[non-blocking\\]")) then .nonblocking += 1
      elif ($l | test("^[ \t]*-[ \t]*\\[blocking\\]"))     then .blocking += 1
      else . end);

. as $raw
# 无评审新规生效时间。**调用方必须传** `--argjson no_review_ts <epoch>`（`tools/check_review_cadence.sh`
# 恒定传，缺省取常量 NO_REVIEW_TS_DEFAULT）。这里的 `// 0` **兜不住漏传**：jq 对未定义变量是
# **编译期错误**（实测 `jq -f check.jq --argjson contract_ts …` ⇒ `$no_review_ts is not defined` rc=3），
# 不是运行期 null。`//` 只处理显式传 `--argjson no_review_ts null` 的情形。
# 取到 0 的语义 = 全量生效（不豁免任何合并）。
# (g) 判据只对**此时间之后**的合并生效，更早的历史合并豁免——与「契约前条目豁免」同口径，
# 否则会把 owner 明确授权降档的那批历史卡追溯判红。
| ($no_review_ts // 0) as $no_review_ts
# 注意：不能用 `verified // true` —— jq 的 `//` 把 false 也当“缺失”，
# 会把 verified=false 悄悄翻成 true，让 (f) 判据永远不触发（selftest 逮到过）。
| [range(0; ($raw | length))
   | {idx: ., oid: $raw[.].oid, e: $raw[.].entry,
      verified: (if ($raw[.] | has("verified")) then $raw[.].verified else true end)}] as $items
# oid → 条目，用于沿 parent 链回溯（判据 (g) 的「合并前评审」必须落在合并的祖先链上）。
| ( reduce $items[] as $it ({}; .[$it.oid] = $it.e) ) as $by_oid

| [ $items[]
    | select(.e.kind == "review")
    | { idx: .idx,
        oid: .oid,
        ts: (.e.ts // 0),
        actor: (.e.actor // null),
        verified: .verified,
        decision: (.e.body.decision // null),
        contract: ((.e.body.note // "") | contract_of),
        labels: ((.e.body.note // "") | label_counts),
        # 判据 (b) 要按「两轮 blocking 是否指向同一处文件」判断是否收敛，
        # 所以要把 note 原文留在这里（`contract_of`/`label_counts` 都是摘要，取不出路径）。
        note_text: (.e.body.note // "") } ] as $reviews
# 结构性判据（§0b/§4）需要看“谁签的”：patch 作者、merge 执行者、以及每条 review 的 actor。
| [ $items[] | select(.e.kind == "patch") | {oid, actor: (.e.actor // null), ts: (.e.ts // 0)} ] as $patches
| [ $items[] | select(.e.kind == "merge_result" and (.e.body.merged == true)) | {oid, actor: (.e.actor // null), ts: (.e.ts // 0), idx} ] as $merge_items
| [ $items[] | select(.e.kind == "merge_result" and (.e.body.merged == true)) | .idx ] as $merges
# 判据 (b) 的输入：blocking 轮次 + 每轮的位置指纹（按 ts 升序）。
# **不是简单计数**（卡 abb-cadence-b-rounds-semantics-20261104）：旧写法只数条数，
# 把「同一个问题被反复提」（该拦）和「每轮收敛但中途引入新问题」（不该拦）算成一样。
| [ $reviews[] | select(.decision != "comment" and .contract != null and .contract.blocking > 0)
    | {oid, ts, pos: ((.note_text // "") | position_fingerprints)} ] as $blocking_items
| ($blocking_items | length) as $blocking_rounds
| ($blocking_items | worst_convergence_chain) as $worst_chain
# 历史自审（告警，不阻塞）：逐条比对该 review 之前的最新 patch。只用于 summary 上报，
# 让「曾经自审过、后来被纠正」这件事仍可审计 —— 但不进 violations，否则自我纠正的
# 真实卡会被永久锁死（见 (d) 的注释）。
| ( [ $reviews[] | . as $r
      | select($r.decision == "approve" or $r.decision == "request_changes")
      | ( $patches | map(select(.ts < $r.ts)) | last ) as $p
      | select($p != null and $p.actor != null and $r.actor == $p.actor)
      | $r.oid ] ) as $self_reviews_all
| ($reviews | map(select((.decision == "approve" or .decision == "request_changes")
                         and .contract == null and .ts < $contract_ts)) | length) as $skipped
# 未知 decision 的历史豁免也计入同一个计数：两者都是「契约前、口径未定型」的条目。
| (($reviews | map(select(.decision != "comment"
                         and .decision != "approve" and .decision != "request_changes"
                         and .ts < $contract_ts)) | length) + $skipped) as $skipped
# 历史契约不一致（告警，不阻塞）：所有判决类 review 里、除最后一条之外的那些问题条数。
# 与 (d) 的 `self_reviews_historical` 同性质——保留可见性，但不再锁死卡片。
| ( ( [ $reviews[] | select(.decision != "comment") ] ) as $all_judged
    | if ($all_judged | length) > 0 then
        # 排除 **ts 最大那一整组**（而不是单条）：平局时组内每条都算「最后」，
        # 因此都不计入历史——这与 (a) 把它们全部纳入判定是互补的，不会重复也不会漏。
        ( $all_judged | final_judged ) as $finals
        | ( $all_judged
            | map(select((.ts == $finals[0].ts) | not))
            | map(contract_problems($contract_ts) | select(. != "")) | length )
      else 0 end ) as $contract_mismatches_historical
# 已合并 thread 里「终审早于最后改动」的条数（告警，不阻塞）。
# 为什么单开一个计数：那类记录是真违规（合并晚于未审 patch），但已经合了、改不了事实，
# 而追溯判红会误导使用者忽略门禁（本仓反复踩过）。留着可见，但不当阻塞。
| ( ( $reviews
      | map(select(.ts >= $contract_ts
                   and (.decision == "approve" or .decision == "request_changes"))) ) as $jr
    | if (($jr | length) > 0) and (($merge_items | length) > 0) then
        ( $jr | final_judged ) as $f
        | [ $f[] | . as $v
            | select(( $patches | map(select(.ts > $v.ts)) | length ) > 0) ] | length
      else 0 end ) as $stale_review_merged_historical
| (
    [
      # (a) 判决类 review 的契约一致性 —— **只判最后一条判决类 review**。
      #
      # 为什么只看最后一条（卡 abb-cadence-a-scans-all-reviews-20261022）：
      # 本判据原先用 `$reviews[]` 扫描**全部** review，于是任何一条历史 review 的格式笔误
      # 都会让整张卡**永久不可合并**——即使它已被后续合法 approve 取代、并在链上写了更正。
      # 真实实例：卡 abb-413-image-budget-20261022 的第 1 轮 review 写了
      # `findings: blocking=0` 却 `decision=request_changes`（正文说"非阻塞"，元数据说"阻塞"），
      # 第 2 轮已合法 approve，但 (a) 仍揪住那条历史条目。
      #
      # 这与已经合并的 (d) 是同一设计原则：**合并闸门要回答的是"能不能放行这一次批准"**，
      # 历史条目已被取代，不该无限追溯。历史不一致改为计数上报（见 summary 的
      # `contract_mismatches_historical`），保留可审计性、但不阻塞。
      #
      # **没有放宽的证明**：最后一条判决类 review 仍逐条校验（未知 decision / 缺契约行 /
      # 标签不符 / blocking>0 却 approve / blocking=0 却 request_changes），
      # 且 (c) 仍要求它是 blocking=0 的 approve。样本 bad_last_review_contract_mismatch
      # 专门钉住"最后一条本身写错必须红"。
      ( ( $reviews | map(select(.decision != "comment")) ) as $judged
        | ( $judged | final_judged )[] | contract_problems($contract_ts) ),

      # (b) blocking 轮数 ≤ 2
      # (b) **同一问题的阻塞链 ≤ 2 轮**（语义见 `worst_convergence_chain` 的注释）。
      #
      # 旧写法 `$blocking_rounds > 2` 只数条数，把两种相反的情形判成同一种：
      # 代码真没收敛（该拦） vs 每轮都在收敛但中途引入新问题（不该拦）。
      # 卡 abb-cadence-b-rounds-semantics-20261104 的实测：三轮 blocking 全是真缺陷、
      # 全部修好、每条都因此加强了判据，却被数成「三轮拉锯」。
      ( if ($worst_chain > 2) then
          "同一问题的 blocking 已连续 \($worst_chain) 轮未收敛（> 2，应升级为架构问题）；共 \($blocking_rounds) 轮 blocking"
        else empty end ),

      # (c) 可合并：thread 内最后一条（ts ≥ 契约阈值）判决类 review 必须是 blocking=0 的 approve。
      #     必须在**合并前**就能判（旧写法以「已有 merge_result」为前置 ⇒ 活卡恒绿，评审 blocking 项）。
      #
      #     **终审之后不得再有 patch**（卡 abb-cadence-approve-then-patch-20261102）：
      #     早先只看「最后一条是不是 approve」，而 approve 之后追加 patch **不会使它失效**
      #     ⇒ 门禁会把「从未被那轮评审看过的 head」放行。现实实例：2026-11-02 推进
      #     `abb-mcp-bus-architecture-20261102` 时，评审已 approve `8a1a57e`，作者随后又推
      #     `69909d6`，若据此合并就是「审 A 合 B」。
      #
      #     **先量误红再改**（卡内要求）：全账本 1011 个 thread、150 个有判决类 review，
      #     加上本判据后在契约阈值之后的命中仅 **3 条**，且逐条核实**全部是真违规**
      #     （终审 ts 之后确实有 patch）：
      #       · abb-mcp-bus-architecture-20261102（未合并）→ 正确判红，正是该卡要拦的
      #       · abb-sched-count-no-upper-bound-20261029（已合并，合并晚于未审 patch）
      #       · abb-v11-message-actions（已合并，合并晚于未审 patch）
      #     误红 0 条 ⇒ 可以判死，不需要降级成「只报警」。
      #
      #     **已合并的不追溯判红**（与 (g) 的 `NO_REVIEW_TS` 同取向）：(c) 是**合并前**闸门，
      #     问的是「我现在能不能合」。已经合了的再去判红既改不了事实，又会训练使用者忽略
      #     门禁（本仓反复踩过：误红比缺判据更糟）。那两条转为 summary 里的
      #     `stale_review_merged_historical` 计数上报——可审计、不阻塞。
      #     ⚠️ 这意味着「先绕过门禁合了、再补 patch」不会被本判据追责；那类行为由
      #     `abb-coordinator-bypassed-merge-gate-20261101` 的口径（人工留痕）承担。
      ( ( $reviews
          | map(select(.ts >= $contract_ts
                       and (.decision == "approve" or .decision == "request_changes"))) ) as $post
        | ( $post | final_judged )[] | . as $v
        | if ($v.decision != "approve"
              or ($v.contract != null and $v.contract.blocking > 0)) then
            "thread 最后一条判决类 review（\($v.oid)）不是 blocking=0 的 approve（decision=\($v.decision)）⇒ 不得合并"
          else
            # 只在**未合并**时判红：这是合并前闸门（同段首句），已合并的取证改不了事实，
            # 且追溯判红会误导使用者忽略门禁。已合并的过期终审转 summary 计数上报。
            ( if ($merge_items | length) == 0 then
                ( $patches | map(select(.ts > $v.ts)) | last ) as $later
                | if $later != null then
                    "thread 最后一条判决类 review（\($v.oid)，ts=\($v.ts)）之后还有 patch（\($later.oid)，ts=\($later.ts)）"
                    + " ⇒ 终审早于最后改动，那条 approve 未覆盖当前 head，不得合并"
                  else empty end
              else empty end )
          end ),

      # (d) 独立评审（§0b）：**最后一条判决类 review** 的 actor 不得与**它所审的那份 patch**
      #     （即该 review 之前 ts 最新的 patch）的 actor 相同。
      #
      #     为什么必须机检：自审可以签出一条完全合法的 approve（契约行、标签、轮次全都对），
      #     上面 (a)-(c) 一条都拦不住；而 author 自己的 approve 从不算满足闸门。
      #
      #     为什么是「该 review 之前的最新 patch」而不是「全 thread 最后一条 patch」
      #     （评审 abb-reviewer-141 的 BLOCKING-1，第 3 轮）：后者可被 decoy 绕过——
      #     构造 `patch(self) → review(self, approve) → patch(other)` 时，「最后一条 patch」
      #     是 other ⇒ 自审被洗白。而 decoy patch 甚至可以是空的，不需要真改任何东西。
      #     闸门要回答的是「放行这一次批准」，所以必须对齐「这份 review 在审谁」。
      #
      #     为什么只看最后一条 review：更早的判决类 review 已被后续 review 取代，其价值是
      #     历史记录而非合并授权。真实卡 abb-channel-mcp-remaining-injection-surfaces-20261012
      #     的历史是「coordinator 自审 → 被发现 → patch 重签为真实作者 → 另人 approve」，
      #     若逐条判则那条已纠正的自审会永久锁死该卡。历史自审改由 summary 里的
      #     `self_reviews_historical` 计数上报（告警，不阻塞）。
      #     **按 ts 取最大一组（final_judged）而不是数组末元素**（评审 abb-reviewer-141 的 NB-1；
      #     平局处理见 final_judged 的注释与评审 abb-reviewer-142 的 BLOCKING-1）：
      #     thread JSON 是 parent-ordered，数组顺序不等于 ts 顺序；而 `ts` 由客户端在
      #     签名时写入、不是服务端盖章 ⇒ 构造「self 的 review ts 更晚却排在数组前面」
      #     即可绕过。同一条 thread 里 (a)/(c)/(e) 也一律走 final_judged。
      #     口径：只看 ts ≥ 契约阈值的 review（历史条目豁免，与 (a) 同口径）。
      ( ( $reviews
          | map(select(.ts >= $contract_ts
                       and (.decision == "approve" or .decision == "request_changes"))) ) as $judged
        | ( $judged | final_judged )[] | . as $r
        | ( $patches | map(select(.ts < $r.ts)) | last ) as $p
        | if $p != null and $p.actor != null and $r.actor == $p.actor then
            "\($r.oid): 最后一条判决类 review 的 actor=\($r.actor) 也是它所审 patch \($p.oid) 的作者 ⇒ 自审不算独立评审（§0b）"
          else empty end ),

      # (e) 终审不得与合并者同人（§4）：已合并的 thread，最后一条判决类 review 与 merge_result 的 actor 必须不同。
      #     注意：同一个人多轮评审（request_changes → approve）是**合法**的，不在这里拦；只有
      #     “评审者又去合并”才是违规——这两件事极易混，样本里各有对应用例。
      #
      #     **对每一条 merge_result{merged:true} 各判一次，不是只判某一条**（与 (g) 的 blocking-3
      #     属同一类洞；本卡实现者在上一轮报告里自行指出，经协调者决定并入同卡）：`ts` 由客户端
      #     在签名时写入、不是服务端盖章 ⇒ 只要判据挑定**数组里的某一条** merge，构造一条 ts
      #     回溯的 merge（它排在数组末尾，`$merge_items[-1]` 恰好取到它）就能把整个 (e) 关掉。
      #     实测（旧实现）：
      #       approve(M) → merge(M, ts=1000200) → merge(X, ts=999000)，阈值 1000000
      #       ⇒ 旧实现 rc=0（取到末尾那条 X，X ≠ M 故“不同人”），尽管终审 M 就是第一次合并的
      #         执行者。删掉尾部那条则 rc=1。
      #     样本 bad_merged_backdated_merge_tail_hides_final_reviewer 钉住这一条；每一条 merge
      #     都与终审成对判定 ⇒ 多条同人 merge 各报一条违规（多报只更严，不更松）。
      #     历史豁免口径不变：只有 ts ≥ $contract_ts 的 review 参与（与 (a) 同口径），样本
      #     ok_merged_historical_review_by_merger 钉住「契约前的 review 与合并同人不追溯」。
      #     空 $merge_items 的 `[]` 迭代是**零次** ⇒ 不需要 `if length > 0` 守卫（同 final_judged 的注释）。
      ( ( $reviews
          | map(select(.ts >= $contract_ts
                       and (.decision == "approve" or .decision == "request_changes"))) ) as $post2
        | $merge_items[] | . as $m
        | ( $post2 | final_judged )[] | . as $last
        | if ($m.actor != null and $last.actor != null and $m.actor == $last.actor) then
            "\($last.oid): 终审 review 与 merge_result(\($m.oid)) 同为 \($m.actor) ⇒ 终审不得与合并者同人（§4）"
          else empty end ),

      # (f) 签名未验签的**判决性**条目不得参与合并：patch / review / merge_result。
      #     为什么只算判决性条目：`comment` 上的未验签往往只是“写错身份重签”的记录，
      #     它污染的是审计叙述，不是代码授权；拿它堵住整张卡会让事故记录变成永久阻塞。
      #
      #     `card_claim` 也**不列入**（评审 abb-reviewer-141 的 BLOCKING-1）：claim 授权的是
      #     “谁占着这张卡干活”，不是“谁的代码可以合入”。本仓真实存在这样的死结（卡
      #     abb-413-body-limit-20261020）：用错密钥签的 claim 被作者按 append-only 口径
      #     **原样复原**后，若 claim 参与判定，则“删掉它”（违规）才能合、“保留它”（正确）
      #     永远不能合。同 thread 已有 verified 的 claim，足以证明认领发生过。
      ( $items[]
        | select(.verified == false
                 and (.e.kind == "patch" or .e.kind == "review"
                      or .e.kind == "merge_result")
                 and (.e.ts // 0) >= $contract_ts)
        | "\(.oid): \(.e.kind) 条目签名验不过（verified=false）⇒ 不得据它合并" ),

      # (h) `decisions` 字段形状（AGENTS.md 触发清单 T1–T5 的落地判据）。
      #
      # T2 的信号词**不能含裸「文案」**：实测误红——`what` 写着「自己打 skip 文案」
      # （改的是跳过提示=开发者/门禁输出，不是终端用户界面文案）被判成越界。
      # 而仓里原则是「误红会训练人忽略门禁，比缺判据更糟」。
      # 故收窄成「文案」下**指向终端用户**的三个具体形态。
      #
      # **不补全** T2 原文另列的「导出范围 / 分享内容 / 提示措辞」：问过 Jev
      # （rid gen-dec-1793633765-uAMRCmffQzP8XDuKcTcu，`keep_narrow_only` p=0.98
      #  /**conf=0.96**；同问题第一次问时 conf=0.68，那次用的 `jev_choice` 不返 rid，
      #  我照着格式编过一个假 rid——已自曝并更正，见 docs/decision-defaults.md 局限 4），
      # 主要理由是补全只增加误红面（任何提到这几个字样的句子都会命中，哪怕是在讨论它们），
      # 而本仓原则是「误红会训练人忽略门禁，比缺判据更糟」。
      # 代价如实记着：改「导出范围 / 分享内容」目前判据抓不到。
      ( "默认值|阈值|上限|界面文案|UI ?文案|用户文案|用户看到|产品决定|按惯例" ) as $t_signal |
      #
      # 为什么需要（2026-11-02 实证）：AGENTS.md 写着「有取舍先问 Jev」，而我整轮
      # **一次都没问**——因为我把「替产品做的决定」归类成了技术判断。规则只用散文写，
      # 落不到具体时刻；于是改成 `patch` 条目带结构化 `decisions`，门禁能查形状。
      #
      # 只判**三条确定性的事实**，不判「该不该问」（那需要读懂语义，判不准）：
      #
      #   A1 `rid` 必须像真的：`^gen-dec-<数字>-<字母数字>$`。编不出这个格式 ⇒
      #      说明在假装问过（比不问更坏：它让审计链看起来有痕迹）。
      #   A2 `self_decided` 不得命中 T1–T4 的信号词：那些本该问却被记成自决。
      #      命中即红——这是本判据唯一的"语义"判断，但只查**固定词表**，不猜意图。
      #   A3 低置信（`conf < 0.6`）必须被如实转述：`q` 里要出现「不确定」。
      #      Jev 是概率决策器，把 p=0.7 说成定论是失真（AGENTS.md 明确要求带分布+置信）。
      #
      # **不判「asked 为空」**：纯技术活真的可以不问。字段在，人一眼能看出
      # 「这轮拍了 N 个、问了 0 个」——异常可见就够了，不制造误红。
      #
      # 历史条目无此字段：只对 `ts >= $contract_ts` 的 patch 判，不追溯。
      #
      # **错记账可被后续 patch 显式修正**（问过 Jev：`all_but_allow_correction`
      # p=0.69 / conf=0.53——**它自己也不确定**；同时 `risk=high` p=0.71，
      # 即「用后续条目洗白」的风险被它判为高）。
      #
      # 为什么必须有这条：账本 append-only，一条早期 patch 把该项记成 `self_decided`
      # 之后**改不回来**；若照旧永久判红，作者只剩两条路——弃卡，或改账本（禁止）。
      # 实测就卡在 `abb-mcp-bus-s2d`：早期 patch 把「重连封顶/放弃阈值」记成自决，
      # 后来按纪律问了 Jev、改了参数、在新 patch 里记成 asked，旧条目仍永久红。
      #
      # 防「静默洗白」的做法：修正必须**显式指名**被修正的 patch oid
      # （`asked[].corrects == "<oid>"`），且被点名的 oid 必须**真是一条更早的 patch**。
      # 于是"修正"是一次公开指认，不是悄悄放过。
      ( [ $items[] | select(.e.kind == "patch") ] | map({
          oid: .oid,
          ts: (.e.ts // 0),
          corrects: [ .e.body.decisions.asked[]? | .corrects // empty ]
        }) ) as $all_patches |
      ( $items[]
        | select(.e.kind == "patch" and (.e.ts // 0) >= $contract_ts)
        | .e.body.decisions as $d
        | select($d != null)
        # 绑定当前 patch 的身份：下面遍历 `$self[]` 时 `.` 会变成数组元素，
        # 拿不到 oid/ts。不绑的话「修正豁免」永远不生效（且不报错）。
        | . as $p
        | if ($d | type) != "object" then
            "\(.oid): decisions 必须是对象（{asked:[…], self_decided:[…]}）"
          else
            ( ( $d.asked // [] ) as $asked
            | ( $d.self_decided // [] ) as $self
            # A1 形状
            | ( $asked[]
                | select((.rid | type) == "string"
                         and (.rid | test("^gen-dec-[0-9]+-[A-Za-z0-9]+$")) | not)
                | "\(.rid // "?"): decisions.asked 的 rid 不像真的 Jev request_id"
                  + "（应形如 gen-dec-1793630641-f00bippeCVxQdUiNrRjA）⇒ 不得假装问过" ),
              # A2 自决越界
              #
              # **命中前先剥掉否定式提及**：`不是产品决定` / `并非取舍` / `不涉及上限`
              # 这类句子是在**说明它不属于**该类，却因为含那个词被判红 —— 实测误红过
              # （what=「模块级 allow(dead_code) 改成逐项标注」，
              #   why=「…是代码卫生手法，不是产品决定」）。
              # 问过 Jev：`strip_negation` p=0.9 / conf=0.87。
              # 同一根源与 A2 的已知局限同族：**讨论规则本身的句子会被词表误伤**。
              ( $self[]
                | select(
                    ((.what // "") + " " + (.why // ""))
                    | gsub("(不是|并非|而非|而不是|不涉及|不属于|非)"
                           + "(默认值|阈值|上限|界面文案|UI ?文案|用户文案|用户看到|产品决定|按惯例)"; "")
                    | test($t_signal)
                  )
                # 被**更早更晚**的 patch 显式 `corrects` 指名 ⇒ 视为已修正，不判红。
                # 条件写全（更晚 + 指向更早）是为了「不能用未来的条目修过去的」同时
                # 也不允许「用更早的条目修更晚的」。
                | select(
                    ( [ $all_patches[]
                        | select(.ts > ($p.e.ts // 0))
                        | .corrects[] ]
                      | any(. == ($p.oid // "")) ) | not
                  )
                | "\(.what // "?"): 这一项命中了触发清单（T1–T4 的信号词）却记成 self_decided"
                  + " ⇒ 应先问 Jev 再落；口径见 docs/decision-defaults.md" ),
              # A3 低置信失真
              #
              # **与 A2 同样的 `corrects` 豁免**（Jev `extends_corrects` p=0.98/conf=0.97）：
              # 后一条 patch 指名修正旧条目时，A2 与 A3 都该生效。
              # 实测缺口：A2 有豁免而 A3 没有，于是「旧条目的 q 没写不确定」这种事
              # 即使在新 patch 里重记过、也指名了，仍然永久判红。
              ( $asked[]
                | select((.conf // 1) < 0.6)
                | select(((.q // "") | test("不确定")) | not)
                | select(
                    ( [ $all_patches[]
                        | select(.ts > ($p.e.ts // 0))
                        | .corrects[] ]
                      | any(. == ($p.oid // "")) ) | not
                  )
                | "\(.rid): conf=\(.conf) < 0.6（Jev 自己也不确定），但 q 里没如实写成不确定"
                  + " ⇒ 不得把概率判断包装成强结论" )
            )
          end ),

      # (g) 「合并于本判据之后，却一条判决类 review 都没有」⇒ 判红。
      #
      # 为什么需要：判据 (c) 的实现是 `( $post | final_judged )[]`，jq 对空数组的 `[]`
      # 迭代是**零次** ⇒ 零 review 天然不产生违规。于是「从未被评审过的卡」与
      # 「评审通过的卡」在 (c) 眼里一样（2026-11-01 实测：有 patch + 有 merge_result、
      # 零 review 的样本判 ✅ 通过）。这不是笔误——`final_judged` 为简化「平局组为空」
      # 与「组内多条」两个边界而刻意这么写，`ok_no_reviews` 样本（只含一条 issue、
      # 还没开工的活卡）期望 rc=0 也是对的。真盲区只在「已合并」这个组合上。
      #
      # 为什么用**独立阈值**而不是复用 $contract_ts：本判据落地前的历史合并（含 owner
      # 明确授权降档的那批）不该被追溯判红。与既有「契约前条目豁免」同口径。
      #
      # **逐条判，不是只看一条**（评审 abb-reviewer-171 的 blocking-3）：`ts` 由客户端在
      # 签名时写入、不是服务端盖章 ⇒ 只要判据只挑**数组里的某一条** merge，构造一条 ts
      # 回溯的 merge 就能把整条判据关掉。实测（旧实现取 `$merge_items[-1]`）：
      #   issue → patch(A) → merge(ts=1000300, 零 review) → merge(ts=999000)，阈值 1000000
      #   ⇒ 旧实现 rc=0（只看末尾那条 999000，判据整条跳过），尽管 1000300 那次合并明确在
      #     新规之后且零 review。
      # 所以这里对**每一条** `ts >= $no_review_ts` 的 merge_result 各判一次：同一 thread 里
      # 多条违规各报一条，谁也遮不住谁。
      #
      # 豁免（降档）机制：必须由**独立第三方**签一条 `comment`，其 `body.waiver` 字段
      # **等于字符串** `"no-independent-review"`。约定与理由：
      #   · **只看结构化字段，不看散文**（评审 abb-reviewer-171 的 blocking-1）：早先实现在
      #     `body.body + "\n" + body.note` 全文里匹配 `review-waiver: no-independent-review`，
      #     没有行锚定 ⇒ 任何**提到**这句话的普通评论都成了豁免，连「不接受用
      #     review-waiver: no-independent-review 跳过评审」这种**明确拒绝豁免**的评论也能
      #     把红卡刷绿（两条均实测 rc=0）。豁免是授权动作，必须显式编码，不能靠引用。
      #   · 为什么是 comment 而不是补一条 approve review：补 approve 会被判据 (d)
      #     「最后一条判决类 review 的 actor 不得是它所审 patch 的作者」当自审拦下——
      #     实验证实（patch(作者) → review(作者, approve) → merge ⇒ (d) 判红）。
      #     所以豁免只能用不参与判决的 comment 载体。
      #   · 效力约束（评审 abb-reviewer-171 的 blocking-2 + non-blocking-4，每条都有实测
      #     绕过用例，见 samples/bad_waiver_*.json）：
      #     1. actor **不得是本 thread 任何一份 patch 的作者**（集合判定；早先只比「merge 前
      #        最后一份 patch」⇒ 双 patch 时更早那位作者 A 可自签豁免，实测 rc=0）；
      #     2. actor **不得是本次合并的执行者**（合并者不得自签豁免）；
      #     3. `ts <= merge.ts`（不得事后补签）；
      #     4. `ts >= $no_review_ts`（新规之前写的评论不构成对本次降档的授权）；
      #     5. `verified != false`（未验签的豁免不算数）；
      #     6. actor 不得缺失（无主体的豁免无法追责）。
      #     豁免是**一次一议**：每条 merge 各自校验，A 次合并的豁免不外溢到 B 次。
      #
      # **`$judged_n` 只数合并之前的 review**（2026-11-01 补，评审 non-blocking 2）：
      # 早先它数的是「thread 内任意（ts >= 契约阈值的）判决类 review」，**不看时序**。
      # 配上 (c) 也只问「最后一条是不是 approve」，两条叠起来让「先上车后补票」完全
      # 绕开门禁 —— 实测：
      #   patch → merge(零评审) → approve(晚 500s)
      #   ⇒ (c) 看到最后一条是 approve、(g) 看到 $judged_n>0  ⇒  **rc=0 全绿**
      # 即「先合并、事后补一条 approve」与「先评审、后合并」在账本上无法区分。
      # 收紧后：合并之前一条判决类 review 都没有 ⇒ 判红（走豁免分支的除外）。
      # 不改 (c) 的原因：(c) 的设计前提是「在写 merge_result **之前**就能判」（活卡也要能用），
      # 让它去看 merge 时序会把这个前提拧反。
      ( $merge_items[]
        | select(.ts >= $no_review_ts)
        | . as $m
        | ( ancestor_oids($by_oid; $m.oid) ) as $anc
        | ( [ $reviews[]
              | select(.ts >= $contract_ts and (. as $r | ($anc | index($r.oid)) != null)
                       and (.decision == "approve" or .decision == "request_changes")) ]
            | length ) as $judged_n
        | ( [ $reviews[]
              | select(.ts >= $contract_ts and (. as $r | ($anc | index($r.oid)) != null)
                       and (.decision == "approve" or .decision == "request_changes")) ]
            | last ) as $last_before
        # **完备性**，不只是存在性（第三轮评审 blocking-1）：`merge.parent` 由签名者**任选**、
        # walgit 不校验（本仓 tools/check_collab_parents.sh 头注释即写明 `--parent` 不校验）。
        # 所以「merge 选中的那条链上有没有干净 approve」仍是攻击者可以挑的 —— 只要把
        # parent 指向前一个 approve、跳过真正的末条 review 即可。
        # 实测 S1（只用合并者密钥，零伪造）：(g) 判绿，唯一挡路的是 ts 序的 (c)；
        # 叠加 reviewer 侧 ts 伪造（S2：approve 前推 + rc 回溯）⇒ **整条门禁 rc=0**。
        # 改成完备性：thread 里**每一条**判决类 review 都必须是该次 merge 的祖先
        # （等价于「merge.parent 必须指向该 thread 当时的最新条目」）。这条不可挑 ——
        # 想跳过任何一条 review，就得让它挂在别的分支上或成为第二个 root，而那样仍然判红。
        # 语义：**凡声称早于本次合并的判决类 review，都必须在本次合并的祖先链上**。
        # 用 `ts < $m.ts` 而不是 `ts >= $contract_ts`（第四轮评审 blocking-1）：
        # 后者让「把 request_changes 的 ts 写到契约阈值之前」绕过整套判据 —— (a)~(g)
        # 每一条的视野都钉在 ts>=契约 这个客户端自选的比较上，所以这条 rc 对全部判据
        # 隐形（实测：skipped=0 judged=1）。换成 `ts < $m.ts` 后方向 fail-closed：
        # 回溯 ts ⇒ 它声称早于合并 ⇒ **必须上链**（这是本判据实际堵住的）；**但仅此而已**：
        # 上链之后它是否被 (g) 认成"合并前最后一条"，取决于 `$last_before`，而后者取的是
        # 数组 `last`——数组序是 `thread()` 按 (ts,actor,oid) 排序的结果，回溯后它反而排到
        # 前面，于是 `last` 取到的是那条 approve ⇒ **仍然判绿**（实测 D4 形状：
        # `approve(ts=1000900) ← rc(ts=999999) ← merge(parent=rc)` ⇒ rc=0）。
        # 所以本行只关掉 off-chain 那一半；**上链回溯是已知未修洞 D4**，登记在架构卡
        # `abb-cadence-timing-axes-20261101`。前推 ts 则由 (c) 的 final_judged 逮住。
        # 同时这个限定给「当时还不存在」的 review 留了豁免（第四轮评审 blocking-2，
        # 真实账本 8 个多合并 thread 里 5 个含合并之后的 review，否则会大面积误红）。
        | ( [ $reviews[]
              | select(.ts < $m.ts
                       and (.decision == "approve" or .decision == "request_changes")
                       and (. as $r | ($anc | index($r.oid)) == null)) ] ) as $offchain
        | if ($offchain | length) > 0 then
            "\($m.oid): 合并（ts=\($m.ts)）的父链没有涵盖这些判决类 review："
            + ([ $offchain[] | .oid ] | join(", "))
            + " ⇒ merge.parent 必须指向该 thread 当时的最新条目（不得跳过前驱）"
          elif ($judged_n > 0
              and $last_before != null
              and $last_before.decision == "approve"
              and ($last_before.contract == null or $last_before.contract.blocking == 0))
          then empty
          else
            ( [ $items[]
                | select(.e.kind == "comment"
                         and ((.e.body.waiver // null) == "no-independent-review"))
                | { oid, actor: (.e.actor // null), ts: (.e.ts // 0), verified, idx } ]
              | map(. as $w
                    | { oid: $w.oid,
                        reasons: [
                          (if $w.ts < $no_review_ts then
                             "ts=\($w.ts) 早于无评审新规 ts=\($no_review_ts)"
                           else empty end),
                          (if ($anc | index($w.oid)) == null then
                             "不在本次合并的父链上（未在账本因果链上先于该合并）⇒ 不得事后补签"
                           else empty end),
                          (if $w.verified == false then
                             "签名验不过（verified=false）"
                           else empty end),
                          (if $w.actor == null then
                             "缺 actor（无法确认豁免主体）"
                           elif ([ $patches[] | .actor ] | index($w.actor)) != null then
                             "actor=\($w.actor) 是本 thread 某份 patch 的作者"
                           elif $w.actor == $m.actor then
                             "actor=\($w.actor) 就是本次合并的执行者"
                           else empty end)
                        ] } ) ) as $waivers
            | ( [ $waivers[] | select((.reasons | length) == 0) ] | length ) as $good_n
            | if $good_n > 0 then empty
              elif ($waivers | length) > 0 then
                # 与 else 分支同口径（第四轮评审 NB-1 的另一半）：thread **可能**有判决类
                # review，不能一律说「无任何」。
                "\($m.oid): 合并（ts=\($m.ts)）时该 thread 有 \([ $reviews[] | select(.ts >= $contract_ts and (.decision == "approve" or .decision == "request_changes")) ] | length) 条判决类 review（其中在本次合并之前 \($judged_n) 条），但 review-waiver 条目不成立 ⇒ "
                + ([ $waivers[] | "\(.oid): \(.reasons | join("；"))" ] | join(" / "))
                + "（豁免须由独立第三方在 comment.body.waiver 显式声明）"
              else
                # 文案要分成因（第四轮评审 NB-1，连点两轮没修）：走到这里时 thread **可能**
                # 有判决类 review，只是它们（a）在本次合并之后才发生，或（b）不在本次合并的
                # 父链上，或（c）合并前最后一条不是 blocking=0 的 approve。原来一律写
                # 「该 thread 没有任何判决类 review」是与事实相反的审计叙述。
                "\($m.oid): 合并（ts=\($m.ts)）未经有效独立评审：本次合并之前"
                + "没有落在其父链上的 blocking=0 approve（该 thread 判决类 review 共 \([ $reviews[] | select(.ts >= $contract_ts and (.decision == "approve" or .decision == "request_changes")) ] | length) 条，"
                + "其中在本次合并之前的 \($judged_n) 条），"
                + "且没有 review-waiver 条目（comment.body.waiver == \"no-independent-review\"）"
                + " ⇒ 未经独立评审即合并"
              end
          end )
    ]
  ) as $violations| { violations: $violations,
    summary: { reviews: ($reviews | length),
               judged_reviews: ([ $reviews[] | select(.ts >= $contract_ts
                                    and (.decision == "approve" or .decision == "request_changes")) ] | length),
               blocking_rounds: $blocking_rounds,
               skipped_historical: $skipped,
               merges: ($merges | length),
               self_reviews_historical: ($self_reviews_all | length),
               contract_mismatches_historical: $contract_mismatches_historical,
               stale_review_merged_historical: $stale_review_merged_historical },
    reviews: [ $reviews[] | {oid, ts, decision, contract} ] }
