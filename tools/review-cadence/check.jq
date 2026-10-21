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

def label_counts:
  (. // "")
  | split("\n")
  | reduce .[] as $l ({blocking: 0, nonblocking: 0};
      if   ($l | test("^[ \t]*-[ \t]*\\[non-blocking\\]")) then .nonblocking += 1
      elif ($l | test("^[ \t]*-[ \t]*\\[blocking\\]"))     then .blocking += 1
      else . end);

. as $raw
# 注意：不能用 `verified // true` —— jq 的 `//` 把 false 也当“缺失”，
# 会把 verified=false 悄悄翻成 true，让 (f) 判据永远不触发（selftest 逮到过）。
| [range(0; ($raw | length))
   | {idx: ., oid: $raw[.].oid, e: $raw[.].entry,
      verified: (if ($raw[.] | has("verified")) then $raw[.].verified else true end)}] as $items
| [ $items[]
    | select(.e.kind == "review")
    | { idx: .idx,
        oid: .oid,
        ts: (.e.ts // 0),
        actor: (.e.actor // null),
        verified: .verified,
        decision: (.e.body.decision // null),
        contract: ((.e.body.note // "") | contract_of),
        labels: ((.e.body.note // "") | label_counts) } ] as $reviews
# 结构性判据（§0b/§4）需要看“谁签的”：patch 作者、merge 执行者、以及每条 review 的 actor。
| [ $items[] | select(.e.kind == "patch") | {oid, actor: (.e.actor // null), ts: (.e.ts // 0)} ] as $patches
| [ $items[] | select(.e.kind == "merge_result" and (.e.body.merged == true)) | {oid, actor: (.e.actor // null), ts: (.e.ts // 0)} ] as $merge_items
| [ $items[] | select(.e.kind == "merge_result" and (.e.body.merged == true)) | .idx ] as $merges
| ($reviews | map(select(.decision != "comment" and .contract != null and .contract.blocking > 0)) | length) as $blocking_rounds
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
      ( if ($blocking_rounds > 2) then
          "thread blocking 轮数 \($blocking_rounds) > 2（应升级为架构问题，不再开第 3 轮）"
        else empty end ),

      # (c) 可合并：thread 内最后一条（ts ≥ 契约阈值）判决类 review 必须是 blocking=0 的 approve。
      #     必须在**合并前**就能判（旧写法以「已有 merge_result」为前置 ⇒ 活卡恒绿，评审 blocking 项）。
      ( ( $reviews
          | map(select(.ts >= $contract_ts
                       and (.decision == "approve" or .decision == "request_changes"))) ) as $post
        | ( $post | final_judged )[] | . as $v
        | if ($v.decision != "approve"
              or ($v.contract != null and $v.contract.blocking > 0)) then
            "thread 最后一条判决类 review（\($v.oid)）不是 blocking=0 的 approve（decision=\($v.decision)）⇒ 不得合并"
          else empty end ),

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
      ( if ($merge_items | length) > 0 then
          ( $reviews
            | map(select(.ts >= $contract_ts
                         and (.decision == "approve" or .decision == "request_changes"))) ) as $post2
          | ($merge_items[-1]) as $m
          | ( $post2 | final_judged )[] | . as $last
          | if ($m.actor != null and $last.actor != null and $m.actor == $last.actor) then
              "\($last.oid): 终审 review 与 merge_result(\($m.oid)) 同为 \($m.actor) ⇒ 终审不得与合并者同人（§4）"
            else empty end
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
        | "\(.oid): \(.e.kind) 条目签名验不过（verified=false）⇒ 不得据它合并" )
    ]
  ) as $violations| { violations: $violations,
    summary: { reviews: ($reviews | length),
               blocking_rounds: $blocking_rounds,
               skipped_historical: $skipped,
               merges: ($merges | length),
               self_reviews_historical: ($self_reviews_all | length),
               contract_mismatches_historical: $contract_mismatches_historical },
    reviews: [ $reviews[] | {oid, ts, decision, contract} ] }
