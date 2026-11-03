#!/bin/sh
# 项目监督循环：每轮把「偏差」写到状态文件，供下一轮决策时读。
#
# ## 它做什么 / 不做什么
#
# **做**：确定性检查——main 有没有未推送的提交、看板有没有卡住、几个快守卫绿不绿。
# **不做**：替你决策。它只把事实写成结论 + 明细，决策留给读它的人（或 agent）。
#
# 为什么不假装能自主推进：「该做什么」需要读代码、权衡代价、判断取舍——shell 脚本做不了。
# **假装能做的脚本比没有更糟**：它会给出「一切正常」的假信号，而那正是本仓今天反复踩的坑。
#
# ## 三态而非二态
#
# 每个检查必须区分 **通过 / 偏差 / 无法判定**。
# `❓`（不知道）**绝不当成通过**——本仓教训：「skip 的默认行为是通过」。
#
# ## 用法
#
#     tools/supervise.sh            # 跑一轮，结果写状态文件 + stdout
#     tools/supervise.sh --quiet    # 只在有偏差时输出（定时任务用）
#
# 状态文件：`$ABB_SUPERVISE_STATE`（默认 /Volumes/DataExt/tmp/abb-supervise-status.txt）

set -u

cd "$(dirname "$0")/.." || exit 2

quiet=0
[ "${1:-}" = "--quiet" ] && quiet=1

state_file="${ABB_SUPERVISE_STATE:-/Volumes/DataExt/tmp/abb-supervise-status.txt}"
log_file="${ABB_SUPERVISE_LOG:-/Volumes/DataExt/tmp/abb-supervise.log}"

ok=0
warn=0
unknown=0
notes=""

note() { notes="$notes$1
"; }
pass_note() { ok=$((ok + 1)); note "  ✅ $1"; }
warn_note() { warn=$((warn + 1)); note "  ⚠️  $1"; }
unknown_note() { unknown=$((unknown + 1)); note "  ❓ $1"; }

# ── 1. main 与 origin/main 是否一致 ─────────────────────────────
if git rev-parse --verify -q origin/main >/dev/null 2>&1; then
    local_head=$(git rev-parse main 2>/dev/null || echo "")
    remote_head=$(git rev-parse origin/main 2>/dev/null || echo "")
    if [ -z "$local_head" ] || [ -z "$remote_head" ]; then
        unknown_note "main / origin/main 解析不出（git 异常？）"
    elif [ "$local_head" = "$remote_head" ]; then
        pass_note "main == origin/main（$(git rev-parse --short main 2>/dev/null)）"
    else
        ahead=$(git rev-list --count origin/main..main 2>/dev/null || echo "?")
        behind=$(git rev-list --count main..origin/main 2>/dev/null || echo "?")
        warn_note "main 与 origin/main 不一致（领先 $ahead / 落后 $behind）——有提交没推？"
    fi
else
    unknown_note "没有 origin/main（远程未配置或未 fetch）"
fi

# ── 2. 工作区 ───────────────────────────────────────────────────
dirty=$(git status --porcelain 2>/dev/null | wc -l | tr -d ' ')
if [ "$dirty" -eq 0 ]; then
    pass_note "工作区干净"
else
    warn_note "工作区有 $dirty 个未提交改动（别的 worktree 在干活，或忘了提交）"
fi

# ── 3. 看板 ─────────────────────────────────────────────────────
# 只在 walgit CLI 可用时才查；不可用算「无法判定」，不算通过。
if ! command -v walgit >/dev/null 2>&1; then
    unknown_note "没有 walgit CLI ⇒ 看板状态未知（迁移到远程后属预期，请改用 MCP）"
elif ! command -v python3 >/dev/null 2>&1; then
    unknown_note "没有 python3 ⇒ 看板 JSON 解析不了"
else
    board_out=$(WALGIT_LOCAL_COMMANDS=1 walgit collab board --format json 2>/dev/null)
    if [ -z "$board_out" ]; then
        unknown_note "walgit collab board 返回空 ⇒ 看板状态未知"
    else
        verdict_line=$(printf '%s' "$board_out" | python3 -c '
import sys, json
try:
    d = json.load(sys.stdin)
except Exception as e:
    print("❓ 看板 JSON 解析失败：%s" % e); raise SystemExit(0)
counts = {c["name"]: len(c["cards"]) for c in d.get("columns", [])}
stuck = {k: v for k, v in counts.items()
         if k in ("blocked", "needs-human", "merge-blocked") and v}
if stuck:
    for k, v in stuck.items():
        print("⚠️  看板 %s 有 %d 张——需要人/需要决策" % (k, v))
elif counts.get("needs-review", 0):
    print("⚠️  看板 needs-review 有 %d 张——等评审" % counts["needs-review"])
elif counts.get("in-progress", 0):
    print("✅ 看板 in-progress %d 张（有人在推）" % counts["in-progress"])
else:
    print("✅ 看板无阻塞（open %d / closed %d）"
          % (counts.get("open", 0), counts.get("closed", 0)))
' 2>/dev/null)
        case "$verdict_line" in
            "⚠️"*) warn=$((warn + 1)); note "  $verdict_line" ;;
            "✅"*)  ok=$((ok + 1));    note "  $verdict_line" ;;
            *)      unknown=$((unknown + 1)); note "  ❓ 看板检查无输出" ;;
        esac
    fi
fi

# ── 4. 快守卫（不编译）──────────────────────────────────────────
for guard in tools/arch-check.sh tools/check-prose-claims.sh \
             tools/check_gates_order.sh tools/check_script_modes.sh; do
    if [ ! -f "$guard" ]; then
        unknown_note "$guard 不存在"
        continue
    fi
    if out=$(sh "$guard" 2>&1); then
        pass_note "$(basename "$guard") 通过"
    else
        reason=$(printf '%s' "$out" | grep -E '^❌' | head -1 | cut -c1-90)
        warn_note "$(basename "$guard") 失败：${reason:-（无 ❌ 行，需人工看）}"
    fi
done

# ── 结论 ───────────────────────────────────────────────────────
if [ "$warn" -gt 0 ]; then
    verdict="⚠️  有 $warn 项偏差 / $ok 项通过 / $unknown 项未知"
    exit_code=1
elif [ "$unknown" -gt 0 ]; then
    verdict="❓ $ok 项通过，但 $unknown 项无法判定——**不要当成绿**"
    exit_code=1
else
    verdict="✅ 全部 $ok 项检查通过"
    exit_code=0
fi

{
    printf '=== %s ===\n%s\n%s\n' "$(date '+%Y-%m-%d %H:%M:%S')" "$verdict" "$notes"
} > "$state_file"
cat "$state_file" >> "$log_file"

if [ "$quiet" -eq 1 ] && [ "$exit_code" -eq 0 ]; then
    exit 0
fi
cat "$state_file"
exit "$exit_code"
