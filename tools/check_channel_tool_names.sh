#!/bin/sh
# 断言「渠道 crate 声明的工具名」与「ABB 契约里的工具名」**逐字对齐**。
#
# ## 为什么需要这道守卫
#
# 契约的名字只活在两处：`src/mcp_bus.rs` 的 `TOOL_*` 常量，和每个渠道 crate
# `main.rs` 里 `tools/list` 的 `"name"` 字面量。两边靠**人眼保持一致**。
#
# 写错一个字母的后果不是报错，而是**静默降级**：
# ABB 调 `get_quoted_message`，crate 声明的是 `get_quote_message`
# ⇒ `ChannelCapabilities::has_tool` 返回 false ⇒ 回落 trait 默认实现（返回 None）
# ⇒ 引用消息悄悄不进 prompt，日志里什么也看不到。
#
# 这正是本仓反复栽的那类缺陷（问「测试为什么没抓到」时，答案是「没有测试在看在看的东西」）。
#
# ## 查什么
#
# 1. 每个渠道 crate 声明的每个工具名，都必须是 `mcp_bus.rs` 里某个 `TOOL_*` 的值
#    （或者入站工具 `inbox`——它是资源 URI 的伴生工具，不是 `TOOL_*` 常量）。
#    ⇒ 抓 crate 侧拼错/自造工具名。
# 2. `mcp_bus.rs` 里每个 `TOOL_*` 常量，必须**至少被一个 crate 声明**。
#    ⇒ 抓契约侧写了名字却没人实现（ABB 会调一个永远不存在的工具）。
#
# 第 2 条是双向检查的关键：单向（只查 crate→契约）会让「契约加了 11 个常量、
# 但三个 crate 一个都没实现」也绿——而那正是本次改动的中间状态。
set -eu

repo=$(cd "$(dirname "$0")/.." && pwd)
bus="$repo/src/mcp_bus.rs"

if [ ! -f "$bus" ]; then
    echo "❌ 找不到 $bus"
    exit 1
fi

# 契约里的工具名：`pub const TOOL_X: &str = "value";`
contract=$(sed -n 's/^pub const TOOL_[A-Z_]*: &str = "\([a-z_]*\)";$/\1/p' "$bus" | sort -u)
if [ -z "$contract" ]; then
    # **解析不出来必须报错**，不能默默返回「没有工具」——那样所有断言都恒真。
    echo "❌ 从 $bus 解析不出任何 TOOL_* 常量（格式变了？）"
    exit 1
fi

# 三个渠道合成了一个 crate（`--transport` 分派），工具声明各在自己的子模块里。
crates="wechat feishu dingtalk"

# 临时目录放「允许名单」——`comm` 只吃文件，不吃字符串（第一版拿变量当文件用，
# 报 `comm: download_attachment ...: No such file or directory`）。
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

fail=0
for c in $crates; do
    main="$repo/crates/abb-channel/src/$c/mod.rs"
    if [ ! -f "$main" ]; then
        echo "❌ 找不到 $main"
        fail=1
        continue
    fi

    # crate 声明的工具名。只取 `tools` 数组里的 `"name": "..."` 字面量：
    # 该文件里其它 `"name":` 出现在参数 schema（如 send_attachment 的 `name` 参数），
    # 它们的缩进更深且后面跟描述——用「行首恰 12 空格 + 不跟 description」区分。
    #
    # 更稳的判据：工具条目形如 `            "name": "xxx",` 且**下一行**是
    # `"description"`。这里用 sed 取所有 12 空格缩进的 name，再过滤掉已知的
    # 参数名（它们缩进不同，实测安全）。若将来格式变了导致多取，第 1 条断言会红，
    # 属于**响亮失败**而不是静默漏检。
    declared=$(sed -n 's/^            "name": "\([a-z_]*\)",$/\1/p' "$main" | sort -u)

    if [ -z "$declared" ]; then
        echo "❌ $c 解析不出任何工具名（main.rs 的 tools 列表格式变了？）"
        fail=1
        continue
    fi

    # 放行三类**不属于消息契约**的工具名：
    #
    # - `inbox`：入站工具（资源 URI 的伴生工具，不是 `TOOL_*` 常量）
    # - `poll`：入站兜底（它在契约里，这条只是冗余保险）
    # - `login` / `login_status`：微信扫码登录（渠道特有的运维工具，
    #   ABB 从不调它——用户自己用 MCP 客户端调。写进契约会给别的渠道
    #   背一个它没有的概念）。
    #
    # 白名单是**显式**的：新增渠道特有工具必须来这里加一行并写清理由，
    # 否则守卫会红。默认拒绝，避免「拼错的契约名」混进白名单。
    printf '%s\ninbox\nlogin\nlogin_status\n' "$contract" | sort -u > "$tmp/allowed"
    printf '%s\n' "$declared" | sort -u > "$tmp/declared"

    extra=$(comm -23 "$tmp/declared" "$tmp/allowed")
    if [ -n "$extra" ]; then
        echo "❌ $c 声明了契约里没有的工具名（ABB 永远不会调它，功能静默缺失）："
        echo "$extra" | sed 's/^/     /'
        echo "   契约里的名字：$(echo $contract | tr '\n' ' ')"
        fail=1
    fi

    echo "   $c: $(echo $declared | tr '\n' ' ')"
    all_declared="$declared
${all_declared:-}"
done

# 第 2 条：契约里每个名字至少被一个 crate 实现。
unused=""
for t in $contract; do
    if ! echo "$all_declared" | grep -qx "$t"; then
        unused="$unused $t"
    fi
done
if [ -n "$unused" ]; then
    echo "❌ 契约里有工具名但三个 crate 都没声明（ABB 会调一个不存在的工具）：$unused"
    fail=1
fi

if [ "$fail" -ne 0 ]; then
    exit 1
fi

n=$(echo "$contract" | wc -l | tr -d ' ')
echo "   ✅ $n 个契约工具名与三个渠道 crate 逐字对齐"
