#!/usr/bin/env bash
# 守卫（#330）：全量单测必须在隔离 HOME 下运行，且不得创建 `.agent-bridge`。
#
# 直接对真实 `~/.agent-bridge` 做前后 diff 会被正在运行的 service 干扰：bridge.out、
# bot-status.json 等活文件本来就会持续变化，无法区分“测试写的”还是“service 写的”。
# 因此这里把 HOME 指到临时目录：若代码仍走 `crate::bridge_dir()` / `dirs::home_dir()`，
# 落点一定会出现在 `$tmp_home/.agent-bridge`；测试结束后该目录存在即失败。
#
# 用法：tools/check_test_isolation.sh [cargo test 的过滤参数...]
# 退出码：0=隔离通过；1=测试失败或发现 `.agent-bridge` 落点。
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
real_home="${HOME:?HOME 未设置}"
tmp_home="$(mktemp -d "${TMPDIR:-/tmp}/abb-test-home.XXXXXX")"
trap 'rm -rf -- "$tmp_home"' EXIT

export HOME="$tmp_home"
# Cargo/rustup 仍用用户原配置，避免隔离 HOME 触发重新下载工具链。
export CARGO_HOME="${CARGO_HOME:-$real_home/.cargo}"
export RUSTUP_HOME="${RUSTUP_HOME:-$real_home/.rustup}"

(cd "$repo_root" && cargo test "$@")

if [ -e "$tmp_home/.agent-bridge" ]; then
  files="$(find "$tmp_home/.agent-bridge" -type f -print | sort)"
  if [ -n "$files" ]; then
    echo "❌ 测试在隔离 HOME 下写入了运行数据文件：" >&2
    printf '%s\n' "$files" >&2
    exit 1
  fi
  # 空目录不构成数据覆盖，但仍打印出来便于发现可继续收敛的构造副作用。
  echo "ℹ️ 测试只留下了空目录（无运行数据文件）："
  find "$tmp_home/.agent-bridge" -type d -print | sort
fi

echo "✅ 测试未写入 .agent-bridge 运行数据文件；隔离守卫通过"
