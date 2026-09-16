#!/usr/bin/env bash
# 守卫（#251）：断言 macOS 产物**真的带上了三项 TCC entitlements**。
#
# 背景（#251）：ABB.app 在 hardened runtime 下若缺 entitlements，agent 子进程访问
# 摄像头/麦克风会被系统**直接拒绝且不弹授权框**（用户看不到任何提示）；AppleScript
# 自动化同理。issue 里最初只在 CI 分发路径带上了 entitlements，本机
# `scripts/build.sh --notarize` 那条路会经 `~/scripts/notarize.sh` 用
# `--deep --force` 重签，**不传 --entitlements 就会把它们签没**。
#
# 为什么要有这个脚本：签名产物是否正确**没有任何自动化断言**（漏带只在真机点相机时才
# 暴露，且表现为"静默拒绝"）。这里把三件事钉死：
#   1) bundle 本身带三项；
#   2) bundle 内的可执行文件（agent-bridge / buzz-agent / abb-helper）也带
#      —— 它们才是真正去碰设备/发 AppleEvent 的责任进程；
#   3) 缺任意一项即失败（不静默放过）。
#
# 用法：tools/check_entitlements.sh <App.app> [<App.app>...]
# 退出码：0=齐全；1=缺项；2=用法/文件错误。
set -uo pipefail

REQUIRED=(
  "com.apple.security.device.camera"
  "com.apple.security.device.audio-input"
  "com.apple.security.automation.apple-events"
)
# 需要逐一带上上述 entitlements 的内部可执行（存在才检查；不存在不算失败——
# 分发包可能不含 abb-helper，见 build.sh 的 `[ -f ... ]` 条件拷贝）。
INNER=(agent-bridge buzz-agent abb-helper)

if [ "$#" -eq 0 ]; then
  echo "用法：$0 <App.app> [<App.app>...]" >&2
  exit 2
fi

fail=0
for app in "$@"; do
  if [ ! -d "$app" ]; then
    echo "❌ 不是 .app 目录：$app" >&2
    exit 2
  fi
  # 逐个目标检查：bundle 自身 + 存在的内部可执行
  targets=("$app")
  for exe in "${INNER[@]}"; do
    [ -f "$app/Contents/MacOS/$exe" ] && targets+=("$app/Contents/MacOS/$exe")
  done
  for t in "${targets[@]}"; do
    # **必须校验值，不能只查 key 文本**（审查实测的假阴性）：entitlement 存在但值为
    # `false` 时 TCC 照样不授权，而字符串匹配会判绿。故取 XML plist（`:-`）后用
    # PlistBuddy 逐个读布尔值，只有 `true` 才算带；缺失/`false`/解析失败（未签名、
    # 空输出、格式异常）一律算缺。
    plist="$(mktemp)"
    codesign -d --entitlements :- "$t" >"$plist" 2>/dev/null || true
    missing=()
    for key in "${REQUIRED[@]}"; do
      value="$(/usr/libexec/PlistBuddy -c "Print :${key}" "$plist" 2>/dev/null || true)"
      if [ "$value" != "true" ]; then
        missing+=("$key")
      fi
    done
    rm -f "$plist"
    if [ "${#missing[@]}" -gt 0 ]; then
      fail=1
      echo "❌ ${t}：缺 entitlements → ${missing[*]}" >&2
    else
      echo "✅ ${t}：三项 entitlements 齐全"
    fi
  done
done

if [ "$fail" -ne 0 ]; then
  echo "❌ 有目标缺 entitlements：相机/麦克风/自动化会被 hardened runtime 静默拒绝（#251）" >&2
  exit 1
fi
echo "✅ entitlements 守卫通过（#251）"
