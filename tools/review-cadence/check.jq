# review cadence guard core — input: `walgit collab thread <id>` JSON array.
# Requires: --argjson contract_ts <epoch seconds>（契约落地时间阈值）。
#
# 判据（与 docs/review-findings-contract.md 一致）：
#   (a) 每条**判决类** review（approve / request_changes）的 decision 必须与其 note 顶部的
#       `findings: blocking=N non-blocking=M` 契约行一致：
#         blocking>0 ⇒ request_changes；blocking=0 ⇒ approve；
#       若 note 里同时用了逐条 `[blocking]` / `[non-blocking]` 标签，标签计数必须与契约行相等；
#       ts < contract_ts 的历史条目缺契约行 ⇒ 跳过并计数（不判红）；ts ≥ contract_ts 缺 ⇒ 判红。
#   (b) 同一 thread 的 blocking 轮数（契约行 blocking>0 的**判决类** review 条数）≤ 2。
#   (c) thread 内最后一条（ts ≥ contract_ts）判决类 review 必须是 blocking=0 的 approve；
#       否则判红。该判据在**合并前**即可判定（不依赖是否已有 merge_result）。
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
| [range(0; ($raw | length)) | {idx: ., oid: $raw[.].oid, e: $raw[.].entry}] as $items
| [ $items[]
    | select(.e.kind == "review")
    | { idx: .idx,
        oid: .oid,
        ts: (.e.ts // 0),
        decision: (.e.body.decision // null),
        contract: ((.e.body.note // "") | contract_of),
        labels: ((.e.body.note // "") | label_counts) } ] as $reviews
| [ $items[] | select(.e.kind == "merge_result" and (.e.body.merged == true)) | .idx ] as $merges
| ($reviews | map(select(.decision != "comment" and .contract != null and .contract.blocking > 0)) | length) as $blocking_rounds
| ($reviews | map(select((.decision == "approve" or .decision == "request_changes")
                         and .contract == null and .ts < $contract_ts)) | length) as $skipped
| (
    [
      # (a) 判决类 review 的契约一致性
      ( $reviews[] | . as $r
        | if ($r.decision == "comment") then empty
          elif ($r.decision != "approve" and $r.decision != "request_changes") then
            "\($r.oid): 未知 decision=\($r.decision)"
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
          else empty end )
    ]
  ) as $violations
| { violations: $violations,
    summary: { reviews: ($reviews | length),
               blocking_rounds: $blocking_rounds,
               skipped_historical: $skipped,
               merges: ($merges | length) },
    reviews: [ $reviews[] | {oid, ts, decision, contract} ] }
