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
# 阈值邻域：所有样本用 --contract-ts 1000000 显式喂入，
#   ts=999999 ⇒ 契约前（缺契约行跳过）；ts=1000000 ⇒ 恰好阈值（视为契约后，缺则判红）；
#   ts=1000001 ⇒ 契约后。
set -uo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)"
check="$here/../check_review_cadence.sh"
samples="$here/samples"
ts=1000000

pass=0
fail=0

# expect_rc <描述> <期望 rc> <命令...>
expect_rc() {
  local desc="$1" want="$2"
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

echo "── check_review_cadence 自测（contract-ts=${ts}）"

# ── 合规样本（rc=0）─────────────────────────────────────────────────────────
for s in ok_basic ok_comment_exempt ok_comment_exempt_blocking ok_no_reviews ok_merge_resolved ok_historical_missing \
         ok_unknown_decision_historical ok_same_reviewer_multi_round ok_unverified_comment \
         ok_historical_contract_mismatch; do
  expect_rc "合规：$s" 0 "$check" --json "$samples/$s.json" --contract-ts "$ts"
done

# ── 违规样本（rc=1）─────────────────────────────────────────────────────────
for s in bad_missing_after_threshold bad_missing_at_threshold bad_label_mismatch \
         bad_clean_but_request_changes bad_blocking_but_approve bad_three_rounds \
         bad_merge_unresolved bad_premerge_unresolved bad_unknown_decision_after_threshold \
         bad_self_review bad_final_review_by_merger bad_unverified_patch \
         bad_decoy_patch_after_self_review bad_review_array_order_bypass \
         bad_last_review_contract_mismatch bad_last_review_blocks_but_approves \
         bad_tie_blocking_hidden_first bad_tie_blocking_hidden_last \
         bad_tie_self_review_first bad_tie_self_review_last; do
  expect_rc "违规：$s" 1 "$check" --json "$samples/$s.json" --contract-ts "$ts"
done

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

echo "── pass=$pass fail=$fail"
[ "$fail" -eq 0 ]
