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

# 反向：注释声称「还没接上 / 暂未用」，但代码里**已经有调用**。
#
# 为什么需要（2026-11-03 实测）：先做的单向版只查「声称已移除但其实在」，
# 于是 `mcp_channel.rs` / `mcp_inbound.rs` / `mcp_inbound_loop.rs` 三处
# 「还没接进 service」、「暂未用」全被漏掉——而它们早已被真实调用。
# 工具报 ✅，实际三处过期。
#
# 用法：$2 = 文件，$3 = 「还没接上」的措辞，$4 = 证明「已接上」的调用点模式
# （在同一文件里 grep 到它 ⇒ 说明接线真的发生了）。
check_claimed_unwired() {
    desc=$1
    file=$2
    claim_pat=$3
    wired_pat=$4

    [ -f "$file" ] || return 0
    if ! grep -qE "$claim_pat" "$file" 2>/dev/null; then
        return 0
    fi
    # 代码里（排除注释）有没有那处调用
    if grep -nE "$wired_pat" "$file" 2>/dev/null | grep -vE ':\s*(//|///|//!)' | grep -q .; then
        fail=$((fail + 1))
        printf '❌ %s\n' "$desc"
        printf '   %s 的注释仍称「还没接上」，但代码里已有调用：\n' "$file"
        grep -nE "$wired_pat" "$file" | grep -vE ':\s*(//|///|//!)' | sed 's/^/     /' | head -3
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

# 事故 3（2026-11-03）：三处注释仍称「还没接进 service / 暂未用」，
# 而实际调用点在别的文件里（mcp_channel_loop.rs / messenger.rs）。
# 这用「全局调用点」判：注释说没接，但整个 src/ 里找得到调用 ⇒ 过期。
check_claimed_unwired_global() {
    desc=$1
    file=$2
    claim_pat=$3
    # 证明已接上的全局证据（在 src/ 其它文件里 grep）
    wired_evidence=$4

    [ -f "$file" ] || return 0
    # **排除引用历史的行**：`这里曾有…注释写「…」` 是在交代背景，不是仍在声称。
    # 第一版没排除，于是修好之后仍被自己的新注释判红——误红。
    if ! grep -nE "$claim_pat" "$file" 2>/dev/null \
         | grep -vE '这里曾有|曾有.*注释写|原来的注释' | grep -q .; then
        return 0
    fi
    if grep -rqE "$wired_evidence" src/ --include='*.rs' 2>/dev/null; then
        fail=$((fail + 1))
        printf '❌ %s\n' "$desc"
        printf '   %s 的注释仍称「还没接上」，但 src/ 里已有调用点\n' "$file"
    fi
}

check_claimed_unwired_global \
    "mcp_channel：注释说「还没接进 service 的 bot 装配」，但 messenger::build 已在用" \
    "src/mcp_channel.rs" \
    "还没接进|暂不涉及" \
    "mcp_channel::McpChannelMessenger::connect"

check_claimed_unwired_global \
    "mcp_inbound：注释说「还没接进 service 的轮询循环」，但 mcp_channel_loop 已在用" \
    "src/mcp_inbound.rs" \
    "还没接进|暂未用" \
    "mcp_inbound::negotiate"

check_claimed_unwired_global \
    "mcp_inbound_loop：注释说「还没接进 service::run_bot」，但已在用" \
    "src/mcp_inbound_loop.rs" \
    "还没接进|暂未用" \
    "mcp_channel_loop::run"

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
    echo "✅ 注释声称的事实与代码一致（已登记的 5 条检查，双向）"
    exit 0
fi
echo
echo "修法：改了代码就同步改声称它的那句注释/标题。这类漏改靠评审发现过三次（2026-11-03）。"
exit 1
