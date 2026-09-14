#!/usr/bin/env bash
# 守卫（#331）：断言分发的 Windows 可执行文件**不依赖 VC++ Redistributable**。
#
# 背景：用户首次安装后启动 agent-bridge.exe 直接报
#   「由于找不到 VCRUNTIME140.dll，无法继续执行代码」
# —— MSVC 默认动态链接 CRT，而 `VCRUNTIME140.dll` / `MSVCP140.dll` 来自
# Visual C++ Redistributable，干净 Windows 上并不存在（`ucrtbase.dll` 是系统自带，
# 不属此列）。修法是 `-C target-feature=+crt-static`（见 `.cargo/config.toml`），
# 本脚本负责「修完不许回退」。
#
# 为什么用字节扫描而不是 dumpbin：
# - PE 导入表里的 DLL 名就是文件内的 ASCII 串，有就是有；
# - CI 上不必装 VS 开发环境、不必引第三方 action，**本地（macOS）也能跑**，
#   于是「喂一个故意坏的样本看它红」这条纪律能在本机兑现，而不是只在 CI 上口头保证。
#
# 用法：tools/check_dynamic_crt.sh <exe> [<exe>...]
# 退出码：0=干净；1=仍有动态 CRT 依赖；2=用法/文件错误。
set -uo pipefail

if [ "$#" -eq 0 ]; then
  echo "用法：$0 <exe> [<exe>...]" >&2
  exit 2
fi

# 只列**来自 Redist** 的那些；ucrtbase 是 Windows 自带组件，不在此列。
FORBIDDEN=("VCRUNTIME140.dll" "VCRUNTIME140_1.dll" "MSVCP140.dll")

fail=0
for f in "$@"; do
  if [ ! -f "$f" ]; then
    echo "❌ 找不到文件：$f" >&2
    exit 2
  fi
  hits=""
  for pat in "${FORBIDDEN[@]}"; do
    # -a：把二进制当文本搜（Git Bash 的 grep 也认）；命中即记下。
    if LC_ALL=C grep -q -a -- "$pat" "$f" 2>/dev/null; then
      hits="$hits $pat"
    fi
  done
  if [ -n "$hits" ]; then
    echo "❌ $f 仍动态依赖 CRT：$hits"
    echo "   → 该文件在没装 VC++ Redistributable 的机器上会起不来（#331）。"
    fail=1
  else
    echo "✅ $f 未出现 VCRUNTIME/MSVCP 导入名（CRT 已静态链接）"
  fi
done

if [ "$fail" -ne 0 ]; then
  echo "::error::可执行文件依赖 VC++ Redistributable，干净机器会起不来（#331）"
  exit 1
fi
