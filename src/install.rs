//! 服务进程监控 —— 托盘 GUI 是 service 的看门（取代 launchd）。
//! 跨平台：用 std::process 起/杀子进程 + pid 文件追踪，不碰 launchctl/systemd/任务计划。
//! GUI 启动 service 子进程并把 pid 写进 logs/service.pid；status() 读 pid 文件 + 探测存活；
//! svc_stop() 终止该 pid 并清 pid 文件；svc_restart() = stop + start。
//! 日志：service 的 stdout/stderr 追加到 logs/bridge.out（对齐旧 launchd 行为）。

use anyhow::{Context, Result};
use std::path::PathBuf;
use std::process::{Command, Stdio};

pub struct ServiceStatus {
    pub running: bool,
    pub pid: u32,
}

fn pid_file() -> PathBuf {
    crate::bridge_dir().join("logs").join("service.pid")
}

fn logs_dir() -> PathBuf {
    crate::bridge_dir().join("logs")
}

/// 「用户意图让 service 跑」标记文件。存在=崩溃时看门应重拉；不存在=用户手动停，别拉。
fn desired_flag() -> PathBuf {
    logs_dir().join("service.desired")
}
pub fn set_desired(running: bool) {
    let f = desired_flag();
    std::fs::create_dir_all(logs_dir()).ok();
    if running {
        std::fs::write(&f, b"1").ok();
    } else {
        let _ = std::fs::remove_file(&f);
    }
}
pub fn is_desired() -> bool {
    desired_flag().exists()
}

/// 探测 pid 是否存活（跨平台）。注意：**zombie（已死但父进程未 wait）也算「不存活」**——
/// 否则 GUI 作为父进程不 wait 时，service 被杀变 zombie，kill(pid,0) 仍成功，看门会误判存活。
fn pid_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    #[cfg(unix)]
    {
        if unsafe { libc::kill(pid as i32, 0) } != 0 {
            return false; // 进程不存在
        }
        // 存在但可能是 zombie → 查 /proc（Linux）或用 sysctl（macOS）判 state
        !is_zombie(pid)
    }
    #[cfg(target_os = "windows")]
    {
        use std::ffi::c_void;
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn OpenProcess(
                dwDesiredAccess: u32,
                bInheritHandle: i32,
                dwProcessId: u32,
            ) -> *mut c_void;
            fn CloseHandle(hObject: *mut c_void) -> i32;
        }
        // 只查存活：PROCESS_QUERY_LIMITED_INFORMATION 足够；句柄有效 = 进程在跑。
        // 已退出/不存在的 pid → OpenProcess 返回 NULL（Windows 无 zombie 概念）。
        const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
        let h = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
        if h.is_null() {
            return false;
        }
        unsafe { CloseHandle(h) };
        true
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = pid;
        false
    }
}

#[cfg(target_os = "macos")]
fn is_zombie(pid: u32) -> bool {
    // ps -o stat= -p pid，含 'Z' 即僵尸
    std::process::Command::new("ps")
        .args(["-o", "stat=", "-p", &pid.to_string()])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.contains('Z'))
        .unwrap_or(false)
}
#[cfg(all(unix, not(target_os = "macos")))]
fn is_zombie(pid: u32) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/stat"))
        .ok()
        .and_then(|s| s.rsplit(')').next().map(|t| t.to_string()))
        .map(|rest| rest.split_whitespace().next() == Some("Z"))
        .unwrap_or(false)
}

/// 当前 service 状态：读 pid 文件 + 探测存活。
pub fn status() -> ServiceStatus {
    let pid = std::fs::read_to_string(pid_file())
        .ok()
        .and_then(|s| s.trim().parse::<u32>().ok())
        .unwrap_or(0);
    ServiceStatus {
        running: pid_alive(pid),
        pid,
    }
}

/// 启动 service 子进程（若已在跑则先停），并起线程收割（防僵尸）。
/// 会顺带把「意图=运行」标记置上（看门据此在崩溃时重拉）。
pub fn svc_start() -> Result<()> {
    let st = status();
    if st.running {
        svc_stop();
    }
    set_desired(true);
    let exe = crate::platform::current_exe()?;
    let logs = logs_dir();
    std::fs::create_dir_all(&logs).ok();
    // service 输出追加到 logs/bridge.out（对齐旧 launchd StandardOutPath）
    let out = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(logs.join("bridge.out"))
        .context("打开日志文件失败")?;
    let out_err = out.try_clone().context("克隆日志句柄失败")?;

    let mut child = Command::new(exe)
        .arg("--service")
        // 显式设 cwd=home：launchd 默认 cwd 是 /，GUI spawn 会继承 GUI 的 cwd（不确定）。
        // 设成 home 保证 service 内相对路径（如 ~/.local symlink）正确解析，agent 子进程才有正确 PATH。
        .current_dir(dirs::home_dir().unwrap_or_else(|| PathBuf::from("/")))
        .stdin(Stdio::null())
        .stdout(Stdio::from(out))
        .stderr(Stdio::from(out_err))
        .spawn()
        .context("启动 service 子进程失败")?;
    let pid = child.id();
    std::fs::write(pid_file(), pid.to_string()).ok();
    crate::log!("[watchdog] 已启动 service pid={pid}");
    // 起个线程收割子进程：wait() 阻塞到退出并回收，避免僵尸进程累积
    // （GUI 是 service 的父进程，不 wait 就会留 <defunct>，导致 pid_alive 误判）。
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

/// 停止 service（按 pid 文件终止 + 清文件 + 清「意图」标记——用户手动停，看门不再重拉）。
pub fn svc_stop() {
    set_desired(false);
    let st = status();
    if st.running && st.pid != 0 {
        terminate(st.pid);
        crate::log!("[watchdog] 已停止 service pid={}", st.pid);
    }
    let _ = std::fs::remove_file(pid_file());
}

pub fn svc_restart() {
    svc_stop();
    // 稍等子进程退出再拉
    std::thread::sleep(std::time::Duration::from_millis(300));
    if let Err(e) = svc_start() {
        crate::log!("[watchdog] 重启失败: {e:#}");
    }
}

/// 跨平台终止进程。
fn terminate(pid: u32) {
    #[cfg(unix)]
    unsafe {
        libc::kill(pid as i32, libc::SIGTERM);
    }
    #[cfg(not(unix))]
    {
        // Windows：taskkill（stub）
        let _ = Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/F"])
            .spawn();
    }
}
