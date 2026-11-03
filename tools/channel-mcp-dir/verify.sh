#!/bin/sh
# 一键验证 ABB 的 MCP 渠道总线：不用配 ABB、不用 IM 账号、不用网络。
#
# 干什么：起一个「目录当渠道」的 MCP server，冒充 ABB 走完整条协议
#         （initialize → tools/list → poll 收消息 → send_text 回消息），
#         然后打印它收到了什么、回了什么。
#
# 为什么需要它：单测证明的是「代码符合契约」，而这个脚本证明的是
#         「**这台机器上**真的能跑」——两者不是一回事（缺 python3、权限、
#         路径里有空格都会让后者失败而前者照绿）。
#
# 用法：
#     sh tools/channel-mcp-dir/verify.sh
#     sh tools/channel-mcp-dir/verify.sh --keep     # 保留目录供检查
#
# 真实接入时，把这个 server 换成真正连 IM 的那个，其余一模一样。

set -eu

here=$(cd "$(dirname "$0")" && pwd)
script="$here/channel_mcp_dir.py"
keep=0
[ "${1:-}" = "--keep" ] && keep=1

command -v python3 >/dev/null 2>&1 || {
  echo "❌ 找不到 python3。这个参考实现需要它（真渠道不必是 Python）" >&2
  exit 2
}
[ -f "$script" ] || { echo "❌ 找不到 $script" >&2; exit 2; }

dir=$(mktemp -d "${TMPDIR:-/tmp}/abb-verify-XXXXXX")
cleanup() { [ "$keep" = 1 ] || rm -rf "$dir"; }
trap cleanup EXIT

echo "📁 渠道目录：$dir"
[ "$keep" = 1 ] && echo "   （--keep：不会删除）"

mkdir -p "$dir/inbox"

# ── 1. 投递一条入站消息（模拟「别人给 ABB 发消息」）────────────────────
cat > "$dir/inbox/0001.json" <<'JSON'
{"chat_id":"demo-chat","message_id":"demo-1","sender_id":"demo-user",
 "sender_name":"演示","text":"你好，这是一条测试消息","ts":1793600001}
JSON
echo "📥 已投递 1 条消息到 inbox/0001.json"

# ── 2. 冒充 ABB 走一遍协议 ────────────────────────────────────────
echo
echo "🔌 冒充 ABB 与渠道握手、取消息、回消息…"
out=$(
  printf '%s\n%s\n%s\n' \
    '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"verify","version":"1"}}}' \
    '{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"poll","arguments":{}}}' \
    '{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"send_text","arguments":{"chat_id":"demo-chat","text":"收到，这是 ABB 的回复"}}}' \
  | python3 "$script" --dir "$dir" --no-resources 2>/dev/null
)

# ── 3. 断言（失败要**响**，不能只是打印一堆 JSON 让人自己看）──────────
fail=0
check() { # check <描述> <条件表达式结果 0/1>
  if [ "$2" = 0 ]; then printf '  ✅ %s\n' "$1"; else printf '  ❌ %s\n' "$1"; fail=1; fi
}

echo "🔍 检查："
# 握手成功
echo "$out" | grep -q '"serverInfo"' && check "渠道回了 initialize（含 serverInfo）" 0 \
  || check "渠道回了 initialize（含 serverInfo）" 1
# 收到了消息
echo "$out" | grep -q '你好，这是一条测试消息' && check "poll 取到了投递的消息" 0 \
  || check "poll 取到了投递的消息" 1
# 取完归档（不重复投递）
[ ! -f "$dir/inbox/0001.json" ] && check "消息已从 inbox 归档（不会重复投递）" 0 \
  || check "消息已从 inbox 归档（不会重复投递）" 1
# 回复写出去了
if [ -n "$(ls -A "$dir/outbox" 2>/dev/null || true)" ]; then
  check "send_text 写出了 outbox 文件" 0
  # 正文逐字送达
  if grep -q '收到，这是 ABB 的回复' "$dir"/outbox/* 2>/dev/null; then
    check "回复正文逐字送达" 0
  else
    check "回复正文逐字送达" 1
  fi
else
  check "send_text 写出了 outbox 文件" 1
  check "回复正文逐字送达" 1
fi

echo
if [ "$fail" = 0 ]; then
  echo "✅ 全部通过 —— 这台机器上 MCP 渠道总线可用。"
  echo
  echo "   这条链就是 ABB 接任何真渠道走的路："
  echo "   渠道 server 自报能力 → ABB 选入站方式（订阅/轮询）→ 取消息 → 翻译 → 回复。"
  echo "   换成连真 IM 的 server，其余一行不用改。"
  exit 0
else
  echo "❌ 有检查未通过（见上）。"
  echo
  echo "   若上面已打印「找不到 python3」之类，是环境问题；"
  echo "   否则请把完整输出发给维护者。"
  exit 1
fi
