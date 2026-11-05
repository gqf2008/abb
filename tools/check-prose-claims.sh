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

# 文档声称「只有 A 实现了 X」时，核对 B/C 是否也实现了。
#
# 事故（2026-11-04）：`docs/channel-retirement-plan.md` 写着
# 「`bot_info` / `update_chat` / `delete_chat` **只有飞书 crate 实现**」。
# 实际是：
#
# | 工具 | 飞书 | 钉钉 |
# |---|---|---|
# | `create_chat` / `get_chat_info` | ✅ | ✅（文档压根没提）|
# | `update_chat` | ✅ | **底层搬了、工具没暴露** ⇒ 真实遗漏，不是边界 |
#
# 两类错误叠在一起，而**用户一追问就露了**：
#
# 1. **全称量词没核对**：「只有 A」这种句子，反例只要一个就假。
# 2. **把「遗漏」归成「边界」**：两者在文字上长得一样（都是「某渠道没有某能力」），
#    但对用户含义完全相反——边界=平台做不到，遗漏=我能修。归错的后果是
#    **把该修的活藏进「设计取舍」里，文档一写就再没人回头看**。
#
# 判据：文档里出现「只有 <渠道> 实现」，而其它渠道 crate 的 `main.rs` 里
# 确实有该工具名 ⇒ 断言为假。
check_only_one_channel_implements() {
    doc=$1
    [ -f "$doc" ] || return 0

    # 只在该文档**声称**「只有…实现」时检查。
    #
    # **必须排除「引用历史错误」的句子**：修复之后文档会写
    # 「`d80b042` 之前这里写的是「只有飞书实现」——那句话不准」，
    # 那是在交代背景，不是当前的断言。第一版没排除它 ⇒ 我刚改好的文档立刻被自己
    # 的守卫误红（「会误红的工具比没工具更糟」）。
    #
    # 排除口径：同一行里带「之前 / 原来 / 曾 / 那句话」等交代词。
    if ! grep -E '只有.*实现' "$doc" 2>/dev/null \
        | grep -vE '之前|原来|曾|那句话|过去' | grep -q .; then
        return 0
    fi

    # **只看「只有…实现」那句话本身点名的工具**，不是文档里出现过的所有工具名。
    #
    # 第一版抓全文的 `` `tool` `` 清单逐个核对 ⇒ 文档里同时写了「`create_chat` 也有」
    # 时，只要有一句「只有…实现」就把它一起报了（实测变异暴露）。那是误报：
    # 该检查的对象是**那一句话**的断言范围，不是整篇文档。
    # 先取出「只有…实现」那几句话（排除交代历史的），再看句中点名的工具。
    claims=$(grep -E '只有.*实现' "$doc" 2>/dev/null \
             | grep -vE '之前|原来|曾|那句话|过去' || true)
    [ -n "$claims" ] || return 0

    for tool in $(printf '%s\n' "$claims" | grep -oE '`[a-z_]+`' | tr -d '`' | sort -u); do
        [ -n "$tool" ] || continue
        hit=""
        for c in feishu dingtalk wechat; do
            f="crates/abb-channel/src/$c/mod.rs"
            [ -f "$f" ] || continue
            if grep -q "\"name\": \"$tool\"" "$f" 2>/dev/null; then
                hit="$hit $c"
            fi
        done
        n=$(echo "$hit" | wc -w | tr -d ' ')
        if [ "$n" -gt 1 ]; then
            fail=$((fail + 1))
            printf '❌ %s：文档称 `%s`「只有…实现」，实际有 %s 个渠道声明了它：%s\n' \
                "$doc" "$tool" "$n" "$hit"
            printf '   全称量词必须逐格核对——「只有 A」的反例只要一个就假。\n'
        fi
    done
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

# 事故 6（2026-11-04）：退役方案文档称「群管理工具只有飞书实现」，
# 实际钉钉也有（而它的 update_chat 是漏搬，不是边界）。
check_only_one_channel_implements "docs/channel-retirement-plan.md"

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
    echo "✅ 注释声称的事实与代码一致（已登记的 6 条检查，双向）"
    exit 0
fi
echo
echo "修法：改了代码就同步改声称它的那句注释/标题。这类漏改靠评审发现过三次（2026-11-03）。"
exit 1
