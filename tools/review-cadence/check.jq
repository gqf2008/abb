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
| ($patches | map(.actor) | unique) as $patch_actors
| [ $items[] | select(.e.kind == "merge_result" and (.e.body.merged == true)) | .idx ] as $merges
| ($reviews | map(select(.decision != "comment" and .contract != null and .contract.blocking > 0)) | length) as $blocking_rounds
| ($reviews | map(select((.decision == "approve" or .decision == "request_changes")
                         and .contract == null and .ts < $contract_ts)) | length) as $skipped
# 未知 decision 的历史豁免也计入同一个计数：两者都是「契约前、口径未定型」的条目。
| (($reviews | map(select(.decision != "comment"
                         and .decision != "approve" and .decision != "request_changes"
                         and .ts < $contract_ts)) | length) + $skipped) as $skipped
| (
    [
      # (a) 判决类 review 的契约一致性
      ( $reviews[] | . as $r
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
          else empty end ),

      # (b) blocking 轮数 ≤ 2
      ( if ($blocking_rounds > 2) then
          "thread blocking 轮数 \($blocking_rounds) > 2（应升级为架构问题，不再开第 3 轮）"
        else empty end ),

      # (c) 可合并：thread 内最后一条（ts ≥ 契约阈值）判决类 review 必须是 blocking=0 的 approve。
      #     必须在**合并前**就能判（旧写法以「已有 merge_result」为前置 ⇒ 活卡恒绿，评审 blocking 项）。
      ( ( $reviews
          | map(select(.ts >= $contract_ts
                       and (.decision == "approve" or .decision == "request_changes"))) ) as $post
        | if ($post | length) > 0 then
            ($post[-1]) as $v
            | if ($v.decision != "approve"
                  or ($v.contract != null and $v.contract.blocking > 0)) then
                "thread 最后一条判决类 review（\($v.oid)）不是 blocking=0 的 approve（decision=\($v.decision)）⇒ 不得合并"
              else empty end
          else empty end ),

      # (d) 独立评审（§0b）：判决类 review 的 actor 不得与本 thread 任何 patch 的 actor 相同。
      #     为什么必须机检：自审可以签出一条完全合法的 approve（契约行、标签、轮次全都对），
      #     上面 (a)-(c) 一条都拦不住；而 author 自己的 approve 从不算满足闸门。
      #     口径：只看 ts ≥ 契约阈值的 review（历史条目豁免，与 (a) 同口径）。
      ( $reviews[] | . as $r
        | select($r.ts >= $contract_ts
                 and ($r.decision == "approve" or $r.decision == "request_changes")
                 and $r.actor != null
                 and ($patch_actors | index($r.actor)) != null)
        | "\($r.oid): review 的 actor=\($r.actor) 同时是本 thread 的 patch 作者 ⇒ 自审不算独立评审（§0b）" ),

      # (e) 终审不得与合并者同人（§4）：已合并的 thread，最后一条判决类 review 与 merge_result 的 actor 必须不同。
      #     注意：同一个人多轮评审（request_changes → approve）是**合法**的，不在这里拦；只有
      #     “评审者又去合并”才是违规——这两件事极易混，样本里各有对应用例。
      ( if ($merge_items | length) > 0 then
          ( $reviews
            | map(select(.ts >= $contract_ts
                         and (.decision == "approve" or .decision == "request_changes"))) ) as $post2
          | ($merge_items[-1]) as $m
          | if ($post2 | length) > 0 then
              ($post2[-1]) as $last
              | if ($m.actor != null and $last.actor != null and $m.actor == $last.actor) then
                  "\($last.oid): 终审 review 与 merge_result(\($m.oid)) 同为 \($m.actor) ⇒ 终审不得与合并者同人（§4）"
                else empty end
            else empty end
        else empty end ),

      # (f) 签名未验签的**判决性**条目不得参与合并：patch / review / merge_result / card_claim。
      #     为什么只算判决性条目：`comment` 上的未验签往往只是“写错身份重签”的记录，
      #     它污染的是审计叙述，不是代码授权；拿它堵住整张卡会让事故记录变成永久阻塞。
      ( $items[]
        | select(.verified == false
                 and (.e.kind == "patch" or .e.kind == "review"
                      or .e.kind == "merge_result" or .e.kind == "card_claim")
                 and (.e.ts // 0) >= $contract_ts)
        | "\(.oid): \(.e.kind) 条目签名验不过（verified=false）⇒ 不得据它合并" )
    ]
  ) as $violations| { violations: $violations,
    summary: { reviews: ($reviews | length),
               blocking_rounds: $blocking_rounds,
               skipped_historical: $skipped,
               merges: ($merges | length) },
    reviews: [ $reviews[] | {oid, ts, decision, contract} ] }
