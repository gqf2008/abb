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

# ── 判据 (c) 补：终审之后不得再有 patch（卡 abb-cadence-approve-then-patch-20261102）──
#    2026-11-02 的真实代价：评审 approve 了 head A，作者随后又推 B，而 (c) 只看
#    「最后一条是不是 approve」⇒ 门禁会把「从未被那轮评审看过的 head」放行。
#    三例各有分工，缺一不可：
#      ① 违规必须红（否则新判据是摆设）
#      ② approve 之后没有新 patch 必须绿（否则把正常工作流判红）
#      ③ 已合并的不追溯判红（(c) 是合并前闸门；追溯判红改不了事实，只会训练人忽略门禁），
#         但必须打一条 ⚠️ 告警——登记了不打印等于没登记。
expect_rc "终审后有 patch（未合并）⇒ 红" 1 \
  "$check" --json "$samples/bad_patch_after_approve.json" --contract-ts "$ts" --no-review-ts "$ts"
expect_rc "终审后无 patch ⇒ 绿（防误红）" 0 \
  "$check" --json "$samples/ok_no_patch_after_approve.json" --contract-ts "$ts" --no-review-ts "$ts"
expect_rc "已合并的过期终审不追溯 ⇒ 绿" 0 \
  "$check" --json "$samples/ok_merged_stale_review_not_retroactive.json" --contract-ts "$ts" --no-review-ts "$ts"
# 报因要指名「终审早于最后改动」，而不是笼统地变成别的错——否则新判据被别的原因顶住，
# 用例还是会"红"，但对这条判据零判别力。
# 注意：本脚本是 `set -uo pipefail`（第 18 行）——违规样本让 $check 返回 1，
# 直接写 `$check ... | grep -q` 时 pipefail 会把整条 pipeline 判成失败，
# 于是**即使 grep 匹配到也走 else**（实测：这两条用例曾因此假红）。
# 所以先捕获文本，再判内容，不依赖 pipeline 的退出码。
stale_msg="$("$check" --json "$samples/bad_patch_after_approve.json" \
             --contract-ts "$ts" --no-review-ts "$ts" 2>&1 || true)"
case "$stale_msg" in
  *"终审早于最后改动"*) pass=$((pass + 1)); echo 'ok   (1) 报因指名「终审早于最后改动」' ;;
  *) fail=$((fail + 1)); echo 'FAIL 报因没指名「终审早于最后改动」'
     printf '%s\n' "$stale_msg" | head -4 | sed 's/^/     | /' ;;
esac
# 已合并的过期终审要看得见（不阻塞但可审计）
merged_msg="$("$check" --json "$samples/ok_merged_stale_review_not_retroactive.json" \
              --contract-ts "$ts" --no-review-ts "$ts" 2>&1 || true)"
case "$merged_msg" in
  *"已合并的过期终审"*) pass=$((pass + 1)); echo 'ok   (0) 已合并的过期终审有可见告警' ;;
  *) fail=$((fail + 1)); echo 'FAIL 已合并的过期终审没有告警行（登记了看不见=没登记）'
     printf '%s\n' "$merged_msg" | head -4 | sed 's/^/     | /' ;;
esac

# ── 判据 (h)：`decisions` 字段形状（AGENTS.md 触发清单 T1–T5 的落地判据）──────────
#    为什么需要（2026-11-02 实证）：AGENTS.md 写着「有取舍先问 Jev」，而协调者整轮
#    **一次都没问**——因为把「替产品做的决定」归类成了技术判断。散文规则落不到具体时刻，
#    所以改成结构化字段 + 门禁查形状。
#    三红三绿的分工：假 rid / 自决越界 / 低置信包装成定论 各红一条；
#    纯技术自决、真问过且如实写明、无字段（纯技术卡）各绿一条（防误红）。
# `bad_decisions_ui_wording`：判据 (h) 第一版把**裸「文案」**放进词表，于是
# 「自己打 skip 文案」（改的是跳过提示=开发者/门禁输出）被判越界 —— 实测过一次误红，
# Jev 判 `fix_jq_wordlist` p=1.0/conf=1.0、`gate_false_positive` p=0.95/conf=0.92。
# 所以两条并排钉住**收窄后仍有效**：改界面文案 ⇒ 红；改跳过提示 ⇒ 绿。
for s in bad_decisions_fake_rid bad_decisions_self_decided_crosses_trigger \
         bad_decisions_low_confidence_not_stated bad_decisions_ui_wording \
         bad_decisions_affirmative_mention bad_decisions_corrects_itself; do
  expect_rc "(h) 违规：$s" 1 \
    "$check" --json "$samples/$s.json" --contract-ts "$ts" --no-review-ts "$ts"
done
for s in ok_decisions_self_decided_technical_only \
         ok_decisions_asked_with_low_confidence_stated ok_decisions_absent \
         ok_decisions_debug_output_wording ok_decisions_negated_mention \
         ok_decisions_corrected_by_later_patch; do
  expect_rc "(h) 合规：$s" 0 \
    "$check" --json "$samples/$s.json" --contract-ts "$ts" --no-review-ts "$ts"
done
# 报因要指对地方（否则被别的原因顶住，对这条判据零判别力）
for pair in "bad_decisions_fake_rid 假装问过" \
            "bad_decisions_self_decided_crosses_trigger 触发清单" \
            "bad_decisions_low_confidence_not_stated 包装成强结论" \
            "bad_decisions_ui_wording 触发清单" \
            "bad_decisions_affirmative_mention 触发清单"; do
  set -- $pair
  msg="$("$check" --json "$samples/$1.json" --contract-ts "$ts" --no-review-ts "$ts" 2>&1 || true)"
  case "$msg" in
    *"$2"*) pass=$((pass + 1)); echo "ok   (1) (h) 报因含「$2」" ;;
    *) fail=$((fail + 1)); echo "FAIL (h) $1 的报因没含「$2」"
       printf '%s\n' "$msg" | head -3 | sed 's/^/     | /' ;;
  esac
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
expect_rc "thread 取不到（walgit 非零 ⇒ rc=2）" 2 env -u WALGIT_MCP_TOKEN -u WALGIT_REPO ABB_CADENCE_SKIP_FETCH=1 PATH="$shim:$PATH" "$check" some-nonexistent-thread
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
  env -u WALGIT_MCP_TOKEN -u WALGIT_REPO ABB_CADENCE_SKIP_FETCH=1 PATH="$shim2:$PATH" "$check" some-thread --contract-ts "$ts" --no-review-ts "$ts"
rm -rf "$shim2"

# 陈旧数据防护：MCP 的 `collab_publish` 不写本地 refs，门禁读本地 refs 会漏判评审。
# 默认先 `git fetch refs/collab/*`；同步不了要 fail-closed，而不是拿陈旧数据判绿。
# 用一个 origin 打不通的临时仓验证（不碰网络、不在本仓里做）。
stale="$(mktemp -d)"
( cd "$stale" && git init -q . && git remote add origin "$stale/does-not-exist.git" ) >/dev/null 2>&1
shim3="$(mktemp -d)"
printf '#!/bin/sh\necho "[]"\n' > "$shim3/walgit"
chmod +x "$shim3/walgit"
# 同步失败 ⇒ rc=2（不许拿陈旧数据判绿）
expect_rc "同步失败必须 fail-closed（不许用陈旧 refs 判绿）" 2 \
  env -u WALGIT_MCP_TOKEN -u WALGIT_REPO PATH="$shim3:$PATH" sh -c "cd '$stale' && \"$check\" some-thread --contract-ts '$ts' --no-review-ts '$ts'"
# 显式 --no-fetch ⇒ 允许降级为 best-effort（空 thread 无判据可红）
expect_rc "--no-fetch 显式降级后可离线判定" 0 \
  env -u WALGIT_MCP_TOKEN -u WALGIT_REPO PATH="$shim3:$PATH" sh -c "cd '$stale' && \"$check\" some-thread --no-fetch --contract-ts '$ts' --no-review-ts '$ts'"
rm -rf "$stale" "$shim3"

# 「判定依据」必须自报状态：同步过与拿陈旧 refs 不能长得一样。否则协调者合并前
# 加 --no-fetch 就能让守卫给自己开绿灯，而输出里看不出差别（同一个毛病：
# abb-coordinator-bypassed-merge-gate-20261101）。
shim4="$(mktemp -d)"
printf '#!/bin/sh\necho "[]"\n' > "$shim4/walgit"
chmod +x "$shim4/walgit"
out_nofetch="$(env -u WALGIT_MCP_TOKEN -u WALGIT_REPO ABB_CADENCE_SKIP_FETCH=1 PATH="$shim4:$PATH" "$check" some-thread --contract-ts "$ts" --no-review-ts "$ts" 2>&1)"
# 必须锚在「判定依据：」这一行上：末尾的「⚠️ 通过（未同步…）」本身也含「未同步」，
# 只 grep 全输出会让「basis 硬编码成已同步」这种改法也通过（评审 N1 实测）。
basis_line="$(printf '%s' "$out_nofetch" | grep '判定依据' || true)"
if [ -n "$basis_line" ] && printf '%s' "$basis_line" | grep -q '未同步'; then
  pass=$((pass + 1)); echo 'ok   (0) 判定依据行必须自报「未同步」（不许与同步过长得一样）'
else
  fail=$((fail + 1)); echo 'FAIL 判定依据行没有自报未同步，--no-fetch 可静默降级'
  printf '%s\n' "$basis_line" | sed 's/^/     | /'
fi
if printf '%s' "$out_nofetch" | grep -q '✅ 通过'; then
  fail=$((fail + 1)); echo 'FAIL 未同步仍打干净的 ✅ 通过（应打带警示的通过）'
else
  pass=$((pass + 1)); echo 'ok   (0) 未同步时不得打干净的 ✅ 通过'
fi
rm -rf "$shim4"

# ── 数据源：MCP 优先，CLI 只作过渡回落，都没有则 fail-closed ────────────────
# walgit 本地二进制将不再部署（协作面 = 纯 git + MCP）。门禁不能只认 CLI。
# 这里的断言不需要真实 MCP/网络：只验「没有凭据且没有 walgit」时是否 fail-closed，
# 以及给了 token 时是否**尝试** MCP（用假 python3 模拟，不触网）。
shim5="$(mktemp -d)"
# 一个「既无 walgit 也无 token」的 PATH：门禁必须 exit 2 并说明怎么配
mkdir -p "$shim5/bin"
for t in git jq awk sed grep bash sh env mktemp dirname basename cat rm printf; do
  real=$(command -v "$t" 2>/dev/null) && ln -sf "$real" "$shim5/bin/$t" 2>/dev/null
done
# python3 故意不放：模拟「本机什么都没有」，同时确认不会静默判绿
expect_rc "无 MCP token 且无 walgit ⇒ fail-closed（不许判绿）" 2 \
  env -u WALGIT_MCP_TOKEN PATH="$shim5/bin" "$check" some-thread --contract-ts "$ts" --no-review-ts "$ts"
rm -rf "$shim5"

# MCP 路径：假 mcp_thread.py 返回 []，断言走的是 MCP（判定依据里有 "MCP"）
# 并断言 MCP 路径**不**打 best-effort 的 ⚠（它读服务端，不存在陈旧问题）。
mcpshim="$(mktemp -d)"
mkdir -p "$mcpshim/review-cadence"
# 门禁自身需要同目录的 check.jq；这里把真 jq 程序带过去，只替换 mcp_thread.py。
cp "$here/check.jq" "$mcpshim/review-cadence/check.jq"
cat > "$mcpshim/review-cadence/mcp_thread.py" <<'PYEOF'
import sys
sys.stdout.write("[]")
PYEOF
cp "$check" "$mcpshim/check.sh"
chmod +x "$mcpshim/check.sh"
mout="$(WALGIT_MCP_TOKEN=dummy "$mcpshim/check.sh" some-thread --contract-ts "$ts" --no-review-ts "$ts" 2>&1)"
mrc=$?
if [ "$mrc" -eq 0 ] && printf '%s' "$mout" | grep -q 'MCP collab_thread'; then
  pass=$((pass + 1)); echo 'ok   (0) 有 token 时走 MCP 数据源'
else
  fail=$((fail + 1)); echo "FAIL MCP 路径没走通（rc=${mrc}）"
fi
if printf '%s' "$mout" | grep -q '✅ 通过'; then
  pass=$((pass + 1)); echo 'ok   (0) MCP 路径打干净的 ✅（服务端读，无陈旧问题）'
else
  fail=$((fail + 1)); echo 'FAIL MCP 路径被误降级为 best-effort'
fi
rm -rf "$mcpshim"

# ── mcp_thread.py 的退出码契约 ──────────────────────────────────────────────
# 门禁的数据源已改为「MCP 优先」，所以这个取数器的失败面必须自己可测：
#   2 = 用法错  3 = 缺凭据（调用方据此回落）  4 = MCP 调用失败（不许回落掩盖）
MCP_PY="$here/mcp_thread.py"
expect_rc "mcp_thread：参数缺失 ⇒ 2" 2 python3 "$MCP_PY"
expect_rc "mcp_thread：缺 WALGIT_MCP_TOKEN ⇒ 3（可回落）" 3 \
  env -u WALGIT_MCP_TOKEN python3 "$MCP_PY" gqf2008/abb some-thread
# 打不通的端口 ⇒ 4（不是 3：凭据在，是调用失败，调用方不得静默回落）
expect_rc "mcp_thread：端点打不通 ⇒ 4（不得伪装成缺凭据）" 4 \
  env WALGIT_MCP_TOKEN=dummy WALGIT_MCP_URL=http://127.0.0.1:1/mcp \
  python3 "$MCP_PY" gqf2008/abb some-thread

# ── lint：$var 紧跟全角字符（unbound variable，且被 EXIT trap 吞成 exit 0）──────
# 这个坑在本仓已出现三次（pass / fail / mcp_rc 各一次）。它不只是笔误：
#   · bash 会把紧随 $var 的多字节字符算进变量名 ⇒ "unbound variable"；
#   · `set -u` 触发后 **EXIT trap 看到的状态是 0**，脚本于是以 0 退出 ——
#     也就是「门禁内部出错，却对外报绿」。契约要求内部错误必须非零，所以要防复发。
# 修法：写 ${var}）。扫描范围 = `git ls-files` 的已跟踪 shell 脚本（CI 稳定），
# 跳过注释行（注释里提到该模式本身不算违规）。
lint_out="$(git ls-files '*.sh' | python3 tools/review-cadence/lint_shell_vars.py)"
if [ -z "$lint_out" ]; then
  pass=$((pass + 1)); echo 'ok   (0) lint：无「$var 紧跟全角字符」（否则内部错误会退化成 exit 0）'
else
  fail=$((fail + 1)); echo 'FAIL lint：以下行的 $var 紧跟全角字符，会变成 unbound variable 且被 trap 吞成 exit 0：'
  printf '%s\n' "$lint_out" | sed 's/^/     | /'
fi

# ── 内部错误不得伪装成绿（verdict_emitted 守卫）────────────────────────────
# bash 在 `set -u` 这类内部错误后，EXIT trap 里看到的 `$?` 是 0 ⇒ 一个**崩掉**的门禁
# 会对外报「通过」。脚本因此有一个显式凭据：只有真正打印了判定才允许 0 退出。
# 这里把一份**注入了内部错误**的副本跑起来，断言它退 2 而不是 0。
internal="$(mktemp -d)"
mkdir -p "$internal/review-cadence"
cp "$here/check.jq" "$internal/review-cadence/check.jq"
python3 - "$check" "$internal/check.sh" <<'PYINJ'
import sys
src = open(sys.argv[1]).read()
# 注入点：最终判定**打印之前**（NB-1 之后 verdict_emitted 改为打印后置位，
# 所以这里锚在判定分支上，而不是锚在置位行上）。
needle = 'if [ -n "$json_input" ] || [ "${source:-}" = "MCP collab_thread" ]'
assert needle in src, "注入点没找到（脚本结构变了，请同步更新这条用例）"
src = src.replace(
    needle,
    'echo "internal=$this_var_does_not_exist\\uff09" >/dev/null\n' + needle,
)
open(sys.argv[2], "w").write(src)
PYINJ
chmod +x "$internal/check.sh"
expect_rc "内部错误（set -u）⇒ exit 2，不得伪装成绿" 2 \
  "$internal/check.sh" --json "$samples/ok_basic.json" --contract-ts "$ts" --no-review-ts "$ts"
rm -rf "$internal"

# ── 数据源隔离：MCP 调用失败（rc=4）绝不许回落到 CLI（评审 B3）──────────────
# 这是本卡最关键的不变量：MCP 坏了却换 CLI 判绿 = 假绿（而且没人知道 MCP 坏了）。
# 早先只测了 mcp_thread.py 自己返回 4，没测**消费方**怎么处理 4，于是把
# `-ne 3` 改成 `-gt 9`（等于 4 也允许回落）能绿着进主干。
#
# 做法：造一个**假 mcp_thread.py 返回 4** 的门禁副本，PATH 里放一个「必定成功且有留痕」
# 的 walgit shim。断言：① 门禁 rc=2；② 输出里没有 CLI 数据源字样；③ shim 没被调用过。
iso="$(mktemp -d)"
mkdir -p "$iso/review-cadence"
cp "$here/check.jq" "$iso/review-cadence/check.jq"
printf 'import sys\nsys.stderr.write("mcp boom\\n")\nsys.exit(4)\n' > "$iso/review-cadence/mcp_thread.py"
cp "$check" "$iso/check.sh"
chmod +x "$iso/check.sh"
isos="$(mktemp -d)"
printf '#!/bin/sh\ntouch "%s/called"\necho "[]"\n' "$isos" > "$isos/walgit"
chmod +x "$isos/walgit"
# 必须带 ABB_CADENCE_SKIP_FETCH=1：本用例要测的是「MCP 失败会不会回落 CLI」，
# 若允许 fetch，在无 origin 的环境里变异版会因 fetch 失败而 rc=2，**碰巧**满足
# 「rc=2」这一条断言 —— 于是变异逃逸（评审 NB-a 指出，实测确实如此）。
iso_out="$(WALGIT_MCP_TOKEN=dummy ABB_CADENCE_SKIP_FETCH=1 PATH="$isos:$PATH" "$iso/check.sh" some-thread \
  --contract-ts "$ts" --no-review-ts "$ts" 2>&1)"
iso_rc=$?
expect_rc_is() {  # 保持与 expect_rc 一致的口径，但复用已跑出的 rc/输出
  local desc="$1" want="$2" got="$3" out="$4"
  if [ "$got" -eq "$want" ]; then
    pass=$((pass + 1)); printf 'ok   (%d) %s\n' "$got" "$desc"
  else
    fail=$((fail + 1)); printf 'FAIL (want %d, got %d) %s\n' "$want" "$got" "$desc"
    printf '%s\n' "$out" | head -6 | sed 's/^/     | /'
  fi
}
expect_rc_is "MCP 返回 4 ⇒ 门禁 exit 2（不许回落 CLI）" 2 "$iso_rc" "$iso_out"
if printf '%s' "$iso_out" | grep -q 'walgit CLI'; then
  fail=$((fail + 1)); echo 'FAIL MCP 失败时输出里出现了 CLI 数据源（说明回落了）'
else
  pass=$((pass + 1)); echo 'ok   (0) MCP 失败时判定依据不含 CLI 数据源'
fi
if [ -f "$isos/called" ]; then
  fail=$((fail + 1)); echo 'FAIL MCP 失败时竟然调用过 walgit（回落已发生）'
else
  pass=$((pass + 1)); echo 'ok   (0) MCP 失败时根本没有调用 walgit'
fi
rm -rf "$iso" "$isos"

# 反向对照：MCP 缺凭据（rc=3）时**必须**允许回落 CLI —— 否则「一律 exit 2」
# 这种把回落整个砍掉的改法也会被判过（这个洞同样会伤可用性）。
allow="$(mktemp -d)"
mkdir -p "$allow/review-cadence"
cp "$here/check.jq" "$allow/review-cadence/check.jq"
printf 'import sys\nsys.exit(3)\n' > "$allow/review-cadence/mcp_thread.py"
cp "$check" "$allow/check.sh"
chmod +x "$allow/check.sh"
allows="$(mktemp -d)"
printf '#!/bin/sh\necho "[]"\n' > "$allows/walgit"
chmod +x "$allows/walgit"
allow_out="$(WALGIT_MCP_TOKEN=dummy ABB_CADENCE_SKIP_FETCH=1 PATH="$allows:$PATH" \
  "$allow/check.sh" some-thread --contract-ts "$ts" --no-review-ts "$ts" 2>&1)"
allow_rc=$?
rm -rf "$allow" "$allows"
expect_rc_is "MCP 缺凭据（rc=3）⇒ 允许回落 CLI（rc=0）" 0 "$allow_rc" "$allow_out"
if printf '%s' "$allow_out" | grep -q 'walgit CLI'; then
  pass=$((pass + 1)); echo 'ok   (0) rc=3 时判定依据显示走的是 CLI 回落路径'
else
  fail=$((fail + 1)); echo 'FAIL rc=3 时没有回落 CLI（可用性被砍掉）'
fi

# ── MCP 端点从 origin 推导（本机 walgit 服务退役后的正确性）──────────────────
# 背景：本机 walgit 服务正在退役，只能远程访问。若端点写死 `127.0.0.1:8081`，
# 迁移那一刻门禁就不可用；而 `origin` 一定会跟着迁移改（否则 git push 先就不工作了），
# 所以**从 origin 推导端点**是唯一自洽的默认值。
# 这里锁住三件事：① 各种 remote 形态推导正确；② 推不出来时 fail-closed（不猜）；
# ③ 显式 WALGIT_MCP_URL 优先于推导。
mcp_derive_out="$(python3 - "$MCP_PY" <<'PYDERIVE'
import importlib.util, sys
spec = importlib.util.spec_from_file_location("m", sys.argv[1])
m = importlib.util.module_from_spec(spec); spec.loader.exec_module(m)
cases = [
    # (origin, 期望端点)
    ("http://127.0.0.1:8081/gqf2008/abb.git", "http://127.0.0.1:8081/mcp"),
    ("https://walgit.example.com/gqf2008/abb.git", "https://walgit.example.com/mcp"),
    ("https://walgit.example.com:8443/gqf2008/abb", "https://walgit.example.com:8443/mcp"),
    ("git@walgit.example.com:gqf2008/abb.git", "https://walgit.example.com/mcp"),
    ("/local/path/repo.git", ""),
    ("", ""),
]
bad = []
for origin, want in cases:
    got = m.derive_mcp_url(origin)
    if got != want:
        bad.append(f"{origin!r} → {got!r}（期望 {want!r}）")
print("\n".join(bad))
PYDERIVE
)"
if [ -z "$mcp_derive_out" ]; then
  pass=$((pass + 1)); echo 'ok   (0) MCP 端点推导：http/https/端口/ssh 形态均正确，本地路径不猜'
else
  fail=$((fail + 1)); echo 'FAIL MCP 端点推导有误：'
  printf '%s\n' "$mcp_derive_out" | sed 's/^/     | /'
fi

# 推不出来时必须 fail-closed：给一个没有 origin 的临时仓，且不设 WALGIT_MCP_URL
noorig="$(mktemp -d)"
( cd "$noorig" && git init -q . ) >/dev/null 2>&1
# 只看 rc=4 不够：若默认值退回「写死本机地址」，本机恰好有服务时它会因为
# 「仓库不存在」也返回 4 —— 两种原因撞在一起，测试就分不清了（变异实测逃逸）。
# 所以还要断言**报因**是「无法确定端点」，而不是「MCP 调用失败」。
noorig_out="$(env -u WALGIT_MCP_URL WALGIT_MCP_TOKEN=dummy \
  sh -c "cd '$noorig' && python3 '$MCP_PY' owner/repo some-thread" 2>&1)"
noorig_rc=$?
if [ "$noorig_rc" -eq 4 ] && printf '%s' "$noorig_out" | grep -q '无法确定 MCP 端点'; then
  pass=$((pass + 1)); echo 'ok   (4) 无 origin 且未设 WALGIT_MCP_URL ⇒ 4 且报因是「无法确定端点」'
else
  fail=$((fail + 1)); echo "FAIL 无 origin 时应 fail-closed 且报「无法确定端点」（rc=${noorig_rc}）"
  printf '%s\n' "$noorig_out" | head -3 | sed 's/^/     | /'
fi
rm -rf "$noorig"

# 显式 WALGIT_MCP_URL 必须优先于推导（否则迁移时无法覆盖推导结果）
urltest="$(mktemp -d)"
( cd "$urltest" && git init -q . && git remote add origin https://derive-me.invalid/o/r.git ) >/dev/null 2>&1
# 直接问 resolve_mcp_url() 返回什么，而不是看错误文本（错误文本里可能不含 URL）
override_out="$(cd "$urltest" && WALGIT_MCP_URL=http://127.0.0.1:1/mcp python3 - "$MCP_PY" <<'PYOVR'
import importlib.util, sys
spec = importlib.util.spec_from_file_location("m", sys.argv[1])
m = importlib.util.module_from_spec(spec); spec.loader.exec_module(m)
url, src = m.resolve_mcp_url()
print(url)
PYOVR
)"
if [ "$override_out" = "http://127.0.0.1:1/mcp" ]; then
  pass=$((pass + 1)); echo 'ok   (0) 显式 WALGIT_MCP_URL 优先于 origin 推导'
else
  fail=$((fail + 1)); echo "FAIL 显式 WALGIT_MCP_URL 没有优先（拿到 ${override_out:-空}）"
fi
rm -rf "$urltest"

echo "── pass=$pass fail=$fail"
[ "$fail" -eq 0 ]
