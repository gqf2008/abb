#!/usr/bin/env bash
# mutation-selftest.sh — 用「故意改坏」的方式验证自测真的能咬住回归。
#
# ## 为什么要有这个文件（本轮评审的教训）
#
# 前面几轮我都是**手工**应用变异、跑测试、再手工还原。评审恰好在这一点上发现了最严重的
# blocking：它自己造了一个变异（`if [ "$mcp_rc" -ne 3 ]` → `-gt 9`，即「MCP 调用失败也
# 允许回落 CLI」），selftest 却 100/0 全绿 —— 那条最关键的**不变量当时没有任何用例**，
# 而「变异逃逸」靠人是发现不了的。
#
# 还有更直接的教训：评审接手时，它的 worktree 里躺着一处**未还原的残留变异**（正是
# `-gt 9`）。手工流程必然出现这种情况，而残留变异意味着「你以为在测主干，其实在测被改坏的代码」。
#
# 所以把变异做成脚本：每个变异都在**临时副本**里应用（主检出只读），跑完自动销毁。
#
# ## 用法
#
#   tools/review-cadence/mutation-selftest.sh         # 全部变异
#   tools/review-cadence/mutation-selftest.sh -v      # 同时打印每个变异跑出的 FAIL 行
#
# 退出码：0 = 每个变异都被咬住且没有一个被跳过；1 = 有逃逸 / 有跳过 / 清单过期。
#
# ## 判定「被咬住」的严格条件（三条同时成立）
#
#   1. 自测退出码非 0；
#   2. 输出里出现 `FAIL` 或 `FATAL` 行 —— **只看 rc≠0 不够**：runner 因为无关原因失败
#      （脚本不存在 → 127、路径错、环境缺依赖）也会非 0，那会被误算成「咬住」（假阳性）。
#   3. 该变异不是「跳过」状态。
#
# 并且**跳过一律算失败**：早先版本里 `TMPDIR` 不可用会让 6 个变异全部跳过，而脚本仍打印
# 「✅ 全部 0 个变异都被自测咬住」并以 0 退出 —— 「什么都没测却报告通过」，与这个卡要消灭的
# 假绿是同一类（评审 NB-1 实测）。
#
# ## 变异清单
#
# 在 `tools/review-cadence/mutations.py`。为什么单独成文件：shell 的引号对「跨行替换 +
# 中文 + 正则元字符」极其脆，本清单最初写在 `sed -E` 里时，`/^    exit 2$/` 会连
# `cd "$repo_root" || exit 2` 一起命中，把脚本改崩 —— 于是那次「变异逃逸」是假的。

set -uo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
mut_dir="$repo_root/tools/review-cadence"
verbose="${1:-}"
pass=0
fail=0
skipped=0

# 副本人选：`git archive` 出跟踪的文件，然后**独立 git init**。
# 不复制 `.git` —— 在 worktree 里 `.git` 是个 gitfile（指向真实 git 目录），`cp -R` 只会
# 复制那个指针，副本里的 git 命令就会读写**真实仓库**，与「主检出只读」的承诺相冲
# （评审 NB-3）；普通 checkout 下复制 `.git` 又要拷 4500+ refs，慢且无必要。
# selftest 的 lint 只需要 `git ls-files` 能工作，独立 init + add 足够。
make_copy() {
  local dest="$1"
  # 复制**工作区**而不是 `git archive HEAD`：要能在提交之前用它自检「我这次的改动
  # 有没有让某个不变量失去用例」。用 HEAD 会测到上一个提交 —— 改了代码却看到旧结果，
  # 甚至出现「我明明修了它还说逃逸」（本工具第一版就是这样，被自己抓到）。
  # 取「已跟踪 + 未忽略的新文件」，等价于「这次会提交的东西」。
  ( cd "$repo_root" && git ls-files -z --cached --others --exclude-standard \
      | tar --null -T - -cf - ) | tar -xf - -C "$dest" || return 1
  ( cd "$dest" && git init -q . && git add -A >/dev/null 2>&1 ) || return 1
  return 0
}

while IFS='	' read -r name file runner marker; do
  [ -z "$name" ] && continue
  [ -z "$runner" ] && runner="tools/review-cadence/selftest.sh"
  # 每个 runner 的失败行样式不同（review-cadence 用 FAIL/FATAL，parents 用 ❌ / 自测失败）。
  # 变异可自带 fail_marker，默认认 FAIL/FATAL。
  [ -z "$marker" ] && marker='^(FAIL|FATAL)'

  work="$(mktemp -d "${TMPDIR:-/tmp}/mut.XXXXXX" 2>/dev/null)"
  if [ -z "${work:-}" ] || [ ! -d "$work" ]; then
    fail=$((fail + 1))
    echo "  ❌ $name —— 无法创建临时副本（TMPDIR=${TMPDIR:-/tmp} 不可用）"
    continue
  fi
  if ! make_copy "$work"; then
    fail=$((fail + 1))
    echo "  ❌ $name —— 副本准备失败（git archive / git init 出错）"
    rm -rf "$work"; continue
  fi

  target="$work/$file"
  if [ ! -f "$target" ]; then
    fail=$((fail + 1))
    echo "  ❌ ${name} —— 目标文件不存在（${file}）；清单指向了不存在的路径"
    rm -rf "$work"; continue
  fi

  # 精确应用：匹配不到或匹配到多处都算清单过期 ⇒ **算失败**（不是静默跳过）
  mut_err="$(MUT_FILE="$target" MUT_NAME="$name" MUT_DIR="$mut_dir" python3 -c '
import os, sys
sys.path.insert(0, os.environ["MUT_DIR"])
from mutations import MUTATIONS
name = os.environ["MUT_NAME"]
path = os.environ["MUT_FILE"]
spec = next((m for m in MUTATIONS if m["name"] == name), None)
if spec is None:
    print("清单里找不到这一项", file=sys.stderr); sys.exit(2)
src = open(path).read()
n = src.count(spec["old"])
if n != 1:
    print(f"原文出现 {n} 次（应为 1）——变异清单已过期", file=sys.stderr); sys.exit(3)
open(path, "w").write(src.replace(spec["old"], spec["new"]))
' 2>&1)"
  if [ $? -ne 0 ]; then
    fail=$((fail + 1))
    echo "  ❌ $name —— 清单过期：${mut_err:-未知原因}"
    rm -rf "$work"; continue
  fi

  out="$(cd "$work" && TMPDIR="${TMPDIR:-/tmp}" bash -c "$runner" 2>&1)"
  rc=$?
  # 三条同时成立才算「咬住」：rc≠0 **且** 输出里有 FAIL/FATAL 行。
  # 只看 rc 会把「runner 根本跑不起来」（127/缺文件）也当成咬住（评审 NB-2）。
  if [ "$rc" -ne 0 ] && printf '%s\n' "$out" | grep -qE "$marker"; then
    pass=$((pass + 1))
    echo "  ✅ $name —— 被咬住"
    if [ "$verbose" = "-v" ]; then
      printf '%s\n' "$out" | grep -E '^(FAIL|FATAL)' | head -3 | sed 's/^/       /'
    fi
  elif [ "$rc" -ne 0 ]; then
    fail=$((fail + 1))
    echo "  ❌ $name —— 判定不可信：runner rc=${rc} 但没有 FAIL/FATAL 行（可能是 runner 自己起不来）"
    printf '%s\n' "$out" | tail -3 | sed 's/^/       /'
  else
    fail=$((fail + 1))
    echo "  ❌ $name —— 逃逸（自测全绿，说明这条不变量没有用例）"
    printf '%s\n' "$out" | tail -2 | sed 's/^/       /'
  fi
  rm -rf "$work"
done < <(MUT_DIR="$mut_dir" python3 -c '
# 用 **import** 而不是执行 mutations.py：执行时 `if __name__ == "__main__"` 的打印块会在
# 文件末尾的 append 之前跑完，于是「后来追加的变异被静默忽略」——那等于少测几个变异，
# 而工具仍然报「全部被咬住」，是一种假绿。import 会执行全部顶层语句，拿到的总是完整清单。
import sys, os
sys.path.insert(0, os.environ["MUT_DIR"])
from mutations import MUTATIONS, DEFAULT_RUNNER
for m in MUTATIONS:
    print(m["name"] + "\t" + m["file"] + "\t" + m.get("runner", DEFAULT_RUNNER) + "\t" + m.get("fail_marker", ""))
')

total=$((pass + fail))
echo
if [ "$total" -eq 0 ]; then
  echo "❌ 一个变异都没跑（清单为空或解析失败）—— 这不是通过"
  exit 1
fi
if [ "$fail" -eq 0 ]; then
  echo "✅ 全部 ${pass} 个变异都被自测咬住（0 跳过）"
  exit 0
fi
echo "❌ ${fail} 个有问题（共 ${total} 个：被咬住 ${pass}）；逃逸/跳过/不可信都算失败"
exit 1
