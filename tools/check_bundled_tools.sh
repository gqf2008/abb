#!/bin/bash
# 校验"随包工具目录"真的齐了：按 tools/tool-lock.tsv 的包 + tools/bundled-cli-tools.tsv 的名字
# （按平台过滤）逐个检查 <bin>/<name>[.exe] 存在且可执行。
#
# 为什么需要它：多调包（coreutils/diffutils/busybox）一个包暴露多个名字，靠"包产物名"检查不出
# "名字没链接上/少链接了一个"；而 `--dump-tools --require-bundled-tools` 只在装好的 bundle 里跑。
# 本脚本在 **fetch 之后、打包之前**就能给出判据，且两个平台同一份逻辑（Windows 侧用 bash 跑，
# GH runner 自带 bash；ps1 的取件/链接结果由它统一断言）。
#
# 用法：tools/check_bundled_tools.sh --bin-dir <目录> --platform <macos-arm64|windows-x64>
set -euo pipefail
cd "$(dirname "$0")/.."

BIN=""
PLATFORM=""
while [ $# -gt 0 ]; do
  case "$1" in
    --bin-dir) BIN="${2:-}"; shift 2 ;;
    --platform) PLATFORM="${2:-}"; shift 2 ;;
    -h|--help) sed -n '2,12p' "$0"; exit 0 ;;
    *) echo "未知参数：$1" >&2; exit 2 ;;
  esac
done
[ -n "$BIN" ] || { echo "缺 --bin-dir" >&2; exit 2; }
case "$PLATFORM" in
  macos-arm64|windows-x64) ;;
  *) echo "--platform 只能是 macos-arm64 / windows-x64" >&2; exit 2 ;;
esac
mkdir -p "$BIN"

# Windows 侧产物带 .exe 后缀。
if [ "$PLATFORM" = "windows-x64" ]; then suffix=".exe"; else suffix=""; fi

# 1) 一包一名：tool-lock 的行（包产物名 == 工具名，goawk 除外——它输出名是 awk）
missing=0
while IFS=$'\t' read -r tool version platform package url sha256 inner out; do
  [ "$tool" = "tool" ] && continue
  [ "$platform" = "$PLATFORM" ] || continue
  case "$out" in
    *"$suffix") ;;
    *) echo "  ⚠️ lock 行 output_name「${out}」与平台后缀不符（${PLATFORM}）" >&2; missing=$((missing+1)); continue ;;
  esac
  if [ ! -x "$BIN/$out" ] && [ ! -f "$BIN/$out" ]; then
    echo "  ❌ 缺包产物：$BIN/${out}（包 ${tool}）" >&2; missing=$((missing+1))
  fi
done < tools/tool-lock.tsv

# 2) 多调包暴露的工具名（按平台过滤）
while IFS=$'\t' read -r name provider platform; do
  [ "$name" = "name" ] && continue
  [ "$platform" = "all" ] || [ "$platform" = "$PLATFORM" ] || continue
  if [ ! -f "$BIN/$name$suffix" ]; then
    echo "  ❌ 缺工具名：$BIN/$name${suffix}（provider=${provider}）" >&2; missing=$((missing+1))
  fi
done < tools/bundled-cli-tools.tsv

# 3) 本平台应能解析到的名字全集，与 src/deps.rs 的名字表对齐（防"清单加了名字、deps 没加"）
expected=0
while IFS=$'\t' read -r name provider platform; do
  [ "$name" = "name" ] && continue
  [ "$platform" = "all" ] || [ "$platform" = "$PLATFORM" ] || continue
  expected=$((expected+1))
done < tools/bundled-cli-tools.tsv
echo "  [check] ${PLATFORM}：包产物 + $expected 个多调名字已校验"
if [ "$missing" -ne 0 ]; then
  echo "❌ 随包工具不齐：$missing 项缺失（${PLATFORM}）" >&2
  exit 1
fi
echo "✅ 随包工具齐全（${PLATFORM}）"
