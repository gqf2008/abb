#!/bin/sh
# rebase 后核对「内容真的没变」——用 blob 比对，而不是看 diff。
#
# ## 为什么需要它
#
# 本仓的合并流程里，分支经常要 `git rebase main` 才能 ff 合并。每次 rebase 都会让
# 之前的 approve「过期」（判据 (c)：终审早于最后改动），于是要重新确认。
#
# 而「重新确认」真正要回答的问题只有一个：**补丁内容变了吗？**
#
# - 看 `git diff main...HEAD` **不行**：它会把 rebase 带进来的别人的改动也算进去
#   （评审 0bf5229 教我的：要按提交归因，不能按最终 diff）
# - 看 `git diff A B --stat` 也不够：mode 变更不体现在 patch hunk 里
#   （评审 af57363 用逐文件 (mode, blob) 比对发现了这一点）
#
# 所以正确判据是：**逐文件比 (mode, blob oid)**。相同 ⇒ 内容逐字节一致。
#
# ## 用法
#
#     tools/blob-diff.sh <已批的旧 commit> <当前 head>
#     tools/blob-diff.sh 228b087 47abddf
#
# 输出：
#   `SAME`  —— 该文件的 (mode, blob) 两边一致
#   `DIFF`  —— 内容或 mode 变了（**要说明为什么**，不能默认它是对的）
#   `NEW`   —— 旧版本没有这个文件（新增）
#   `GONE`  —— 旧版本有、新版本没有（删除）
#
# 退出码：全 SAME/NEW/GONE 且无 DIFF ⇒ 0；有 DIFF ⇒ 1。
#
# ## 注意
#
# 它比的是**整棵树的差异**，不只是本卡改的文件。rebase 带进来的别人的改动也会显示
# 成 DIFF/NEW——那是预期的。判断时只看**本卡涉及的文件**是否 SAME，其余归因到
# 「main 带进来的」。脚本会把文件名打全，方便你对照。

set -eu

if [ $# -ne 2 ]; then
    echo "用法：$0 <已批的旧 commit> <当前 head>" >&2
    exit 2
fi

old=$1
new=$2

for c in "$old" "$new"; do
    if ! git rev-parse --verify -q "$c^{commit}" >/dev/null; then
        echo "❌ 不是有效提交：$c" >&2
        exit 2
    fi
done

# 取两边的「路径 → mode blob」映射。用 `git ls-tree -r` 拿全树，
# 只比路径集合的并集（新增/删除都要看到）。
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

git ls-tree -r "$old" | awk '{print $1, $3, $4}' | sort -k3 > "$tmp/old"
git ls-tree -r "$new" | awk '{print $1, $3, $4}' | sort -k3 > "$tmp/new"

# 用 join 按键（路径，第 3 列）做外连接。
join -j 3 -a 1 -a 2 -e '-' -o 0,1.1,1.2,2.1,2.2 "$tmp/old" "$tmp/new" 2>/dev/null \
| while read -r path omode oblob nmode nblob; do
    if [ "$omode" = "-" ]; then
        printf 'NEW  %s\n' "$path"
    elif [ "$nmode" = "-" ]; then
        printf 'GONE %s\n' "$path"
    elif [ "$omode" = "$nmode" ] && [ "$oblob" = "$nblob" ]; then
        printf 'SAME %s\n' "$path"
    else
        # 同一个路径但内容或 mode 变了——这是真正要解释的那类
        if [ "$omode" != "$nmode" ]; then
            printf 'DIFF %s (mode %s → %s)\n' "$path" "$omode" "$nmode"
        else
            printf 'DIFF %s (blob %s → %s)\n' "$path" "${oblob%"${oblob#???????}"}" "${nblob%"${nblob#???????}"}"
        fi
    fi
done > "$tmp/out"

same=$(grep -c '^SAME' "$tmp/out" || true)
diff=$(grep -c '^DIFF' "$tmp/out" || true)
new=$(grep -c '^NEW' "$tmp/out" || true)
gone=$(grep -c '^GONE' "$tmp/out" || true)

if [ "${VERBOSE:-0}" = 1 ]; then
    cat "$tmp/out"
else
    # 默认只打非 SAME 的（干净 rebase 时输出很短）
    grep -E '^(DIFF|NEW|GONE)' "$tmp/out" || true
fi

echo "---"
printf 'SAME %s / DIFF %s / NEW %s / GONE %s\n' "$same" "$diff" "$new" "$gone"

if [ "$diff" -gt 0 ]; then
    echo
    echo "有 $diff 个文件内容或 mode 变化。判断时注意："
    echo "  · 本卡**涉及**的文件变了 ⇒ 补丁真的改了，approve 需要重新审内容"
    echo "  · 本卡**不涉及**的文件变了 ⇒ 那是 rebase 带进来的别人的改动，归因到 main"
    exit 1
fi

if [ "${new}" -gt 0 ] || [ "${gone}" -gt 0 ]; then
    echo
    echo "有新增/删除文件——同样按上面的规则归因。"
fi
exit 0
