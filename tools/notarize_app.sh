#!/usr/bin/env bash
# Developer ID 签名 + 公证 + 装订的**可提交入口**（#251 收口）。
#
# 为什么需要它：负责签名的那套脚本（`scripts/build.sh`、`~/scripts/notarize.sh`）都在
# `.gitignore` 覆盖的本地目录里，不在版本控制内 —— 于是"必须带 entitlements"这条要求
# 过去只活在注释里，谁也评审不到、换台机器就丢。这里把契约落成仓库内可评审的一层：
#
#   1. **强制**把 `app-assets/abb.entitlements` 传给公证脚本（它用 `--deep --force
#      --options runtime` 重签，不传就会把三项 TCC 授权签没，症状是 hardened runtime
#      下相机/麦克风/自动化被静默拒绝、连授权弹窗都没有）；
#   2. 重签后跑 `tools/check_entitlements.sh` 断言最终产物（bundle + 内部可执行）真带上了；
#   3. `--self-test` 用 mock 公证脚本断言"确实把 --entitlements 传下去了"（不联网、不签名）。
#
# 用法：tools/notarize_app.sh <App.app>
#       tools/notarize_app.sh --self-test
# 退出码：0=成功；1=守卫/公证失败；2=用法或缺少依赖。
set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
ENT="$REPO/app-assets/abb.entitlements"
GUARD="$REPO/tools/check_entitlements.sh"

# 公证脚本路径可覆盖（self-test 用 mock；生产默认 ~/scripts/notarize.sh）。
notarize_sh() { echo "${NOTARIZE_SH:-$HOME/scripts/notarize.sh}"; }

run_self_test() {
  tmp="$(mktemp -d "${TMPDIR:-/tmp}/abb-notarize-selftest.XXXXXX")"
  trap 'rm -rf -- "$tmp"' EXIT
  # mock：只记录收到的参数，不签名不联网
  mock="$tmp/mock-notarize.sh"
  cat >"$mock" <<'MOCK'
#!/usr/bin/env bash
printf '%s\n' "$@" >"${MOCK_RECORD:?}"
MOCK
  chmod +x "$mock"
  fake_app="$tmp/Fake.app"
  mkdir -p "$fake_app"
  # 守卫会被调用，这里用"跳过守卫"的开关把自测聚焦在参数转发上
  MOCK_RECORD="$tmp/args.txt" NOTARIZE_SH="$mock" SKIP_ENTITLEMENT_GUARD=1 \
    "$0" "$fake_app" >/dev/null
  got="$(tr '\n' ' ' <"$tmp/args.txt")"
  case "$got" in
    *"--entitlements $ENT"*)
      echo "✅ self-test：公证脚本收到的参数含 --entitlements $ENT"
      echo "   实际：$got"
      ;;
    *)
      echo "❌ self-test：公证脚本没收到 --entitlements（实际：$got）" >&2
      exit 1
      ;;
  esac
}

if [ "${1:-}" = "--self-test" ]; then
  run_self_test
  exit $?
fi

if [ "$#" -ne 1 ]; then
  echo "用法：$0 <App.app>   |   $0 --self-test" >&2
  exit 2
fi
APP="$1"
[ -d "$APP" ] || { echo "❌ 不是 .app 目录：$APP" >&2; exit 2; }
[ -f "$ENT" ] || { echo "❌ 未找到 entitlements：$ENT" >&2; exit 2; }
NOTARY_SH="$(notarize_sh)"
if [ ! -x "$NOTARY_SH" ]; then
  echo "❌ 未找到可执行的公证脚本：$NOTARY_SH（可用 NOTARIZE_SH 覆盖）" >&2
  exit 2
fi

# 重签（Developer ID + hardened runtime + entitlements）→ 公证 → 装订
"$NOTARY_SH" "$APP" --entitlements "$ENT"

# 最终产物自检：三项 TCC entitlements 必须真在（bundle + 内部可执行）
if [ "${SKIP_ENTITLEMENT_GUARD:-0}" != "1" ]; then
  "$GUARD" "$APP"
fi
echo "✅ 公证 + 装订完成，entitlements 已核：$APP"
