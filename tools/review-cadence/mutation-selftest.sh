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
# 主检出既不可能被污染，也不存在「忘了还原」。
#
# ## 用法
#
#   tools/review-cadence/mutation-selftest.sh         # 全部变异
#   tools/review-cadence/mutation-selftest.sh -v      # 同时打印每个变异跑出的 FAIL 行
#
# 退出码：0 = 每个变异都被自测咬住（红）；1 = 有变异逃逸（自测有洞）/ 清单过期。
#
# ## 变异清单
#
# 在 `tools/review-cadence/mutations.py`。为什么单独成文件：shell 的引号对「跨行替换 +
# 中文 + 正则元字符」极其脆，本清单最初写在 `sed -E` 里时，`/^    exit 2$/` 会连
# `cd "$repo_root" || exit 2` 一起命中，把脚本改崩 —— 于是「变异逃逸」是假的。
# 改成 Python 的整段字符串替换后，语义精确：**匹配不到就报错，而不是猜**。

set -uo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
mut_dir="$repo_root/tools/review-cadence"
verbose="${1:-}"
pass=0
fail=0

while IFS=$'\t' read -r name file runner; do
  [ -z "$name" ] && continue

  work="$(mktemp -d "${TMPDIR:-/tmp}/mut.XXXXXX")"
  # 只复制跑 selftest 需要的东西（.git 必须带：selftest 的 lint 用 git ls-files）
  ( cd "$repo_root" && git archive HEAD ) | tar -x -C "$work"
  ( cd "$repo_root" && cp -R .git "$work/.git" ) >/dev/null 2>&1 || true

  target="$work/$file"
  if [ ! -f "$target" ]; then
    echo "  ⚠ $name —— 跳过（$file 不存在）"
    rm -rf "$work"; continue
  fi

  # 精确应用：匹配不到或匹配到多处都算清单过期（报错，不猜）
  if ! MUT_FILE="$target" MUT_NAME="$name" MUT_DIR="$mut_dir" python3 -c '
import os, sys
sys.path.insert(0, os.environ["MUT_DIR"])
from mutations import MUTATIONS
name = os.environ["MUT_NAME"]
path = os.environ["MUT_FILE"]
spec = next((m for m in MUTATIONS if m["name"] == name), None)
if spec is None:
    print("清单里找不到这一项", file=sys.stderr)
    sys.exit(2)
src = open(path).read()
n = src.count(spec["old"])
if n != 1:
    print(f"原文出现 {n} 次（应为 1）—— 变异清单已过期", file=sys.stderr)
    sys.exit(3)
open(path, "w").write(src.replace(spec["old"], spec["new"]))
'; then
    echo "  ⚠ $name —— 跳过（清单过期，见上面的原因）"
    rm -rf "$work"; continue
  fi

  # 每个变异跑**它自己那条不变量所属**的自测入口。跑错文件会让变异永远"逃逸"
  # （M6 的守卫有独立自测，最初就是被错跑成 review-cadence 的 selftest 才假逃逸）。
  out="$(cd "$work" && TMPDIR="${TMPDIR:-/tmp}" bash $runner 2>&1)"
  rc=$?
  if [ "$rc" -ne 0 ]; then
    pass=$((pass + 1))
    echo "  ✅ $name —— 被咬住"
    if [ "$verbose" = "-v" ]; then
      printf '%s\n' "$out" | grep -E '^(FAIL|FATAL)' | head -3 | sed 's/^/       /'
    fi
  else
    fail=$((fail + 1))
    echo "  ❌ $name —— 逃逸（自测全绿，说明这条不变量没有用例）"
    printf '%s\n' "$out" | tail -2 | sed 's/^/       /'
  fi
  rm -rf "$work"
done < <(python3 "$mut_dir/mutations.py")

echo
if [ "$fail" -eq 0 ]; then
  echo "✅ 全部 ${pass} 个变异都被自测咬住"
  exit 0
fi
echo "❌ ${fail} 个变异逃逸（共 $((pass + fail)) 个）；逃逸 = 自测有洞，先补用例"
exit 1
