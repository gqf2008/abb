#!/usr/bin/env bash
# 门禁一键入口：**先把构建中间产物赶到 checkout 之外**，再跑 AGENTS.md 定义的门禁——
# **两组共八条**：root 包四条（fmt / clippy / build --locked / 隔离全量测试）+ `crates/abb-chart`
# 四条（同形命令，但都带 `--manifest-path`）。
#
# 为什么 chart 要单独一组：`crates/abb-chart` 是**独立包**（自带 manifest + lock + MSRV 1.85），
# 而 root 的 `Cargo.toml` **没有 `[workspace]`** ⇒ 不带 `--manifest-path` 的 root 命令
# **语义上完全碰不到它**。补这一组之前实测：一次门禁日志 3654 行里 `Running tests/cli_e2e`
# 出现 0 次——chart 的测试红了，本机合并门禁照样全绿。它此前的覆盖只有 GitHub Actions 的
# `abb-chart-lint` / `abb-chart-windows`，而本仓合并门禁**不要求 CI run** ⇒ 缺口就在这里。
#
# 为什么存在：2026-09-17 的 macOS 政策（`/Volumes/DataExt/tmp`）只覆盖 macOS，
# Windows 上照文档执行 = 裸 `cargo` = 中间产物全落 checkout（2026-10-11 实测把仓库撑到
# 38 G：`target/` 32 G + `crates/abb-agent/target/` 2.4 G + 僵尸 `crates/buzz-agent/target/`）。
# 本脚本把「外置 target」变成**默认行为**，并**硬拒绝**落在 checkout 内的 base——
# 这样这条政策才有机器判据（`RULE_规则可执行性.md` 第 1 条）。
#
# 用法：
#   tools/gates.sh                 # 跑两组门禁（root 四条 + crates/abb-chart 四条）
#   tools/gates.sh --print-env     # 只打印解析结果（os、TMPDIR、TMP、TEMP、CARGO_TARGET_DIR、repo）
#   ABB_BUILD_TMP=/e/tmp tools/gates.sh   # 覆盖 base（默认按平台取数据卷；**必须已存在**）
#   ABB_GATES_OS=darwin tools/gates.sh --print-env   # 覆盖平台探测（用于在本机核 macOS 口径）
#
# 两条硬保证（都有阳性对照，见 RULE_可执行性）：
#   1. base 落在 checkout 内 ⇒ 直接失败；
#   2. base 不存在 ⇒ 直接失败（不代建，避免把产物静默写到启动卷/系统盘）。
#
# 退出码：0=门禁全绿；1=政策违规（base 在 checkout 内 / base 不存在）；其它=cargo/测试自身的失败码
#   （两组任一条失败都会立刻以该条的状态码退出，后续门禁不再跑）。
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

print_only=0
while [ $# -gt 0 ]; do
  case "$1" in
    --print-env | --dry-run) print_only=1 ;;
    -h | --help)
      # 头注自 `# 门禁一键入口` 起到 `set -euo pipefail` **之前**：不写死行号，改注释不会错位。
      sed -n '/^# 门禁一键入口/,/^set -euo pipefail$/p' "$0" | sed -n '/^set -euo pipefail$/q;p'
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
# Windows（Git Bash/MSYS）本机数据卷是 E:，统一放 E:\tmp；其它平台退回 ${TMPDIR}。
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
# **每个 worktree 用自己的 target 目录**（卡 abb-target-dir-per-worktree-20261022）。
#
# 为什么：原来固定写 `$base/abb-target`，多个 worktree 并行时全部指向同一个目录，
# cargo 的文件锁互相饿等——实测单次 `cargo build` 卡 25+ 分钟无进展、系统 load 到 12.4、
# `check_test_isolation.sh` 连续超时（测试本身只要 15 秒），而这些超时**看起来像测试失败**。
#
# 命名用 worktree 目录名：它天然唯一、可读（能一眼看出是哪个 worktree 的产物），且同一个
# worktree 反复跑仍是同一个路径 ⇒ 增量缓存照旧生效。
# 例外：`ABB_TARGET_SHARED=1` 时退回共用目录（给「只有一个 worktree、想省磁盘」的场景留退路）。
if [ "${ABB_TARGET_SHARED:-0}" = "1" ]; then
  export CARGO_TARGET_DIR="$base/abb-target"
else
  export CARGO_TARGET_DIR="$base/abb-target-$(basename "$repo_root")"
fi
# Windows 原生进程（rustc/link、cmd）**不认 `TMPDIR`**（那是 POSIX 约定），只看 `TMP`/`TEMP`：
# 不补这两个，link/rustc 的 scratch 仍留在系统盘 C: 的 `%TEMP%`。cygpath 只在 MSYS/Cygwin 存在，
# 正好当平台判据，并把 `/e/tmp` 转成原生 `E:\tmp`。
if command -v cygpath >/dev/null 2>&1; then
  native_base="$(cygpath -w "$base")"
  export TMP="$native_base"
  export TEMP="$native_base"
fi

repo_real="$(cd "$repo_root" && pwd -P)"
case "$base" in
  "$repo_real" | "$repo_real"/*)
    echo "❌ 构建中间产物不许落在 checkout 内：base=${base}（repo=${repo_real}）" >&2
    echo "   macOS: ABB_BUILD_TMP=/Volumes/DataExt/tmp；Windows: ABB_BUILD_TMP=/e/tmp" >&2
    echo "   详见 AGENTS.md 的 Build/Test 段与线程 abb-build-tmp-windows-20261011。" >&2
    exit 1
    ;;
esac

if [ "$print_only" = 1 ]; then
  echo "os=$os"
  echo "TMPDIR=$TMPDIR"
  echo "TMP=${TMP:-<unset>}"
  echo "TEMP=${TEMP:-<unset>}"
  echo "CARGO_TARGET_DIR=$CARGO_TARGET_DIR"
  echo "repo=$repo_real"
  exit 0
fi

# **base 必须已存在**：数据卷挂载点/临时根目录是运维先备好的，不由本脚本代建。
# 代建会把产物静默写到启动卷/系统盘上（`/Volumes/DataExt` 没挂载时，`mkdir -p` 会在
# 启动卷上建出一个同名目录，然后照常构建）——那正是这条政策要禁的形态。
if [ ! -d "$base" ]; then
  echo "❌ base 目录不存在：$base" >&2
  echo "   数据卷没挂载？本脚本不会替你创建，以免产物静默落到启动卷/系统盘。" >&2
  echo "   先准备好该目录（macOS: mkdir -p /Volumes/DataExt/tmp；Windows: mkdir -p /e/tmp），" >&2
  echo "   或用 ABB_BUILD_TMP 指到一个已存在的 checkout 外目录。" >&2
  exit 1
fi

# 只在真跑门禁时建产物子目录：`--print-env`（含 macOS 口径探测）不写磁盘。
mkdir -p "$CARGO_TARGET_DIR"

# `crates/abb-chart` 的 target **另开一个子目录**：与 root 共用同一个目录会重演上面那段
# 「cargo 文件锁互相饿等」（不同包、不同 feature 集，锁的粒度却是整目录）。
chart_manifest="$repo_root/crates/abb-chart/Cargo.toml"
chart_target="$CARGO_TARGET_DIR-chart"
mkdir -p "$chart_target"

cd "$repo_root"
echo "== ABB 门禁：root 包（中间产物外置于 ${CARGO_TARGET_DIR}）=="
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
cargo build --locked
tools/check_test_isolation.sh
# 兜底：清掉 TMPDIR 里陈旧的测试夹具（见脚本头注；根因修复在 src/testtmp.rs 的 RAII 归属）。
tools/clean_test_fixtures.sh

# crates/abb-chart 是独立包，root 的四条碰不到它（见头注）⇒ 这里补一组 `--manifest-path` 门禁。
# 测试不用 tools/check_test_isolation.sh：那个守卫查的是「测试不得写 ~/.agent-bridge」（root 包
# 有大量运行数据落点），而 chart 只管出图、不碰运行目录；它要的是**离线可跑**（`--offline`）：
# 随包分发的 CLI 不该为了跑单测去联网。cargo 跑测试二进制时 cwd 就是包根，manifest 仍用绝对
# 路径（免得将来有人在这段前面 `cd` 到别处，静默跑错包）。
echo "== ABB 门禁：crates/abb-chart（独立包，中间产物外置于 ${chart_target}）=="
(
  export CARGO_TARGET_DIR="$chart_target"
  cargo fmt --manifest-path "$chart_manifest" --check
  cargo clippy --manifest-path "$chart_manifest" --locked --all-targets -- -D warnings
  cargo build --manifest-path "$chart_manifest" --locked
  cargo test --manifest-path "$chart_manifest" --offline
)

echo "✅ 八条门禁全绿（root 四条 + crates/abb-chart 四条）；checkout 内未产生 target/（中间产物在 ${CARGO_TARGET_DIR} 与 ${chart_target}）；测试夹具残留已清扫"
