#!/usr/bin/env bash
# check_review_cadence.sh — 「审查收敛」机器守卫（card abb-review-cadence-gate-20261013）
#
# 作用：读一条 walgit thread 的条目，检查 review 的严重度**契约行**与轮次是否自洽，
# 把 `RULE_审查规范.md` 第 7–12 条从人核变成可机查。
#
# 用法
#   tools/check_review_cadence.sh <thread-id>          # 跑 `walgit collab thread <id>`
#   tools/check_review_cadence.sh --json <file|->      # 离线：直接喂 thread JSON（自测/样本）
#
# 选项
#   --contract-ts <epoch>   契约落地时间阈值（秒）。缺省取 env ABB_REVIEW_CONTRACT_TS，
#                           再缺省取本脚本内常量（见下）。ts 小于该值的 review 视为**契约前**
#                           历史条目，缺契约行时跳过并计数、不判红。
#   -h | --help
#
# 判据（与 docs/review-findings-contract.md 一致；核心实现见 tools/review-cadence/check.jq）
#   (a) 判决类 review（approve / request_changes）note 顶部必须有契约行
#       `findings: blocking=N non-blocking=M`，且：
#         blocking>0 ⇒ decision=request_changes；blocking=0 ⇒ decision=approve；
#         若 note 另有逐条 `[blocking]` / `[non-blocking]` 标签，标签计数须与契约行相等；
#         契约前（ts < 阈值）缺契约行 ⇒ 跳过计数；契约后缺 ⇒ 判红。
#   (b) 同一 thread 的 blocking 轮数（契约行 blocking>0 的 review 条数）≤ 2。
#   (c) 存在 merge_result{merged:true} 时，其前最后一次 blocking 轮之后必须有 blocking=0 的 approve。
#   `decision=="comment"` 的 review 不参与判定（不要求契约行、不计 blocking 轮）。
#
# 退出码
#   0 = 通过（可能含历史跳过）
#   1 = 违规（violations 非空）
#   2 = 用法/工具错误（缺 jq、thread 取不到、JSON 不可解析等）
#
# 依赖：jq（本仓随包工具之一）。契约落地时间常量 CONTRACT_TS_DEFAULT 见下方。
set -euo pipefail

# 契约落地时间（epoch 秒）。此日期之前的 review 视为历史，缺契约行跳过。
# = 「现存 review 最大 ts」+ 1（落地前扫全部 thread 得出，保证不误判历史）；**不得调低**，
# 否则会把契约前的历史条目当成契约后而误红。改动本常量须重跑自测的时间阈值邻域用例。
CONTRACT_TS_DEFAULT=1791876192

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)"
jq_program="$script_dir/review-cadence/check.jq"

usage() { sed -n '2,40p' "${BASH_SOURCE[0]}"; }

thread_id=""
json_input=""
contract_ts="${ABB_REVIEW_CONTRACT_TS:-$CONTRACT_TS_DEFAULT}"

while [ $# -gt 0 ]; do
  case "$1" in
    --json) [ $# -ge 2 ] || { echo "❌ --json 需要参数" >&2; exit 2; }; json_input="$2"; shift 2 ;;
    --contract-ts) [ $# -ge 2 ] || { echo "❌ --contract-ts 需要参数" >&2; exit 2; }; contract_ts="$2"; shift 2 ;;
    -h | --help) usage; exit 0 ;;
    --*) echo "❌ 未知参数：$1" >&2; exit 2 ;;
    *) thread_id="$1"; shift ;;
  esac
done

command -v jq >/dev/null 2>&1 || { echo "❌ 需要 jq（本仓随包工具）" >&2; exit 2; }
[ -f "$jq_program" ] || { echo "❌ 缺 jq 程序：$jq_program" >&2; exit 2; }
case "$contract_ts" in
  '' | *[!0-9]*) echo "❌ --contract-ts 必须是整数秒：$contract_ts" >&2; exit 2 ;;
esac

if [ -n "$json_input" ] && [ -n "$thread_id" ]; then
  echo "❌ 只能给 <thread-id> 或 --json 二者之一" >&2
  exit 2
fi
if [ -z "$json_input" ] && [ -z "$thread_id" ]; then
  echo "❌ 需要 <thread-id> 或 --json <file|->" >&2
  usage >&2
  exit 2
fi

tmp="$(mktemp)"
trap 'rm -f "$tmp"' EXIT

if [ -n "$json_input" ]; then
  if [ "$json_input" = "-" ]; then
    cat > "$tmp"
  else
    [ -f "$json_input" ] || { echo "❌ JSON 文件不存在：$json_input" >&2; exit 2; }
    cp "$json_input" "$tmp"
  fi
else
  if ! walgit collab thread "$thread_id" > "$tmp" 2>"$tmp.err"; then
    echo "❌ 取不到 thread '$thread_id'（walgit collab thread 非零退出）：" >&2
    cat "$tmp.err" >&2 || true
    rm -f "$tmp.err"
    exit 2
  fi
  rm -f "$tmp.err"
fi

if ! jq empty "$tmp" >/dev/null 2>&1; then
  echo "❌ 输入不是合法 JSON" >&2
  exit 2
fi
if ! jq -e 'type == "array"' "$tmp" >/dev/null 2>&1; then
  echo "❌ 输入顶层不是数组（thread 条目应为一个数组）" >&2
  exit 2
fi

if ! result="$(jq -f "$jq_program" --argjson contract_ts "$contract_ts" "$tmp")"; then
  echo "❌ jq 守卫执行失败（见上方错误）" >&2
  exit 2
fi

echo "── review cadence：${thread_id:-<json>}（契约阈值 ts=${contract_ts}）"
echo "$result" | jq -r '
  "review 条目 \(.summary.reviews) 条；blocking 轮数 \(.summary.blocking_rounds)；"
  + "历史跳过 \(.summary.skipped_historical) 条；merge_result(merged) \(.summary.merges) 条"'
violations="$(echo "$result" | jq -r '.violations[]')"
if [ -n "$violations" ]; then
  echo "❌ 违规："
  echo "$violations" | sed 's/^/  - /'
  exit 1
fi
echo "✅ 通过"
