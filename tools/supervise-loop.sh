#!/bin/sh
# 起一个 5 分钟间隔的监督循环（后台常驻）。
#
# ## 为什么不用 launchd
#
# 实测：`launchctl bootstrap gui/501` 在这个环境对**任何** plist 都返回
# `Input/output error`（连只 `/bin/echo hi` 的最小 plist 也一样）。
# 所以 launchd 路不通，改用后台 `while` 循环。
#
# 后台循环反而更可控：
# - `ps` 能查到、`kill` 能停
# - 日志直接落文件
# - 状态文件每轮覆盖，随时 `cat` 就知道最新情况
#
# ## 用法
#
#     tools/supervise-loop.sh start     # 起（幂等：已在跑就不重复起）
#     tools/supervise-loop.sh stop      # 停
#     tools/supervise-loop.sh status    # 看有没有在跑 + 最后状态
#
# 间隔：`$ABB_SUPERVISE_INTERVAL`（默认 300 秒 = 5 分钟）

set -u

cd "$(dirname "$0")/.." || exit 2

interval="${ABB_SUPERVISE_INTERVAL:-300}"
pid_file="${ABB_SUPERVISE_PID:-/Volumes/DataExt/tmp/abb-supervise.pid}"
state_file="${ABB_SUPERVISE_STATE:-/Volumes/DataExt/tmp/abb-supervise-status.txt}"
log_file="${ABB_SUPERVISE_LOG:-/Volumes/DataExt/tmp/abb-supervise.log}"

running() {
    [ -f "$pid_file" ] || return 1
    pid=$(cat "$pid_file" 2>/dev/null) || return 1
    [ -n "$pid" ] || return 1
    kill -0 "$pid" 2>/dev/null || return 1
    return 0
}

case "${1:-}" in
    start)
        if running; then
            echo "已在跑（pid $(cat "$pid_file")，间隔 ${interval}s）"
            exit 0
        fi
        # **每轮都先跑一次**再睡：起完立刻有状态，不用等 5 分钟。
        #
        # `--quiet` 只在有偏差时输出到 stdout；但状态文件每轮都写，
        # 所以「安静」不等于「没在检查」。
        nohup sh -c '
            while true; do
                sh tools/supervise.sh --quiet
                sleep "$1"
            done
        ' _ "$interval" > /dev/null 2>> "$log_file" &
        echo $! > "$pid_file"
        sleep 2
        if running; then
            echo "已启动（pid $(cat "$pid_file")，每 ${interval}s 一轮）"
            echo "状态：$state_file"
            echo "日志：$log_file"
        else
            echo "❌ 启动失败，看 $log_file" >&2
            exit 1
        fi
        ;;
    stop)
        if ! running; then
            echo "没在跑"
            rm -f "$pid_file"
            exit 0
        fi
        pid=$(cat "$pid_file")
        kill "$pid" 2>/dev/null
        sleep 1
        kill -9 "$pid" 2>/dev/null
        rm -f "$pid_file"
        echo "已停（pid $pid）"
        ;;
    status)
        if running; then
            echo "运行中（pid $(cat "$pid_file")，每 ${interval}s 一轮）"
        else
            echo "未运行"
        fi
        echo '--- 最后一轮状态 ---'
        cat "$state_file" 2>/dev/null || echo '（还没有状态文件）'
        ;;
    *)
        echo "用法：$0 {start|stop|status}" >&2
        exit 2
        ;;
esac
