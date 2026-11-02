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
# 陈旧数据防护（card abb-d68-local-commands-20261102）：MCP 的 `collab_publish` **不写本地
# refs**，而本门禁读的是本地 refs。若评审者用 MCP 发布 `request_changes`，不同步就会
# **看不到它而判绿**。所以 <> 模式默认先 `git fetch` 同步 `refs/collab/*`；同步失败即
# fail-closed（exit 2，宁红不假绿）。`--no-fetch` 或 `ABB_CADENCE_SKIP_FETCH=1` 跳过，
# 跳过等于把门禁降级为 best-effort。
#
# 注：walgit D68 起 `walgit collab` 不再是协作的对外接口（对外面是 MCP `/mcp`），
# 本地子命令默认拒绝执行；本脚本是**合并前的本地闸**，读的是本检出里的 refs、
# 不该为了它去要 MCP 的 token 与网络，因此显式走 walgit 保留的本地门
# `WALGIT_LOCAL_COMMANDS=1`（walgit 自己的文案即「survives for scripts and emergencies」）。
#
# 选项
#   --contract-ts <epoch>   契约落地时间阈值（秒）。缺省取 env ABB_REVIEW_CONTRACT_TS，
#                           再缺省取本脚本内常量（见下）。ts 小于该值的 review 视为**契约前**
#                           历史条目，缺契约行时跳过并计数、不判红。
#   --no-review-ts <epoch>  无评审新规生效时间（秒）。缺省取 env ABB_NO_REVIEW_TS，再缺省
#                           取本脚本常量 NO_REVIEW_TS_DEFAULT。判据 (g) 只对**此时间之后**
#                           的 merge_result 生效（更早的历史合并豁免，不追溯判红）。
#   -h | --help
#
# 判据（与 docs/review-findings-contract.md 一致；核心实现见 tools/review-cadence/check.jq）
#   (a) 判决类 review（approve / request_changes）note 顶部必须有契约行
#       `findings: blocking=N non-blocking=M`，且：
#         blocking>0 ⇒ decision=request_changes；blocking=0 ⇒ decision=approve；
#         若 note 另有逐条 `[blocking]` / `[non-blocking]` 标签，标签计数须与契约行相等；
#         契约前（ts < 阈值）缺契约行 ⇒ 跳过计数；契约后缺 ⇒ 判红。
#       历史未知 decision（曾出现 needs-changes / null）同口径：契约前跳过、契约后判红。
#   (b) 同一 thread 的 blocking 轮数（契约行 blocking>0 的判决类 review 条数）≤ 2。
#   (c) thread 内**最后一条**（ts ≥ 契约阈值）判决类 review 必须是 blocking=0 的 approve，
#       否则判红。该判据在**合并之前**即可跑（不依赖是否已有 merge_result）。
#   (d) 独立评审（§0b）：最后一条判决类 review 的 actor 不得与本 thread 任何 patch 的
#       actor 相同（自审就算签出合法 approve 也不算独立评审）。
#   (e) 终审不得与合并者同人（§4）：已合并的 thread，最后一条判决类 review 的 actor 与
#       **每一条** merge_result{merged:true} 的 actor 都必须不同（**逐条 merge 判**：只挑某一条
#       merge 时，一条 ts 回溯的尾部 merge 就能把本条洗白）。同一个人**多轮评审**是合法的，
#       不在此拦。历史豁免口径与 (a) 同：(e) 只看 ts ≥ 契约阈值的 review。
#   (f) 未验签（verified=false）的**判决性**条目（patch / review / merge_result）不得据以
#       合并。`comment` 上的未验签不算：那通常是“写错身份后重签”的记录。
#   (g) 合并于**无评审新规之后**却没有任何判决类 review ⇒ 判红。**逐条 merge 判**：每条
#       ts ≥ no_review_ts 的 merge_result{merged:true} 各判一次，多条违规各报一条。
#       降档豁免：由**独立第三方**签一条 comment，其 `body.waiver` 字段等于字符串
#       "no-independent-review"（**散文里提到该词不算**）；该 comment 还必须
#        ① ts ≥ no_review_ts ② 在该次合并的父链上（因果先于合并）（不得事后补签）③ verified ≠ false
#        ④ actor 既不是本 thread 任何一份 patch 的作者、也不是该次合并的执行者。
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
# 否则会把契约前的历史条目当成契约后而误红。改动本常量须重跑自测的时间阈值邻域用例
# （`bad_missing_at_threshold` = 恰好阈值必须判、`ok_historical_missing` = 阈值 -1 必须跳过）。
CONTRACT_TS_DEFAULT=1791876192

# 无评审新规生效时间（epoch 秒）：判据 (g) 只对**此时间之后**的 merge_result 生效。
# 推导（2026-11-01 扫全仓 refs/collab/inbox 的 4104 条 entry，命令见
# docs/review-findings-contract.md 的「常量推导（可复现）」）：
#   · 全仓 merge_result{merged:true} 共 199 条，**最大 ts = 1793356481**；
#   · 本常量取 1793524600（= 2026-11-01 09:16:40 UTC，判据 (g) 落地当刻），
#     比最后一条历史合并晚 168119 s ≈ 1.95 天 ⇒ 满足「严格大于全仓现存 merge ts」
#     这条硬约束，落地时**0 条**历史合并被追溯判红（含 owner 明确授权降档的那批）。
# **不得调低到 ≤ 1793356481**（否则追溯判红历史合并）；调高只会多豁免，但会放过真实违规。
# 改动本常量须重跑自测的 (g) 阈值邻域用例（`bad_merged_at_no_review_ts` = 恰好阈值必须判、
# `ok_merged_before_no_review_ts` = 阈值 -1 必须豁免）。
NO_REVIEW_TS_DEFAULT=1793524600

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)"
jq_program="$script_dir/review-cadence/check.jq"

# 打印本文件头部注释（第 2 行到 `set -euo pipefail` 之前一行）。
# 为什么不用 `sed -n '2,40p'`：行号是脆的——头部注释每加几行，帮助就会被**静默截断**
# （上一版加完判据列表后正好吃到 40 行边界，再添一行就会把「退出码」段整段吃掉）。
# 用标记行定位，头部注释多长都不会断。
usage() { awk 'NR == 1 { next } /^set -euo pipefail$/ { exit } { print }' "${BASH_SOURCE[0]}"; }

thread_id=""
json_input=""
contract_ts="${ABB_REVIEW_CONTRACT_TS:-$CONTRACT_TS_DEFAULT}"
no_review_ts="${ABB_NO_REVIEW_TS:-$NO_REVIEW_TS_DEFAULT}"
# 默认先同步 refs/collab/*（见下方陈旧数据防护）；1 = 跳过。
skip_fetch="${ABB_CADENCE_SKIP_FETCH:-0}"

while [ $# -gt 0 ]; do
  case "$1" in
    --json) [ $# -ge 2 ] || { echo "❌ --json 需要参数" >&2; exit 2; }; json_input="$2"; shift 2 ;;
    --no-fetch) skip_fetch=1; shift ;;
    --contract-ts) [ $# -ge 2 ] || { echo "❌ --contract-ts 需要参数" >&2; exit 2; }; contract_ts="$2"; shift 2 ;;
    --no-review-ts) [ $# -ge 2 ] || { echo "❌ --no-review-ts 需要参数" >&2; exit 2; }; no_review_ts="$2"; shift 2 ;;
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
case "$no_review_ts" in
  '' | *[!0-9]*) echo "❌ --no-review-ts 必须是整数秒：$no_review_ts" >&2; exit 2 ;;
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
# 「没走到判定就退 0」是这类守卫最危险的失效模式：bash 在 `set -u` 之类的内部错误后
# 会让 EXIT trap 看到 `$?`=0，于是一个**崩掉**的门禁对外报「通过」。
# 所以用一个显式凭据 `verdict_emitted`：只有真正打印了判定（通过/违规）才允许 0 退出；
# 否则一律按工具错误（exit 2）处理。这让「内部错误」不可能再伪装成绿。
verdict_emitted=0
trap 'rc=$?; rm -f "$tmp"; if [ "$verdict_emitted" != "1" ] && [ "$rc" -eq 0 ]; then rc=2; fi; exit "$rc"' EXIT

if [ -n "$json_input" ]; then
  if [ "$json_input" = "-" ]; then
    cat > "$tmp"
  else
    [ -f "$json_input" ] || { echo "❌ JSON 文件不存在：$json_input" >&2; exit 2; }
    cp "$json_input" "$tmp"
  fi
else
  # ── thread 数据源：MCP 优先，CLI 仅作过渡回落 ──────────────────────────────
  #
  # walgit 的协作面是「纯 git + MCP」：本地子命令自 D68 起默认拒绝执行，且**本地二进制
  # 将不再部署**（主机搬到服务器上）。所以门禁不能把 `walgit collab thread` 当唯一来源。
  #
  # 1) MCP（主路径）：`collab_thread` 返回服务端权威聚合——parent 链已排好、每条带
  #    verified/broken_refs。不需要本仓复刻聚合与验签（复刻判据语义的风险更大）。
  #    它读的是服务端状态，因此**没有「本地 refs 陈旧」问题**（`collab_publish` 不写
  #    本地 refs ⇒ 读本地 refs 会漏判评审，那是假绿）。
  # 2) CLI（过渡）：本机还留着二进制时可用，需要先 fetch 同步 refs/collab/*。
  # 3) 都没有 ⇒ fail-closed 并说清怎么配，不许拿陈旧数据判绿。
  fetched=0
  source=""
  mcp_py="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/review-cadence/mcp_thread.py"
  # 仓库名**从 origin 推导**，不要写死：主机会搬到服务器、路径可能变，写死会让门禁去查
  # 另一个仓库的同名 thread —— 那是**静默假绿**（拿别人的数据判自己的卡）。
  # 只在推导失败时退回本仓的规范名，并让调用方能用 WALGIT_REPO 覆盖。
  if [ -n "${WALGIT_REPO:-}" ]; then
    repo_slug="$WALGIT_REPO"
  else
    # 纯 bash 推导（**不依赖 python3/jq**：本脚本要在精简 PATH 下也能跑起来，
    # 依赖解释器会让「无凭据 ⇒ fail-closed」那条路径变成 command-not-found）。
    _url="$(git remote get-url origin 2>/dev/null || true)"
    _url="${_url%.git}"
    _url="${_url%/}"
    case "$_url" in
      *://*) : ;;                       # https://host/owner/repo
      *:*)   _url="${_url%%:*}/${_url#*:}" ;;  # git@host:owner/repo
    esac
    _owner="${_url%/*}"; _owner="${_owner##*/}"
    _repo="${_url##*/}"
    if [ -n "$_owner" ] && [ -n "$_repo" ]; then
      repo_slug="$_owner/$_repo"
    else
      repo_slug="gqf2008/abb"           # 推导失败才退回规范名
    fi
  fi
  if [ -n "${WALGIT_MCP_TOKEN:-}" ] && [ -f "$mcp_py" ]; then
    if python3 "$mcp_py" "$repo_slug" "$thread_id" > "$tmp" 2>"$tmp.err"; then
      source="MCP collab_thread"
    else
      mcp_rc=$?
      # 缺凭据（3）才回落；真的调用失败（4）不该被 CLI 悄悄盖过去。
      if [ "$mcp_rc" -ne 3 ]; then
        echo "❌ MCP 取不到 thread '$thread_id'（mcp_thread.py rc=${mcp_rc}）：" >&2
        cat "$tmp.err" >&2 || true
        echo "   ↳ 门禁不以 CLI 回落掩盖 MCP 失败（两者的数据源不同）。" >&2
        rm -f "$tmp.err"
        exit 2
      fi
    fi
  fi

  if [ -z "$source" ]; then
    if ! command -v walgit >/dev/null 2>&1; then
      echo "❌ 无法读取 thread '$thread_id'：既没有 WALGIT_MCP_TOKEN（MCP），本机也没有 walgit。" >&2
      echo "   ↳ 配一个只读/写入 token：export WALGIT_MCP_TOKEN=…（可选 WALGIT_MCP_URL / WALGIT_REPO）。" >&2
      echo "   ↳ 或用 --json <file> 喂一份离线 thread JSON。" >&2
      rm -f "$tmp.err"
      exit 2
    fi
    # 陈旧数据防护：CLI 读的是本地 refs，必须先同步。
    # `--no-fetch` / `ABB_CADENCE_SKIP_FETCH=1` 跳过（离线/自测），跳过 = 降级为 best-effort。
    if [ "$skip_fetch" != "1" ]; then
      if ! git fetch --quiet origin \
          'refs/collab/inbox/*:refs/collab/inbox/*' \
          'refs/collab/meta/*:refs/collab/meta/*' 2>"$tmp.err"; then
        echo "❌ 无法从 origin 同步 refs/collab/*（本地 refs 陈旧会漏判评审）：" >&2
        cat "$tmp.err" >&2 || true
        echo "   ↳ 确认要离线判定陈旧数据（best-effort，可能假绿）再显式加 --no-fetch。" >&2
        rm -f "$tmp.err"
        exit 2
      fi
      fetched=1
    fi
    if ! WALGIT_LOCAL_COMMANDS=1 walgit collab thread "$thread_id" > "$tmp" 2>"$tmp.err"; then
      echo "❌ 取不到 thread '$thread_id'（WALGIT_LOCAL_COMMANDS=1 walgit collab thread 非零退出）：" >&2
      cat "$tmp.err" >&2 || true
      rm -f "$tmp.err"
      exit 2
    fi
    source="walgit CLI（过渡）"
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

if ! result="$(jq -f "$jq_program" --argjson contract_ts "$contract_ts" --argjson no_review_ts "$no_review_ts" "$tmp")"; then
  echo "❌ jq 守卫执行失败（见上方错误）" >&2
  exit 2
fi

# 判定依据必须自报：「同步过」与「拿陈旧 refs」不能长得一样，否则协调者合并前
# 加 --no-fetch 就能让守卫给自己开绿灯，而账本里看不出差别（card
# abb-coordinator-bypassed-merge-gate-20261101 的同一个毛病）。
if [ -n "$json_input" ]; then
  basis="外部 JSON（离线输入）"
elif [ "${source:-}" = "MCP collab_thread" ]; then
  basis="MCP collab_thread @ ${repo_slug:-?}（服务端权威聚合）"
elif [ "${fetched:-0}" = "1" ]; then
  basis="walgit CLI + 已同步 refs/collab/*（过渡路径）"
else
  basis="⚠ walgit CLI + 未同步（陈旧本地 refs，best-effort）"
fi
echo "── review cadence：${thread_id:-<json>}（契约阈值 ts=${contract_ts}；无评审新规 ts=${no_review_ts}）"
echo "   判定依据：${basis}"
echo "$result" | jq -r '
  "review 条目 \(.summary.reviews) 条（其中判决类 \(.summary.judged_reviews) 条）；"
  + "blocking 轮数 \(.summary.blocking_rounds)；"
  + "历史跳过 \(.summary.skipped_historical) 条；merge_result(merged) \(.summary.merges) 条"'
violations="$(echo "$result" | jq -r '.violations[]')"
if [ -n "$violations" ]; then
  verdict_emitted=1
  echo "❌ 违规："
  echo "$violations" | sed 's/^/  - /'
  exit 1
fi
# 只有「走 CLI 且没同步」这一种组合是 best-effort：MCP 读服务端、--json 由调用方负责，
# 两者都不存在陈旧问题，必须打干净的 ✅（否则真绿也被降级，信噪比会崩）。
verdict_emitted=1
if [ -n "$json_input" ] || [ "${source:-}" = "MCP collab_thread" ] || [ "${fetched:-0}" = "1" ]; then
  echo "✅ 通过"
else
  echo "⚠️ 通过（未同步：本地 refs 可能陈旧，此结果 best-effort，不构成假绿防护）"
fi
