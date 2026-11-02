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
    if grep -q "\b${val}\b" "$doc"; then
        return 0
    fi
    fail=$((fail + 1))
    printf '❌ %s：实测 %s，但文档里没有这个数字 ⇒ 文档过期\n' "$label" "$val"
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
MCP_BUS=$(lines_of src/mcp_bus.rs)
MESSENGER=$(lines_of src/messenger.rs)
FEISHU=$(lines_of src/feishu.rs)
WECHAT=$(lines_of src/wechat.rs)
DINGTALK=$(lines_of src/dingtalk.rs)
RUNTIME_TOTAL=$((ACP + HARNESS + POOL + QUEUE + ONESHOT))
GUI_TOTAL=$((UI_RS + SYSCHAT + RENDER))

if [ "$print_only" -eq 1 ]; then
    cat <<EOF
src/ 合计            $TOTAL_RS
ui/app.slint         $SLINT
ui.rs                $UI_RS
syschat.rs           $SYSCHAT
ui_headless_render   $RENDER
task_run.rs          $TASK_RUN
platform.rs          $PLATFORM
main.rs              $MAIN_RS
service.rs           $SERVICE
guard.rs             $GUARD
deliver.rs           $DELIVER
runtime 合计         $RUNTIME_TOTAL （acp $ACP / harness $HARNESS / pool $POOL / queue $QUEUE / oneshot $ONESHOT）
GUI 三件合计         $GUI_TOTAL
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

# ── 校验：几条结构性断言（数字对了但结构变了更危险）──────────────────────────
# Messenger 实现数 = 3 个生产实现（测试假件不算）
n_msgr=$(grep -c 'impl Messenger for' src/messenger.rs)
if [ "$n_msgr" -ne 3 ]; then
    fail=$((fail + 1))
    printf '❌ Messenger 生产实现数：实测 %s，文档写 3\n' "$n_msgr"
fi
# 入站三入口仍在（id 变了说明有人重构）
for fn in 'on_payload' 'on_weixin' 'on_dingtalk'; do
    if ! grep -rq "pub async fn ${fn}" src/bridge/; then
        fail=$((fail + 1)); printf '❌ 入站入口 %s 不存在了（文档 §2 描述过期）\n' "$fn"
    fi
done
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

if [ "$fail" -eq 0 ]; then
    echo "✅ docs/architecture.md 与代码一致（规模数字与结构断言全部对上）"
    exit 0
fi
echo
echo "修法：bash tools/arch-check.sh --print 取实测值，更新文档后重跑。"
exit 1
