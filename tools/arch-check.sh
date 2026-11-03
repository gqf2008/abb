#!/usr/bin/env bash
# 校验 docs/architecture.md 里的数字与代码一致。不一致就红并指名哪一处过期。
#
# 为什么需要：架构图最容易烂成"看着对、其实早不是这样"。图里每个数字都是承重判据
# （例如「重心在 GUI」是靠 ui.rs/syschat.rs 的行数支撑的），数字漂了，结论就可能是错的。
# 人不会主动去核数字，所以让脚本核。
#
#   bash tools/arch-check.sh          # 只校验，红即说明文档过期
#   bash tools/arch-check.sh --print  # 打印当前实测值（用于更新文档）
set -uo pipefail

cd "$(dirname "$0")/.." || exit 2
doc=docs/architecture.md
print_only=0
[ "${1:-}" = "--print" ] && print_only=1

fail=0
# 断言某个数字在文档里出现过。$2=标签 $3=实测值
want() {
    local label="$1" val="$2"
    if ! grep -q "\b${val}\b" "$doc"; then
        fail=$((fail + 1))
        printf '❌ %s：实测 %s，但文档里没有这个数字 ⇒ 文档过期\n' "$label" "$val"
        return 1
    fi
    # **唯一性也得断言**（评审 blocking-2 的真实根因，我为此踩了三次）:
    # 数字重复时，改对一处、另一处依旧匹配 ⇒ `grep -q` 照样通过，校验形同虚设。
    # 实测过的两道题：文档把 runtime 写成 13600，我改成 13727 只改了 §6 表里的那处，
    # §6 结论行里还留着 13727 的另一份…… 结果「把错数字改对」这件事**永远验不出来**。
    # 依赖人「记得改两处」是不可靠的；让重复本身报错才可靠。
    local n
    n=$(grep -o "\b${val}\b" "$doc" | wc -l | tr -d ' ')
    if [ "$n" -gt 1 ]; then
        fail=$((fail + 1))
        printf '❌ %s：数字 %s 在文档里出现 %s 次 ⇒ 不是唯一事实源（改一处另一处仍匹配，校验会漏抓）\n' \
            "$label" "$val" "$n"
    fi
    return 0
}

lines_of() { wc -l < "$1" | tr -d ' '; }
total_rs() { find src -name '*.rs' -print0 | xargs -0 cat 2>/dev/null | wc -l | tr -d ' '; }

# ── 实测 ────────────────────────────────────────────────────────────────────
TOTAL_RS=$(total_rs)
SLINT=$(lines_of ui/app.slint)
UI_RS=$(lines_of src/ui.rs)
SYSCHAT=$(lines_of src/syschat.rs)
RENDER=$(lines_of src/ui_headless_render_test.rs)
TASK_RUN=$(lines_of src/task_run.rs)
PLATFORM=$(lines_of src/platform.rs)
MAIN_RS=$(lines_of src/main.rs)
SERVICE=$(lines_of src/service.rs)
GUARD=$(lines_of src/guard.rs)
DELIVER=$(lines_of src/deliver.rs)
ACP=$(lines_of src/runtime/acp.rs)
HARNESS=$(lines_of src/runtime/harness.rs)
POOL=$(lines_of src/runtime/pool.rs)
QUEUE=$(lines_of src/runtime/queue.rs)
ONESHOT=$(lines_of src/runtime/oneshot.rs)
# 整个 runtime 目录（不只 5 个大文件）。初版只手累加 5 个 ⇒ 漏掉 keys/log_out/mod/
# prompt_framing 共 227 行，而那个数还被当成「整个 runtime」写进文档（评审 blocking-2）。
# 用 find 真求和，别再手列文件名——手列就一定会漏，而漏了不报错。
RUNTIME_ALL=$(find src/runtime -name '*.rs' -print0 | xargs -0 cat 2>/dev/null | wc -l | tr -d ' ')
MCP_BUS=$(lines_of src/mcp_bus.rs)
MESSENGER=$(lines_of src/messenger.rs)
FEISHU=$(lines_of src/feishu.rs)
WECHAT=$(lines_of src/wechat.rs)
DINGTALK=$(lines_of src/dingtalk.rs)
RUNTIME_TOTAL=$((ACP + HARNESS + POOL + QUEUE + ONESHOT))
GUI_TOTAL=$((UI_RS + SYSCHAT + RENDER))
if [ "$print_only" -eq 1 ]; then
    # ⚠️ heredoc 里变量后面紧跟全角标点时中间必须留空格：bash 会把全角字节并进变量名，
    # `set -u` 下直接 unbound 中止（实测：初版把变量名紧贴全角括号，让 --print **输出 0 行且 rc=0**）。
    # 这跟 2026-11-02 下午修过的 gates.sh 同类 bug 是一个形态——同一个坑踩了两次，
    # 所以这里顺手记下来：写完带全角标点的 heredoc 必须实际跑一次看输出。
    cat <<EOF
src/ 合计            $TOTAL_RS
ui/app.slint         $SLINT
ui.rs                $UI_RS
syschat.rs           $SYSCHAT
ui_headless_render   $RENDER
GUI 三件合计         $GUI_TOTAL
task_run.rs          $TASK_RUN
platform.rs          $PLATFORM
main.rs              $MAIN_RS
service.rs           $SERVICE
guard.rs             $GUARD
deliver.rs           $DELIVER
runtime 合计         $RUNTIME_ALL  （全目录：find src/runtime -name *.rs）
  acp $ACP / harness $HARNESS / pool $POOL / queue $QUEUE / oneshot $ONESHOT
  （上面 5 个之外的其余文件合计 $((RUNTIME_ALL - RUNTIME_TOTAL)) 行）
mcp_bus.rs           $MCP_BUS
messenger.rs         $MESSENGER
feishu/wechat/ding   $FEISHU / $WECHAT / $DINGTALK
EOF
    exit 0
fi

# ── 校验：文档里出现过的规模数字必须与实测一致 ──────────────────────────────
want "src/ 总行数"            "$TOTAL_RS"
want "ui/app.slint 行数"      "$SLINT"
want "ui.rs 行数"             "$UI_RS"
want "syschat.rs 行数"        "$SYSCHAT"
want "渲染台行数"             "$RENDER"
want "task_run.rs 行数"       "$TASK_RUN"
want "platform.rs 行数"       "$PLATFORM"
want "main.rs 行数"           "$MAIN_RS"
want "service.rs 行数"        "$SERVICE"
want "guard.rs 行数"          "$GUARD"
want "deliver.rs 行数"        "$DELIVER"
want "runtime/acp.rs 行数"    "$ACP"
want "mcp_bus.rs 行数"        "$MCP_BUS"
want "feishu.rs 行数"         "$FEISHU"
want "wechat.rs 行数"         "$WECHAT"
want "dingtalk.rs 行数"       "$DINGTALK"
# 两个**汇总**也得守（评审 blocking-2）：初版它们从未被断言，于是文档写「整个 runtime
# 13600 行」而实测是 13727 也能全绿——一个错数字横在「重心在 GUI」这个承重结论旁边
# 而无人发觉。汇总值比单项值更容易错（手列文件必漏），所以更需要断言。
want "runtime 全目录行数"     "$RUNTIME_ALL"
want "GUI 三件合计行数"       "$GUI_TOTAL"

# ── 校验：几条结构性断言（数字对了但结构变了更危险）──────────────────────────
# Messenger 生产实现数 = 1（内置渠道 2026-11-04 退役后只剩 MCP 一个）。
#
# 为什么改成断言「恰好 1」而不是「至少 1」：多出来的实现只可能是有人把内置渠道
# 加了回来——那会让文档 §2「MCP 渠道是唯一入站形态」变成假话，而代码照样编译。
n_msgr=$(grep -c 'impl Messenger for' src/mcp_channel.rs)
if [ "$n_msgr" -ne 1 ]; then
    fail=$((fail + 1))
    printf '❌ Messenger 生产实现数：实测 %s，文档写 1（内置渠道退役后只应有 MCP）\n' "$n_msgr"
fi
# 内置渠道的源文件**必须不存在**（退役不可逆的机器断言）。
#
# 反向断言比「MCP 入口存在」更有价值：缺 MCP 入口编译就红了，
# 而内置渠道只要有一个文件被误恢复，编译照样通过、行为悄悄分叉。
for f in 'src/feishu.rs' 'src/wechat.rs' 'src/dingtalk.rs' 'src/ws.rs' 'src/proto.rs'; do
    if [ -e "$f" ]; then
        fail=$((fail + 1)); printf '❌ %s 又出现了（内置渠道已于 2026-11-04 退役）\n' "$f"
    fi
done
# MCP 入站主循环仍在（唯一入站形态）
if ! grep -q 'pub async fn run' src/mcp_channel_loop.rs; then
    fail=$((fail + 1)); printf '❌ MCP 入站主循环 mcp_channel_loop::run 不存在了\n'
fi
# 两条 task 执行通道仍在
for ch in 'Schedule' 'Manual'; do
    if ! grep -q "    ${ch}," src/task_run.rs; then
        fail=$((fail + 1)); printf '❌ task 执行通道 %s 不存在了（文档 §2 描述过期）\n' "$ch"
    fi
done
# 四档沙箱仍在
for m in Auto ReadOnly WorkspaceWrite FullAccess; do
    if ! grep -q "    ${m}," src/config.rs; then
        fail=$((fail + 1)); printf '❌ 沙箱档位 %s 不存在了（文档 §4 描述过期）\n' "$m"
    fi
done

# ── 自测：--print 必须真的输出（评审 blocking-1）──────────────────────────────
# 它曾因 heredoc 里变量名紧贴全角括号而输出 0 行且 rc=0。而它正是脚本报错时指路的
# **唯一修法**——坏了等于「告诉你去用 A」，而 A 是死的。
printed=$(bash "$0" --print 2>&1)
n_printed=$(printf '%s\n' "$printed" | grep -c . || true)
if [ "$n_printed" -lt 10 ]; then
    fail=$((fail + 1))
    printf '❌ --print 只输出 %s 行（应 >10）⇒ 报错时指路的修法本身不可用\n' "$n_printed"
fi
for key in 'src/ 合计' 'runtime 合计' 'GUI 三件合计'; do
    if ! printf '%s' "$printed" | grep -q "$key"; then
        fail=$((fail + 1)); printf '❌ --print 缺关键行「%s」\n' "$key"
    fi
done

# ── 退役方案文档里的行数也必须对得上 ────────────────────────────────
#
# 为什么单独校验它（评审 fa7c2cb9 的 non-blocking）：那份文档的核心就是
# 「S4 要删多少行」，而这些数字**会随删除本身变小**。S4 阶段 1 删完钉钉后
# 1764 就该消失，但没人检查的话文档会一直写着旧值——而 owner 正是按这些
# 数字做授权决定。**错的决策依据比没有依据更糟。**
plan=docs/channel-retirement-plan.md
if [ -f "$plan" ]; then
    for mod in feishu dingtalk wechat ws; do
        f="src/$mod.rs"
        # 文档里该模块行的行数列（表格列 3，形如 "| `src/feishu.rs` | 2206 |"）
        doc_n=$(awk -F'|' -v m="src/$mod.rs" '
            index($2, m) { gsub(/[^0-9]/, "", $3); print $3; exit }' "$plan")
        [ -n "$doc_n" ] || continue
        if [ ! -f "$f" ]; then
            # 文件已删 ⇒ 文档可以保留行数**作为历史记录**，但必须明说它已删除。
            # 判据：该模块所在小节的标题里要有「已删除」/「退役前」字样。
            # 否则就是在描述一个已经不存在的东西——而 owner 按它做判断。
            if ! grep -q '已删除\|退役前' "$plan"; then
                fail=$((fail + 1))
                printf '❌ 退役方案提到 %s（%s 行）但文件已删除，且未标明是历史记录\n' \
                    "src/$mod.rs" "$doc_n"
            fi
            continue
        fi
        real_n=$(wc -l < "$f" | tr -d ' ')
        if [ "$doc_n" != "$real_n" ]; then
            fail=$((fail + 1))
            printf '❌ 退役方案写 src/%s.rs 为 %s 行，实测 %s 行 ⇒ 数字漂移（owner 按它做授权决定）\n' \
                "$mod" "$doc_n" "$real_n"
        fi
    done
fi

if [ "$fail" -eq 0 ]; then
    echo "✅ docs/architecture.md 与代码一致（规模数字与结构断言全部对上）"
    exit 0
fi
echo
echo "修法：bash tools/arch-check.sh --print 取实测值，更新文档后重跑。"
exit 1
