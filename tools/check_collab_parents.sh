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
#   tools/check_collab_parents.sh --self-test  # 自测：注入坏 parent，断言本守卫变红
#   tools/check_collab_parents.sh --json       # 机器可读输出
#
# 退出码：0=全部合规；1=存在异常 parent；2=环境错误（不是 git 仓 / 无 refs /
#         扫描器不可信）。**任何「扫描不到」都必须落 2，不允许被当成 0。**
#
# 判据（每条 parent 都必须同时满足 A 和 B）：
#   A. 形态：空字符串（根节点）或完整 oid（40 位 sha1 / 64 位 sha256 的小写 hex）。
#      **不接受缩写**——理由见上：账本的父子关系不是 git 的 rev-parse 语义。
#      非字符串 parent（69 / true / [] / {} / null）不是根节点，记为一条 type 异常。
#   B. 存在：非空时必须 `git cat-file` 能取到（不能指向别仓/拼错的 oid）。
#
# fail-closed 约定（评审阻塞项，2026-10-27）：
#   · 解析器（python3）的 stderr 不重定向，退出码必须检查；非 0 ⇒ 用 2 退出。
#   · python 末尾打印完成哨兵 `--TOTAL--<TAB>条目数`，shell 只在「退出码 0 **且**
#     哨兵存在」时才认为输出可信；缺哨兵（例如 python3 被替换成静默空输出）⇒ 2。
#   · 解析失败 / 非字符串 parent 记为一条 parse / type 异常并继续，不抛异常、不静默跳过。
#   · 异常行边查边出（先落临时文件），中途失败不会连带吞掉已检出的行。
#   · 扫描条目数必须与 `git for-each-ref refs/collab/inbox` 的引用数相等，不等 ⇒ 2。
set -uo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
script_path="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/$(basename "${BASH_SOURCE[0]}")"
cd "$repo_root" || exit 2

mode="scan"
json=0
while [ $# -gt 0 ]; do
  case "$1" in
    --self-test) mode="self-test" ;;
    --json) json=1 ;;
    -h|--help)
      # 打印文件头的注释块（第一条非注释行之前），不依赖固定行号
      awk 'NR == 1 { next } /^#/ { sub(/^# ?/, ""); print; next } { exit }' "$0"
      exit 0
      ;;
    *) echo "check_collab_parents: 未知参数 $1（用 --help）" >&2; exit 2 ;;
  esac
  shift
done

git rev-parse --git-dir >/dev/null 2>&1 || { echo "check_collab_parents: 不是 git 仓库" >&2; exit 2; }

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

# 全量扫描。用法：scan <结果文件>
#   stdout 不直接对外：异常行（短 oid<TAB>reason<TAB>parent）与末行完成哨兵
#   `--TOTAL--<TAB>条目数` 一起写进结果文件；扫描条目数写入全局 scan_total。
#   返回 0 = 输出可信；2 = 扫描器不可信（调用方必须据此退出 2）。
scan() {
  # 性能：3781 条 × 每条 1~2 个 git 子进程 = 6 分钟。改成
  #   ① 一次 `git cat-file --batch` 取全部 blob（`<oid> <type> <size>` 头 + 精确
  #      size 字节的内容，按 size 切块，不靠「整行像 oid」猜边界）；
  #   ② 一个 python 解 JSON + 判形态；
  #   ③ 对「形态对但需验存在性」的 oid 批量查一次 `--batch-check`。
  # 实测 6 分钟 → ~0.7 秒。
  local out_file="$1"
  local raw rc tag total_line
  scan_total=0
  # 单引号里是 Python 源码，按下标 shell 展开无意义（SC2016 是误报）
  # shellcheck disable=SC2016
  raw=$(git cat-file --batch < <(git for-each-ref --format='%(objectname)' refs/collab/inbox) \
    | python3 -c '
import json, re, subprocess, sys

OID = re.compile(r"^[0-9a-f]{40}([0-9a-f]{24})?$")
MISSING = object()

def clean(v):
    return v.replace("\t", "\\t").replace("\r", "\\r").replace("\n", "\\n")

def emit(short, reason, parent):
    # 边查边出：即便后续步骤抛错，已检出的行也已经写到 stdout，不会被一起吞掉。
    print("%s\t%s\t%s" % (short, reason, parent), flush=True)

data = sys.stdin.buffer.read()
pos = 0
blocks = []
while pos < len(data):
    nl = data.find(b"\n", pos)
    if nl < 0:
        break
    parts = data[pos:nl].decode("utf-8", "replace").split()
    pos = nl + 1
    if len(parts) == 3 and parts[1] in ("blob", "commit", "tag", "tree") and parts[2].isdigit():
        size = int(parts[2])
        blocks.append((parts[0], data[pos:pos + size]))
        pos += size + 1
    else:
        # "<oid> missing" 之类：对象取不到，无法审计 => 记一条 parse，不静默跳过
        blocks.append((parts[0] if parts else "?", None))

total = 0
need = []
for oid, body in blocks:
    short = oid[:10]
    total += 1
    if body is None:
        emit(short, "parse", "<unreadable>")
        continue
    try:
        d = json.loads(body.decode("utf-8"))
    except Exception:
        emit(short, "parse", "<invalid-json>")
        continue
    if not isinstance(d, dict):
        emit(short, "parse", "<not-object>")
        continue
    p = d.get("parent", MISSING)
    if p is MISSING:
        emit(short, "type", "<missing>")
        continue
    if not isinstance(p, str):
        # 69 / true / [] / {} / null / 0：旧写法 `d.get("parent") or ""` 会把 0/null
        # 当根节点放行，其余类型则让 OID.match 抛 TypeError 整盘丢失。这里逐条记异常。
        emit(short, "type", clean(json.dumps(p, ensure_ascii=False)))
        continue
    if p == "":
        continue
    if not OID.fullmatch(p):
        emit(short, "shape", clean(p))
        continue
    need.append((short, p))

if need:
    proc = subprocess.run(["git", "cat-file", "--batch-check"],
                          input="\n".join(p for _, p in need),
                          capture_output=True, text=True)
    if proc.returncode != 0:
        print("check_collab_parents: git cat-file --batch-check 失败：%s"
              % proc.stderr.strip(), file=sys.stderr)
        sys.exit(3)
    lines = proc.stdout.splitlines()
    if len(lines) != len(need):
        print("check_collab_parents: 存在性回读行数 %d != 待查 %d（git 输出不可信）"
              % (len(lines), len(need)), file=sys.stderr)
        sys.exit(3)
    for (short, p), line in zip(need, lines):
        if line.rstrip().endswith("missing") or " missing" in line:
            emit(short, "missing", clean(p))

# 完成哨兵：shell 只在看到它（且退出码为 0）时才相信上面的输出。
print("--TOTAL--\t%d" % total, flush=True)
')
  rc=$?
  # 已检出的行先落盘：扫描器中途失败时这些行仍在文件里，不会被一起吞掉。
  printf '%s\n' "$raw" > "$out_file"
  if [ "$rc" -ne 0 ]; then
    echo "check_collab_parents: 扫描器失败（python3 退出码 ${rc}；缺失/启动失败/解析异常都按环境错误处理）" >&2
    return 2
  fi
  total_line=$(grep -E '^--TOTAL--[[:space:]]' "$out_file" | tail -n 1)
  if [ -z "$total_line" ]; then
    echo "check_collab_parents: 扫描器没有输出完成哨兵（输出不可信，按环境错误处理）" >&2
    return 2
  fi
  read -r tag scan_total <<<"$total_line"
  if [ "$tag" != "--TOTAL--" ] || ! printf '%s' "$scan_total" | grep -Eq '^[0-9]+$'; then
    echo "check_collab_parents: 完成哨兵格式异常：$total_line" >&2
    return 2
  fi
  return 0
}

# 用隔离仓库跑 scan()/输出全链路：不碰本仓 refs。
scan_self_test() {
  local tmp bad=0
  tmp=$(mktemp -d "${TMPDIR:-/tmp}/ccp-selftest.XXXXXX") || { echo "  ❌ mktemp 失败"; return 1; }
  mkdir -p "$tmp/tools"
  cp "$script_path" "$tmp/tools/check_collab_parents.sh"
  (
    cd "$tmp" || exit 1
    git init -q .
    root=$(printf '{"id":"t","kind":"issue","parent":""}' | git hash-object -w --stdin)
    ok=$(printf '{"id":"t","kind":"comment","parent":"%s"}' "$root" | git hash-object -w --stdin)
    shape=$(printf '{"id":"t","kind":"comment","parent":"PLACEHOLDER"}' | git hash-object -w --stdin)
    missing=$(printf '{"id":"t","kind":"comment","parent":"7a41f9095e2f4a2b8e9c3d7b1a2e8c4d9f6a0b3c"}' | git hash-object -w --stdin)
    intp=$(printf '{"id":"t","kind":"comment","parent":69}' | git hash-object -w --stdin)
    zerop=$(printf '{"id":"t","kind":"comment","parent":0}' | git hash-object -w --stdin)
    nullp=$(printf '{"id":"t","kind":"comment","parent":null}' | git hash-object -w --stdin)
    boolp=$(printf '{"id":"t","kind":"comment","parent":true}' | git hash-object -w --stdin)
    listp=$(printf '{"id":"t","kind":"comment","parent":[]}' | git hash-object -w --stdin)
    broken=$(printf '{"id":"t", "parent": ' | git hash-object -w --stdin)
    git update-ref refs/collab/inbox/0001-root "$root"
    git update-ref refs/collab/inbox/0002-ok "$ok"
    git update-ref refs/collab/inbox/0003-shape "$shape"
    git update-ref refs/collab/inbox/0004-missing "$missing"
    git update-ref refs/collab/inbox/0005-int "$intp"
    git update-ref refs/collab/inbox/0006-zero "$zerop"
    git update-ref refs/collab/inbox/0007-null "$nullp"
    git update-ref refs/collab/inbox/0008-bool "$boolp"
    git update-ref refs/collab/inbox/0009-list "$listp"
    git update-ref refs/collab/inbox/0010-broken "$broken"
  ) >/dev/null 2>&1
  local text json tstatus jstatus tcount jcount
  text=$(bash "$tmp/tools/check_collab_parents.sh" 2>/dev/null); tstatus=$?
  json=$(bash "$tmp/tools/check_collab_parents.sh" --json 2>/dev/null); jstatus=$?
  tcount=$(printf '%s\n' "$text" | grep -cE '^  [0-9a-f]{10}' || true)
  jcount=$(printf '%s\n' "$json" | grep -c '"entry"' || true)
  # 期望：shape 1 + missing 1 + type 5（69/0/null/true/[]）+ parse 1 = 8
  local cshape ctype cmissing cparse
  cshape=$(printf '%s\n' "$text" | grep -c ' shape ' || true)
  ctype=$(printf '%s\n' "$text" | grep -c ' type ' || true)
  cmissing=$(printf '%s\n' "$text" | grep -c ' missing ' || true)
  cparse=$(printf '%s\n' "$text" | grep -c ' parse ' || true)
  if [ "$tstatus" -eq 1 ] && [ "$jstatus" -eq 1 ] && [ "$tcount" -eq 8 ] && [ "$jcount" -eq 8 ] \
     && [ "$cshape" -eq 1 ] && [ "$ctype" -eq 5 ] && [ "$cmissing" -eq 1 ] && [ "$cparse" -eq 1 ]; then
    echo "  ✅ 扫描路径（形态/存在性/类型/解析四类异常）⇒ exit 1，文本/JSON 均 8 条"
  else
    echo "  ❌ 扫描路径：text exit=$tstatus/$tcount 条（shape=$cshape type=$ctype missing=$cmissing parse=${cparse}），json exit=$jstatus/$jcount 条（期望各 8）"
    bad=1
  fi
  # 阳性对照：全合规仓库必须 exit 0（防「恒红」）
  ( cd "$tmp" && for n in 0003-shape 0004-missing 0005-int 0006-zero 0007-null 0008-bool 0009-list 0010-broken; do
      git update-ref -d "refs/collab/inbox/$n"
    done ) >/dev/null 2>&1
  local okstatus
  bash "$tmp/tools/check_collab_parents.sh" >/dev/null 2>&1; okstatus=$?
  if [ "$okstatus" -eq 0 ]; then
    echo "  ✅ 扫描路径（全合规仓库）⇒ exit 0"
  else
    echo "  ❌ 扫描路径（全合规仓库）被误判为异常（exit=${okstatus}）"
    bad=1
  fi
  # ── 阴性对照：解析器不可用时必须 fail-closed（旧实现全绿）──────────────────
  local fake="$tmp/fakebin" nout nst
  mkdir -p "$fake"
  # N1：python3 启动即失败（等价于解析器坏了）
  printf '#!/bin/sh\nexit 1\n' > "$fake/python3"
  chmod +x "$fake/python3"
  nout=$(PATH="$fake:$PATH" bash "$tmp/tools/check_collab_parents.sh" 2>/dev/null); nst=$?
  if [ "$nst" -eq 2 ] && ! printf '%s' "$nout" | grep -q '✅'; then
    echo "  ✅ 阴性对照（python3 启动失败）⇒ exit 2，未误报合规"
  else
    echo "  ❌ 阴性对照（python3 启动失败）：exit=${nst}（期望 2），或误报合规"
    bad=1
  fi
  # N2：python3 静默成功但什么都不输出（经典 fail-open：空输出 ≠ 全合规）
  printf '#!/bin/sh\ncat >/dev/null\nexit 0\n' > "$fake/python3"
  chmod +x "$fake/python3"
  nout=$(PATH="$fake:$PATH" bash "$tmp/tools/check_collab_parents.sh" 2>/dev/null); nst=$?
  if [ "$nst" -eq 2 ] && ! printf '%s' "$nout" | grep -q '✅'; then
    echo "  ✅ 阴性对照（python3 静默空输出）⇒ exit 2，未误报合规"
  else
    echo "  ❌ 阴性对照（python3 静默空输出）：exit=${nst}（期望 2），或误报合规"
    bad=1
  fi
  # N3：哨兵存在但总数与 ref 数不符（对账必须变红；此处仓库有 2 条 ref）
  cat > "$fake/python3" <<'FAKE'
#!/bin/sh
cat >/dev/null
printf -- '--TOTAL--\t1\n'
exit 0
FAKE
  chmod +x "$fake/python3"
  nout=$(PATH="$fake:$PATH" bash "$tmp/tools/check_collab_parents.sh" 2>/dev/null); nst=$?
  if [ "$nst" -eq 2 ] && ! printf '%s' "$nout" | grep -q '✅'; then
    echo "  ✅ 阴性对照（扫描数 1 ≠ ref 数 2）⇒ exit 2，未误报合规"
  else
    echo "  ❌ 阴性对照（扫描数与 ref 数不符）：exit=${nst}（期望 2），或误报合规"
    bad=1
  fi
  rm -rf "$tmp"
  return "$bad"
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
    real=$(git for-each-ref --format='%(objectname)' refs/collab/inbox | head -1)
    [ -n "$real" ] || real=$(git rev-parse HEAD 2>/dev/null)
    if [ -z "$real" ]; then
      echo "  ❌ 阳性对照：仓库里没有任何对象可用（无协作条目、无 HEAD）"
      fail=1
    else
      res=$(check_one "$real")
      case "$res" in
        ok*) echo "  ✅ 阳性对照（真实 oid）⇒ ok" ;;
        *) echo "  ❌ 阳性对照被误判为异常（守卫会误报一切）"; fail=1 ;;
      esac
    fi
    # 根节点（空 parent）必须放行
    res=$(check_one "")
    case "$res" in
      ok*) echo "  ✅ 根节点（空 parent）⇒ ok" ;;
      *) echo "  ❌ 根节点被误判"; fail=1 ;;
    esac
    # 扫描路径自测：上面的 check_one 只验单条判据，验不到 scan()/输出这一层。
    # 曾因此漏掉「文本模式吞末行」（28 vs 29 两条路给出不同答案）。
    scan_self_test || fail=1
    [ "$fail" -eq 0 ] && { echo "── 自测通过"; exit 0; }
    echo "── 自测失败"; exit 1
    ;;

  scan)
    refs=$(git for-each-ref --format='%(objectname)' refs/collab/inbox 2>/dev/null)
    [ -n "$refs" ] || { echo "check_collab_parents: refs/collab/inbox 为空（没有协作条目）" >&2; exit 2; }
    ref_count=$(git for-each-ref --format='%(refname)' refs/collab/inbox | wc -l | tr -d ' ')
    rows_file=$(mktemp "${TMPDIR:-/tmp}/ccp-rows.XXXXXX") || {
      echo "check_collab_parents: mktemp 失败" >&2; exit 2;
    }
    trap 'rm -f "$rows_file"' EXIT
    if ! scan "$rows_file"; then
      # 扫描器不可信 ⇒ 2；但中断前已检出的行仍然展示出来，不让它们消失。
      if grep -qE '^[0-9a-f]{10}\t' "$rows_file" 2>/dev/null; then
        echo "check_collab_parents: 扫描中断前已检出的异常行（不完整）：" >&2
        grep -vE '^--TOTAL--[[:space:]]' "$rows_file" >&2 || true
      fi
      exit 2
    fi
    # 对账：扫到的条目数必须等于 ref 数，否则说明有 ref 没被读到（截断/漏读）⇒ 不可信
    if [ "$scan_total" -ne "$ref_count" ]; then
      echo "check_collab_parents: 扫描条目数 $scan_total ≠ refs/collab/inbox 引用数 ${ref_count}（扫描不完整，按环境错误处理）" >&2
      exit 2
    fi
    rows=$(grep -vE '^--TOTAL--[[:space:]]' "$rows_file" || true)
    if [ "$json" -eq 1 ]; then
      local_json=$(printf '%s' "$rows" | python3 -c '
import json,sys
rows=[l.split("\t") for l in sys.stdin.read().splitlines() if l]
print(json.dumps({"bad": [{"entry": r[0], "reason": r[1], "parent": r[2]} for r in rows]}, ensure_ascii=False, indent=2))') || {
        echo "check_collab_parents: JSON 输出失败（python3 不可用，按环境错误处理）" >&2
        exit 2
      }
      printf '%s\n' "$local_json"
      [ -n "$rows" ] && exit 1
      exit 0
    fi
    # scan() 已保证输出可信；「有没有异常」由**输出是否为空**决定。
    if [ -n "$rows" ]; then
      printf '%s\n' "$rows" | while IFS=$'\t' read -r entry reason parent; do
        [ -z "$entry" ] && continue
        printf '  %s  %-8s parent=%s\n' "$entry" "$reason" "$parent"
      done
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
