#!/usr/bin/env bash
# 兜底守卫：清掉 $TMPDIR 里**陈旧的**测试夹具（abb-* 前缀白名单），防止数据卷被它们填满。
#
# 为什么存在：单测的夹具此前由**用例结尾**的 `let _ = remove_dir_all(&root);` 收尾 ——
# 断言一红就跳过收尾，于是夹具整份留在 TMPDIR；本机政策把 TMPDIR 指到数据卷
# `/Volumes/DataExt/tmp`（AGENTS.md「Build intermediates go to /Volumes/DataExt/tmp」），
# 实测那儿积了 1.4 万个 `abb-*` 目录（7 千余个只剩空壳）。
# 根因修复是把夹具改成 RAII 归属（`src/testtmp.rs`，Drop 时删）；本脚本是**兜底**：
# 给「以后又有人写了新夹具忘了管」留一道门禁级清扫，让堆积不可能无界增长。
#
# 只删两类之外的都不碰：
#   ① 前缀在下面的白名单里（就是测试夹具；构建产物/工作树/暂存目录都**不在**名单里），
#   ② 且够老（默认 2 小时）——正在跑的测试新建的夹具不够老，所以不会误删。
# 另外对名字里含 `target` 的一律跳过（`abb-target` / `abb-ci-target` / `abb-agent-target`
# 是活跃构建目录，与夹具同前缀、绝不能按名字匹配掉）。
#
# 用法：tools/clean_test_fixtures.sh [--dry-run] [--hours N]
# 退出码：0=清扫完成（含 0 个可清）；2=用法错误
set -euo pipefail

hours=2
dry_run=0
while [ $# -gt 0 ]; do
  case "$1" in
    --dry-run) dry_run=1 ;;
    --hours)
      [ $# -ge 2 ] || { echo "用法: $0 [--dry-run] [--hours N]" >&2; exit 2; }
      hours="$2"
      shift
      ;;
    *) echo "用法: $0 [--dry-run] [--hours N]" >&2; exit 2 ;;
  esac
  shift
done
case "$hours" in ''|*[!0-9]*) echo "❌ --hours 需要非负整数：$hours" >&2; exit 2 ;; esac

# TMPDIR 就是夹具落点（测试用 std::env::temp_dir()）；没设时与 std::env::temp_dir() 一致取 /tmp。
base="${TMPDIR:-/tmp}"
base="${base%/}"
if [ ! -d "$base" ]; then
  echo "✅ 夹具目录不存在，无需清扫：$base"
  exit 0
fi

# 前缀白名单。**新增测试夹具时记得把前缀加进来**——漏一个，那类夹具就永远清不掉
# （2026-10-30 实测：白名单只有 11 个前缀，而 $TMPDIR 里积了 ~2500 个漏网目录，
# `abb-msgstore-test-*` 一类 792 个、`abb-hist-*` 660 个）。
fixture_prefixes=(
  abb-guard- abb-task- abb-migrate- abb-syschat- abb-hints- abb-read-
  abb-att- abb-cap- abb-trash- abb-sessions- abb-test-home.
  # 2026-10-30 补：下面这些前缀此前一直在漏
  abb-msgstore-test- abb-sched- abb-teambuilder- abb-sc-t abb-sc-att-
  abb-hist- abb-ki-config- abb-oneshot- abb-roleprompt- abb-chart-
  abb-teams- abb-logs- abb-acp- abb-mock- abb-lark-
)

found=0
removed=0
for prefix in "${fixture_prefixes[@]}"; do
  while IFS= read -r -d '' entry; do
    name="$(basename "$entry")"
    # 同前缀的活跃构建目录（abb-target / abb-agent-target …）绝不按名字匹配掉。
    case "$name" in *target*) continue ;; esac
    found=$((found + 1))
    if [ "$dry_run" = 1 ]; then
      echo "  [dry-run] ${entry}"
    else
      rm -rf -- "$entry"
      removed=$((removed + 1))
    fi
  done < <(find "$base" -maxdepth 1 -name "${prefix}*" -mmin "+$((hours * 60))" -print0 2>/dev/null)
done

if [ "$dry_run" = 1 ]; then
  echo "ℹ️ 干跑：$base 下有 $found 个超过 ${hours} 小时的测试夹具（未删除）"
else
  echo "✅ 已清扫 $base 下 ${hours} 小时前的测试夹具 $removed 个（命中 $found 个）"
fi
