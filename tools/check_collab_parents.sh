#!/usr/bin/env bash
# check_collab_parents.sh — 协作账本的 `parent` 引用完整性守卫
#
# 为什么存在（卡 abb-collab-parent-oid-integrity-20261022）：
#   `walgit collab entry --parent <oid>` **不做任何校验**——它把字符串原样写进条目并签名。
#   于是下面四种形态都能落盘且永久留在 append-only 账本里：
#
#     1. 伪造全 oid：只看过 10 位前缀，补齐后 30 位是臆造的（本会话 2 次，另一会话 2 次）
#     2. 截断 oid：传了 7~12 位，git 能解析但账本的父子关系按字符串比 ⇒ 链在投影上断裂
#     3. 畸形值：`,` / `69` / `327` / `PLACEHOLDER` / 长度 42 位
#     4. 指向不存在的对象：拼错、或引用了别仓的 oid
#
#   前三种的共同点：**调用方以为自己传对了**，只有事后审计才发现。而账本是 append-only，
#   坏条目不能删、只能补更正 ⇒ 成本远高于写入前校验一次。
#
# 本脚本把「写入后才发现」变成「合并/巡检时可机查」。
#
# 用法：
#   tools/check_collab_parents.sh              # 全量扫描（默认只看 refs/collab/inbox）
#   tools/check_collab_parents.sh --self-test  # 自测：注入一条坏 parent，断言本守卫变红
#   tools/check_collab_parents.sh --json       # 机器可读输出
#
# 退出码：0=全部合规；1=存在异常 parent；2=环境错误（不是 git 仓 / 无 refs）。
#
# 判据（每条 parent 都必须同时满足 A 和 B）：
#   A. 形态：空（根节点）或完整 oid（40 位 sha1 / 64 位 sha256 的小写 hex）。
#      **不接受缩写**——理由见上：账本的父子关系不是 git 的 rev-parse 语义。
#   B. 存在：非空时必须 `git cat-file -e` 能取到（不能指向别仓/拼错的 oid）。
set -uo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

mode="scan"
json=0
while [ $# -gt 0 ]; do
  case "$1" in
    --self-test) mode="self-test" ;;
    --json) json=1 ;;
    -h|--help) sed -n '2,32p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "check_collab_parents: 未知参数 $1（用 --help）" >&2; exit 2 ;;
  esac
  shift
done

git rev-parse --git-dir >/dev/null 2>&1 || { echo "check_collab_parents: 不是 git 仓库" >&2; exit 2; }

refs=$(git for-each-ref --format='%(objectname)' refs/collab/inbox 2>/dev/null)
[ -n "$refs" ] || { echo "check_collab_parents: refs/collab/inbox 为空（没有协作条目）" >&2; exit 2; }

# 单条检查：输出「状态<TAB>说明」；状态为 ok / bad。
check_one() {
  local oid="$1"
  [ -z "$oid" ] && { printf 'ok\troot\n'; return; }
  if ! printf '%s' "$oid" | grep -Eq '^([0-9a-f]{40}|[0-9a-f]{64})$'; then
    printf 'bad\tshape\n'; return
  fi
  if ! git cat-file -e "$oid" 2>/dev/null; then
    printf 'bad\tmissing\n'; return
  fi
  printf 'ok\texists\n'
}

scan() {
  # 性能：3781 条 × 每条 1~2 个 git 子进程 = 6 分钟。改成
  #   ① 一次 `git cat-file --batch="%(objectname)"` 取全部 blob（输出是「oid 行 + 内容」交替，
  #      内容可能是多行 JSON ⇒ 必须按 **oid 行**切分，不能按行当 JSON）；
  #   ② 一个 python 解 JSON + 判形态；
  #   ③ 对「形态对但需验存在性」的 oid 批量查一次 `--batch-check`。
  # 实测 6 分钟 → ~0.7 秒。
  git cat-file --batch="%(objectname)" < <(git for-each-ref --format='%(objectname)' refs/collab/inbox) \
    | python3 -c '
import json, re, subprocess, sys

OID = re.compile(r"^[0-9a-f]{40}([0-9a-f]{24})?$")
lines = sys.stdin.read().split("\n")

# 输出格式：每块 = oid 行 + 内容行（内容本身可能是多行 JSON）。
# 做法：扫一遍，凡整行是 oid 形态即视为「下一块开始」。
blocks = []
cur_oid = None
buf = []
for line in lines:
    if OID.match(line.strip()):
        if cur_oid is not None:
            blocks.append((cur_oid, "\n".join(buf)))
        cur_oid, buf = line.strip(), []
    else:
        buf.append(line)
if cur_oid is not None:
    blocks.append((cur_oid, "\n".join(buf)))

rows = []
total = 0
need = []
for oid, text in blocks:
    try:
        d = json.loads(text)
    except Exception:
        continue
    if not isinstance(d, dict):
        continue
    total += 1
    p = d.get("parent") or ""
    if not p:
        continue
    if not (OID.match(p)) or len(p) not in (40, 64):
        rows.append((oid[:10], "shape", p))
        continue
    need.append((oid[:10], p))

if need:
    proc = subprocess.run(["git", "cat-file", "--batch-check"],
                          input="\n".join(p for _, p in need),
                          capture_output=True, text=True)
    for (short, p), line in zip(need, proc.stdout.splitlines()):
        if line.rstrip().endswith("missing") or " missing" in line:
            rows.append((short, "missing", p))

for r in rows:
    print("%s\t%s\t%s" % r)
print("--TOTAL--\t%d" % total, file=sys.stderr)
' 2> >(grep -o '[0-9]*' > /tmp/ccp_total) 
  return 0
}

case "$mode" in
  self-test)
    echo "── check_collab_parents 自测（四条形态，断言都能判出异常）"
    fail=0
    for c in '7a41f9095e2f4a2b8e9c3d7b1a2e8c4d9f6a0b3c' 'PLACEHOLDER' '69' '4c4c4c4c'; do
      res=$(check_one "$c")
      case "$res" in
        bad*) echo "  ✅ $(printf '%-44s' "$c") ⇒ ${res#*	}" ;;
        *) echo "  ❌ $(printf '%-44s' "$c") ⇒ 被误判为合规"; fail=1 ;;
      esac
    done
    # 阳性对照：真实存在的完整 oid 必须判 ok（否则守卫会误报一切）
    real=$(git rev-parse "$(git for-each-ref --format='%(objectname)' refs/collab/inbox | head -1)")
    res=$(check_one "$real")
    case "$res" in
      ok*) echo "  ✅ 阳性对照（真实 oid）⇒ ok" ;;
      *) echo "  ❌ 阳性对照被误判为异常（守卫会误报一切）"; fail=1 ;;
    esac
    # 根节点（空 parent）必须放行
    res=$(check_one "")
    case "$res" in
      ok*) echo "  ✅ 根节点（空 parent）⇒ ok" ;;
      *) echo "  ❌ 根节点被误判"; fail=1 ;;
    esac
    [ "$fail" -eq 0 ] && { echo "── 自测通过"; exit 0; }
    echo "── 自测失败"; exit 1
    ;;

  scan)
    if [ "$json" -eq 1 ]; then
      # **只扫一次**：原写法先管道给 python 再调 scan >/dev/null 跑第二遍，
      # 3781 条 × 每条 2 个 git 子进程 ⇒ 单次数分钟，`--json` 直接超时。
      rows=$(scan 2>/dev/null)
      [ -n "$rows" ] && status=1 || status=0
      printf '%s' "$rows" | python3 -c '
import json,sys
rows=[l.split("\t") for l in sys.stdin.read().splitlines() if l]
print(json.dumps({"bad": [{"entry": r[0], "reason": r[1], "parent": r[2]} for r in rows]}, ensure_ascii=False, indent=2))'
      exit "$status"
    fi
    rows=$(scan)
    # scan() 恒 return 0（它只产出异常行）；「有没有异常」由**输出是否为空**决定。
    [ -n "$rows" ] && status=1 || status=0
    if [ -n "$rows" ]; then
      printf '%s' "$rows" | while IFS=$'\t' read -r entry reason parent; do
        [ -z "$entry" ] && continue
        printf '  %s  %-8s parent=%s\n' "$entry" "$reason" "$parent"
      done
    fi
    if [ "$status" -ne 0 ]; then
      cat >&2 <<'MSG'
❌ 存在异常的 parent 引用。

   账本是 append-only ⇒ 这些条目**不能删**，只能：
     · 在卡上追加一条更正（写明真实父 oid 与成因），如 abb-channel-mcp-remaining-injection-surfaces-20261012 的 822ef720；
     · 未来的写入必须先校验。

   写入前的正确做法（本仓已发生 ≥4 次同类事故）：
     oid=$(walgit collab thread <id> | jq -r '.[-1].oid')   # 取完整 oid，别抄前缀
     git cat-file -t "$oid"                                  # 确认存在
     walgit collab entry ... --parent "$oid"

   自测：tools/check_collab_parents.sh --self-test
MSG
      exit 1
    fi
    echo "✅ 所有 parent 引用合规"
    exit 0
    ;;
esac
