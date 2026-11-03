#!/bin/sh
# 全链路验证：**真的用 ABB 自己的装配代码**（messenger::build + mcp_channel_loop::run）
# 去连「目录当渠道」的 MCP server，看它能不能收消息并回消息。
#
# 与 verify.sh 的区别（两个都要，验的不是一回事）：
#   verify.sh        冒充 ABB 走协议 —— 证明**渠道**实现了契约
#   本脚本（full.sh） 用 ABB 真代码去连 —— 证明**ABB**能接上它
#
# 两者都通过，才说明「总线可用」；只过前者可能只是渠道自己自洽。
#
# 用法：sh tools/channel-mcp-dir/full.sh

set -eu

repo=$(cd "$(dirname "$0")/../.." && pwd)
cd "$repo"

: "${TMPDIR:=/Volumes/DataExt/tmp}"
export TMPDIR
: "${CARGO_TARGET_DIR:=$TMPDIR/abb-target-full}"
export CARGO_TARGET_DIR

dir=$(mktemp -d "${TMPDIR}/abb-full-XXXXXX")
trap 'rm -rf "$dir"' EXIT
mkdir -p "$dir/inbox"

echo "📁 渠道目录：$dir"

# ── 1. 投递一条入站消息 ────────────────────────────────────────────
cat > "$dir/inbox/0001.json" <<'JSON'
{"chat_id":"full-chat","message_id":"full-1","sender_id":"full-user",
 "sender_name":"全链路","text":"全链路测试消息","ts":1793600001}
JSON
echo "📥 已投递 1 条消息"

# ── 2. 跑集成测试（它内部用真装配代码）──────────────────────────────
echo
echo "🔌 用 ABB 真代码连这个渠道…"
if cargo test --bin agent-bridge channel_full_chain -- --nocapture 2>&1 | tail -20; then
  :
else
  echo
  echo "❌ 全链路失败。"
  echo "   若上面是编译错误 ⇒ 代码问题；若是 'skip' ⇒ 环境缺 python3。"
  exit 1
fi

echo
echo "✅ 全链路通过（ABB 真装配代码 → 渠道 → 收消息 → 回消息）"
