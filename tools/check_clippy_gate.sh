#!/usr/bin/env bash
# check_clippy_gate.sh — clippy 门禁的**绝对量**守卫（exit 0 才算绿）
#
# 为什么存在（卡 abb-clippy-gate-red-on-main-20261022）：
#   clippy 曾是四条门禁之一，但基线变红之后，实际的通关标准退化成了「与 base 比对
#   零新增 lint」。这是**相对量**：只要不比 base 更红就能过，于是红色永远不会被清掉。
#   实测轨迹：2026-10-11 修绿 → 10-14～10-17 的 syschat v11 批与 macOS 窗口改造
#   又攒出 25 条基线红 → 期间每张卡自述「clippy 对 base 零新增」。
#   本脚本把「必须绿」变回**可机判的绝对量**：
#     cargo clippy --all-targets -- -D warnings  ⇒ exit 0
#
# 同时钉死工具链：`rust-toolchain.toml` 存在时，本脚本额外断言 rustup 在该 checkout
# 内解析到的版本**就是**文件里写的版本。否则「绿」取决于本机上次 rustup update 是哪天，
# 那还是不可复现——lint 集合与默认级别随版本变化。
#
# 用法：
#   tools/check_clippy_gate.sh              # 跑门禁（exit 0 = 绿）
#   tools/check_clippy_gate.sh --self-test  # 自测：注入一条无害 warning，断言本守卫变红
#
# 退出码：0=绿；1=门禁红（有 lint）；2=用法/环境错误（工具链不符、cargo 缺失等）。
# 自测模式下：0=守卫确实能变红（判别力成立）；1=守卫没变红（守卫失效）。
set -uo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

# 构建中间产物外置（用户政策：启动卷不存构建输出）。导出即生效，也便于自测复用。
export TMPDIR="${TMPDIR:-/Volumes/DataExt/tmp}"

die() { printf 'check_clippy_gate: %s\n' "$*" >&2; exit 2; }

# 钉死版本的自证：rust-toolchain.toml 里的 channel 必须与 rustup 实际解析结果一致。
# 为什么这条要单独断言：文件存在≠生效（比如 CI 里显式 `rustup default` 会盖过它）。
expect_toolchain() {
  local f="$repo_root/rust-toolchain.toml"
  [ -f "$f" ] || die "缺少 rust-toolchain.toml（工具链未钉死 ⇒ 「绿」不可复现）"
  local want
  want="$(sed -n 's/^channel[[:space:]]*=[[:space:]]*"\(.*\)"[[:space:]]*$/\1/p' "$f" | head -1)"
  [ -n "$want" ] || die "rust-toolchain.toml 里找不到 channel"
  local got
  got="$(rustup show active-toolchain 2>/dev/null | awk '{print $1}')"
  case "$got" in
    "$want"|"$want"-*) : ;;
    *) die "rustup 解析到 '$got'，但 rust-toolchain.toml 钉的是 '$want'（两者必须一致）" ;;
  esac
  printf '  工具链：%s（与 rust-toolchain.toml 一致）\n' "$got"
}

run_clippy() {
  # --all-targets：只查 bin 会漏掉 test/example 里的 lint（本仓 25 条里有 4 条就在
  #   `src/ui_headless_render_test.rs`，靠 --all-targets 才看得见）。
  # -D warnings：warning 即失败——否则「有 25 条 warning」与「零」在退出码上无差别。
  cargo clippy --all-targets -- -D warnings
}

case "${1:-}" in
  --self-test)
    echo "── check_clippy_gate 自测（注入无害 warning，断言守卫变红）"
    expect_toolchain || exit 1

    # 探针必须注入到**确实参与编译**的文件里。
    # 曾经的第一版把探针写成 `src/zzz_*.rs`：那是个没有 `mod` 声明的孤儿文件，
    # cargo 根本不编译它 ⇒ clippy 当然 exit 0，自测就变成了「验证了一个不存在的东西」。
    # 这条错误正是本脚本要防的那类：判据看着在跑，其实没测到目标。
    probe_file="$repo_root/src/syschat.rs"
    backup="$(mktemp)"
    cp "$probe_file" "$backup"
    before_sha="$(sha256sum "$probe_file" | awk '{print $1}')"
    cleanup() {
      cp "$backup" "$probe_file"
      rm -f "$backup"
    }
    trap cleanup EXIT INT TERM
    # 无害但必然触发 lint：`let _ = &x;` 是 needless_borrow（clippy 各版本都报）。
    # 追加到已声明模块的末尾，保证被 bin target 编译到。
    printf '\n#[allow(dead_code)]\nfn zzz_clippy_gate_probe() {\n    let x = 1u8;\n    let _ = &x;\n}\n' >> "$probe_file"

    if run_clippy >/tmp/clippy_gate_selftest.log 2>&1; then
      echo "❌ 守卫失效：注入了必然触发 lint 的代码，clippy 却仍 exit 0"
      echo "   （注入点 ${probe_file}，日志 /tmp/clippy_gate_selftest.log）"
      exit 1
    fi
    echo "✅ 判别力成立：注入无害 warning ⇒ clippy 变红（exit != 0）"
    cleanup
    after_sha="$(sha256sum "$probe_file" | awk '{print $1}')"
    trap - EXIT INT TERM
    if [ "${before_sha}" != "${after_sha}" ]; then
      echo "❌ 还原失败：${probe_file} 与注入前不一致" >&2
      exit 1
    fi
    echo "   还原核对：${probe_file} sha256 = ${after_sha}（与注入前一致）"
    echo "── 自测通过"
    exit 0
    ;;

  "")
    echo "── clippy 门禁（绝对量：必须 exit 0）"
    expect_toolchain || exit 1
    if run_clippy; then
      echo "✅ clippy 绿"
      exit 0
    fi
    echo "❌ clippy 有 lint（见上方 cargo 输出）。" >&2
    echo "   注意：本门禁是绝对量，「我对 base 零新增」**不算通过** ——" >&2
    echo "   基线红就是红，请把它在本次提交里清掉（卡 abb-clippy-gate-red-on-main-20261022）。" >&2
    exit 1
    ;;

  -h|--help)
    sed -n '2,20p' "$0" | sed 's/^# \{0,1\}//'
    exit 0
    ;;

  *)
    die "未知参数：$1（用 --help 看用法）"
    ;;
esac
