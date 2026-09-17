#!/bin/sh
# 启动/接管本机 walgit CI runner（ABB）。
#
# 为什么必须设 TMPDIR/CARGO_TARGET_DIR：runner 把待测 commit 检出到 $TMPDIR 下并在那里跑
# cargo。默认 $TMPDIR 在 228 GiB 内盘上，每个 run 一份完整 target/ 树，内盘很快见底，链接期
# 报 `errno=28 (No space left on device)`（2026-09-17 实测：内盘 97%、6 GiB 可用，
# docs/p3-keepalive-brief 的 build 任务即因此失败）。两者都指到数据卷后构建不再吃内盘。
#
# 用法：
#   tools/run_ci_runner.sh              # 用 screen 托管（已在跑则直接复用，不重复启动）
#   tools/run_ci_runner.sh --foreground # 前台常驻（自己用 nohup 托管）
#   tools/run_ci_runner.sh --once       # 只跑一轮就退出（cron/调试）
#
# 注意：**同一台机器只起一个 runner**——两个 runner 会各自去认领同一个 run 的任务。
set -eu

repo=${ABB_REPO:-/Volumes/DataExt/GitHub/abb}
tmp=${ABB_CI_TMP:-/Volumes/DataExt/ci-tmp}
target=${ABB_CI_TARGET:-$repo/target-ci}
config=${WALGIT_CONFIG:-$HOME/.walgit/walgit.toml}
key=${ABB_CI_KEY:-$HOME/.walgit/keys/ci-runner.ed25519}
screen_name=${ABB_CI_SCREEN:-walgit-ci-abb}

if [ ! -f "$key" ]; then
    echo "缺少 CI 签名 key: $key" >&2
    exit 1
fi
mkdir -p "$tmp" "$target"

# 用临时启动脚本固定环境变量，避免把参数拼进 `screen sh -c` 时踩转义。
case "${1:-}" in
    --once)       extra='--once' ;;
    --foreground) extra='' ;;
    "")           extra='' ;;
    *)
        echo "用法: $0 [--foreground|--once]" >&2
        exit 2
        ;;
esac

launcher="$tmp/ci-runner-launch.$$"
cat > "$launcher" <<EOF
#!/bin/sh
export TMPDIR='$tmp'
export CARGO_TARGET_DIR='$target'
exec walgit --config '$config' ci run --repo '$repo' --remote origin --actor ci-runner --key '$key' $extra
EOF
chmod +x "$launcher"

case "${1:-}" in
    --once|--foreground)
        exec "$launcher"
        ;;
    "")
        if screen -ls 2>/dev/null | grep -q "[.]${screen_name}[[:space:]]"; then
            echo "runner 已在 screen $screen_name 中运行（不重复启动）"
            exit 0
        fi
        screen -dmS "$screen_name" "$launcher"
        echo "CI runner 已启动：screen -r $screen_name（TMPDIR=$tmp CARGO_TARGET_DIR=$target）"
        ;;
esac
