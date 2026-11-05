#!/bin/sh
# 检查 Inno Setup 脚本（app-assets/ABB.iss）的块结构是否自洽。
#
# ## 为什么需要它（2026-11-05 真实事故）
#
# `ABB.iss` 在 macOS/Linux 上**无法编译**（ISCC 只有 Windows 有），所以它的语法错
# 在本地门禁里完全看不见。发版时才有机会发现——而那时已经在跑 release job 了。
#
# 事故：给 `[UninstallRun]` 加 `abb-channel` 时，`TASK: "abb-channel.exe";` 这一行
# 被插进了 `[Code]` 段的 Pascal 函数体（`procedure StopBridgeForInstall` 里）。
# 本地门禁 40 项全绿，Windows release job 在 ISCC 处红：
#
#     Error on line 161 in app-assets\ABB.iss: Column 3
#
# 那次发版的 Windows 安装包直接没产出（macOS 侧不受影响，所以更容易被漏看）。
#
# ## 判据（只查**能在文本层确定**的错，不假装是 ISCC）
#
# 1. `[Section]` 行必须顶格（`[Files]` 之类），出现在 Pascal 函数体内一定是错位。
# 2. `[Code]` 段里的 `procedure`/`function` 必须有配对的 `begin`...`end;`，
#    且期间不得出现只属于其它段的 `Key: value;` 行（`TASK:` / `Source:` /
#    `Filename:` / `Parameters:` / `Flags:`）。
# 3. 每个 `[Section]` 名字必须是 ISCC 认识的那个集合（拼错段名 ISCC 会忽略整段，
#    表现为「写了没生效」——静默失效，比红更糟）。
#
# 退出码：0 = 结构自洽；1 = 发现问题；2 = 脚本自身出错。

set -eu
cd "$(dirname "$0")/.." || exit 2

ISS="app-assets/ABB.iss"
[ -f "$ISS" ] || { echo "❌ 找不到 $ISS"; exit 2; }

fail=0
tmp=$(mktemp)
trap 'rm -f "$tmp"' EXIT

# ── 判据 1 + 3：段名白名单 + 段必须顶格 ─────────────────────────────────
# ISCC 认识的段（本文件用到的都在这；多写一个不存在的段名 ISCC 会整段忽略）。
known="Setup Languages Tasks InstallDelete Files Icons UninstallRun Run Code Registry Dirs Ini Types Components Messages CustomMessages UninstallDelete UninstallRun"
awk -v known="$known" '
    /^[[:space:]]*\[[A-Za-z]+\]/ {
        # 段头带前导空白 ⇒ 它缩在某个 Pascal 块里，几乎必然是插错位置
        if ($0 ~ /^[[:space:]]/) {
            printf "❌ %d: 段头有缩进（可能被插进了 Pascal 函数体）：%s\n", NR, $0
            bad = 1
        }
        name = $0
        gsub(/^[[:space:]]*\[|\][[:space:]]*$/, "", name)
        n = split(known, arr, " ")
        found = 0
        for (i = 1; i <= n; i++) if (arr[i] == name) found = 1
        if (!found) {
            printf "❌ %d: 段名 [%s] 不是 ISCC 认识的段 ⇒ 整段会被静默忽略\n", NR, name
            bad = 1
        }
    }
    END { exit bad ? 1 : 0 }
' "$ISS" || fail=$((fail + 1))

# ── 判据 2：[Code] 段内的 Pascal 结构 ────────────────────────────────────
# [Code] 从段头到下一个段头/文件尾。只在该区间内检查。
code_start=$(grep -n '^\[Code\]' "$ISS" | head -1 | cut -d: -f1 || true)
if [ -n "${code_start:-}" ]; then
    code_end=$(awk -v s="$code_start" 'NR > s && /^\[[A-Za-z]+\]/ { print NR; exit }' "$ISS")
    [ -n "${code_end:-}" ] || code_end=$(( $(wc -l < "$ISS") + 1 ))
    sed -n "${code_start},$((code_end - 1))p" "$ISS" > "$tmp"

    # 2a. 段专属的 `Key: value;` 行不该出现在 [Code] 里
    #     （只认段落专属键名，避免把 Pascal 自己的 `X := Y;` 误判）
    awk '
        /^[[:space:]]*(TASK|Source|DestDir|Filename|Parameters|Flags|Name|Description|Components|Tasks|Root):/ {
            printf "❌ [Code] 段里出现段专属键值行（属于 [Files]/[UninstallRun]/[Run]）：%s\n", $0
            bad = 1
        }
        END { exit bad ? 1 : 0 }
    ' "$tmp" || fail=$((fail + 1))

    # 2b. 每个 routine 的 begin/end 必须配对
    #     （只数 `begin`/`end`/`end;`，忽略行尾注释）
    awk '
        {
            line = $0
            sub(/\/\/.*/, "", line)
            if (line ~ /^[[:space:]]*(procedure|function)[[:space:]]/) routines++
            if (line ~ /^[[:space:]]*begin[[:space:]]*$/) begins++
            if (line ~ /^[[:space:]]*end;?[[:space:]]*$/) ends++
        }
        END {
            if (routines > 0 && begins != ends) {
                printf "❌ [Code] 段 begin/end 不配对：%d 个 begin vs %d 个 end（%d 个 routine）\n", begins, ends, routines
                exit 1
            }
            exit 0
        }
    ' "$tmp" || fail=$((fail + 1))
fi

if [ "$fail" -eq 0 ]; then
    echo "✅ ABB.iss 块结构自洽（段名合法 / 无缩进段头 / [Code] 无段专属行 / begin-end 配对）"
    exit 0
fi
echo
echo "修法：ABB.iss 在 macOS 上编译不了（ISCC 只有 Windows 有），所以插错位置只能靠这条"
echo "      文本级判据。段专属行（TASK:/Source:/Filename:…）只能待在它自己的段里。"
exit 1
