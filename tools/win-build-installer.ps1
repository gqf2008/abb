# ============================================================================
# ABB Windows 安装包：**唯一正确入口**（仓库内，勿再手写临时脚本）
#
# 为什么存在这个文件（2026-10-05 owner：「不希望后面再出现这种很低级的问题」）：
#   我为发版手写的临时脚本里给 fork 构建加了 CARGO_TARGET_DIR，于是 cargo 把
#   buzz-agent.exe 编到了别的目录，而 ABB.iss 仍从 crates/buzz-agent/target/release
#   取件 —— 结果第一次打出的 2.23.103 安装包里是**上一版的 buzz-agent**（不含本次
#   修复）。CI 的 release.yml 没这个问题（它不设 CARGO_TARGET_DIR），是这个脚本的问题。
#
# 本脚本用两条断言把这一类错误钉死：
#   ① 根程序与 fork 各自构建到**自己的** target 目录（不设/不继承 CARGO_TARGET_DIR）；
#   ② 打包前断言两个 exe 都比各自的源码新（源码有改动却取到旧产物 ⇒ 直接失败）。
#
# 用法：pwsh -NoProfile -File tools\win-build-installer.ps1
#   可选 -SkipBuild 复用已有产物（仍会跑新鲜度断言）
# ============================================================================
param(
  [switch]$SkipBuild
)
$ErrorActionPreference = 'Stop'
$repo = Split-Path -Parent $PSScriptRoot
Set-Location $repo
$env:TMPDIR = if ($env:TMPDIR) { $env:TMPDIR } else { Join-Path $env:SystemDrive 'tmp' }

function Say($m) { Write-Host ('[' + (Get-Date -Format 'HH:mm:ss') + '] ' + $m) }
function Fail($m) { Write-Host ('✗ ' + $m) -ForegroundColor Red; exit 1 }

# 明确的解毒：本脚本**只允许**用各自默认 target 目录（见文件头 ①）。
if ($env:CARGO_TARGET_DIR) {
  Say ('清掉外部带入的 CARGO_TARGET_DIR=' + $env:CARGO_TARGET_DIR + '（否则 fork 产物会落错地方）')
  Remove-Item Env:CARGO_TARGET_DIR
}

$rootExe = Join-Path $repo 'target\release\agent-bridge.exe'
$forkExe = Join-Path $repo 'crates\buzz-agent\target\release\buzz-agent.exe'

if (-not $SkipBuild) {
  Say '构建根程序（release）…'
  & cargo build --release --locked --bin agent-bridge | Out-Host
  if ($LASTEXITCODE -ne 0) { Fail '根程序构建失败' }
  Remove-Item $rootExe -ErrorAction SilentlyContinue
  Copy-Item 'target\release\agent-bridge.exe' $rootExe -ErrorAction SilentlyContinue

  Say '构建 fork buzz-agent（release，用 fork 自己的 target 目录）…'
  & cargo build --release --manifest-path crates/buzz-agent/Cargo.toml | Out-Host
  if ($LASTEXITCODE -ne 0) { Fail 'fork 构建失败' }
}

# ② 新鲜度断言：产物必须比各自源码新（源码改了却取到旧产物 = 打错包）
function Assert-Fresh($exe, $srcDir, $label) {
  if (-not (Test-Path $exe)) { Fail ($label + ' 产物不存在：' + $exe) }
  $exeTime = (Get-Item $exe).LastWriteTimeUtc
  if (-not (Test-Path $srcDir)) { return }
  $newest = Get-ChildItem $srcDir -Recurse -File -Include *.rs,*.toml,*.slint -ErrorAction SilentlyContinue |
            Sort-Object LastWriteTimeUtc -Descending | Select-Object -First 1
  if ($newest -and $newest.LastWriteTimeUtc -gt $exeTime) {
    Fail ($label + ' 产物比源码旧：exe=' + $exeTime.ToString('MM-dd HH:mm:ss') +
          '  最新源码=' + $newest.FullName + ' @' + $newest.LastWriteTimeUtc.ToString('MM-dd HH:mm:ss'))
  }
  Say ($label + ' 新鲜度 ✓  ' + (Split-Path -Leaf $exe) + '  ' + [string][int]((Get-Item $exe).Length/1KB) + 'KB  ' + (Get-Item $exe).LastWriteTime.ToString('MM-dd HH:mm:ss'))
}
Assert-Fresh $rootExe (Join-Path $repo 'src') '根程序'
Assert-Fresh $forkExe (Join-Path $repo 'crates\buzz-agent\src') 'fork buzz-agent'

# 运行期同目录解析：确保 .iss 取到的就是这两份
$version = [regex]::Match((Get-Content (Join-Path $repo 'Cargo.toml') -Raw), 'version = "([^"]+)"').Groups[1].Value
$iscc = Join-Path $env:LOCALAPPDATA 'Programs\Inno Setup 6\ISCC.exe'
if (-not (Test-Path $iscc)) { Fail ('找不到 ISCC：' + $iscc) }
Say ('ISCC 打包 version=' + $version + ' …')
& $iscc "/DMyAppVersion=$version" 'app-assets\ABB.iss' | Out-Host
if ($LASTEXITCODE -ne 0) { Fail 'ISCC 编译失败' }

$setup = Get-ChildItem 'app-assets\Output\ABB-Setup-*.exe' | Sort-Object LastWriteTime -Descending | Select-Object -First 1
$hash = (Get-FileHash $setup.FullName -Algorithm SHA256).Hash
Set-Content -Path (Join-Path $setup.DirectoryName 'SHA256SUMS') -Value ($hash + '  ' + $setup.Name)
Say ('产物：' + $setup.FullName + '  ' + [string][int]($setup.Length/1MB) + 'MB')
Say ('sha256：' + $hash)
