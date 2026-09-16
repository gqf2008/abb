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
# 守卫路径可覆盖：只为 self-test 用 mock，生产默认仓库内真脚本（**没有**跳过开关——
# 生产路径不允许静默绕过守卫）。
GUARD="${GUARD:-$REPO/tools/check_entitlements.sh}"

# 公证脚本路径可覆盖（self-test 用 mock；生产默认 ~/scripts/notarize.sh）。
notarize_sh() { echo "${NOTARIZE_SH:-$HOME/scripts/notarize.sh}"; }

run_self_test() {
  tmp="$(mktemp -d "${TMPDIR:-/tmp}/abb-notarize-selftest.XXXXXX")"
  # trap 必须保留原退出码：否则 rm 成功后会把失败的 self-test 变成 rc=0（审查实测踩到）
  rc=0
  trap 'rc=$?; rm -rf -- "$tmp"; exit "$rc"' EXIT
  log="$tmp/log.txt"
  : >"$log"
  # 两个 mock：只往同一个日志追加"谁被调用、收到什么参数"，不签名不联网。
  mock_notarize="$tmp/mock-notarize.sh"
  cat >"$mock_notarize" <<'MOCK'
#!/usr/bin/env bash
printf 'notarize %s\n' "$*" >>"${MOCK_LOG:?}"
MOCK
  mock_guard="$tmp/mock-guard.sh"
  cat >"$mock_guard" <<'MOCK'
#!/usr/bin/env bash
printf 'guard %s\n' "$*" >>"${MOCK_LOG:?}"
exit "${MOCK_GUARD_RC:-0}"
MOCK
  chmod +x "$mock_notarize" "$mock_guard"
  fake_app="$tmp/Fake.app"
  mkdir -p "$fake_app"

  # 用例 1（正）：公证 mock 必须收到 --entitlements；守卫 mock 必须被调用；顺序 = 公证 → 守卫
  MOCK_LOG="$log" NOTARIZE_SH="$mock_notarize" GUARD="$mock_guard" \
    "$0" "$fake_app" >/dev/null
  if ! grep -q -- "--entitlements $ENT" "$log"; then
    echo "❌ self-test：公证脚本没收到 --entitlements（实际：$(tr '\n' '|' <"$log")）" >&2
    exit 1
  fi
  if ! grep -q "^guard .*Fake.app" "$log"; then
    echo "❌ self-test：守卫没被调用（实际：$(tr '\n' '|' <"$log")）" >&2
    exit 1
  fi
  first="$(head -1 "$log")"
  case "$first" in
    notarize*) ;;
    *) printf '❌ self-test：调用顺序不对（首行应为 notarize，实际：%s）\n' "$first" >&2; exit 1 ;;
  esac

  # 用例 2（反）：守卫失败必须让 wrapper 失败（不能被静默吞掉）
  : >"$log"
  if MOCK_LOG="$log" MOCK_GUARD_RC=1 NOTARIZE_SH="$mock_notarize" GUARD="$mock_guard" \
       "$0" "$fake_app" >/dev/null 2>&1; then
    echo "❌ self-test：守卫失败时 wrapper 仍返回 0（失败被吞）" >&2
    exit 1
  fi

  echo "✅ self-test：转发 --entitlements ✔ / 守卫被调用 ✔ / 顺序 公证→守卫 ✔ / 守卫失败会传播 ✔"
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
[ -x "$GUARD" ] || { echo "❌ 守卫不可执行：$GUARD" >&2; exit 2; }
NOTARY_SH="$(notarize_sh)"
if [ ! -x "$NOTARY_SH" ]; then
  printf '❌ 未找到可执行的公证脚本：%s（可用 NOTARIZE_SH 覆盖）\n' "$NOTARY_SH" >&2
  exit 2
fi

# 重签（Developer ID + hardened runtime + entitlements）→ 公证 → 装订
"$NOTARY_SH" "$APP" --entitlements "$ENT"

# 最终产物自检：三项 TCC entitlements 必须真在（bundle + 内部可执行）
"$GUARD" "$APP"
echo "✅ 公证 + 装订完成，entitlements 已核：$APP"
