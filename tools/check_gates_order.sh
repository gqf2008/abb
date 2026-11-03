#!/bin/sh
# 检查 `tools/gates.sh` 的**顺序约束**：独立包的组必须在 root 组之前。
#
# ## 为什么这条要机器化
#
# 2026-11-03 一天里，**同一个顺序错误犯了两次**：
#
# | 次 | 现象 |
# |---|---|
# | 微信 | `export ABB_WECHAT_BIN` 在 root 测试之后 ⇒ 跨包 e2e 一直 skip |
# | 飞书 | 整组加在 root 之后 ⇒ 连带三个错：`feishu_target: unbound variable`、`iso_log: unbound variable`、多余 `fi` 语法错 |
#
# 两次都是「凭记忆把新组放到某处」，而不是「照着约束放」。
# 而 skip 的默认行为是**通过**——所以这类错误不红，只是悄悄少测。
#
# ## 约束
#
# 1. `crates/*` 的组（`== ABB 门禁：crates/…`）必须在 root 组之前：
#    root 的第 4 条 `check_test_isolation.sh` 会跑跨包 e2e，那些 e2e 要找
#    独立包的构建产物。
# 2. 每个 `export ABB_*_BIN` 必须出现在对应 crate 组**之后**、root 组**之前**：
#    组之后才有产物；root 之前才来得及传给测试。
# 3. 不允许出现「变量在使用之后才赋值」（`set -u` 下会直接报 unbound，
#    这是飞书那次的实际现象）。
#
# ## 用法
#
#     tools/check_gates_order.sh
#
# 退出码：0 = 顺序正确；1 = 违反；2 = 脚本自身无法判定（比如找不到 root 组）。

set -eu

cd "$(dirname "$0")/.." || exit 2

gates=tools/gates.sh
[ -f "$gates" ] || { echo "❌ 找不到 $gates" >&2; exit 2; }

fail=0

# ── 约束 1：crate 组必须在 root 组之前 ──────────────────────────
root_line=$(grep -n '^echo "== ABB 门禁：root 包' "$gates" | head -1 | cut -d: -f1)
if [ -z "$root_line" ]; then
    echo "❌ 找不到 root 组节头（gates.sh 被重写了？）" >&2
    exit 2
fi

# 每个 crate 组节头的行号
crate_lines=$(grep -n '^echo "== ABB 门禁：crates/' "$gates" | cut -d: -f1)

# **只有「产物要被 root 测试用到」的组才受约束**。
#
# 反例（第一版把这条写宽了、立刻误红）：`crates/abb-chart` 的组一直在 root 之后，
# 但 root 测试**不执行** chart 二进制（它是随包分发的独立 CLI，测试里不跑它）。
# 判据应当是「有没有跨包 e2e」，而不是「是不是 crate 组」。
#
# 判法：看 gates.sh 里有没有 `export ABB_*_BIN` 指向该组——
# 那种组必须前移；纯自检的组位置随意。
bins_exported=$(grep -oE 'export ABB_[A-Z]+_BIN=' "$gates" | sed 's/export //; s/=$//' || true)

for ln in $crate_lines; do
    [ "$ln" -lt "$root_line" ] && continue
    # 该组附近（往下 25 行内）有没有对应的 export
    window_end=$((ln + 25))
    if sed -n "${ln},${window_end}p" "$gates" | grep -qE '^\s*export ABB_[A-Z]+_BIN='; then
        fail=$((fail + 1))
        printf '❌ crate 组在第 %s 行（含 export …_BIN），但 root 组在第 %s 行 ⇒ 产物传不到测试\n' \
            "$ln" "$root_line"
        printf '   root 的第 4 条会跑跨包 e2e，那时产物还不存在 ⇒ e2e 会 skip（而 skip = 通过）\n'
        printf '   把该组（连同它的 export）移到 root 组之前。\n'
    fi
done

# ── 约束 2：export ABB_*_BIN 的位置 ─────────────────────────────
# 逐个检查：export 行必须在某个 crate 组之后、root 组之前。
for exp_ln in $(grep -n '^\s*export ABB_[A-Z]*_BIN=' "$gates" | cut -d: -f1); do
    if [ "$exp_ln" -gt "$root_line" ]; then
        fail=$((fail + 1))
        printf '❌ `export …_BIN` 在第 %s 行，晚于 root 组（第 %s 行）⇒ 传不到测试\n' \
            "$exp_ln" "$root_line"
    fi
    # 必须晚于至少一个 crate 组（否则产物还没建）
    before_ok=0
    for ln in $crate_lines; do
        if [ "$ln" -lt "$exp_ln" ]; then before_ok=1; fi
    done
    if [ "$before_ok" -eq 0 ]; then
        fail=$((fail + 1))
        printf '❌ `export …_BIN` 在第 %s 行，但它之前没有任何 crate 组 ⇒ 那时产物还不存在\n' "$exp_ln"
    fi
done

# ── 约束 3：变量不得在使用之前才赋值（`set -u` 会 unbound）──────
# 只查 gates.sh 里那些 `xxx_target=` 形式的局部变量：它们的首次**使用**
# 不能出现在首次**赋值**之前。
for var in $(grep -oE '^[a-z_]+_target=' "$gates" | tr -d '=' | sort -u); do
    assign_ln=$(grep -n "^$var=" "$gates" | head -1 | cut -d: -f1)
    [ -n "$assign_ln" ] || continue
    use_ln=$(grep -n "\$$var" "$gates" | head -1 | cut -d: -f1)
    [ -n "$use_ln" ] || continue
    if [ "$use_ln" -lt "$assign_ln" ]; then
        fail=$((fail + 1))
        printf '❌ `%s` 在第 %s 行就被使用，但第 %s 行才赋值 ⇒ `set -u` 下报 unbound\n' \
            "$var" "$use_ln" "$assign_ln"
    fi
done

if [ "$fail" -eq 0 ]; then
    crate_n=$(printf '%s\n' $crate_lines | grep -c . || true)
    echo "✅ gates.sh 顺序正确（${crate_n} 个 crate 组都在 root 之前；export 位置与变量赋值顺序都对）"
    exit 0
fi

echo
echo "修法：crate 组 → 各自 export → root 组。这个顺序今天错了两次（2026-11-03）。"
exit 1
