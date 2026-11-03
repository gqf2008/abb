#!/usr/bin/env bash
# 断言「被门禁裸路径调用的脚本」在 git 里是 100755。
#
# ## 为什么需要（同一个错误一天犯了两次）
#
# `tools/gates.sh` 与 `.walgit/ci.toml` 都用**裸路径**调脚本（不是 `bash script.sh`）。
# 裸路径要求文件有可执行位。而 `chmod +x` 只改**工作区**，git 只跟踪内容的 mode 位——
# 不在索引里改成 755，干净检出就是 644，门禁直接 `Permission denied`。
#
# 两次实例：
#   · 2026-11-02 上午  tools/review-cadence/mutation-selftest.sh  100644
#   · 2026-11-02 晚间  tools/arch-check.sh                       100644
# 两次的共同点：**本地自测始终是绿的**（我 chmod 过工作区），只有干净检出才红。
# 也就是说，靠「跑一遍门禁」发现不了它——必须查 git 索引。
#
# 所以这条检查读的是 `git ls-files -s`，不是文件系统权限。
#
#   bash tools/check_script_modes.sh
set -uo pipefail

cd "$(dirname "$0")/.." || exit 2

fail=0
checked=0

# 从 gates.sh 与 ci.toml 里抽出「裸路径调用」的脚本名。
# 判据：行首（可含前后空白）是 `tools/…` 或 `scripts/…` 且以 .sh 结尾，
# 不出现 `bash `/`sh `/`source ` 前缀 —— 那就是裸调用。
collect() {
    local f="$1"
    [ -f "$f" ] || return 0
    grep -Eo '^[[:space:]]*(tools|scripts)/[A-Za-z0-9_./-]+\.sh' "$f" 2>/dev/null \
        | sed -E 's/^[[:space:]]*//'
}

# 第三类数据源：**文档里承诺「直接执行」的脚本**。
#
# 为什么需要（2026-11-02 实测）：`tools/channel-mcp-dir/verify.sh` 与
# `channel_mcp_dir.py` 在 git 里是 100644，而 README/提交信息里写的是
# `sh verify.sh`（照做也能跑）和「可执行」。评审发现时门禁是绿的——
# 因为上面两个数据源只覆盖「gates.sh / ci.toml 里的裸调用」。
#
# 判据不看「文档怎么写」（那太脆），而是看**同目录下已被允许的可执行位**：
# 任何 `tools/` 下的 `.sh` / `.py`，若同目录已有 100755 的兄弟，就视为
# 「这类文件应当可执行」。听起来绕，但它挡住了真实的漏：新增脚本时
# 人会记得 chmod 同目录第一个，忘了后来加的。
siblings_exec() {
    local dir="$1"
    git ls-files -s "$dir" 2>/dev/null \
        | awk '$1=="100755"{found=1} END{exit found?0:1}'
}

# 找出 tools/ 下所有 .sh/.py，按目录分组；目录里有 100755 兄弟的，全部要求 100755。
dirs_with_exec="$(
    git ls-files 'tools/*.sh' 'tools/*.py' 2>/dev/null | xargs -n1 dirname 2>/dev/null | sort -u \
    | while IFS= read -r d; do
        [ -n "$d" ] || continue
        siblings_exec "$d" && echo "$d"
      done
)"
sibling_targets=""
if [ -n "$dirs_with_exec" ]; then
    sibling_targets="$(
        printf '%s\n' "$dirs_with_exec" | while IFS= read -r d; do
            [ -n "$d" ] || continue
            git ls-files "$d/*.sh" "$d/*.py" 2>/dev/null
        done | sort -u
    )"
fi

targets="$(
    { collect tools/gates.sh; collect .walgit/ci.toml; \
      [ -n "$sibling_targets" ] && printf '%s\n' "$sibling_targets"; } | sort -u
)"

if [ -z "$targets" ]; then
    echo "⚠️  没有从 gates.sh / ci.toml 里抽出任何裸调用脚本——判据可能已失效"
    echo "    （脚本被重写？请检查 collect 的正则是否还匹配当前写法）"
    exit 2
fi

while IFS= read -r p; do
    [ -n "$p" ] || continue
    checked=$((checked + 1))
    if [ ! -f "$p" ]; then
        fail=$((fail + 1))
        printf '❌ %s：门禁里裸调它，但文件不存在\n' "$p"
        continue
    fi
    mode=$(git ls-files -s -- "$p" | awk '{print $1}')
    if [ -z "$mode" ]; then
        fail=$((fail + 1))
        printf '❌ %s：门禁里裸调它，但它没被 git 跟踪（干净检出不会有这个文件）\n' "$p"
        continue
    fi
    if [ "$mode" != "100755" ]; then
        fail=$((fail + 1))
        ws=$(ls -l "$p" | awk '{print $1}')
        # 文案不再只说「门禁裸路径调用」——本判据现在也覆盖「同目录有可执行兄弟」
        # 这一类（见上面 siblings_exec 的注释）。说错原因会让人往错方向修。
        printf '❌ %s：git 里是 %s，但按本仓约定它应当可执行 ⇒ 干净检出 Permission denied\n' "$p" "$mode"
        printf '     （工作区当前是 %s——所以本地自测可能是绿的，别被骗）\n' "$ws"
        printf '     修法：git update-index --chmod=+x %s\n' "$p"
    fi
done <<< "$targets"

if [ "$fail" -eq 0 ]; then
    echo "✅ 门禁裸调用的 $checked 个脚本在 git 里都是 100755"
    exit 0
fi
exit 1
