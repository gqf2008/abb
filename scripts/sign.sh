#!/bin/bash
# 用自签证书 agent-bridge-dev 重签 agent-bridge 二进制，固定「身份」。
#
# 为什么必须：TCC（macOS 隐私权限）对裸二进制的授权行会锚定 code identity。cargo 默认
# ad-hoc 签名的 DR 只有 cdhash —— 每次 rebuild cdhash 都变 → TCC 把每个新构建当「新应用」，
# 已授权的权限全部失配（系统设置显示已授权、程序内检测却是未授权，就是这个坑）。
# 自签证书签名的 DR 锚定 identifier + 证书根：重编重签后身份不变，授权可永续。
#
# 用法：cargo build 之后跑一次；services 重启自动生效。
#   ./scripts/sign.sh
# 无证书（agent-bridge-dev 未导入 keychain）时跳过并提示，不阻塞开发。
set -euo pipefail

BIN="${1:-target/debug/agent-bridge}"
CERT_ID="agent-bridge-dev"
# 注意：不传 entitlements —— 见下方 codesign 调用的 ⚠️ 注释（usage-description 会杀进程）

cd "$(dirname "$0")/.."

if ! security find-identity -p codesigning 2>/dev/null | grep -q "$CERT_ID"; then
  echo "[sign] ⚠️ 未找到自签证书 $CERT_ID，跳过重签（可运行 scripts/make-certs.sh 生成）"
  exit 0
fi

# 固定 identifier（与 DR 里的 identifier "agent-bridge" 一致；每次重签都不变）。
# ⚠️ 不要给裸二进制加 usage-description entitlements（NSCameraUsageDescription 等）：
#    实测系统会对带这些键的进程启动早期直接 SIGKILL（无 bundle 结构无 Info.plist 配对，
#    AMFI 校验不通过）。TCC 按 code identity 记录授权，裸二进制无需声明也能正常弹窗授权。
codesign --sign "$CERT_ID" --force --timestamp=none \
  --identifier "agent-bridge" \
  "$BIN"

echo "[sign] 已用 $CERT_ID 重签 $BIN"
codesign -d -r- "$BIN" 2>&1 | sed 's/^/  /'
