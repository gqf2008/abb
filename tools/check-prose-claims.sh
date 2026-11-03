#!/bin/sh
# 检查「注释声称的事实」与代码是否一致。
#
# ## 为什么需要它
#
# 2026-11-03 一天里，**三次「改了代码没改叙述」全部由人工评审才发现**：
#
# | 改了什么 | 没改什么 |
# |---|---|
# | 加了两节文档 | 正文「上面**五条**」 |
# | 删了 `#![allow(dead_code)]` | 模块文档「整模块暂时 allow(dead_code)」 |
# | 收紧了词表 | 判据注释里的旧词表 |
#
# 而**机器能查的数字从来没漏过**——因为 `tools/arch-check.sh` 会断言
# 「这个数字在文档里出现恰好一次」。散文没有等价机制。
#
# 这个脚本是最小版本：只查**几条最容易过期的断言**，不是通用语义检查。
# 它承认自己覆盖有限——但每加一条都对应一次真实事故。
#
# ## 用法
#
#     tools/check-prose-claims.sh
#
# 退出码：0 = 无矛盾；1 = 有矛盾（打印位置）；2 = 脚本自身出错（比如 grep 模式坏了）。

set -eu

cd "$(dirname "$0")/.." || exit 2

fail=0

# claim <描述> <文件> <「声称已不存在」的模式> <「代码里真的不存在」的验证命令>
#
# 用法：若注释里出现「不再/已移除/暂时」+ 某符号，而代码里其实还有它（或反之），报红。
# 这里只做**最容易出错的那一类**：注释说「X 已经移除」，代码里却还有 X。

check_removed_but_present() {
    desc=$1
    file=$2
    # 「已移除 / 已删除 / 不再有」的措辞 + 符号名
    claim_pat=$3
    # 代码里是否还真的存在（不含注释行）
    symbol_pat=$4

    [ -f "$file" ] || return 0

    # 注释里有没有这句「已移除」的声称
    if ! grep -qE "$claim_pat" "$file" 2>/dev/null; then
        return 0
    fi

    # 代码里（排除注释行）还有没有该符号
    present=$(grep -nE "$symbol_pat" "$file" 2>/dev/null \
              | grep -vE ':\s*(//|///|//!)' \
              | grep -c . || true)

    if [ "$present" -gt 0 ]; then
        fail=$((fail + 1))
        printf '❌ %s\n' "$desc"
        printf '   %s 里注释声称已移除，但代码里仍有 %s 处：\n' "$file" "$present"
        grep -nE "$symbol_pat" "$file" | grep -vE ':\s*(//|///|//!)' | sed 's/^/     /' | head -3
    fi
}

# ── 逐条登记（每条对应一次真实事故）────────────────────────────────

# 事故 1（2026-11-03）：删了模块级 allow，模块文档还写着「整模块暂时 allow(dead_code)」。
# 现在的正确状态：文档说它「已经移除」，代码里只剩单点 allow。
check_removed_but_present \
    "mcp_bus 的模块级 allow：注释说已移除但代码里还在" \
    "src/mcp_bus.rs" \
    "那个 allow 已经移除|已经移除" \
    '^#!\[allow\(dead_code\)\]'

# 事故 2（2026-11-03）：假绿清单里「上面五条」没跟着变成七条/八条。
# 这个用总数一致性检查：标题里的「N 种」必须等于 `### ` 小节数。
plan_doc="docs/false-green-checklist.md"
if [ -f "$plan_doc" ]; then
    declared=$(grep -oE '^## [一二三四五六七八九十]+种形态' "$plan_doc" | head -1 \
               | grep -oE '[一二三四五六七八九十]+' || true)
    actual=$(grep -c '^### [0-9]' "$plan_doc" || true)
    if [ -n "$declared" ] && [ "$actual" -gt 0 ]; then
        # 中文数字转阿拉伯（够用即可：这里只可能出现个位数到十几）
        case "$declared" in
            一) d=1 ;; 二) d=2 ;; 三) d=3 ;; 四) d=4 ;; 五) d=5 ;;
            六) d=6 ;; 七) d=7 ;; 八) d=8 ;; 九) d=9 ;; 十) d=10 ;;
            *)  d=0 ;;
        esac
        if [ "$d" -ne 0 ] && [ "$d" -ne "$actual" ]; then
            fail=$((fail + 1))
            printf '❌ %s：标题声称「%s 种」，实际有 %s 个小节\n' \
                "$plan_doc" "$declared" "$actual"
            printf '   改数量时同步标题与正文里的「上面 N 条」——这条 2026-11-03 漏过两次\n'
        fi
    fi
fi

if [ "$fail" -eq 0 ]; then
    echo "✅ 注释声称的事实与代码一致（已登记的 2 类检查）"
    exit 0
fi
echo
echo "修法：改了代码就同步改声称它的那句注释/标题。这类漏改靠评审发现过三次（2026-11-03）。"
exit 1
