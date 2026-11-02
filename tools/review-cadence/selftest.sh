#!/usr/bin/env bash
# selftest.sh — tools/check_review_cadence.sh 的自测
#
# 为什么存在：守卫自身也会失效（正则写错、阈值判反、jq 程序崩），必须能用
# **故意违规的样本**证明它真的会红（RULE_规则可执行性.md）。本脚本随卡提交，
# 并被 .walgit/ci.toml 的 `review-cadence-selftest` 任务执行（可机查、不依赖 walgit/网络）。
#
# 用法：tools/review-cadence/selftest.sh
# 退出码：0 = 全部用例符合预期；1 = 有用例不符合预期。
#
# 阈值邻域：所有样本用 --contract-ts 1000000 / --no-review-ts 1000000 显式喂入，
#   契约阈值（判据 (a)-(f)）：ts=999999 ⇒ 契约前（缺契约行跳过）；ts=1000000 ⇒ 恰好阈值
#     （视为契约后，缺则判红）；ts=1000001 ⇒ 契约后。
#   无评审阈值（判据 (g)）：merge ts=999999 ⇒ 新规前（祖父条款豁免）；ts=1000000 ⇒ 恰好阈值
#     **必须判**（`>=` 不是 `>`）；ts=1000001 ⇒ 新规后。这两条邻域用例是「阈值方向」的
#     唯一守卫（实测：把 (g) 的 `>=` 改成 `>`，之前 40 例全绿 ⇒ 现已由
#     bad_merged_at_no_review_ts / ok_merged_before_no_review_ts 各自钉住）。
set -uo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)"
check="$here/../check_review_cadence.sh"
samples="$here/samples"
ts=1000000

pass=0
fail=0

# expect_rc <描述> <期望 rc> <命令...>
expect_rc() {  local desc="$1" want="$2"
  shift 2
  local out rc
  out="$("$@" 2>&1)"
  rc=$?
  if [ "$rc" -eq "$want" ]; then
    pass=$((pass + 1))
    printf 'ok   (%d) %s\n' "$rc" "$desc"
  else
    fail=$((fail + 1))
    printf 'FAIL (want %d, got %d) %s\n' "$want" "$rc" "$desc"
    printf '%s\n' "$out" | sed 's/^/     | /'
  fi
}

# expect_violations <描述> <期望违规条数> <命令...>
# 为什么要数条数：判据 (g) 改成「对每一条 merge_result 各判一次」后，最危险的回归是
# **只判一条**（首条/末条）—— 那样多条违规仍会 rc=1，单看 rc 看不出差别。
# rc 也一并断言（want=0 ⇒ rc=0，want>0 ⇒ rc=1），避免「条数对但退出码错」的漏网。
expect_violations() {
  local desc="$1" want="$2"
  shift 2
  local out rc got want_rc=1
  [ "$want" -eq 0 ] && want_rc=0
  out="$("$@" 2>&1)"
  rc=$?
  got="$(printf '%s\n' "$out" | grep -c '^  - ' || true)"
  if [ "$got" -eq "$want" ] && [ "$rc" -eq "$want_rc" ]; then
    pass=$((pass + 1))
    printf 'ok   (violations=%d) %s\n' "$got" "$desc"
  else
    fail=$((fail + 1))
    printf 'FAIL (want %s violations & rc=%d, got %s & rc=%d) %s\n' "$want" "$want_rc" "$got" "$rc" "$desc"
    printf '%s\n' "$out" | sed 's/^/     | /'
  fi
}

# expect_msg <描述> <输出里必须出现的片段> <命令...>
expect_msg() {
  local desc="$1" needle="$2"
  shift 2
  local out
  out="$("$@" 2>&1 || true)"
  if printf '%s' "$out" | grep -qF -- "$needle"; then
    pass=$((pass + 1))
    printf 'ok   (msg ∋ %s) %s\n' "$needle" "$desc"
  else
    fail=$((fail + 1))
    printf 'FAIL (输出缺片段「%s」) %s\n' "$needle" "$desc"
    printf '%s\n' "$out" | sed 's/^/     | /'
  fi
}

echo "── check_review_cadence 自测（contract-ts=${ts}；no-review-ts=${ts}）"

# ── 前置：jq 程序必须能编译（变异/编辑把 jq 写崩时，下面的用例会集体「红」，
#    看起来像覆盖良好，实际是语法炸 —— 2026-11-01 实测踩过：一次变异少写一个括号，
#    jq 编译失败，输出「红 56 例」。rc=2（工具错误）与 rc=1（真违规）在 expect_rc
#    眼里都是 FAIL，必须在这里先分开。 ──────────────────────────────────────
preflight_ok=0
if jq -n -f "$here/../review-cadence/check.jq" --argjson contract_ts "$ts" --argjson no_review_ts "$ts" >/dev/null 2>&1; then
  preflight_ok=1
else
  out="$(jq -n -f "$here/../review-cadence/check.jq" --argjson contract_ts "$ts" --argjson no_review_ts "$ts" 2>&1)"
fi
if [ "$preflight_ok" -eq 1 ]; then
  pass=$((pass + 1))
  echo 'ok   (0) 前置：check.jq 可编译'
else
  fail=$((fail + 1))
  echo 'FATAL check.jq 编译失败（下面所有用例的红绿都不可信）：'
  printf '%s\n' "$out" | sed 's/^/     | /'
  echo "── pass=$pass fail=$fail"
  exit 1
fi

# ── 合规样本（rc=0）─────────────────────────────────────────────────────────
for s in ok_basic ok_comment_exempt ok_comment_exempt_blocking ok_no_reviews ok_merge_resolved ok_historical_missing \
         ok_unknown_decision_historical ok_same_reviewer_multi_round ok_unverified_comment \
         ok_historical_contract_mismatch ok_merged_with_independent_waiver ok_merged_before_no_review_ts \
         ok_merged_historical_review_by_merger ok_merged_multi_merge_not_by_reviewer \
         ok_merged_reviewed_before_merge ok_merged_blocking_then_fixed \
         ok_merged_review_on_ancestor_chain ok_merged_two_merges_each_reviewed; do
  expect_rc "合规：$s" 0 "$check" --json "$samples/$s.json" --contract-ts "$ts" --no-review-ts "$ts"
done

# ── 违规样本（rc=1）─────────────────────────────────────────────────────────
for s in bad_missing_after_threshold bad_missing_at_threshold bad_label_mismatch \
         bad_clean_but_request_changes bad_blocking_but_approve bad_three_rounds \
         bad_merge_unresolved bad_premerge_unresolved bad_unknown_decision_after_threshold \
         bad_self_review bad_final_review_by_merger bad_unverified_patch \
         bad_decoy_patch_after_self_review bad_review_array_order_bypass \
         bad_last_review_contract_mismatch bad_last_review_blocks_but_approves \
         bad_tie_blocking_hidden_first bad_tie_blocking_hidden_last \
         bad_tie_self_review_first bad_tie_self_review_last \
         bad_merged_without_any_review bad_merged_with_self_waiver \
         bad_merged_at_no_review_ts bad_merged_backdated_merge_tail \
         bad_merged_grandfathered_then_violation bad_merged_two_violations \
         bad_merged_only_comment_decision_review bad_merged_approved_after_merge \
         bad_merged_review_at_merge_ts bad_merged_while_blocking \
         bad_waiver_backdated_by_ts bad_merged_review_forked_off_chain \
         bad_merged_review_second_root bad_waiver_forked_off_chain \
         bad_merged_before_uncontracted_blocking bad_merged_before_approved_with_blocking \
         bad_merged_parent_skips_review bad_merged_parent_skips_head_review \
         bad_merged_backdated_review_offsets_chain \
         bad_waiver_prose_mention bad_waiver_prose_refusal \
         bad_merged_with_merger_waiver bad_waiver_after_merge \
         bad_waiver_by_earlier_patch_author bad_waiver_unverified \
         bad_waiver_before_no_review_ts bad_waiver_missing_actor \
         bad_merged_backdated_merge_tail_hides_final_reviewer \
         bad_merged_two_final_reviewer_merges; do
  expect_rc "违规：$s" 1 "$check" --json "$samples/$s.json" --contract-ts "$ts" --no-review-ts "$ts"
done

# ── 违规**条数**：判据 (g) 逐条 merge 判，多条违规不得合并成一条、也不得丢 ──────
for pair in "ok_merged_before_no_review_ts 0" "bad_merged_two_violations 2" \
            "bad_merged_backdated_merge_tail 1" "bad_merged_grandfathered_then_violation 1"; do
  set -- $pair
  expect_violations "违规条数：$1" "$2" "$check" --json "$samples/$1.json" --contract-ts "$ts" --no-review-ts "$ts"
done

# ── 违规**条数**：判据 (e) 也是逐条 merge 判（abb-worker-78 的同类洞）──────────────
#    只判某一条 merge ⇒ 一条 ts 回溯的尾部 merge 就能洗白「终审 = 合并者」。
#    所以两个用例：回溯尾部那条**遮不住**真违规（仍 1 条），两条 merge 同人则**各报一条**（2 条）。
for pair in "bad_merged_backdated_merge_tail_hides_final_reviewer 1" \
            "bad_merged_two_final_reviewer_merges 2"; do
  set -- $pair
  expect_violations "(e) 违规条数：$1" "$2" "$check" --json "$samples/$1.json" --contract-ts "$ts" --no-review-ts "$ts"
done

# ── 违规**理由**：每条豁免失效必须报出对应原因，而不是笼统地变成“另一处坏了”───────
#    （否则把 blocking-2 的某个子约束删掉，用例只会因为其他原因变红，等于没有守卫）
expect_msg "豁免事后补签的报因" "不得事后补签" \
  "$check" --json "$samples/bad_waiver_after_merge.json" --contract-ts "$ts" --no-review-ts "$ts"
expect_msg "豁免由某份 patch 作者签的报因" "是本 thread 某份 patch 的作者" \
  "$check" --json "$samples/bad_waiver_by_earlier_patch_author.json" --contract-ts "$ts" --no-review-ts "$ts"
expect_msg "豁免由合并者签的报因" "就是本次合并的执行者" \
  "$check" --json "$samples/bad_merged_with_merger_waiver.json" --contract-ts "$ts" --no-review-ts "$ts"
expect_msg "豁免未验签的报因" "签名验不过" \
  "$check" --json "$samples/bad_waiver_unverified.json" --contract-ts "$ts" --no-review-ts "$ts"
expect_msg "豁免早于新规的报因" "早于无评审新规" \
  "$check" --json "$samples/bad_waiver_before_no_review_ts.json" --contract-ts "$ts" --no-review-ts "$ts"
# (e) 的报因必须来自 (e) 本身：回溯尾部样本若因别的判据变红（如 (g)），这条断言就会失败。
expect_msg "(e) 终审=合并者的报因" "终审不得与合并者同人" \
  "$check" --json "$samples/bad_merged_backdated_merge_tail_hides_final_reviewer.json" --contract-ts "$ts" --no-review-ts "$ts"

# ── 守卫自身失效模式 / 用法错误（rc=2）───────────────────────────────────────
expect_rc "JSON 文件不存在" 2 "$check" --json "$samples/does-not-exist.json" --contract-ts "$ts"
expect_rc "JSON 非法" 2 "$check" --json "$samples/bad_invalid_json.json" --contract-ts "$ts"
expect_rc "无参数" 2 "$check"
expect_rc "同时给 thread-id 与 --json" 2 "$check" some-thread --json "$samples/ok_basic.json"
expect_rc "非整数阈值" 2 "$check" --json "$samples/ok_basic.json" --contract-ts abc
expect_rc "顶层非数组（异常应 rc=2，不得漏成 0/5）" 2 "$check" --json "$samples/bad_nonarray_object.json" --contract-ts "$ts"

# thread 取不到（真实 walgit 路径）——用假 walgit 触发非零退出，不依赖真实 walgit/网络
shim="$(mktemp -d)"
printf '#!/bin/sh\nexit 1\n' > "$shim/walgit"
chmod +x "$shim/walgit"
expect_rc "thread 取不到（walgit 非零 ⇒ rc=2）" 2 env PATH="$shim:$PATH" "$check" some-nonexistent-thread
rm -rf "$shim"

# D68 回归：walgit 把本地子命令关在 `WALGIT_LOCAL_COMMANDS=1` 后面（对外面是 MCP `/mcp`），
# 而门禁脚本读的是本检出的 refs、不该去要 MCP 的 token。裸调会在任何机器上直接 rc=2，
# 且上面那条 shim 用例看不出区别（shim 总是 exit 1）——所以这里用一个会**记录自己看到的
# 环境**的 shim：只要脚本没带这个变量，就明显变红并指出原因。
shim2="$(mktemp -d)"
cat > "$shim2/walgit" <<'SHIM'
#!/bin/sh
if [ "${WALGIT_LOCAL_COMMANDS:-}" != "1" ]; then
  echo "shim: WALGIT_LOCAL_COMMANDS not set — 门禁脚本裸调了 walgit collab（D68 会拒）" >&2
  exit 9
fi
echo '[]'
SHIM
chmod +x "$shim2/walgit"
expect_rc "D68：门禁脚本必须带 WALGIT_LOCAL_COMMANDS=1 调 walgit" 0 \
  env PATH="$shim2:$PATH" "$check" some-thread --contract-ts "$ts" --no-review-ts "$ts"
rm -rf "$shim2"

echo "── pass=$pass fail=$fail"
[ "$fail" -eq 0 ]
