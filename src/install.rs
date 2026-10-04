//! 服务进程监控 —— 托盘 GUI 是 service 的看门（取代 launchd）。
//! 跨平台：用 std::process 起/杀子进程 + pid 文件追踪，不碰 launchctl/systemd/任务计划。
//! GUI 启动 service 子进程并把 pid 写进 logs/service.pid；status() 读 pid 文件 + 探测存活；
//! svc_stop() 终止该 pid 并清 pid 文件；svc_restart() = stop + start。
//! 日志：service 的 stdout/stderr 追加到 logs/bridge.out（对齐旧 launchd 行为）。

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
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

/// 「用户意图让 service 跑」标记文件名。存在=崩溃时看门应重拉；不存在=用户手动停，别拉。
///
/// 名字抽成常量而非 `desired_flag()` 单点：`set_desired_at` / `is_desired_at` 是目录
/// 可注入的入口（单测传临时目录），路径拼接由它们自己做，少一层「只有真实目录能用」的包装。
const DESIRED_FILE: &str = "service.desired";

pub fn set_desired(running: bool) {
    set_desired_at(&logs_dir(), running);
}
pub fn is_desired() -> bool {
    is_desired_at(&logs_dir())
}

/// [`set_desired`] 的目录可注入版（单测不碰真实 `~/.agent-bridge`）。
fn set_desired_at(logs: &Path, running: bool) {
    let f = logs.join(DESIRED_FILE);
    std::fs::create_dir_all(logs).ok();
    if running {
        std::fs::write(&f, b"1").ok();
    } else {
        let _ = std::fs::remove_file(&f);
    }
}

/// [`is_desired`] 的目录可注入版。
fn is_desired_at(logs: &Path) -> bool {
    logs.join(DESIRED_FILE).exists()
}

/// 停 service 时怎么处置「意图=运行」标记——抽成纯函数是为了让升级重启的语义被单测钉住。
///
/// `keep_desired=true` 只用于**升级重启**：旧实例必须先把 service 停掉（否则安装器
/// 换 exe 时文件被占用），但这次停不是「用户不要它跑了」。若按手动停的语义清掉标记，
/// 新实例起来后看门狗（`ui.rs` 每 2s `is_desired() && !running → svc_start()`）不会把
/// bridge 拉回来，表现就是「托盘起来了、bridge 却不在」。
fn apply_stop_intent(logs: &Path, keep_desired: bool) {
    if !keep_desired {
        set_desired_at(logs, false);
    }
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
            let err = std::io::Error::last_os_error();
            // EPERM = 进程**存在**但我们无权给它发信号 → 必须算存活。
            // 旧实现把所有非 0 返回都当「不存在」，于是「以更高权限运行的 bridge」在状态里
            // 显示成「未运行」：托盘给的是「启动」而不是「停止」，用户点了自然没反应
            //（与 2026-09-29 owner 实报同型）。ESRCH 才是真不存在。
            if err.raw_os_error() == Some(libc::EPERM) {
                return true;
            }
            return false; // ESRCH / 其它
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
    // 托管形态（macOS launchd / Windows 计划任务）：起停交给 supervisor，托盘不再自己
    // spawn —— 两边同时管会抢单实例锁，表现为反复拉起-退出（见 platform::service_supervised）。
    if crate::platform::service_supervised() {
        set_desired(true);
        // 「启动」不等于「重启」：托管形态下只确保它在跑 —— job 没加载才 bootstrap、
        // **已加载但没在跑要 kickstart**（不带 -k；2026-09-30「升级后 mac 起不来」就是
        // 只判「已加载」不管「在不在跑」造成的静止态），绝不能用 kickstart -k —— 那会杀掉
        // 正在跑的实例，看门狗每 2s 判活失败就再来一次，形成「拉起即被杀」的抖动（评审 P5）。
        return crate::platform::start_service_supervised();
    }
    let st = status();
    if st.running {
        svc_stop()?;
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

/// 「启动」的用户可见版本：起完还要**确认它真的在跑**（起完即退 = 单实例锁被占，
/// 静默返回正是「点了启动/重启没反应」的来源）。只给后台线程用（最多阻塞 `timeout`）。
pub fn svc_start_verified(timeout: std::time::Duration) -> Result<()> {
    svc_start()?;
    if !wait_service_up(timeout) {
        anyhow::bail!(
            "已发起启动，但 {timeout:?} 内没看到 service 跑起来（多半已有实例占着单实例锁；查 logs/bridge.out）"
        );
    }
    Ok(())
}

/// 停止 service（按 pid 文件终止 + 清文件 + 清「意图」标记——用户手动停，看门不再重拉）。
///
/// 失败**必须**如实返回（2026-09-29 owner 实报：Windows 上「停不了也没任何提示」）。
pub fn svc_stop() -> Result<()> {
    svc_stop_impl(false)
}

/// 升级重启前停 service：**保留**「意图=运行」标记，让重启后的新实例把 bridge 拉回来。
///
/// 与 [`svc_stop`] 的唯一差别是不动 desired 标记（该清 pid 文件仍要清，否则新实例
/// 会把一个已退出的 pid 当真值）。语义边界：升级前用户本来就是停掉状态（标记不存在）
/// 时，这里同样保持不存在——升级不替用户改主意，只让「本来在跑」的意图活过这次重启。
pub fn svc_stop_keep_desired() -> Result<()> {
    svc_stop_impl(true)
}

/// [`svc_stop`] / [`svc_stop_keep_desired`] 的共同实现。
///
/// 顺序：**先确认真的停掉，再改意图标记与 pid 文件**。反过来的话，kill 失败时盘上会留下
/// 「标记=已停、pid 文件已删、进程还在跑」的错位状态，看门狗也不会把它拉回来 —— 用户看到
/// 的就是「点了停止，什么都没发生，也没有任何提示」。
fn svc_stop_impl(keep_desired: bool) -> Result<()> {
    let st = status();
    // 诊断先行：下次再出现「点了没反应」，日志里至少能看出当时判成了什么形态。
    crate::log!(
        "[watchdog] 停止 service 请求：keep_desired={keep_desired} supervised={} running={} pid={}",
        crate::platform::service_supervised(),
        st.running,
        st.pid
    );
    if st.running && st.pid != 0 {
        terminate(st.pid)?;
        if keep_desired {
            crate::log!(
                "[watchdog] 已停止 service pid={}（升级重启：保留「意图=运行」，新实例看门狗会拉回）",
                st.pid
            );
        } else {
            crate::log!("[watchdog] 已停止 service pid={}", st.pid);
        }
    }
    apply_stop_intent(&logs_dir(), keep_desired);
    let _ = std::fs::remove_file(pid_file());
    Ok(())
}

/// 重启 service（**带验证**）：起没起来要确认，否则「重启不了」同样是静默失败。
///
/// 只从后台线程调用（GUI 的 UiCmd 循环 / 保存后的热重启）——内含最长 ~10s 的轮询，
/// 不能放到 UI 线程的 2s 看门狗里。
pub fn svc_restart() -> Result<()> {
    if crate::platform::service_supervised() {
        // 托管形态：交给 supervisor 重启（不经过「先杀后起」的托盘子进程路径）。
        set_desired(true);
        // 旧 pid：判据必须落在「**换了一个实例**」上 —— 只看「有进程在跑」会把
        // 「/end 没生效 + /run 被 IgnoreNew 拒」的空转报成重启成功（2026-09-29 复评 F1）。
        let prev = status().pid;
        crate::log!("[watchdog] 托管重启：schtasks /end + /run（旧 pid={prev}）");
        crate::platform::restart_service_supervised()
            .context("托管重启失败（计划任务 /end 或 /run）")?;
        if !wait_service_restarted(prev, std::time::Duration::from_secs(10)) {
            anyhow::bail!(
                "已让计划任务 /end + /run，但 10s 内没看到**新**实例（旧 pid={prev} 仍在或没有新进程）；这通常说明 /end 没生效 —— 查任务计划程序里的 ABB-Bridge 与 logs/bridge.out"
            );
        }
        return Ok(());
    }
    let prev = status().pid;
    svc_stop()?;
    // 稍等子进程退出再拉
    std::thread::sleep(std::time::Duration::from_millis(300));
    svc_start()?;
    if !wait_service_restarted(prev, std::time::Duration::from_secs(5)) {
        anyhow::bail!("重启后 5s 内没看到新实例（旧 pid={prev}；查 logs/bridge.out）");
    }
    Ok(())
}

/// 停止服务 —— **需要授权**（本批 owner 要求：托盘退出不影响服务，停止必须授权）。
///
/// 平台侧各自取授权：macOS = 系统密码框后 `launchctl bootout`；Windows = UAC 提权 helper
/// 执行计划任务 `/end` + 杀 pid（B1b 接线）。
///
/// 顺序要紧：**授权通过、平台侧真停了之后**才清「意图=运行」标记与 pid 文件。用户取消授权
/// 时必须什么都不变——否则看门狗会把「取消授权」误当成「用户想停」而不再拉回服务。
pub fn svc_stop_authorized() -> Result<()> {
    // 平台侧拿授权并停 supervisor（macOS=bootout / Windows=schtasks /end，均经 UAC/密码框）。
    let platform = crate::platform::stop_service_authorized();
    // 评审 P4：**非托管形态**（例如刚关掉自启、bridge 又被看门狗拉成托盘子进程）时，平台侧
    // 没有 supervisor 对象可停 ⇒ 必须把 pid 文件里那个进程也停掉，否则「用户被告知已停止、
    // 服务还在跑」。只在授权**成功**后才动它：取消授权时绝不能用 pid 兜底绕开密码门。
    let st = status();
    // 授权已通过时，把非托管形态那个进程也真停掉 —— 但**终止结果必须检查**：
    // 高完整性进程普通权限杀不掉（Access Denied），旧实现静默吞掉，用户端就成了
    // 「点了停止什么都没发生、也没有任何提示」（2026-09-29 owner 实报）。
    let killed = if platform.is_ok() && st.running && st.pid != 0 {
        match terminate(st.pid) {
            Ok(()) => {
                crate::log!(
                    "[watchdog] 授权停止：已终止 pid={}（非托管形态兜底）",
                    st.pid
                );
                Ok(())
            }
            Err(e) => Err(e.context(format!(
                "授权已通过，但 pid={} 停不掉（非托管形态兜底）",
                st.pid
            ))),
        }
    } else {
        Ok(())
    };
    if let Err(e) = platform {
        // 授权被取消 / 平台侧失败：只有确认进程真的没了才算停成功，否则如实报错。
        let still = status();
        if still.running {
            return Err(e);
        }
        crate::log!("[watchdog] 平台侧返回失败，但 service 已不在运行：按成功处理（{e:#}）");
    }
    killed?;
    apply_stop_intent(&logs_dir(), false);
    let _ = std::fs::remove_file(pid_file());
    crate::log!("[watchdog] 已按授权停止 service（意图标记已清）");
    Ok(())
}

/// bridge（`--service`）启动时自己登记 pid。
///
/// 本批起 bridge 由 launchd/计划任务托管，托盘不再持有它的 pid —— `status()` 与看门狗只能
/// 靠这个文件判活（否则会误判「没在跑」并反复 spawn 出抢锁即退的短命进程）。
pub fn write_own_pid_file() {
    let _ = std::fs::create_dir_all(logs_dir());
    let _ = std::fs::write(pid_file(), std::process::id().to_string());
}

/// 有界等待 pid 消失（terminate 之后的**确认**）。
///
/// 为什么必须确认：Windows 上 `taskkill` 对高完整性进程返回 Access Denied，而旧实现
/// `.spawn()` 之后既不 wait 也不看退出码 —— 于是「停止」静默失败（owner 2026-09-29 实报）。
fn wait_pid_gone(pid: u32, timeout: std::time::Duration) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if !pid_alive(pid) {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return !pid_alive(pid);
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

/// 有界等待 service 起来（状态以 pid 文件 + 存活为准；托管形态下由 service 自己登记 pid）。
pub fn wait_service_up(timeout: std::time::Duration) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if status().running {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

/// 重启成功的判据：**换了一个实例**（有实例在跑，且不是重启前那个 pid）。
///
/// 只看 status().running 会把「/end 没生效 + /run 被 IgnoreNew 拒」这种空转判成功——
/// 用户点「重启」看到的是一句成功，而进程还是原来那个（2026-09-29 复评 F1）。纯函数，便于单测。
fn is_restarted(prev_pid: u32, st: &ServiceStatus) -> bool {
    st.running && st.pid != 0 && st.pid != prev_pid
}

/// 有界等待「换了一个实例」（判据见 is_restarted）。
fn wait_service_restarted(prev_pid: u32, timeout: std::time::Duration) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if is_restarted(prev_pid, &status()) {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

/// 跨平台终止进程（**检查型**：真的停掉才算 Ok）。
fn terminate(pid: u32) -> Result<()> {
    terminate_with_grace(pid, std::time::Duration::from_secs(3))
}

/// terminate 的实现（grace 可注入，单测用短宽限验证升级链路不拖慢测试）。
///
/// 返回值语义：`Ok(())` = 调用返回时该 pid **已不在**（本来就不在也算）；`Err` = 还在跑，
/// 调用方必须把原因交给用户（权限不足最常见），绝不能静默吞掉。
pub(crate) fn terminate_with_grace(pid: u32, grace: std::time::Duration) -> Result<()> {
    if pid == 0 || !pid_alive(pid) {
        return Ok(());
    }
    #[cfg(unix)]
    {
        if unsafe { libc::kill(pid as i32, libc::SIGTERM) } != 0 {
            let err = std::io::Error::last_os_error();
            // ESRCH = 我们判活之后它自己退了：算成功。
            if err.raw_os_error() == Some(libc::ESRCH) {
                return Ok(());
            }
            anyhow::bail!(
                "SIGTERM 失败（{err}）：pid={pid} 以更高权限运行，普通权限停不掉（需要管理员授权）"
            );
        }
        // #179：优雅关闭可能卡死（如 WS 发送挂起）→ SIGTERM 杀不死、锁占着 → 新实例
        // 无限拉起失败。SIGTERM 后 grace 内没退 → SIGKILL 兜底。
        if wait_pid_gone(pid, grace) {
            return Ok(());
        }
        // #183：SIGKILL 前复查存活（wait_pid_gone 刚刚确认过还在），避免对已退出的 pid 盲发
        // （pid 复用后误杀无辜进程）。
        unsafe {
            libc::kill(pid as i32, libc::SIGKILL);
        }
        if wait_pid_gone(pid, std::time::Duration::from_secs(1)) {
            Ok(())
        } else {
            anyhow::bail!("SIGKILL 之后 pid={pid} 仍在运行（不可能？请查进程状态）")
        }
    }
    #[cfg(not(unix))]
    {
        // Windows：taskkill /F 即强杀（无宽限期语义），grace 仅 unix 分支消费
        let _ = grace;
        // CREATE_NO_WINDOW 避免停服务时闪控制台。**必须 `.output()` 收集输出并校验退出码**：
        // 旧实现火并忘，高完整性进程被拒（Access Denied）时用户端完全没有反馈。
        let out = crate::spawn::command("taskkill")
            .args(["/PID", &pid.to_string(), "/F"])
            .output()
            .context("执行 taskkill 失败")?;
        if !out.status.success() {
            let err = String::from_utf8_lossy(&out.stderr);
            let err = err.trim();
            anyhow::bail!(
                "taskkill 失败（{}）：{}；若为「拒绝访问」说明该进程以更高权限运行，普通权限停不掉（需要管理员授权）",
                out.status,
                if err.is_empty() { "无输出" } else { err }
            );
        }
        if wait_pid_gone(pid, std::time::Duration::from_secs(2)) {
            Ok(())
        } else {
            anyhow::bail!("taskkill 报成功但 pid={pid} 仍在运行")
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    /// #183 回归护栏：pid_alive 必须把 zombie（已死未 reap）判为不存活——
    /// SIGKILL 兜底靠它避免对已退出 pid 盲发（pid 复用后误杀无辜进程）。
    #[test]
    fn pid_alive_detects_zombie_as_dead() {
        let child = std::process::Command::new("sh")
            .args(["-c", "exit 0"])
            .spawn()
            .unwrap();
        let pid = child.id();
        std::mem::forget(child); // 不 reap：退出后保持 zombie 供 pid_alive 判定
                                 // 等它退出变 zombie（kill(pid,0) 对 zombie 成功，is_zombie 判死）
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while pid_alive(pid) && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(
            !pid_alive(pid),
            "zombie（已死未 reap）必须判为不存活，否则 SIGKILL 兜底会误发"
        );
    }

    /// 活进程必须判为存活（升级兜底的前提：真卡死的 service 才能被 SIGKILL）。
    #[test]
    fn pid_alive_detects_live_process() {
        let mut child = std::process::Command::new("sh")
            .args(["-c", "sleep 5"])
            .spawn()
            .unwrap();
        assert!(pid_alive(child.id()), "活进程必须判为存活");
        let _ = child.kill();
        let _ = child.wait();
    }

    /// #179/#183 链路端到端：忽略 SIGTERM 的进程必须在宽限后被杀（SIGKILL 升级有效）。
    #[test]
    fn terminate_escalates_to_sigkill_when_graceful_exit_stalls() {
        let mut child = std::process::Command::new("sh")
            .args(["-c", "trap '' TERM; exec sleep 30"])
            .spawn()
            .unwrap();
        let pid = child.id();
        let res = terminate_with_grace(pid, std::time::Duration::from_millis(300));
        let t = std::time::Instant::now();
        let _ = child.wait(); // 阻塞到被杀
        assert!(
            t.elapsed() < std::time::Duration::from_secs(3),
            "忽略 TERM 的进程必须在宽限+SIGKILL 内被杀，实际 {:?}",
            t.elapsed()
        );
        assert!(res.is_ok(), "进程确实被杀掉时必须返回 Ok：{res:?}");
    }

    /// 已经不在的 pid 不算失败（调用方拿到 Ok 就不该报错打扰用户）。
    #[test]
    fn terminate_checked_is_ok_for_dead_pid() {
        let child = std::process::Command::new("sh")
            .args(["-c", "exit 0"])
            .spawn()
            .unwrap();
        let mut child = child;
        let pid = child.id();
        let _ = child.wait();
        assert!(!pid_alive(pid), "前置：进程已退出");
        assert!(
            terminate_with_grace(pid, std::time::Duration::from_millis(100)).is_ok(),
            "已退出的 pid 必须判 Ok（否则「停止」会对着一个不存在的进程报错）"
        );
    }

    /// 停不掉的进程必须**报错**，不能像旧实现那样静默返回。
    ///
    /// 用 `launchd`（pid 1）当靶子：普通用户 `kill(1)` 会 EPERM，正好等价于 Windows 上
    /// 「taskkill 拒绝访问」这一类形态。**以 root 运行时跳过**（那时真的会杀掉 launchd）。
    #[test]
    fn terminate_checked_reports_failure_when_permission_denied() {
        if unsafe { libc::geteuid() } == 0 {
            eprintln!("skip：以 root 运行，kill(1) 真的会生效，不能拿它当靶子");
            return;
        }
        let res = terminate_with_grace(1, std::time::Duration::from_millis(50));
        let err = res.expect_err("无权限终止的 pid 必须返回 Err（旧实现的静默就在这里）");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("权限") || msg.contains("SIGTERM"),
            "错误信息要能让人知道是权限问题：{msg}"
        );
    }
}

/// 升级重启的「意图」语义：平台无关的纯文件逻辑，单独一组用例（目录注入 → 不碰真实
/// `~/.agent-bridge`，也就不会在开发机裸跑 `cargo test` 时误伤正在跑的 service 进程——
/// 这组用例刻意不调 `svc_stop_impl` 的 terminate 分支，只钉标记处置）。
#[cfg(test)]
mod stop_intent_tests {
    use super::*;

    fn tmpdir(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("abb-stop-intent-{tag}-{}", uuid::Uuid::new_v4()))
    }

    /// 升级重启路径必须保留「意图=运行」：清了它，新实例的看门狗就不会把 bridge 拉回来
    /// （2026-09-28 owner 报的「升级完成后无法自动运行」里的一环）。
    #[test]
    fn upgrade_stop_keeps_desired_flag() {
        let dir = tmpdir("keep");
        set_desired_at(&dir, true);
        assert!(is_desired_at(&dir), "前置：标记已落盘");
        apply_stop_intent(&dir, true);
        assert!(
            is_desired_at(&dir),
            "升级重启停 service 不得清「意图=运行」，否则新实例不会自动把 bridge 拉回来"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 用户手动停必须清标记（看门不再重拉），并且升级路径不能把「升级前就是停的」改成开的。
    #[test]
    fn manual_stop_clears_and_upgrade_stop_does_not_invent_intent() {
        let dir = tmpdir("manual");
        set_desired_at(&dir, true);
        apply_stop_intent(&dir, false);
        assert!(!is_desired_at(&dir), "手动停必须清标记");
        // 标记本来就不存在（用户升级前没在跑）：升级路径保持不存在，不替用户改主意
        apply_stop_intent(&dir, true);
        assert!(!is_desired_at(&dir), "升级不得凭空造出「意图=运行」");
        let _ = std::fs::remove_dir_all(&dir);
    }
}

/// 升级**调用点**守卫（`ui.rs`）。
///
/// 为什么必须是它：本文件的 `stop_intent_tests` 只钉「意图标记怎么处置」，覆盖不到调用点接线。
/// 独立评审的反证实测过——把 `ui.rs` 升级成功分支改回 `svc_stop()`，整仓 bin 套件仍
/// 873 passed / 0 failed 全绿，也就是「升级把用户的运行意图抹掉」这个缺陷可以静默回归。
///
/// 手段按本仓既有惯例（`include_str!` 源码断言，见 `agent.rs`/`harness.rs`/`deps.rs`）：
/// 钉「升级成功分支里恰好一处 `svc_stop_keep_desired()`，且紧跟在升级完成日志之后」。
#[cfg(test)]
mod upgrade_call_site_tests {
    /// 升级分支的日志串（`ui.rs` 里「已启动安装包…」那句）。
    ///
    /// 2026-10-01 改文案：原文案写「安装完成」，但更新器是 `cmd /c start` 派生的、**拿不到**
    /// 安装器退出码，安装器随即回滚也照样先打这句 ⇒ 用户以为装好了其实没有（owner 实报）。
    const UPGRADE_LOG: &str = "已启动安装包";
    const KEEP_DESIRED_CALL: &str = "svc_stop_keep_desired()";
    /// 日志与调用之间的允许跨度（**行**，不是字节——那段之间是中文注释，按字节算会随字数漂移）。
    /// 两者目前同在一个 `slint::invoke_from_event_loop` 闭包里、相隔 7 行；留一倍余量。
    const MAX_LINE_GAP: usize = 15;

    #[test]
    fn upgrade_path_uses_svc_stop_keep_desired() {
        let ui = include_str!("ui.rs");
        let calls = ui.matches(KEEP_DESIRED_CALL).count();
        let why = "这是复核 reviewer-38 F1 的守卫：升级成功分支必须用「保留意图」的停服务，改动升级链时请连同本条一起改，别把它删掉";
        assert_eq!(calls, 1, "{why}（现在 {calls} 处 {KEEP_DESIRED_CALL}）");
        let log_at = ui
            .find(UPGRADE_LOG)
            .expect("ui.rs 里应有升级成功的日志行（改文案要同步这里）");
        let call_at = ui.find(KEEP_DESIRED_CALL).expect("上面已断言存在");
        let log_line = ui[..log_at].matches('\n').count();
        let call_line = ui[..call_at].matches('\n').count();
        assert!(
            call_line > log_line,
            "{KEEP_DESIRED_CALL} 必须出现在升级成功分支内（当前在日志之前，可能匹配到了别处）"
        );
        assert!(
            call_line - log_line <= MAX_LINE_GAP,
            "日志与 {KEEP_DESIRED_CALL} 应相邻（相隔 {} 行 > {MAX_LINE_GAP}），否则这条守卫可能盯错了地方；\
             若只是中间的注释变长了，把这个常量一起调大并在此说明",
            call_line - log_line
        );
        // 反向护栏（2026-09-28 语义变更后重写）：托盘侧的「停止」**必须**走授权路径
        // `svc_stop_authorized()`，**不得**出现直接的无授权 `svc_stop();`
        // （owner 要求：停止服务需要密码授权；这条守卫就是防止哪天有人把它改回去）。
        assert!(
            ui.matches("svc_stop_authorized()").count() >= 1,
            "托盘「停止」必须走 install::svc_stop_authorized()（需要授权）"
        );
        assert_eq!(
            ui.matches("install::svc_stop();").count(),
            0,
            "托盘侧不得再直接调无授权的 install::svc_stop()——那等于绕过密码授权（本守卫的立身之本）"
        );
    }

    /// 升级链路的日志必须走 `crate::updater::log_update`（stdout + `logs/update.log` 双写）。
    ///
    /// 为什么：GUI 进程的 stdout 在 Windows（无控制台）与 macOS（`open` 拉起时 0/1/2 指
    /// /dev/null）都会蒸发，而 macOS 侧 `macos_install` 的多个 `bail!`（hdiutil attach /
    /// ditto / 重启辅助脚本）**只有** `ui.rs` 那条「安装失败」日志留下原因文本
    /// （复核 reviewer-41 的问题 1）。本条把它钉成红/绿。
    #[test]
    fn upgrade_path_logs_go_through_update_log() {
        let ui = include_str!("ui.rs");
        // `[update]` 前缀的日志不允许再用裸宏（`crate::log!`）——那在 GUI 下等于不留痕。
        let bare = ui.matches("crate::log!(\"[update]").count();
        assert_eq!(
            bare, 0,
            "ui.rs 里 {bare} 处 `[update]` 日志仍用裸 crate::log!（GUI 下会蒸发）；请改走 crate::updater::log_update"
        );
        // 正向：四条关键日志（发现新版本 / 检查失败 / 安装完成 / 安装失败）都在。
        let via = ui.matches("crate::updater::log_update(").count();
        assert!(
            via >= 4,
            "ui.rs 里应至少有 4 处升级日志走 crate::updater::log_update（实际 {via}）"
        );
    }

    /// 看门狗那一拍（跑在 **UI 线程**）不得**同步**调 `svc_start`：托管形态（macOS launchd /
    /// Windows 计划任务）下它会 spawn 外部命令，而 2026-09-30 实测存在长时间不返回的形态
    /// （`EX_CONFIG` 的 job 上 `launchctl kickstart` >20s 挂住）——同步做就会「bridge 起不来」
    /// 连带「托盘冻死」（评审 R1 的 B1）。必须放后台线程。
    ///
    /// 判别力（本守卫的边界，如实标注）：它能拦住「退回旧写法」与「spawn 写在调用之后」两种
    /// 退化，属于子串+顺序断言——保留 `std::thread::spawn(` 却把调用挪到闭包外的写法它拦不住
    /// （同族教训见 `LESSON_子串式守卫会被同前缀常量顶住须逐项做阳性对照.md`）。
    #[test]
    fn watchdog_tick_does_not_call_svc_start_on_the_ui_thread() {
        // 必须过 `src_lf` 且按字符取窗口：Windows 检出是 CRLF，裸字节切片会切在字符中间 panic。
        let ui = crate::platform::src_lf(include_str!("ui.rs"));
        let at = ui
            .find("[watchdog] service 意外退出，自动重拉")
            .expect("看门狗日志行（改文案请同步这里）");
        let window: String = ui[at..].chars().take(900).collect();
        let call = window
            .find("install::svc_start()")
            .expect("看门狗应调 svc_start 把它拉回来");
        let spawn = window
            .find("std::thread::spawn(")
            .expect("托管形态的拉起必须放后台线程（不得阻塞 UI 线程）");
        assert!(
            spawn < call,
            "`install::svc_start()` 必须在 `std::thread::spawn(` 之后（即包在后台线程里）"
        );
        assert!(
            !window.contains("let _ = install::svc_start();"),
            "不得退回「同步调用 + 丢弃返回值」的旧写法（既阻塞 UI 又不留失败原因）"
        );
    }
}

/// 「服务动作失败不得静默」的源码守卫（2026-09-29 owner 实报：Windows 上停不了/重启不了也没提示）。
///
/// 为什么用源码守卫：两条缺陷都在**异侧/异路径**（Windows 的 taskkill 分支、GUI 的重启命令臂），
/// 本机 macOS 根本执行不到 —— 但它们的**形状**是文本事实，钉住形状就能防回归。
#[cfg(test)]
mod svc_action_guards {
    /// `terminate_with_grace` 的 Windows 分支必须收集输出并校验退出码。
    #[test]
    fn windows_terminate_checks_taskkill_result() {
        // **必须过 `src_lf`**：本仓没有 `.gitattributes` 强制 LF，Windows 检出（windows-latest +
        // core.autocrlf）是 CRLF；而下面要按偏移取窗口。复评 R29 的 P1 实测：不过这一层时裸字节
        // 切片会切在多字节字符中间 ⇒ windows 的 `cargo test` 直接 panic（本机 macOS 全绿）。
        let src = crate::platform::src_lf(include_str!("install.rs"));
        // 只看 taskkill **那一条语句**的调用链（到第一个 `;` 为止）：切到「下一个 fn」会把
        // 后面的测试模块一起圈进来（`.spawn()` 是测试在起进程），属于守卫自匹配。
        let idx = src.find("\"taskkill\"").expect("应调用 taskkill");
        let stmt = src[idx..].split(';').next().unwrap_or("");
        assert!(
            stmt.contains(".output()"),
            "taskkill 必须用 .output() 收集输出（火并忘的 .spawn() 会让 Access Denied 静默）：{stmt}"
        );
        assert!(
            !stmt.contains(".spawn()"),
            "不得再对 taskkill 火并忘（.spawn()）：2026-09-29 owner 实报的静默缺陷就是这么来的"
        );
        // 退出码校验在同一分支的后续几行：按**字符**取有界窗口，别用裸字节偏移（CRLF/多字节安全）。
        let window: String = src[idx..].chars().take(600).collect();
        assert!(
            window.contains("status.success()"),
            "必须校验 taskkill 退出码，失败要 bail"
        );
    }

    /// GUI 的「重启」命令臂必须把错误放进 toast（只 log! = 用户看到「点了没反应」）。
    #[test]
    fn gui_restart_arm_surfaces_failure() {
        let ui = crate::platform::src_lf(include_str!("ui.rs"));
        let arm = ui
            .split("UiCmd::Restart =>")
            .nth(1)
            .expect("ui.rs 应有 UiCmd::Restart 命令臂");
        // 到下一个命令臂为止；找不到就退回固定**字符**数窗口（不用字节偏移，CRLF 下不安全）。
        let arm: String = match arm.find("UiCmd::OpenLogs") {
            Some(end) => arm[..end].to_string(),
            None => arm.chars().take(400).collect(),
        };
        assert!(
            arm.contains("if let Err(e) = install::svc_restart()"),
            "重启臂必须检查错误：{arm}"
        );
        assert!(arm.contains("svc_toast"), "重启失败必须弹 toast：{arm}");
    }

    /// 重启判据：必须**换了一个实例**，而不是「有个进程在跑」（复评 F1 的行为守卫）。
    #[test]
    fn is_restarted_requires_a_different_pid() {
        let st = |running: bool, pid: u32| super::ServiceStatus { running, pid };
        assert!(
            super::is_restarted(100, &st(true, 200)),
            "旧 100 → 新 200 = 重启成功"
        );
        assert!(
            !super::is_restarted(100, &st(true, 100)),
            "还是原来那个 pid = 没重启（旧实现会判成功）"
        );
        assert!(
            !super::is_restarted(100, &st(false, 0)),
            "没实例在跑 = 没起来"
        );
        assert!(
            !super::is_restarted(100, &st(false, 100)),
            "pid 文件还在但进程死了 = 没起来"
        );
        assert!(
            super::is_restarted(0, &st(true, 7)),
            "重启前没在跑 → 任何新实例都算成功"
        );
    }

    /// 源码守卫：托管/非托管两条重启路径都必须按「新 pid」判成功，不得退回只看 running。
    #[test]
    fn restart_waits_for_a_new_pid() {
        let src = crate::platform::src_lf(include_str!("install.rs"));
        let body = src
            .split("pub fn svc_restart()")
            .nth(1)
            .expect("install.rs 应有 svc_restart");
        // 以「下一个函数」为界，别把后面整个文件圈进来（守卫自匹配）。
        let body = body
            .split("pub fn svc_stop_authorized")
            .next()
            .unwrap_or("");
        assert_eq!(
            body.matches("wait_service_restarted(").count(),
            2,
            "托管路径与非托管路径都要按「新 pid」确认（只看 running 会把空转报成成功）：{body}"
        );
        assert!(
            !body.contains("wait_service_up("),
            "重启判据不得退回 wait_service_up（它只问「有没有进程在跑」）：{body}"
        );
    }
}

/// 安装器（`app-assets/ABB.iss`）的**安全前提**守卫 —— 平台无关（纯文本断言），任何平台都能跑。
///
/// 为什么值得钉：这几条不是"配置偏好"，而是本批「登录后服务不能被随便杀死」能成立的前提 ——
/// 一旦有人在改安装脚本时把它们删掉，Windows 上就会静默退回「高完整性常驻 + 用户可写 exe」
/// 的持久化提权形态，而门禁全绿、CI 全绿（`.iss` 只在发版时编译，跑不到单测）。评审 R22 的
/// P2 就是这么被发现的，故补成可红可绿的断言。
#[cfg(test)]
mod installer_guards {
    const ISS: &str = include_str!("../app-assets/ABB.iss");

    /// 安装脚本里的**代码行**（去掉注释行与行尾注释）。
    ///
    /// 评审 R23 §3.3：只对整份文本做子串匹配会被「注释里写一遍」顶住（假绿）——
    /// 例如把 `PrivilegesRequired=lowest` 改回去、却在注释里保留 `PrivilegesRequired=admin`
    /// 字样。故本组守卫一律只看代码行。
    fn code_lines() -> Vec<&'static str> {
        ISS.lines()
            .filter(|l| !l.trim_start().starts_with(';'))
            .collect()
    }

    /// 代码行里是否存在以 `key=` 开头（忽略缩进）的指令，且其值等于 `value`。
    fn directive_is(key: &str, value: &str) -> bool {
        code_lines().iter().any(|l| {
            let t = l.trim();
            if let Some(rest) = t.strip_prefix(key) {
                rest.trim_start()
                    .strip_prefix('=')
                    .map(|v| v.trim() == value)
                    .unwrap_or(false)
            } else {
                false
            }
        })
    }

    /// 程序必须装在**普通用户不可写**的位置、且安装需要管理员：这是「以 HighestAvailable 常驻」
    /// 的前提（否则普通进程一次 UAC 后改写 exe 即可持久化提权）。
    #[test]
    fn installer_requires_admin_and_installs_under_program_files() {
        assert!(
            directive_is("PrivilegesRequired", "admin"),
            "安装器必须要求管理员（per-machine）；per-user 安装会让常驻 exe 落在用户可写目录"
        );
        assert!(
            directive_is("DefaultDirName", r"{autopf}\ABB"),
            "程序必须装到 autopf（Program Files，普通用户不可写），不能再用 localappdata"
        );
        // 反向：这两个值**不得**在代码行里出现（防「注释里写 admin、代码里偷偷改回 lowest」）
        assert!(
            !code_lines().iter().any(|l| l.contains("localappdata")),
            "代码行里不得再出现 localappdata（per-machine 的前提）"
        );
    }

    /// 安装器必须在**复制文件之前**把常驻服务真停掉（2026-10-01 owner 实报「安装程序杀不掉
    /// 进程，必须手动杀才能装成功」）。
    ///
    /// 为什么钉这条：`CloseApplications=yes` 靠 Windows RestartManager，而 RM 只能关有窗口的
    /// GUI 进程 —— 无窗口的常驻服务它关不掉；静默安装（`/SUPPRESSMSGBOXES`）下那个
    /// Abort/Retry/Ignore 默认取 **Abort** ⇒ 安装回滚。少了下面任意一行，用户就又要手动杀进程；
    /// 而 `.iss` 只在发版时编译、跑不到单测，只能靠源码断言兜住。
    #[test]
    fn installer_force_stops_bridge_before_copying_files() {
        let code = code_lines().join("\n");
        // 2026-10-04 更新（P4a，新模型）：没有计划任务要停/禁用了（服务由托盘以普通用户身份
        // 看守，自启 = Run 键）。这里只钉三件仍然成立的事 + 一条反向锁。
        assert!(
            code.contains("'/F /T /IM agent-bridge.exe'"),
            "必须强杀 agent-bridge.exe（RM 关不掉它，这是安装失败的直接原因）"
        );
        assert!(
            code.contains("'/F /T /IM buzz-agent.exe'"),
            "agent 进程也要收（它锁着 buzz-agent.exe）"
        );
        assert!(
            code.contains("function PrepareToInstall"),
            "收工动作必须挂在 PrepareToInstall（复制文件之前）"
        );
        assert!(
            code.contains("StopBridgeForInstall"),
            "PrepareToInstall / ssInstall 必须真的调用 StopBridgeForInstall"
        );
        assert!(
            code.contains("Sleep("),
            "杀完必须等句柄释放（kill 返回 ≠ 句柄已关）"
        );
        // 反向锁：新模型不再有常驻计划任务 —— 谁把 schtasks / 任务名加回安装脚本，这里就红。
        assert!(
            !code.contains("schtasks"),
            "安装器不得再调 schtasks（新模型没有常驻计划任务）"
        );
        assert!(
            !code.contains("ABB-Bridge"),
            "安装器不得再管理 ABB-Bridge 任务"
        );
    }

    /// 托盘必须用 `runasoriginaluser` 拉起：安装器已是管理员，否则管理员令牌会传染给托盘
    /// spawn 的 claude/codex（agent 带管理员权限）。bridge 的高完整性由计划任务单独负责。
    ///
    /// **必须钉「写在 `Flags:` 值里」而不是「这行出现过这个词」**：`runasoriginaluser` 在
    /// Inno 里是**标志（flag）**，写成独立参数位（`… Flags: nowait; runasoriginaluser`）会被
    /// ISCC 直接拒绝 —— 实测报 `Error on line 89 …: Unrecognized parameter name
    /// "runasoriginaluser"`，整条 Build & Release 的 windows job 失败、release job 被跳过
    /// （CI run `36545793263`）。旧写法只断言子串，所以这种「写错位置」的形态照样绿。
    #[test]
    fn installer_relaunches_tray_as_original_user() {
        // 直接按"拉起托盘的 [Run] 行"的特征筛选（不依赖 `[Run]` 段头的位置：注释里也出现过这个词）。
        let launches = code_lines()
            .into_iter()
            .filter(|l| {
                let t = l.trim_start();
                t.starts_with("Filename:") && t.contains("--wait-lock")
            })
            .collect::<Vec<_>>();
        assert_eq!(launches.len(), 2, "交互/静默两条拉起，实得：{launches:?}");
        for l in launches {
            // 取 `Flags:` 的值（到该行下一个 `;` 为止，参数是 `;` 分隔的键值对）。
            let flags = l
                .split_once("Flags:")
                .map(|(_, rest)| rest.split(';').next().unwrap_or(""))
                .unwrap_or("");
            assert!(
                flags.split_whitespace().any(|f| f == "runasoriginaluser"),
                "每条拉起都必须把 runasoriginaluser 写在 `Flags:` 值里（写到参数位 ISCC 会报 \
                 Unrecognized parameter name ⇒ 打包失败；缺了它托盘会以管理员身份跑）：{l}"
            );
        }
    }

    /// **随包 + 去提权守卫（2026-10-04 P4b 起）**：
    ///
    /// 历史：这条原来遍历 `Cargo.toml` 的每个非 macOS-only `[[bin]]`，要求安装脚本 `[Files]`
    /// 里出现同名 `.exe` —— 那个缺口连着被抓两轮（`abb-spawner.exe` 复评 R24、`abb-elev-helper.exe`
    /// 复评 R25）。现在**所有显式 `[[bin]]` 都已随去提权删除**（abb-spawner / abb-elev-helper /
    /// abb-helper），于是本测试改成钉两件事：① 安装包必须带主程序；② **不得**再冒出提权/特权
    /// helper 的可执行目标 —— 那等于把今天删掉的机制重新引进来。
    #[test]
    fn installer_ships_the_main_binary_and_no_privileged_helpers() {
        assert!(
            code_lines()
                .iter()
                .any(|l| l.contains("agent-bridge.exe") && l.contains("DestDir")),
            "安装包必须随带 agent-bridge.exe"
        );
        let manifest = include_str!("../Cargo.toml");
        for banned in ["abb-spawner", "abb-elev-helper", "abb-helper"] {
            assert!(
                !manifest.contains(&format!("name = \"{banned}\"")),
                "提权/特权 helper 不得再作为可执行目标存在（已随 2026-10-04 去提权删除）：{banned}"
            );
        }
    }

    /// 2026-10-04 新模型：**装机不再注册常驻计划任务**（服务由托盘以普通用户身份拉起）。
    ///
    /// 这条守卫是**反向锁**：谁把「注册计划任务」那条路加回来（隐藏子命令 / 高权限 XML /
    /// 任务名），这里立刻红。旧的 `--install-bridge-task` 子命令也已从 main.rs 删除。
    /// 去提权的收尾：旧版本装过这三个提权件，升级时必须删掉 —— 否则它们永远留在
    /// Program Files 里（owner 2026-10-04 专门问过这三个文件还在不在）。
    #[test]
    fn installer_deletes_the_retired_privileged_helpers() {
        let section = ISS
            .split("[InstallDelete]")
            .nth(1)
            .expect("安装脚本必须有 [InstallDelete] 段（清理已下线的提权件）");
        for exe in ["abb-spawner.exe", "abb-elev-helper.exe", "abb-helper.exe"] {
            assert!(section.contains(exe), "升级时必须删除已下线的提权件：{exe}");
        }
    }

    #[test]
    fn installer_pascal_section_has_no_semicolon_comments() {
        // 2026-10-04 真实教训：`[Code]` 段是 **Pascal**，注释只能是 `//` 或 `{}`；`;` 不是注释
        // （它是语句分隔符）。我改 [Code] 时把一处注释写成 `; ...`，ISCC 直接报
        // `'BEGIN' expected` 编译失败 —— 而安装包只在 CI/release 里编，普通测试抓不到。
        // 这条守卫把这类错误钉在测试里（本地 ISCC 实测：修掉后 Successful compile）。
        let code_section = ISS
            .split("[Code]")
            .nth(1)
            .expect("安装脚本必须有 [Code] 段");
        // 只看到下一个段标题为止：`[Run]`/`[Registry]` 那些段里的 `;` 注释是**合法**的，
        // 把整个文件都算进来会误报（首版守卫就这么翻的车）。
        let code_section = code_section.split("\n[").next().unwrap_or(code_section);
        for (i, line) in code_section.lines().enumerate() {
            assert!(
                !line.trim_start().starts_with(';'),
                "[Code] 段第 {} 行用了分号注释（Pascal 里不是注释，ISCC 会编译失败）：{line}",
                i + 1
            );
        }
    }
    #[test]
    fn installer_no_longer_registers_a_bridge_task() {
        assert!(
            !code_lines()
                .iter()
                .any(|l| l.contains("--install-bridge-task")),
            "安装器不得再调用登记常驻任务的隐藏子命令（新模型：服务由托盘看守）"
        );
        assert!(
            !ISS.contains("<RunLevel>HighestAvailable</RunLevel>"),
            "安装脚本里不得再出现高权限计划任务的 XML"
        );
        assert!(
            !code_lines().iter().any(|l| l.contains("ABB-Bridge")),
            "安装脚本不得再注册/管理 ABB-Bridge 常驻任务（新模型不再有它）"
        );
    }
}
