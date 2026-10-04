//! 回收「父进程已死」的 wassette MCP 宿主（Windows）。
//!
//! 背景（2026-09-30 owner 实报「桌上堆了 10 个 wassette」）：`wassette` 是 ABB 的沙箱工具
//! 宿主，由 agent 为**每个 session** 起一个 MCP 服务进程 —— 它是 agent 的**孙**进程。Windows
//! 上杀 agent 只作用于直接子进程（`buzz/acp.rs` 的 `kill_process_group` 在非 unix 是 stub），
//! 于是每来一次 agent 崩溃/被杀就留下一个常驻孤儿。
//!
//! 分工：**新起的**由 `agent_spawn::assign_kill_on_close_job` 的 job 覆盖（agent 死 → 整棵树死）；
//! **存量孤儿**（job 方案上线前的、以及任何漏网的）由本模块在 service 启动时扫一遍收掉。
//!
//! 判据（三条同时成立才杀；宁可漏杀，不可误杀）：
//! 1. 可执行文件名是 `wassette.exe`；
//! 2. 它的**镜像路径**就是本安装目录下的 `tools/bin/wassette.exe`（不是别人装的同名工具）；
//! 3. 父进程 pid 已不在进程快照里（= 真的成了孤儿）。
//!
//! 只按「名字 + 路径 + 父进程已死」判，**不猜命令行**：这是能安全自动杀的最小判据集。

use std::path::{Path, PathBuf};

/// 进程快照里的一条记录。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcEntry {
    pub pid: u32,
    pub parent_pid: u32,
    pub name: String,
}

/// 判定：这条记录该不该回收（纯函数 → 跨平台单测）。
pub fn should_reap(exe_name: &str, image_path: &str, parent_alive: bool, tools_bin: &Path) -> bool {
    if !exe_name.eq_ignore_ascii_case("wassette.exe") {
        return false;
    }
    if parent_alive {
        return false;
    }
    paths_eq(&tools_bin.join("wassette.exe"), Path::new(image_path))
}

/// Windows 路径比较：大小写不敏感 + 分隔符统一（`C:\A/b` 与 `c:\a\b` 视为同一条）。
fn paths_eq(a: &Path, b: &Path) -> bool {
    let norm = |p: &Path| p.to_string_lossy().replace('/', "\\").to_ascii_lowercase();
    norm(a) == norm(b)
}

/// 本安装目录下的 `tools/bin`（wassette 随包位置）；开发构建（target/）下通常不存在。
fn tools_bin_dir() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    Some(exe.parent()?.join("tools").join("bin"))
}

/// 扫一遍并回收孤儿 wassette，返回回收条数。
#[cfg(windows)]
pub fn reap_wassette_orphans() -> usize {
    let Some(tools_bin) = tools_bin_dir() else {
        return 0;
    };
    let snapshot = process_snapshot();
    let alive: std::collections::HashSet<u32> = snapshot.iter().map(|p| p.pid).collect();
    let mut reaped = 0;
    for p in &snapshot {
        if !p.name.eq_ignore_ascii_case("wassette.exe") {
            continue;
        }
        let parent_alive = p.parent_pid != 0 && alive.contains(&p.parent_pid);
        let Some(image) = image_path(p.pid) else {
            continue; // 读不到路径 = 无法证明是「我们的那个」→ 不杀（宁可漏）
        };
        if !should_reap(&p.name, &image, parent_alive, &tools_bin) {
            continue;
        }
        if terminate(p.pid) {
            reaped += 1;
            crate::log!(
                "[reap] 回收孤儿 wassette pid={}（父 pid={} 已退出）",
                p.pid,
                p.parent_pid
            );
        }
    }
    reaped
}

#[cfg(not(windows))]
pub fn reap_wassette_orphans() -> usize {
    0
}

/// 当前进程快照（pid / 父 pid / 可执行文件名）。
#[cfg(windows)]
pub fn process_snapshot() -> Vec<ProcEntry> {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
        TH32CS_SNAPPROCESS,
    };
    let mut out = Vec::new();
    // SAFETY: 只读快照；句柄在函数内关闭；结构体大小按 API 要求填好。
    unsafe {
        let Ok(snap) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else {
            return out;
        };
        let mut e = PROCESSENTRY32W {
            dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        if Process32FirstW(snap, &mut e).is_ok() {
            loop {
                let end = e
                    .szExeFile
                    .iter()
                    .position(|c| *c == 0)
                    .unwrap_or(e.szExeFile.len());
                out.push(ProcEntry {
                    pid: e.th32ProcessID,
                    parent_pid: e.th32ParentProcessID,
                    name: String::from_utf16_lossy(&e.szExeFile[..end]),
                });
                if Process32NextW(snap, &mut e).is_err() {
                    break;
                }
            }
        }
        let _ = CloseHandle(snap);
    }
    out
}

#[cfg(not(windows))]
pub fn process_snapshot() -> Vec<ProcEntry> {
    Vec::new()
}

/// 取进程的镜像路径（读不到返回 None）。
#[cfg(windows)]
pub(crate) fn image_path(pid: u32) -> Option<String> {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Threading::{
        OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_FORMAT,
        PROCESS_QUERY_LIMITED_INFORMATION,
    };
    // SAFETY: 句柄在函数内关闭；缓冲区长度由 API 回填（不足则失败 → 返回 None）。
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut buf = [0u16; 1024];
        let mut len = buf.len() as u32;
        let r = QueryFullProcessImageNameW(
            h,
            PROCESS_NAME_FORMAT(0),
            windows::core::PWSTR(buf.as_mut_ptr()),
            &mut len,
        );
        let _ = CloseHandle(h);
        r.ok()?;
        Some(String::from_utf16_lossy(&buf[..len as usize]))
    }
}

/// 强杀一个 pid（成功返回 true）。
#[cfg(windows)]
fn terminate(pid: u32) -> bool {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Threading::{OpenProcess, TerminateProcess, PROCESS_TERMINATE};
    // SAFETY: 句柄在函数内关闭。
    unsafe {
        let Ok(h) = OpenProcess(PROCESS_TERMINATE, false, pid) else {
            return false;
        };
        let ok = TerminateProcess(h, 1).is_ok();
        let _ = CloseHandle(h);
        ok
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 判据：只有「我们的 wassette + 父进程已死」才回收；三条任一不成立都不动。
    #[test]
    fn reaps_only_our_wassette_with_dead_parent() {
        let tools = Path::new(r"C:\Program Files\ABB\tools\bin");
        assert!(should_reap(
            "wassette.exe",
            r"C:\Program Files\ABB\tools\bin\wassette.exe",
            false,
            tools
        ));
        // 大小写/分隔符不敏感
        assert!(should_reap(
            "WASSETTE.EXE",
            "c:/program files/abb/tools/bin/wassette.exe",
            false,
            tools
        ));
        // 父进程还活着 → 不能杀（正在服务某个会话）
        assert!(!should_reap(
            "wassette.exe",
            r"C:\Program Files\ABB\tools\bin\wassette.exe",
            true,
            tools
        ));
        // 别人的同名工具（路径不同）→ 不能杀
        assert!(!should_reap(
            "wassette.exe",
            r"C:\other\wassette.exe",
            false,
            tools
        ));
        // 名字不对 → 不管
        assert!(!should_reap(
            "mcp-events.exe",
            r"C:\Program Files\ABB\tools\bin\wassette.exe",
            false,
            tools
        ));
    }

    /// Windows：快照必须真的能枚举（FFI 形状正确），且包含本进程。
    #[cfg(windows)]
    #[test]
    fn snapshot_contains_this_process() {
        let snap = process_snapshot();
        assert!(
            snap.len() > 5,
            "快照太小（{} 条），FFI 形状可能不对",
            snap.len()
        );
        assert!(
            snap.iter().any(|p| p.pid == std::process::id()),
            "快照应包含本进程 pid={}",
            std::process::id()
        );
    }
}
