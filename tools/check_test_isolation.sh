#!/usr/bin/env bash
# 守卫（#330）：全量单测必须在隔离 HOME 下运行，且不得创建 `.agent-bridge`。
#
# 直接对真实 `~/.agent-bridge` 做前后 diff 会被正在运行的 service 干扰：bridge.out、
# bot-status.json 等活文件本来就会持续变化，无法区分“测试写的”还是“service 写的”。
# 因此这里把 HOME 指到临时目录，并用 AGENT_BRIDGE_HOME 显式指定运行数据目录；
# Windows 的 dirs::home_dir() 不读 HOME，只有后者能在该平台守住隔离边界。
# 测试结束后 `$tmp_home/.agent-bridge` 存在即失败。
#
# 用法：tools/check_test_isolation.sh [cargo test 的过滤参数...]
# 退出码：0=隔离通过；1=测试失败或发现 `.agent-bridge` 落点。
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
real_home="${HOME:?HOME 未设置}"
tmp_home="$(mktemp -d "${TMPDIR:-/tmp}/abb-test-home.XXXXXX")"
trap 'rm -rf -- "$tmp_home"' EXIT

export HOME="$tmp_home"
export AGENT_BRIDGE_HOME="$tmp_home/.agent-bridge"
# Git Bash 下交给原生 Windows 二进制前要先转成 Windows 路径。
if command -v cygpath >/dev/null 2>&1; then
  export AGENT_BRIDGE_HOME="$(cygpath -w "$AGENT_BRIDGE_HOME")"
fi
# Cargo/rustup 仍用用户原配置，避免隔离 HOME 触发重新下载工具链。
export CARGO_HOME="${CARGO_HOME:-$real_home/.cargo}"
export RUSTUP_HOME="${RUSTUP_HOME:-$real_home/.rustup}"

# `--show-output`：让**通过**的用例也把 stdout/stderr 打出来。理由（卡
# `abb-blur-race-flaky-20261029` 评审 NB-1）：libtest 默认**丢弃通过用例的输出**，于是
# 「环境不满足 ⇒ 用例大声跳过那一条」这类 `[skip]` 说明在默认跑法里完全静音——同一个变异
# 带 `--nocapture` 能看到「实测 295ms，本拍被拖过头」，默认跑法只剩一行 `test ... ok`，
# 门禁日志里「跳过了」和「真验过」长得一模一样。这条门禁要能看见跳过。
#
# 为什么不用 `--nocapture`：它会取消测试线程的 stdout 捕获、把输出与测试进度混在一起，
# 而 `--show-output` 只在单元测试结果之后按用例分段打印，日志仍然可读。
# `--show-output` 是**测试二进制**的参数，必须放在 `--` 之后（放在 cargo 侧会报
# `unexpected argument`）。调用方仍按原约定传「测试名过滤」这类 cargo 侧参数，例如
#   tools/check_test_isolation.sh blur_
# 若真的需要透传测试二进制参数，请放宽下面这行（本仓目前没有这样的调用点）。
(cd "$repo_root" && cargo test --locked "$@" -- --show-output)

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
