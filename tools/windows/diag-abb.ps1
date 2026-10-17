# ABB Windows 诊断快照（卡死 / 任务栏多窗口 / 设置窗打不开 用）
# 用法（普通 PowerShell 即可；dump 那步需要**管理员**）：
#   powershell -ExecutionPolicy Bypass -File tools\windows\diag-abb.ps1
# 产物：桌面 abb-diag\<时间戳>\ 下：snapshot.txt / logs\ / windows.txt / abb.dmp(可选)
param([string]$OutRoot = "$env:USERPROFILE\Desktop\abb-diag")
$ErrorActionPreference = 'Continue'
$stamp = Get-Date -Format 'yyyyMMdd-HHmmss'
$Out = Join-Path $OutRoot $stamp
New-Item -ItemType Directory -Force -Path $Out | Out-Null
$snap = Join-Path $Out 'snapshot.txt'

function W($msg) { $msg | Tee-Object -FilePath $snap -Append | Out-Null }

W "== 时间 =="
W (Get-Date).ToString('s')
W ""
W "== 进程（Responding=False 就是 UI 线程卡住）=="
$procs = Get-Process agent-bridge -ErrorAction SilentlyContinue
if (-not $procs) { W "没找到 agent-bridge 进程（进程名可能不同？）" } else {
  $procs | ForEach-Object {
    W ("PID={0} Responding={1} Threads={2} Start={3}" -f $_.Id, $_.Responding, $_.Threads.Count, $_.StartTime)
    W ("  MainWindowTitle='{0}'" -f $_.MainWindowTitle)
  }
}
W ""
W "== 环境变量（RUST_LOG / SLINT_* / ABB_*）=="
Get-ChildItem env: | Where-Object { $_.Name -match '^(RUST_LOG|SLINT_|ABB_)' } |
  ForEach-Object { W ("{0}={1}" -f $_.Name, $_.Value) }
W ""

# ── 顶层窗口枚举（含 ExStyle：WS_EX_TOOLWINDOW=0x80 / WS_EX_APPWINDOW=0x40000）──────────
# 为什么需要：任务栏按钮由 WS_EX_TOOLWINDOW / WS_EX_APPWINDOW 与 owner 关系决定，
# 「任务栏有 3 个窗口」必须靠这个清单定位是哪三个、各自什么样式。
$winTxt = Join-Path $Out 'windows.txt'
try {
  Add-Type -TypeDefinition @"
using System;
using System.Text;
using System.Runtime.InteropServices;
using System.Collections.Generic;
public class AbbWin {
  delegate bool EnumProc(IntPtr h, IntPtr l);
  [DllImport("user32.dll")] static extern bool EnumWindows(EnumProc cb, IntPtr l);
  [DllImport("user32.dll")] static extern uint GetWindowThreadProcessId(IntPtr h, out uint pid);
  [DllImport("user32.dll")] static extern bool IsWindowVisible(IntPtr h);
  [DllImport("user32.dll", CharSet=CharSet.Unicode)] static extern int GetWindowTextW(IntPtr h, StringBuilder s, int n);
  [DllImport("user32.dll", CharSet=CharSet.Unicode)] static extern int GetClassNameW(IntPtr h, StringBuilder s, int n);
  [DllImport("user32.dll")] static extern IntPtr GetWindowLongPtrW(IntPtr h, int i);
  [DllImport("user32.dll")] static extern IntPtr GetWindow(IntPtr h, uint cmd);
  public static List<string> Dump(uint target) {
    var rows = new List<string>();
    EnumWindows((h, l) => {
      uint pid; GetWindowThreadProcessId(h, out pid);
      if (pid != target) return true;
      var t = new StringBuilder(512); GetWindowTextW(h, t, 512);
      var c = new StringBuilder(256); GetClassNameW(h, c, 256);
      long ex = GetWindowLongPtrW(h, -20).ToInt64();
      long st = GetWindowLongPtrW(h, -16).ToInt64();
      IntPtr owner = GetWindow(h, 4 /*GW_OWNER*/);
      rows.Add(string.Format(
        "HWND=0x{0:X8} visible={1} title='{2}' class='{3}'\n  exStyle=0x{4:X8} (TOOLWINDOW={5} APPWINDOW={6}) style=0x{7:X8} owner=0x{8:X8}",
        h.ToInt64(), IsWindowVisible(h), t, c, ex, (ex & 0x80) != 0, (ex & 0x40000) != 0, st, owner.ToInt64()));
      return true;
    }, IntPtr.Zero);
    return rows;
  }
}
"@ -ErrorAction Stop
  foreach ($p in $procs) {
    "== PID $($p.Id) 的顶层窗口 ==" | Out-File $winTxt -Append -Encoding utf8
    [AbbWin]::Dump([uint32]$p.Id) | Out-File $winTxt -Append -Encoding utf8
  }
  W "窗口清单已写入 windows.txt"
} catch {
  W ("窗口枚举失败（脚本继续）：" + $_.Exception.Message)
}

# ── 日志 ─────────────────────────────────────────────────────────────────────
$logDir = Join-Path $env:USERPROFILE '.agent-bridge\logs'
if (Test-Path $logDir) {
  Copy-Item (Join-Path $logDir '*') (Join-Path $Out 'logs') -Recurse -Force -ErrorAction SilentlyContinue
  W "已复制日志：$logDir"
} else { W "没找到日志目录：$logDir" }

# ── 进程 dump（卡死时最关键：UI 线程栈）─────────────────────────────────────────
$p = $procs | Select-Object -First 1
if ($p) {
  $dmp = Join-Path $Out 'abb.dmp'
  W ""
  W "== 生成 dump（需要管理员；失败就手工：任务管理器 → 右键 agent-bridge → 创建转储文件）=="
  try {
    Start-Process -FilePath "$env:WINDIR\System32\rundll32.exe" `
      -ArgumentList "comsvcs.dll, MiniDump $($p.Id) `"$dmp`" full" -Verb RunAs -Wait -ErrorAction Stop
    if (Test-Path $dmp) { W "dump 已生成：$dmp" } else { W "dump 未生成（可能被 UAC 拒绝）" }
  } catch { W ("dump 失败：" + $_.Exception.Message) }
}

W ""
W "== 完成：把整个目录打包发给维护者 =="
Write-Host "诊断快照在：$Out"
