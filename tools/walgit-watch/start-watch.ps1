# 启动 walgit 事件监听器（后台、隐藏窗口）的便捷入口。
#
# 只负责起进程，不做 sleep/轮询 —— 监听器自身常驻，本脚本立刻返回。
#
# 用法：
#   pwsh -File start-watch.ps1                      # 后台常驻（状态目录 = %LOCALAPPDATA%\abb-walgit-watch）
#   pwsh -File start-watch.ps1 -IntervalSec 30
#   pwsh -File start-watch.ps1 -NotifierBot cli_xxx -NotifierChat oc_xxx
#
param(
  [int]$IntervalSec = 20,
  # 状态目录（须已存在含 abb-mirror.git 的远端镜像）。
  [string]$Base = (Join-Path $env:LOCALAPPDATA 'abb-walgit-watch'),
  [string]$NotifierBot = '',
  [string]$NotifierChat = '',
  [string]$MirrorRemote = 'origin'
)

# param 必须是脚本第一条语句（注释之后），否则不会被当成 param 块。
$ErrorActionPreference = 'Stop'

$Watch = Join-Path $PSScriptRoot 'watch.ps1'
if (-not (Test-Path $Watch)) { throw "找不到 $Watch" }
New-Item -ItemType Directory -Force -Path $Base | Out-Null

# 先清掉停止标记，否则新进程会立刻退出。
Remove-Item (Join-Path $Base 'watch.stop') -Force -ErrorAction SilentlyContinue

$psArgs = @('-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', $Watch, '-IntervalSec', "$IntervalSec",
            '-Base', $Base, '-Remote', $MirrorRemote)
if ($NotifierBot)  { $psArgs += @('-BotKey', $NotifierBot) }
if ($NotifierChat) { $psArgs += @('-ChatId', $NotifierChat) }

$ps = Start-Process -FilePath 'powershell.exe' -ArgumentList $psArgs -WindowStyle Hidden -PassThru
Set-Content -Path (Join-Path $Base 'launcher.pid') -Value $ps.Id -Encoding ASCII
"launcher pid=$($ps.Id)  (state dir: $Base)"
