#!/usr/bin/env bash
# 门禁一键入口：**先把构建中间产物赶到 checkout 之外**，再跑 AGENTS.md 定义的四条门禁。
#
# 为什么存在：2026-09-17 的 macOS 政策（`/Volumes/DataExt/tmp`）只覆盖 macOS，
# Windows 上照文档执行 = 裸 `cargo` = 中间产物全落 checkout（2026-10-11 实测把仓库撑到
# 38 G：`target/` 32 G + `crates/abb-agent/target/` 2.4 G + 僵尸 `crates/buzz-agent/target/`）。
# 本脚本把「外置 target」变成**默认行为**，并**硬拒绝**落在 checkout 内的 base——
# 这样这条政策才有机器判据（`RULE_规则可执行性.md` 第 1 条）。
#
# 用法：
#   tools/gates.sh                 # 跑四条门禁（fmt / clippy / build --locked / 隔离全量测试）
#   tools/gates.sh --print-env     # 只打印解析结果（平台、TMPDIR、CARGO_TARGET_DIR、repo）
#   ABB_BUILD_TMP=/e/tmp tools/gates.sh   # 覆盖 base（默认按平台取数据卷）
#   ABB_GATES_OS=darwin tools/gates.sh --print-env   # 覆盖平台探测（用于在本机核 macOS 口径）
#
# 退出码：0=门禁全绿；1=base 落在 checkout 内（政策违规）；其它=cargo/测试自身的失败码。
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

print_only=0
while [ $# -gt 0 ]; do
  case "$1" in
    --print-env | --dry-run) print_only=1 ;;
    -h | --help)
      sed -n '2,20p' "$0"
      exit 0
      ;;
    *)
      echo "未知参数：$1（支持：--print-env / -h）" >&2
      exit 2
      ;;
  esac
  shift
done

# 平台默认 base：macOS = 数据卷 /Volumes/DataExt/tmp（2026-09-17 政策）；
# Windows（Git Bash/MSYS）本机数据卷是 E:，统一放 E:\tmp；其它平台退回 $TMPDIR。
os="$(printf '%s' "${ABB_GATES_OS:-$(uname -s)}" | tr '[:upper:]' '[:lower:]')"
case "$os" in
  darwin*) default_base=/Volumes/DataExt/tmp ;;
  mingw* | msys* | cygwin*) default_base=/e/tmp ;;
  *) default_base="${TMPDIR:-/tmp}" ;;
esac
base="${ABB_BUILD_TMP:-$default_base}"

# 归一：优先用父目录的真实路径拼，避免 `..`/符号链接绕过下面的前缀比对；
# 父目录不存在（首次使用）时退回原样字符串。
if [ -d "$base" ]; then
  base="$(cd "$base" && pwd -P)"
elif [ -d "$(dirname "$base")" ]; then
  base="$(cd "$(dirname "$base")" && pwd -P)/$(basename "$base")"
fi
case "$base" in
  /*) ;;
  *) base="$PWD/$base" ;;
esac

export TMPDIR="$base"
export CARGO_TARGET_DIR="$base/abb-target"

repo_real="$(cd "$repo_root" && pwd -P)"
case "$base" in
  "$repo_real" | "$repo_real"/*)
    echo "❌ 构建中间产物不许落在 checkout 内：base=$base（repo=$repo_real）" >&2
    echo "   macOS: ABB_BUILD_TMP=/Volumes/DataExt/tmp；Windows: ABB_BUILD_TMP=/e/tmp" >&2
    echo "   详见 AGENTS.md 的 Build/Test 段与线程 abb-build-tmp-windows-20261011。" >&2
    exit 1
    ;;
esac

if [ "$print_only" = 1 ]; then
  echo "os=$os"
  echo "TMPDIR=$TMPDIR"
  echo "CARGO_TARGET_DIR=$CARGO_TARGET_DIR"
  echo "repo=$repo_real"
  exit 0
fi

# 只在真跑门禁时建目录：`--print-env`（含 macOS 口径探测）不该在磁盘上留下任何东西。
mkdir -p "$CARGO_TARGET_DIR"

cd "$repo_root"
echo "== ABB 门禁（中间产物外置于 $CARGO_TARGET_DIR）=="
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo build --locked
tools/check_test_isolation.sh
echo "✅ 四条门禁全绿；checkout 内未产生 target/（中间产物在 $CARGO_TARGET_DIR）"
