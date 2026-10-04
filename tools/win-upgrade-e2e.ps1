# 升级端到端验证（Windows）：装新版 -> 断言「服务/频道/数据」全恢复
# 用法：pwsh -File tools\win-upgrade-e2e.ps1 -Preflight   （只读预检）
#       pwsh -File tools\win-upgrade-e2e.ps1 -Setup app-assets\Output\ABB-Setup-X.exe
# 注意：升级会短暂停掉托盘与服务（约 10~30 秒），期间 bot 不可用。
param([switch]$Preflight, [string]$Setup, [int]$RecoverTimeoutSec = 180)
$ErrorActionPreference = 'Stop'
$H = Join-Path $env:USERPROFILE '.agent-bridge'
$script:fail = 0
function Say($m) { Write-Host ('  ' + $m) }
function Check($n, $ok, $d) { if ($ok) { Write-Host ('  [OK] ' + $n + '  ' + $d) } else { Write-Host ('  [FAIL] ' + $n + '  ' + $d); $script:fail++ } }
# 角色识别：不依赖 CommandLine（受限/沙箱环境下读不到，会把服务误算成托盘）。
# 用 service.pid 对号：pid 文件里的那个进程 = 服务，其余 = 托盘/CLI。
function Get-Roles() {
  $procs = @(Get-Process agent-bridge -ErrorAction SilentlyContinue)
  $svcPid = 0
  $pf = Join-Path $H 'logs\service.pid'
  if (Test-Path $pf) { [void][int]::TryParse(((Get-Content $pf -Raw) -replace '\s', ''), [ref]$svcPid) }
  @{ Tray = @($procs | Where-Object { $_.Id -ne $svcPid }); Svc = @($procs | Where-Object { $_.Id -eq $svcPid }); SvcPid = $svcPid }
}
function Test-Invariants($phase) {
  Write-Host ('== 不变量（' + $phase + '）==')
  $r = Get-Roles
  Check '托盘在跑' ($r.Tray.Count -ge 1) ('tray=' + $r.Tray.Count)
  Check '服务在跑' ($r.Svc.Count -ge 1) ('svcPid=' + $r.SvcPid + ' 命中=' + $r.Svc.Count)
  $bs = Join-Path $H 'logs\bot-status.json'
  if (Test-Path $bs) {
    $age = [int](((Get-Date) - (Get-Item $bs).LastWriteTime).TotalSeconds)
    Check 'bot-status 心跳新鲜(<120s)' ($age -lt 120) ('age=' + $age + 's')
    $t = Get-Content $bs -Raw
    $on = ([regex]::Matches($t, '"conn":\s*"在线"')).Count
    $off = ([regex]::Matches($t, '"conn":\s*"(?!在线)')).Count
    Check '渠道全部在线' ($off -eq 0 -and $on -ge 1) ('在线=' + $on + ' 离线=' + $off)
  } else { Check 'bot-status 存在' $false $bs }
  Check 'config.json 存在' (Test-Path (Join-Path $H 'config.json')) 'config.json'
  $ws = @(Get-ChildItem (Join-Path $H 'workspaces') -Directory -ErrorAction SilentlyContinue).Count
  Check '运行数据完好' ($ws -ge 1) ('workspaces=' + $ws)
  $df = Join-Path $H 'logs\service.desired'
  if (Test-Path $df) { Check '服务意图=运行' ((Get-Content $df -Raw).Trim() -eq '1') ('desired=' + (Get-Content $df -Raw).Trim()) }
  $run = (Get-ItemProperty 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Run' -Name ABB -ErrorAction SilentlyContinue).ABB
  $exe = 'C:\Program Files\ABB\agent-bridge.exe'
  Check 'Run 键指向当前安装位置' ($run -and ($run.Trim('"') -ieq $exe)) ('Run=' + $run)
}
Test-Invariants '升级前'
if ($script:fail -gt 0) { Write-Host ('✗ 预检未通过：' + $script:fail + ' 条'); exit 1 }
if ($Preflight) { Write-Host '✓ 预检通过（未做任何改动）'; exit 0 }
if (-not $Setup -or -not (Test-Path $Setup)) { Write-Host '✗ 需要 -Setup <存在的安装包路径>'; exit 2 }
Write-Host ('== 静默升级：' + $Setup + ' ==')
$t0 = Get-Date
$p = Start-Process -FilePath $Setup -ArgumentList '/VERYSILENT','/SUPPRESSMSGBOXES','/NORESTART','/CLOSEAPPLICATIONS' -PassThru
$p.WaitForExit()
Say ('安装器退出码=' + $p.ExitCode)
$dl = (Get-Date).AddSeconds($RecoverTimeoutSec)
while ((Get-Date) -lt $dl) { $r = Get-Roles; if ($r.Tray.Count -ge 1 -and $r.Svc.Count -ge 1) { break }; Start-Sleep -Seconds 3 }
Say ('升级到恢复用时约 ' + [int]((Get-Date) - $t0).TotalSeconds + 's')
Test-Invariants '升级后'
if ($script:fail -gt 0) { Write-Host ('✗ 升级后断言失败 ' + $script:fail + ' 条'); exit 1 }
Write-Host '✓ 升级端到端通过：服务、频道、数据全部恢复'