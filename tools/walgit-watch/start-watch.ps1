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

# 已在常驻的监听器会独占 watch.lock（watch.ps1 用 FileShare.None 持有）。
# 起第二个进程只会立刻退出，还会误清 watch.stop —— 这里提前拦住并说清楚。
try {
  $probe = [System.IO.File]::Open((Join-Path $Base 'watch.lock'), 'OpenOrCreate', 'ReadWrite', 'None')
  $probe.Dispose()
} catch {
  throw "已有监听器在运行（$Base\watch.lock 被占用）。先停止它，或用 -Base 指定别的状态目录。"
}

# 先清掉停止标记，否则新进程会立刻退出。
Remove-Item (Join-Path $Base 'watch.stop') -Force -ErrorAction SilentlyContinue

# Start-Process 的 -ArgumentList 是按空格拼接的：含空格的路径必须自己加引号，
# 否则 -File / -Base 会被拆开，隐藏窗口里的子进程直接起不来。
function Quote-Arg([string]$s) {
  if ($s -match '[\s"]') { return '"' + ($s -replace '"', '\"') + '"' }
  return $s
}

$psArgs = @('-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', (Quote-Arg $Watch), '-IntervalSec', "$IntervalSec",
            '-Base', (Quote-Arg $Base), '-Remote', (Quote-Arg $MirrorRemote))
if ($NotifierBot)  { $psArgs += @('-BotKey', (Quote-Arg $NotifierBot)) }
if ($NotifierChat) { $psArgs += @('-ChatId', (Quote-Arg $NotifierChat)) }

$ps = Start-Process -FilePath 'powershell.exe' -ArgumentList $psArgs -WindowStyle Hidden -PassThru
Set-Content -Path (Join-Path $Base 'launcher.pid') -Value $ps.Id -Encoding ASCII
"launcher pid=$($ps.Id)  (state dir: $Base)"
