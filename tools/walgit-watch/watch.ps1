<#
  walgit 协作层监听器（常驻）—— 报 + 办

  为什么需要它：walgit 的 `collab watch` 只能通知，不能「唤起一个 ABB 回合」；
  本脚本把「新协作事件」翻译成一次 `job add --once`（到点唤起一轮 agent），
  且**不占会话**（不 sleep/while 阻塞当前聊天）。

  - 每 IntervalSec 秒用 `git ls-remote` 拉取远端 refs/collab/* 的状态
  - 与本地镜像 refs 比对，发现新增/变更/删除；状态存 refs-state.json（重启增量续跑，不漏停机窗口）
  - 变化的 entry 取回解析（kind / 线程 id / actor），汇总成一条消息推送（报）
  - 命中「需处理」规则时，按批唤醒一个 ABB 回合去处理（办）：`job add --once now+1min`
      · 规则：kind ∈ TriggerKinds 且 actor ≠ MyPrincipal 且（新 issue 或 线程 ∈ own-threads）
      · own-threads 既可由 gh-archive/migration-map.json 播种，也会由「本人参与过的 entry」自动扩充
      · 冷却 WakeCooldownSec 秒；待唤醒事件进 pending-wake.json，攒批一起唤醒，不丢不刷
  停止：在状态目录放一个 watch.stop 文件（只影响本监听进程）

  所有路径/身份均可用参数覆盖；默认从 $PSScriptRoot 与 ABB 注入的环境变量推导。
#>
param(
  [int]$IntervalSec = 20,
  [switch]$Once,
  [switch]$NoNotify,
  [switch]$NoWake,
  # 干跑：不推送、不唤醒，也**不写任何状态文件**（否则会把真实监听器该报的事件静默吞掉）。
  [switch]$DryRun,
  [switch]$ReSeedOwnThreads,
  [int]$MaxItems = 15,
  [int]$WakeCooldownSec = 120,
  # 状态目录（watch.pid / refs-state.json / pending-wake.json …），**独立于检出目录**。
  # 默认 %LOCALAPPDATA%\abb-walgit-watch；部署时显式传入。
  [string]$Base = (Join-Path $env:LOCALAPPDATA 'abb-walgit-watch'),
  # ABB 工作区根（用于回落到 gh-archive/migration-map.json 播种 own-threads）。
  [string]$Workspace = (Split-Path $PSScriptRoot -Parent),
  # 要跟踪的远端名（在 $Mirror 里）。指向 walgit 服务或本地镜像均可。
  # 注意：PowerShell 变量名大小写不敏感，脚本内局部变量一律用 $remoteMap，
  # 避免与 $Remote 撞名后被 [string] 类型约束强转。
  [string]$Remote = 'origin',
  # ABB 可执行文件；默认取桥注入的 $env:ABB_BIN。
  [string]$AbbExe = $env:ABB_BIN,
  # 投递目标 bot / 会话；默认取桥注入的环境变量。
  [string]$BotKey = $env:AGENT_BRIDGE_BOT_KEY,
  [string]$ChatId = $env:AGENT_BRIDGE_CHAT_ID,
  # 本人的 principal：actor 等于它的 entry 不会触发唤醒（自己签的不必唤醒自己）。
  [string]$MyPrincipal = 'abb-win'
)

$ErrorActionPreference = 'Continue'

# README 的干跑写法是 -NoNotify -NoWake；-DryRun 显式等价，并且都不写状态文件。
if ($DryRun) { $NoNotify = $true; $NoWake = $true }
$dryRun = [bool]($DryRun -or ($NoNotify -and $NoWake))

if (-not $Base) { $Base = (Get-Location).Path }
if (-not $Workspace) { $Workspace = (Split-Path $Base -Parent) }
if (-not $AbbExe) {
  $candidate = Join-Path $env:LOCALAPPDATA 'Programs\ABB\agent-bridge.exe'
  if (Test-Path $candidate) { $AbbExe = $candidate }
}

$Ws        = $Workspace
$Mirror    = Join-Path $Base 'abb-mirror.git'
$LogFile   = Join-Path $Base 'watch.log'
$EventsLog = Join-Path $Base 'events.jsonl'
$PidFile   = Join-Path $Base 'watch.pid'
$StopFile  = Join-Path $Base 'watch.stop'
$LockFile  = Join-Path $Base 'watch.lock'
$StateFile = Join-Path $Base 'refs-state.json'
$OwnFile   = Join-Path $Base 'own-threads.json'
$PendFile  = Join-Path $Base 'pending-wake.json'
$WakeLog   = Join-Path $Base 'wakes.jsonl'
$WakeDoc   = Join-Path $Base 'wake-prompt.md'
$LastWake  = Join-Path $Base 'last-wake.txt'

# 需要唤醒处理的 entry kind。
#   - 任何 actor != MyPrincipal 的 **新 issue**：一律唤醒（新工作项，不能只报不办）
#   - 其余 kind（patch/review/status）只在**自家线程**（own-threads）上唤醒
#   - comment / merge_result 不单独唤醒（避免刷屏）
$TriggerKinds = @('issue', 'patch', 'review', 'status')

function Write-Log([string]$msg) {
  $line = (Get-Date).ToString('yyyy-MM-dd HH:mm:ss') + '  ' + $msg
  try { Add-Content -Path $LogFile -Value $line -Encoding UTF8 } catch {}
}

# 状态文件一律 tmp + rename（与仓库其它运行期状态同约定）：半截写入会让 Load-State
# 解析失败 → 退回「首次基线」→ 停机窗口内的事件被静默跳过。
function Write-AtomicText([string]$path, [string]$text) {
  $tmp = $path + '.tmp'
  try {
    Set-Content -Path $tmp -Value $text -Encoding UTF8
    Move-Item -Path $tmp -Destination $path -Force
  } catch {
    Write-Log ('atomic write failed: ' + $path + ' : ' + $_.Exception.Message)
    if (Test-Path $tmp) { Remove-Item $tmp -Force -ErrorAction SilentlyContinue }
  }
}

function Send-Text([string]$text) {
  if ($NoNotify) { Write-Log ('DRYRUN>> ' + ($text -replace "`r?`n", ' | ')); return }
  if (-not $AbbExe -or -not $BotKey -or -not $ChatId) {
    Write-Log 'notify skipped: AbbExe/BotKey/ChatId 未配置（用 -AbbExe/-BotKey/-ChatId 或环境变量提供）'
    return
  }
  Remove-Item Env:AGENT_BRIDGE_CHAT_ID -ErrorAction SilentlyContinue
  Remove-Item Env:AGENT_BRIDGE_BOT_KEY  -ErrorAction SilentlyContinue
  try {
    $out = & $AbbExe deliver --bot $BotKey --chat $ChatId --text $text 2>&1
    Write-Log ('deliver rc=' + $LASTEXITCODE + ' out=' + (($out | Out-String).Trim()))
  } catch { Write-Log ('deliver EXCEPTION: ' + $_.Exception.Message) }
}

function Get-RemoteMap {
  $r = & git -C $Mirror ls-remote $Remote 'refs/collab/*' 2>&1
  if ($LASTEXITCODE -ne 0) { throw ('ls-remote rc=' + $LASTEXITCODE + ' : ' + (($r | Out-String).Trim())) }
  $h = @{}
  foreach ($line in $r) {
    if ($line -match '^([0-9a-f]{40})\s+(refs/\S+)$') { $h[$Matches[2]] = $Matches[1] }
  }
  return $h
}

function Get-LocalMap {
  $h = @{}
  $r = & git -C $Mirror for-each-ref '--format=%(objectname) %(refname)' 2>&1
  foreach ($line in $r) {
    if ($line -match '^([0-9a-f]{40})\s+(refs/collab/\S+)$') { $h[$Matches[2]] = $Matches[1] }
  }
  return $h
}

function Load-State {
  if (-not (Test-Path $StateFile)) { return $null }
  try {
    $o = Get-Content $StateFile -Raw -Encoding UTF8 | ConvertFrom-Json
    $h = @{}
    foreach ($p in $o.PSObject.Properties) { $h[$p.Name] = [string]$p.Value }
    if ($h.Count -eq 0) { return $null }
    return $h
  } catch { Write-Log ('state load failed: ' + $_.Exception.Message); return $null }
}

function Fetch-Refs([string[]]$refs) {
  if (-not $refs -or $refs.Count -eq 0) { return }
  $chunk = 40
  for ($i = 0; $i -lt $refs.Count; $i += $chunk) {
    $last = [Math]::Min($i + $chunk - 1, $refs.Count - 1)
    $specs = @()
    foreach ($r in $refs[$i..$last]) { $specs += ('+' + $r + ':' + $r) }
    $out = & git -C $Mirror fetch --no-tags --quiet $Remote @specs 2>&1
    if ($LASTEXITCODE -ne 0) { Write-Log ('fetch rc=' + $LASTEXITCODE + ' : ' + (($out | Out-String).Trim())) }
  }
}

function Test-Object([string]$oid) {
  if (-not $oid) { return $false }
  & git -C $Mirror cat-file -e ($oid + '^{blob}') 2>$null
  return ($LASTEXITCODE -eq 0)
}

function Decode-Object([string]$oid) {
  # 按**远端 oid** 解码，而不是 ref 名：fetch 失败时 ref 还指着旧对象，
  # 按 ref 名解码会把上一条旧 event 当成本次变更重复上报/重复唤醒。
  $txt = & git -C $Mirror cat-file -p $oid 2>$null
  if ($LASTEXITCODE -ne 0 -or -not $txt) { return $null }
  $joined = ($txt | Out-String).Trim()
  try { return ($joined | ConvertFrom-Json) } catch { return $null }
}

function Save-State($map) {
  Write-AtomicText $StateFile ($map | ConvertTo-Json -Compress)
}

# ---------- own-threads：谁算「我的线程」 ----------
function Seed-OwnThreads {
  $set = New-Object System.Collections.Generic.HashSet[string]
  $map = Join-Path $Ws 'gh-archive\migration-map.json'
  if (Test-Path $map) {
    try {
      $o = Get-Content $map -Raw -Encoding UTF8 | ConvertFrom-Json
      foreach ($t in $o.threads) { if ($t.thread_id) { [void]$set.Add([string]$t.thread_id) } }
      if ($o.record_thread -and $o.record_thread.thread_id) { [void]$set.Add([string]$o.record_thread.thread_id) }
    } catch { Write-Log ('seed from migration-map failed: ' + $_.Exception.Message) }
  }
  return ,$set
}

function Load-OwnThreads {
  $set = New-Object System.Collections.Generic.HashSet[string]
  if ($ReSeedOwnThreads -or -not (Test-Path $OwnFile)) {
    $set = Seed-OwnThreads
    if (-not $dryRun) { Save-OwnThreads $set }
    Write-Log ('own-threads seeded: ' + $set.Count + ' threads')
  } else {
    try {
      $o = Get-Content $OwnFile -Raw -Encoding UTF8 | ConvertFrom-Json
      foreach ($x in $o) { [void]$set.Add([string]$x) }
    } catch { Write-Log ('own-threads load failed: ' + $_.Exception.Message) }
  }
  return ,$set
}

function Save-OwnThreads($set) {
  Write-AtomicText $OwnFile (@($set) | Sort-Object | ConvertTo-Json -Compress)
}

# ---------- pending-wake：攒批 ----------
function Load-Pending {
  if (-not (Test-Path $PendFile)) { return @() }
  try {
    $o = Get-Content $PendFile -Raw -Encoding UTF8 | ConvertFrom-Json
    if ($null -eq $o) { return @() }
    return @($o)
  } catch { return @() }
}

function Save-Pending($arr) {
  $n = @($arr).Count
  if ($n -eq 0) { Write-AtomicText $PendFile '[]'; return }
  Write-AtomicText $PendFile (@($arr) | ConvertTo-Json -Compress)
}

function Ensure-WakeDoc {
  # 状态目录里已有自定义指引 → 绝不覆盖。
  if (Test-Path $WakeDoc) { return }
  # 随脚本发布的外部模板优先（仓库内的 wake-prompt.template.md）。
  $template = Join-Path $PSScriptRoot 'wake-prompt.template.md'
  if (Test-Path $template) {
    try { Copy-Item -Path $template -Destination $WakeDoc -Force; return } catch {}
  }
  $doc = @'
# walgit 事件唤醒处理指引

（随脚本发布的默认模板；请把 wake-prompt.template.md 放在脚本同目录以自定义，
或直接编辑本文件 —— 它一旦存在就不会再被覆盖。）

- 先读事件、再判断相关性、最后给结论；不要只转发原文。
- 不可逆动作（merge_result / status: closed / 关 issue / 删 ref）一律先问 owner。
- 不用别人的 key 代签；不抢 worker 的活。
- 回复保持简短。
'@
  try { Set-Content -Path $WakeDoc -Value $doc -Encoding UTF8 } catch {}
}

function Invoke-Wake([System.Collections.ArrayList]$batch) {
  if ($batch.Count -eq 0) { return $false }
  Ensure-WakeDoc
  $items = @()
  foreach ($b in $batch) { $items += ('' + $b.kind + ' ' + $b.id + ' <- ' + $b.actor) }
  $list = ($items -join '；')
  $prompt = '[walgit 事件·需你处理] ' + $batch.Count + ' 条新协作事件：' + $list +
            '。请阅读 ' + $WakeDoc + ' 并按其中要求处理：先用 walgit collab thread 看上下文，判断是否与你（principal ' + $MyPrincipal + '）相关；相关的给结论与建议、需要 owner 拍板的列清楚；无关的一句话说明。不可逆动作（merge_result / status closed / 关 issue）一律先问 owner，不要自行执行。回复保持简短。'

  if ($NoWake) { Write-Log ('DRYRUN-WAKE>> ' + $prompt); return $false }

  if (-not $AbbExe -or -not $BotKey -or -not $ChatId) {
    Write-Log 'wake skipped: AbbExe/BotKey/ChatId 未配置（用 -AbbExe/-BotKey/-ChatId 或环境变量提供）'
    return $false
  }
  $fire = (Get-Date).AddMinutes(1).ToString('yyyy-MM-dd HH:mm')
  Remove-Item Env:AGENT_BRIDGE_CHAT_ID -ErrorAction SilentlyContinue
  $env:AGENT_BRIDGE_BOT_KEY = $BotKey
  try {
    $out = & $AbbExe job add --once $fire --prompt $prompt --note ('walgit-watch: ' + $batch.Count + ' 条事件') --to $ChatId 2>&1
    $rc = $LASTEXITCODE
    Write-Log ('wake job add rc=' + $rc + ' fire=' + $fire + ' n=' + $batch.Count + ' out=' + (($out | Out-String).Trim()))
    if ($rc -ne 0) { return $false }
    foreach ($b in $batch) {
      $rec = [ordered]@{ ts = (Get-Date).ToString('o'); fire_at = $fire; kind = $b.kind; id = $b.id; actor = $b.actor; ref = $b.ref; oid = $b.oid }
      try { Add-Content -Path $WakeLog -Value ($rec | ConvertTo-Json -Compress) -Encoding UTF8 } catch {}
    }
    return $true
  } catch {
    Write-Log ('wake EXCEPTION: ' + $_.Exception.Message)
    return $false
  }
}

function Format-Events([System.Collections.ArrayList]$events, [int]$ciCount, [int]$delCount, [int]$wokeN) {
  if ($events.Count -eq 0 -and $ciCount -eq 0 -and $delCount -eq 0) { return $null }
  $sb = New-Object System.Text.StringBuilder
  $head = '[walgit 监听] ' + $events.Count + ' 条新协作事件'
  if ($ciCount -gt 0)   { $head += '（另有 ci-artifacts 变动 ' + $ciCount + ' 个）' }
  if ($delCount -gt 0)  { $head += '（含 ref 删除 ' + $delCount + ' 个）' }
  if ($wokeN -gt 0)     { $head += ' → 已自动唤醒处理 ' + $wokeN + ' 条' }
  [void]$sb.AppendLine($head)
  $n = 0
  foreach ($e in $events) {
    if ($n -ge $MaxItems) { [void]$sb.AppendLine('… 其余 ' + ($events.Count - $MaxItems) + ' 条见 walgit-watch/events.jsonl'); break }
    [void]$sb.AppendLine('· ' + $e.line)
    $n++
  }
  return $sb.ToString().TrimEnd()
}

function Invoke-PendingFlush {
  $pend = @(Load-Pending)
  if ($pend.Count -eq 0) { return 0 }
  $last = Get-Content $LastWake -ErrorAction SilentlyContinue
  $ok = $true
  if ($last) {
    try { $ok = (((Get-Date) - [datetime]::Parse($last)).TotalSeconds -ge $WakeCooldownSec) } catch { $ok = $true }
  }
  if (-not $ok) { Write-Log ('wake deferred: pending=' + $pend.Count + ' (cooldown ' + $WakeCooldownSec + 's)'); return 0 }
  if (Invoke-Wake $pend) {
    if (-not $dryRun) {
      Write-AtomicText $LastWake ((Get-Date).ToString('o'))
      Save-Pending @()
    }
    Write-Log ('wake flushed: ' + $pend.Count + ' 条')
    return $pend.Count
  }
  return 0
}
# ---------- main ----------
New-Item -ItemType Directory -Force -Path $Base | Out-Null

# 常驻模式单实例：独占锁文件；进程退出（含崩溃）时由 OS 释放。避免两个监听器
# 同时冲刷 pending-wake 造重复 job，或互相覆盖状态文件。干跑/单趟不持锁。
if (-not $Once -and -not $dryRun) {
  try {
    $script:WatchLock = [System.IO.File]::Open($LockFile, 'OpenOrCreate', 'ReadWrite', 'None')
  } catch {
    Write-Log 'another watcher is running (watch.lock is busy) -> exit'
    exit 0
  }
}

if (-not $dryRun) { Write-AtomicText $PidFile ([string]$PID) }

$prev       = Load-State
$first      = ($null -eq $prev)
$own        = Load-OwnThreads
$wokeSum    = 0

Write-Log ('watcher start pid=' + $PID + ' interval=' + $IntervalSec + 's once=' + [bool]$Once +
           ' dry=' + $dryRun +
           ' state=' + $(if ($first) { '无(首次基线)' } else { $prev.Count.ToString() + ' refs(增量续跑)' }) +
           ' own=' + $own.Count + ' trigger=' + ($TriggerKinds -join ',') + ' cooldown=' + $WakeCooldownSec + 's')

while ($true) {
  if (Test-Path $StopFile) { Write-Log 'stop file found -> exit'; break }
  try {
    $remoteMap = Get-RemoteMap
    $local  = Get-LocalMap

    if ($remoteMap.Count -eq 0 -and $prev -and $prev.Count -gt 0) {
      # ls-remote 成功但返回空：多半是远端配置/服务异常。不能当成「全部删除」清空基线，
      # 否则下一次全量 refs 会被当成基线，停机期间的事件永久丢失。
      throw ('remote returned 0 refs while state has ' + $prev.Count + '; refusing to treat as mass deletion')
    }

    # 变更判定对齐「上次已处理的状态」，而不是本地镜像：若进程在 fetch 之后、
    # Save-State 之前崩溃，本地镜像已经追上远端；拿本地镜像比对会把这段事件永久漏掉。
    $changed = New-Object System.Collections.ArrayList
    foreach ($k in $remoteMap.Keys) {
      if (-not $prev -or -not $prev.ContainsKey($k) -or $prev[$k] -ne $remoteMap[$k]) { [void]$changed.Add($k) }
    }
    $deleted = New-Object System.Collections.ArrayList
    if ($prev) { foreach ($k in $prev.Keys) { if (-not $remoteMap.ContainsKey($k)) { [void]$deleted.Add($k) } } }

    # 本地缺对象/落后的 ref 需要 fetch（与「是否变更」无关：上次 fetch 失败、镜像被清理都会这样）。
    $stale = New-Object System.Collections.ArrayList
    foreach ($k in $remoteMap.Keys) {
      if (-not $local.ContainsKey($k) -or $local[$k] -ne $remoteMap[$k]) { [void]$stale.Add($k) }
    }

    if ($first) {
      Write-Log ('baseline: remote=' + $remoteMap.Count + ' local=' + $local.Count + ' fetch=' + $stale.Count)
      Fetch-Refs $stale
      if (-not $dryRun) { Save-State $remoteMap }
      $prev = $remoteMap
      $first = $false
    }
    elseif ($changed.Count -gt 0 -or $deleted.Count -gt 0) {
      Write-Log ('changed refs: ' + $changed.Count + '  deleted refs: ' + $deleted.Count + '  stale: ' + $stale.Count)
      Fetch-Refs $stale

      $events  = New-Object System.Collections.ArrayList
      $ciCount = 0
      $ownChanged = $false
      $cands = New-Object System.Collections.ArrayList       # 命中唤醒规则的事件
      $unresolved = New-Object System.Collections.ArrayList  # fetch 后本地仍缺对象的 ref

      # 第一遍：只解码 + 补齐 own-threads。必须先全量补齐再判唤醒，否则同一批里
      # 「别人的 review」可能先于「我的 patch」被处理（ref 顺序不定），导致漏唤醒。
      $decoded = @{}
      foreach ($r in $changed) {
        if ($r -like 'refs/collab/ci-artifacts/*') { continue }
        $oid = [string]$remoteMap[$r]
        if (-not (Test-Object $oid)) { [void]$unresolved.Add($r); continue }
        $o = Decode-Object $oid
        $decoded[$r] = $o
        if ($o -and $o.actor -eq $MyPrincipal -and $o.id -and -not $own.Contains([string]$o.id)) {
          [void]$own.Add([string]$o.id); $ownChanged = $true
        }
      }

      # 第二遍：生成事件 + 唤醒候选
      foreach ($r in $changed) {
        if ($r -like 'refs/collab/ci-artifacts/*') { $ciCount++; continue }
        if ($unresolved.Contains($r)) { Write-Log ('resolve failed (fetch): ' + $r + ' -> retry next pass'); continue }
        $oid = [string]$remoteMap[$r]
        $o = $decoded[$r]
        if ($o -and $o.kind) {
          [void]$events.Add(@{ line = '' + $o.kind + '  ' + $o.id + '  <- ' + $o.actor })
          # 唤醒规则（own-threads 已在第一遍补齐）
          $k = [string]$o.kind
          $isOwn = ($o.id -and $own.Contains([string]$o.id))
          $isNewIssue = ($k -eq 'issue')
          if ($o.actor -ne $MyPrincipal -and $o.id -and ($isNewIssue -or ($TriggerKinds -contains $k -and $isOwn))) {
            [void]$cands.Add(@{ kind = $k; id = [string]$o.id; actor = [string]$o.actor; ref = $r; oid = $oid; own = [bool]$isOwn })
          }
        } else {
          [void]$events.Add(@{ line = ($r -replace '^refs/collab/', '') })
        }
        $rec = [ordered]@{ ts = (Get-Date).ToString('o'); ref = $r; oid = $oid; kind = $o.kind; id = $o.id; actor = $o.actor }
        try { Add-Content -Path $EventsLog -Value ($rec | ConvertTo-Json -Compress) -Encoding UTF8 } catch {}
      }
      foreach ($r in $deleted) {
        [void]$events.Add(@{ line = 'DELETED  ' + ($r -replace '^refs/collab/', '') })
        $rec = [ordered]@{ ts = (Get-Date).ToString('o'); ref = $r; oid = $null; kind = 'deleted'; id = $null; actor = $null }
        try { Add-Content -Path $EventsLog -Value ($rec | ConvertTo-Json -Compress) -Encoding UTF8 } catch {}
      }
      if ($ownChanged -and -not $dryRun) { Save-OwnThreads $own }

      # ---- 攒批 + 唤醒 ----
      if ($cands.Count -gt 0) {
        $pend = @(Load-Pending)
        foreach ($c in $cands) { $pend += $c }
        if (-not $dryRun) { Save-Pending $pend }
        Write-Log ('wake candidates +' + $cands.Count + ' -> pending ' + $pend.Count)
      }
      $wokeN = Invoke-PendingFlush
      $wokeSum += $wokeN

      $msg = Format-Events $events $ciCount $deleted.Count $wokeN
      if ($msg) { Send-Text $msg }

      if ($dryRun) {
        # 干跑只在内存里推进，不碰磁盘状态（否则真实监听器会以为这些事件已处理）。
        $prev = $remoteMap
      } else {
        # 未解决（fetch 失败）的 ref 不回写进状态：下一轮继续重试，而不是被当成已处理永久丢掉。
        foreach ($u in $unresolved) {
          if ($prev -and $prev.ContainsKey($u)) { $remoteMap[$u] = $prev[$u] }
          else { [void]$remoteMap.Remove($u) }
        }
        Save-State $remoteMap
        $prev = $remoteMap
      }
    }
    # 本轮没有新变更时，pending 仍需按冷却期冲刷（否则会一直攒着）
    $idle = Invoke-PendingFlush
    if ($idle -gt 0) { $wokeSum += $idle; Write-Log ('idle-pass flush: ' + $idle) }
  } catch {
    Write-Log ('pass error: ' + $_.Exception.Message + ' @ ' + $_.InvocationInfo.PositionMessage.Replace("`r`n", ' ') + ' | stack: ' + $_.ScriptStackTrace)
  }
  if ($Once) { break }
  Start-Sleep -Seconds $IntervalSec
}
Write-Log ('watcher exit (woke total ' + $wokeSum + ')')


