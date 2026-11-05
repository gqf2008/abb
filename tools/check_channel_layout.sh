#!/usr/bin/env bash
# 校验渠道二进制"只有一个"。
#
# 为什么需要：2026-11-04 把 `abb-channel-{feishu,dingtalk,wechat}` 三个独立 crate
# 合并成了 `crates/abb-channel` 一个 crate / 一个二进制，动机是体积——
# 三份二进制各自链一次 `rustls + ring + webpki-roots`，那约 4 MB 的 HTTPS 地板
# 被付了三遍（实测 15.3 MB → 合并后 6.9 MB）。
#
# **这个收益唯一的敌人是回退**：哪天有人觉得"飞书要单独升级依赖"，把某个渠道
# 拆回独立 crate，地板又变成两份——而这件事在功能、测试、门禁里**都不会报错**，
# 只在发版体积上悄悄变差。人不会主动去看体积，所以让脚本看。
#
#   bash tools/check_channel_layout.sh          # 校验
#   bash tools/check_channel_layout.sh --print  # 打印实测体积（release 产物存在时）
set -uo pipefail

cd "$(dirname "$0")/.." || exit 2

fail=0
err() {
    fail=$((fail + 1))
    printf '❌ %s\n' "$1"
}

# ① 只有一个渠道 crate 目录。
#    允许 `abb-channel`，其它 `abb-channel-*` 都是拆分回退的痕迹。
# 不用 mapfile：macOS 自带 bash 3.2 没有它（第一版就因此在 mac 上崩成
# "mapfile: command not found"，后面还跟着 unbound variable）。
n_dirs=$(find crates -maxdepth 1 -type d -name 'abb-channel*' | wc -l | tr -d ' ')
dirs=$(find crates -maxdepth 1 -type d -name 'abb-channel*' | sort | tr '\n' ' ')
if [ "$n_dirs" -ne 1 ]; then
    err "crates/ 下应当只有一个渠道 crate（abb-channel），实测 $n_dirs 个：$dirs"
    printf '   拆回多个 crate 会把 ~4 MB 的 TLS 地板再付一遍（见文件头）。\n'
fi
if [ ! -d crates/abb-channel ]; then
    err "找不到 crates/abb-channel"
elif [ "$n_dirs" -eq 1 ] && [ "$dirs" != "crates/abb-channel " ]; then
    err "渠道 crate 目录应当是 crates/abb-channel，实测 $dirs"
fi

# ② 只有一个 [[bin]]，且名叫 abb-channel。
manifest=crates/abb-channel/Cargo.toml
if [ ! -f "$manifest" ]; then
    err "找不到 $manifest"
else
    bins=$(grep -c '^\[\[bin\]\]' "$manifest" || true)
    if [ "$bins" -ne 1 ]; then
        err "$manifest 应当只有一个 [[bin]]，实测 $bins"
        printf '   多个 [[bin]] = 多份二进制 = 多份 TLS 地板。渠道靠 --transport 分派。\n'
    fi
    if ! grep -q '^name = "abb-channel"$' "$manifest"; then
        err "$manifest 的二进制名应当是 abb-channel"
    fi
fi

# ③ `--transport` 的三个渠道值仍在（分派不能被删成单渠道）。
main=crates/abb-channel/src/main.rs
if [ ! -f "$main" ]; then
    err "找不到 $main"
else
    # **判据必须是「分派臂存在」，不是「字面量存在」**。
    # 第一版只 grep `"dingtalk"`，而这个名字在 `TRANSPORTS` 数组里也有一份 ⇒
    # 删掉 `"dingtalk" => dingtalk::run(),` 这条臂，守卫照样绿（变异实测逃逸）。
    # 现在要求 `"<名字>" => ` 这种 match 臂形状，两处都得在。
    for t in feishu dingtalk wechat; do
        if ! grep -qE "^\s*\"$t\" => " "$main"; then
            err "$main 的 dispatch 里没有 \"$t\" => 分派臂（--transport 会放行但分派不到）"
        fi
    done
fi

# ④ 文档里的 `command` 必须指向合并后的 abb-channel，不能是旧名字。
#    旧名字写进文档 ⇒ 用户照着配，进程起不来（缺 --transport 还会退 2）。
for doc in docs/mcp-bus-architecture.md crates/abb-channel/README.md; do
    [ -f "$doc" ] || continue
    # 历史对照（「旧：abb-channel-feishu + …」这类行）不算违规——README 的
    # 体积表必须能写出旧名字，否则没法说明合并省了多少。只拦**当成现役写的**。
    stale=$(grep -nE 'abb-channel-(feishu|dingtalk|wechat)' "$doc" \
            | grep -vE '旧|合并前|曾经|过去|历史' || true)
    if [ -n "$stale" ]; then
        err "$doc 把旧的二进制名当成现役在写——现在只有一个 abb-channel："
        printf '%s\n' "$stale" | sed 's/^/     /'
    fi
done

# ⑤ 门禁必须真的跑这个 crate（否则「有测试但没人跑」，见 false-green 第 4 形态）。
if ! grep -q 'crates/abb-channel/Cargo.toml' tools/gates.sh; then
    err "tools/gates.sh 没有跑 crates/abb-channel ⇒ 这个 crate 的门禁覆盖是缺失的"
fi

if [ "${1:-}" = "--print" ]; then
    echo
    echo "== 体积（release 产物，未 strip）=="
    found=0
    for p in /Volumes/DataExt/tmp/abb-target-*channel*/release/abb-channel; do
        [ -x "$p" ] || continue
        found=1
        sz=$(stat -f%z "$p" 2>/dev/null || stat -c%s "$p" 2>/dev/null)
        awk -v s="$sz" -v p="$p" 'BEGIN{printf "  %.2f MB  %s\n", s/1048576, p}'
    done
    [ "$found" -eq 1 ] || echo "  （没有 release 产物；先跑 cargo build --release --manifest-path crates/abb-channel/Cargo.toml）"
    echo "  对照：合并前三个二进制合计 15.3 MB（未 strip）/ 12.6 MB（strip）"
fi

if [ "$fail" -eq 0 ]; then
    echo "✅ 渠道二进制仍是单个 abb-channel（--transport 分派三个渠道）"
    exit 0
fi
echo
echo "理由与实测数据见 crates/abb-channel/README.md「为什么是一个二进制」。"
exit 1
