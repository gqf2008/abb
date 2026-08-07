#!/bin/zsh
# 构建 + 组装 ABB 菜单栏 app 到 ~/Applications/ABB.app
# Rust release 编译 → 组 bundle → ad-hoc 签名 → 安装。
# （完整 Developer ID 签名 + 公证走 ~/scripts/notarize.sh）
set -e
cd "${0:a:h}/.."
APP_NAME=ABB
BIN=agent-bridge
BUNDLE="$APP_NAME.app"

echo "[1/5] cargo release 编译（aarch64）…"
cargo build --release --target aarch64-apple-darwin

echo "[2/5] 组装 .app bundle…"
rm -rf "$BUNDLE"
mkdir -p "$BUNDLE/Contents/MacOS" "$BUNDLE/Contents/Resources"
cp "target/aarch64-apple-darwin/release/$BIN" "$BUNDLE/Contents/MacOS/$BIN"
cp app-assets/Info.plist "$BUNDLE/Contents/Info.plist"
if [ -f app-assets/AppIcon.icns ]; then
  cp app-assets/AppIcon.icns "$BUNDLE/Contents/Resources/AppIcon.icns"
else
  echo "    (无 AppIcon.icns，跳过图标)"
fi

echo "[3/5] 自签证书签名（身份固定 → TCC 授权永续，不公证）…"
# 必须用 agent-bridge-dev 证书（DR 锚证书而非 cdhash），ad-hoc 每次编译 cdhash 都变 →
# TCC 授权全失配。证书不存在时先跑 scripts/make-certs.sh 生成。
if security find-identity -p codesigning 2>/dev/null | grep -q "agent-bridge-dev"; then
  codesign -s "agent-bridge-dev" --force --timestamp=none --identifier "agent-bridge" "$BUNDLE/Contents/MacOS/$BIN" >/dev/null
  codesign -s "agent-bridge-dev" --force --timestamp=none --identifier "agent-bridge" "$BUNDLE" >/dev/null
  echo "    已用 agent-bridge-dev 证书签名（不公证）"
else
  echo "    ⚠️ 未找到 agent-bridge-dev 证书，回落 ad-hoc（TCC 授权会失配，请先跑 scripts/make-certs.sh）"
  codesign -s - --force "$BUNDLE" >/dev/null 2>&1 || true
fi

echo "[4/5] 安装到 ~/Applications…"
mkdir -p ~/Applications
rm -rf ~/Applications/"$BUNDLE"
mv "$BUNDLE" ~/Applications/

echo "[5/5] 完成：~/Applications/$BUNDLE"
echo "启动: open ~/Applications/$BUNDLE"
echo "公证: ~/scripts/notarize.sh ~/Applications/$BUNDLE"
