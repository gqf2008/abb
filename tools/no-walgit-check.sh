#!/usr/bin/env bash
# no-walgit-check.sh — 证明「本地没有 walgit 二进制时，本仓的门禁与守卫仍然可用」。
#
# 为什么存在（card abb-d68-local-commands-20261102）：
#   walgit 的协作面是「纯 git + MCP」，本地二进制将不再部署（主机搬到服务器）。
#   本仓一度把 `walgit collab thread` 当成门禁的唯一数据源 —— 那种写法在二进制消失后
#   会整道闸失效。这条断言把「不依赖本地二进制」变成**可重复执行的检查**，而不是
#   靠人记住「这次改对了」。
#
# 它怎么做到「没有 walgit」：不删任何东西，只重排 PATH，把所有含 walgit 可执行的目录
# 摘掉，再用这个精简 PATH 跑各守卫。所以可以随时在任何机器上跑。
#
# 注意：需要 MCP 凭据才有「主路径」可测。凭据按下面的顺序取，取不到就只跑离线部分。
#   WALGIT_MCP_TOKEN（环境）→ 仓库 .ki/settings.json 里 walgit server 的 Authorization
#
# 退出码：0 = 全部符合预期；1 = 有断言失败（会逐条说明）。

set -uo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root" || exit 1

fail=0
pass=0
ok()   { pass=$((pass + 1)); echo "  ✅ $1"; }
bad()  { fail=$((fail + 1)); echo "  ❌ $1"; }

# ── 构造一个不含 walgit 的 PATH ──────────────────────────────────────────────
nowalgit_path() {
  local out="" d
  local IFS=':'
  for d in $PATH; do
    [ -z "$d" ] && continue
    [ -x "$d/walgit" ] && continue
    [ -x "$d/walgit.exe" ] && continue
    out="${out:+$out:}$d"
  done
  printf '%s' "$out"
}
NW_PATH="$(nowalgit_path)"
if PATH="$NW_PATH" command -v walgit >/dev/null 2>&1; then
  bad "无法构造不含 walgit 的 PATH（还有 $(PATH="$NW_PATH" command -v walgit)）"
  exit 1
fi
echo "── PATH 中已无 walgit（共 $(printf '%s' "$NW_PATH" | tr ':' '\n' | wc -l | tr -d ' ') 个目录）"

# ── 取 MCP 凭据（只进环境变量，不进命令行）──────────────────────────────────
token="${WALGIT_MCP_TOKEN:-}"
if [ -z "$token" ] && [ -f .ki/settings.json ]; then
  token="$(python3 - <<'PY' 2>/dev/null || true
import json
try:
    s = json.load(open(".ki/settings.json"))
    print(s["mcpServers"]["walgit"]["headers"]["Authorization"].replace("Bearer ", ""))
except Exception:
    pass
PY
)"
fi
thread="${ABB_NO_WALGIT_THREAD:-abb-d68-local-commands-20261102}"
if [ -z "$token" ]; then
  echo "   （无 MCP 凭据：只验 fail-closed 与纯 git 守卫，主路径跳过）"
fi

# ── 1. 门禁：有凭据 ⇒ 走 MCP 且打干净 ✅ ─────────────────────────────────────
if [ -n "$token" ]; then
  out="$(PATH="$NW_PATH" WALGIT_MCP_TOKEN="$token" bash tools/check_review_cadence.sh "$thread" 2>&1)"
  rc=$?
  if printf '%s' "$out" | grep -q 'MCP collab_thread'; then
    ok "门禁在无 walgit 下走 MCP 数据源"
  else
    bad "门禁没有走 MCP（输出里没有 'MCP collab_thread'）"
    printf '%s\n' "$out" | sed 's/^/       | /'
  fi
  if [ "$rc" -eq 0 ] && printf '%s' "$out" | grep -qE '✅ 通过'; then
    ok "门禁经 MCP 判定，打干净的 ✅（rc=0）"
  elif [ "$rc" -eq 1 ]; then
    ok "门禁经 MCP 判定为违规（rc=1，属正常判定而非失效）"
  else
    bad "门禁经 MCP 未能判定（rc=${rc}）"
    printf '%s\n' "$out" | sed 's/^/       | /'
  fi
fi

# ── 2. 门禁：无凭据 ⇒ fail-closed（exit 2，且说清怎么配）─────────────────────
out="$(PATH="$NW_PATH" env -u WALGIT_MCP_TOKEN bash tools/check_review_cadence.sh "$thread" 2>&1)"
rc=$?
if [ "$rc" -eq 2 ]; then
  ok "无凭据且无 walgit ⇒ fail-closed（rc=2，不静默判绿）"
else
  bad "无凭据且无 walgit 时没有 fail-closed（rc=${rc}）"
  printf '%s\n' "$out" | sed 's/^/       | /'
fi
if printf '%s' "$out" | grep -q 'WALGIT_MCP_TOKEN'; then
  ok "fail-closed 的报因给出了配置指引"
else
  bad "fail-closed 没说清怎么配凭据"
fi

# ── 3. check_collab_parents 自测：纯 git，无 walgit 也应全绿 ──────────────────
if PATH="$NW_PATH" bash tools/check_collab_parents.sh --self-test >/tmp/nw-ccp.log 2>&1; then
  ok "check_collab_parents --self-test 在无 walgit 下通过"
else
  bad "check_collab_parents --self-test 在无 walgit 下失败"
  tail -5 /tmp/nw-ccp.log | sed 's/^/       | /'
fi

# ── 4. 门禁自测：不应依赖本机 walgit ─────────────────────────────────────────
if PATH="$NW_PATH" bash tools/review-cadence/selftest.sh >/tmp/nw-selftest.log 2>&1; then
  n="$(grep -o 'pass=[0-9]*' /tmp/nw-selftest.log | tail -1 | cut -d= -f2)"
  ok "review-cadence selftest 在无 walgit 下全绿（pass=${n}）"
else
  bad "review-cadence selftest 在无 walgit 下失败"
  grep -E 'FAIL|FATAL' /tmp/nw-selftest.log | head -5 | sed 's/^/       | /'
fi

rm -f /tmp/nw-ccp.log /tmp/nw-selftest.log
echo
if [ "$fail" -eq 0 ]; then
  echo "✅ 无 walgit 断言全过（pass=${pass}）"
  exit 0
fi
echo "❌ 无 walgit 断言失败 ${fail} 条（pass=${pass}）"
exit 1
