# Windows 侧问题定位手册（卡死 / 任务栏多窗口 / 设置窗打不开）

> 背景：这些问题**只能在 Windows 上复现**，维护机是 macOS ⇒ 用本手册采集现场，把产物发回来。
> 采集脚本：`tools/windows/diag-abb.ps1`（本文件同目录的上级 `tools/`）。

## 一、先跑快照（30 秒）

```powershell
# 普通 PowerShell（dump 那步会弹 UAC 要管理员）
powershell -ExecutionPolicy Bypass -File tools\windows\diag-abb.ps1
```

产物在桌面 `abb-diag\<时间戳>\`：

| 文件 | 内容 | 能回答什么 |
|---|---|---|
| `snapshot.txt` | 进程表（`Responding` / 线程数 / `MainWindowTitle`）、`RUST_LOG`/`SLINT_*`/`ABB_*` 环境、dump 结果 | **`Responding=False` ⇒ UI 线程卡死**（不是变慢）；进程是否只剩 1 个 |
| `windows.txt` | 每个进程的**顶层窗口**：HWND / 可见 / 标题 / 类名 / **exStyle**（`WS_EX_TOOLWINDOW`、`WS_EX_APPWINDOW`）/ **owner** | 「任务栏有 3 个窗口」是哪三个；为什么会有任务栏按钮 |
| `logs\` | `%USERPROFILE%\.agent-bridge\logs\` | 崩/卡之前的最后动作 |
| `abb.dmp` | 全内存转储（需要管理员） | 用 Visual Studio / WinDbg 看**卡在哪个调用**（栈） |

## 二、卡死的两种抓法

1. **进程还在、UI 不响应**（最常见）⇒ 要 dump：
   - 脚本那步（`rundll32 comsvcs.dll, MiniDump <pid> <file> full`，**管理员**）；
   - 或手工：任务管理器 → 详细信息 → 右键 `agent-bridge.exe` → **创建转储文件**。
   - 再补一次「卡住时」的快照（`Responding` / 线程数）——脚本跑两次即可。
2. **进程直接没了** ⇒ `logs\` + 事件查看器：
   ```powershell
   Get-WinEvent -FilterHashtable @{LogName='Application'; StartTime=(Get-Date).AddMinutes(-15)} |
     Where-Object { $_.Message -match 'agent-bridge' } | Format-List TimeCreated,Id,Message
   ```

## 三、Windows 上的已知/待查清单（按怀疑度）

| 现象 | 怀疑 | 我们要的证据 |
|---|---|---|
| 整个界面卡死、设置窗打不开 | UI 线程上有**阻塞调用**（文件/网络/进程/等待）或重入死锁 | `abb.dmp` 的**主线程栈**（有它就能定位到函数） |
| 任务栏有 **3 个**窗口 | 气泡窗 / 对话窗 / 设置窗**都没设** `WS_EX_TOOLWINDOW`，且都没隐藏任务栏按钮 | `windows.txt` 里三行的 `exStyle` 与 owner |
| 气泡黑边（已修，待验） | Windows 透明合成不保证逐像素 alpha | 新版本是否还有；`SLINT_BACKEND=winit-software` 对照 |
| 气泡↔窗口切换卡顿 | 渲染后端 / 窗口显隐成本 | 正常时也卡？（`Responding=True` 但顿）⇒ 试 `SLINT_BACKEND=winit-software` 对照 |

## 四、复现时带上日志级别

```powershell
$env:RUST_LOG='info,agent_bridge=debug,abb_agent=debug'
$env:ABB_SYSCHAT_DEBUG='1'     # 系统对话的流式滚动诊断（写 ~/.agent-bridge/syschat/syschat-debug.log）
& "$env:LOCALAPPDATA\ABB\agent-bridge.exe"   # 或你实际的安装路径
```

> 说明：`ABB_SYSCHAT_DEBUG` 那套是 macOS 上定位白屏时加的，Windows 同样可用。

## 五、发回来什么

`abb-diag\<时间戳>\` **整个目录**（`abb.dmp` 可能较大，可单独传）。有 dump 我就能直接看
「卡在哪个函数」，没有 dump 只能猜。

## 六、已知的平台门控缺口（维护者自查用）

Windows 侧目前**没有**做这些（见 `ui.rs` 的 `#[cfg]`）：

- 任务栏：未设 `skip_taskbar` / `WS_EX_TOOLWINDOW`；
- 透明窗：未做逐像素 alpha 兜底（气泡黑边就是这么来的，已用「Windows 零边距」绕开）；
- ⌫ 删待发附件：Windows 上暂缺（winit 不在依赖里，要改走 Slint `key-pressed`）。
