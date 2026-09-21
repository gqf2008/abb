# tools/walgit-watch — walgit 协作事件监听器

把 walgit `refs/collab/*` 上的新协作事件，翻译成**一次 ABB 回合的唤醒**（`job add --once`）。
它是「报 + 办」两件事：先汇总一条通知，再按规则唤醒一个回合去处理。

## 为什么不直接用 `walgit collab watch`

`walgit collab watch` 只能「通知」——它把 entry JSON 交给 `--exec` 指定的程序，
本身**不能唤起一个 ABB 回合**，也不能攒批、冷却、按线程过滤。本脚本补的正是这一段：
`git ls-remote` 检测变更 → 解析 entry → 规则判定 → `job add --once <now+1min>`。

关键约束：**唤醒回合不能靠 sleep/while 常驻**——那会占住聊天通道，
期间用户的新消息全部排队。所以这里只写一条定时任务就退出。

## 文件

| 文件 | 说明 |
|---|---|
| `watch.ps1` | 监听器本体（常驻）。参数全部可覆盖：状态目录默认 `%LOCALAPPDATA%\abb-walgit-watch`，身份取自 ABB 注入的环境变量。 |
| `start-watch.ps1` | 便捷启动（后台隐藏窗口）。只负责起进程，立刻返回。 |
| `wake-prompt.template.md` | 唤醒回合读到的工作指引模板。首次运行时复制成状态目录里的 `wake-prompt.md`；**已存在则不覆盖**。 |

运行期状态文件（不进版本库）：`watch.pid`、`watch.stop`、`refs-state.json`、
`own-threads.json`、`pending-wake.json`、`wakes.jsonl`、`events.jsonl`、`watch.log`、`last-wake.txt`。

## 用法

```powershell
# 常驻（状态目录 = %LOCALAPPDATA%\abb-walgit-watch，镜像 = 该目录下的 abb-mirror.git）
pwsh -File tools/walgit-watch/start-watch.ps1 -IntervalSec 20 `
     -NotifierBot cli_xxxx -NotifierChat oc_xxxx

# 单趟干跑（不推送、不唤醒），用于验证规则与解析
pwsh -File tools/walgit-watch/watch.ps1 -Once -NoNotify -NoWake

# 停止：在状态目录放一个 watch.stop（只影响监听进程，不碰 ABB 服务）
New-Item -ItemType File "$env:LOCALAPPDATA\abb-walgit-watch\watch.stop"
```

### 前置

1. `$Base`（状态目录）下存在远端镜像 `abb-mirror.git`（`git clone --mirror <walgit 服务> abb-mirror.git`
   或 `git init --bare` 后加 remote）。监听器用 `git -C <镜像> ls-remote $Remote` 取远端 ref 表，
   再按需 `fetch` 变更的 ref；镜像同时是「离线可读的全量 refs 缓存」。
2. 唤醒用 `job add`，需要 `-AbbExe`（默认 `$env:ABB_BIN`）与 `-BotKey`；
   投递目标 `-ChatId` 必须是平台可识别的 `receive_id`（飞书 `oc_*`）；
   **不要用 buzz 频道 UUID**——`deliver` 会收到 230001。

## 唤醒规则

- `actor != MyPrincipal` 的**新 issue** → 一律唤醒（新工作项，不能只报不办）。
- 自家线程（`own-threads`）上的 `patch` / `review` / `status`（`actor != MyPrincipal`）→ 唤醒。
  `own-threads` 可由 `-Workspace` 下的 `gh-archive/migration-map.json` 播种，也会由
  「本人参与过的 entry」自动扩充，持久化在 `own-threads.json`。
- `comment` / `merge_result` 不单独唤醒（避免刷屏）。
- 冷却 `WakeCooldownSec` 秒；未到冷却的事件进 `pending-wake.json` 攒批，攒到下一轮一起唤醒，不丢。

## 红线

- 唤醒提示词里写明：**不可逆动作（`merge_result` / `status: closed` / 关 issue / 删 ref）先问 owner**。
- 监听器只写自己的状态目录与镜像；不写 ABB 的 `~/.agent-bridge` 配置目录。
- `watch.stop` 只停监听器本身；不要用它去停 ABB 服务/进程。
