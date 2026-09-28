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
    svc_stop_impl(false);
}

/// 升级重启前停 service：**保留**「意图=运行」标记，让重启后的新实例把 bridge 拉回来。
///
/// 与 [`svc_stop`] 的唯一差别是不动 desired 标记（该清 pid 文件仍要清，否则新实例
/// 会把一个已退出的 pid 当真值）。语义边界：升级前用户本来就是停掉状态（标记不存在）
/// 时，这里同样保持不存在——升级不替用户改主意，只让「本来在跑」的意图活过这次重启。
pub fn svc_stop_keep_desired() {
    svc_stop_impl(true);
}

/// [`svc_stop`] / [`svc_stop_keep_desired`] 的共同实现。
fn svc_stop_impl(keep_desired: bool) {
    apply_stop_intent(&logs_dir(), keep_desired);
    let st = status();
    if st.running && st.pid != 0 {
        terminate(st.pid);
        if keep_desired {
            crate::log!(
                "[watchdog] 已停止 service pid={}（升级重启：保留「意图=运行」，新实例看门狗会拉回）",
                st.pid
            );
        } else {
            crate::log!("[watchdog] 已停止 service pid={}", st.pid);
        }
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
    terminate_with_grace(pid, std::time::Duration::from_secs(3));
}

/// terminate 的实现（grace 可注入，单测用短宽限验证升级链路不拖慢测试）。
fn terminate_with_grace(pid: u32, grace: std::time::Duration) {
    #[cfg(unix)]
    {
        unsafe {
            libc::kill(pid as i32, libc::SIGTERM);
        }
        // #179：优雅关闭可能卡死（如 WS 发送挂起）→ SIGTERM 杀不死、锁占着 → 新实例
        // 无限拉起失败。SIGTERM 后 grace 未退 → SIGKILL 兜底（后台线程，不阻塞调用方）。
        std::thread::spawn(move || {
            std::thread::sleep(grace);
            // #183（PR #181 审查 P1 补强）：SIGKILL 前复查存活——grace 内已优雅退出
            // （含 zombie）时 pid 可能被系统复用，盲发 SIGKILL 会误杀无辜进程。
            if !pid_alive(pid) {
                return;
            }
            unsafe {
                libc::kill(pid as i32, libc::SIGKILL);
            }
        });
    }
    #[cfg(not(unix))]
    {
        // Windows：taskkill /F 即强杀（无宽限期语义），grace 仅 unix 分支消费
        let _ = grace;
        // Windows：taskkill（stub）；CREATE_NO_WINDOW 避免停服务时闪控制台
        let _ = crate::spawn::command("taskkill")
            .args(["/PID", &pid.to_string(), "/F"])
            .spawn();
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
        terminate_with_grace(pid, std::time::Duration::from_millis(300));
        let t = std::time::Instant::now();
        let _ = child.wait(); // 阻塞到被杀
        assert!(
            t.elapsed() < std::time::Duration::from_secs(3),
            "忽略 TERM 的进程必须在宽限+SIGKILL 内被杀，实际 {:?}",
            t.elapsed()
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
    /// 升级成功分支的日志串（`ui.rs` 里「安装完成，退出并重启到新版本」那句）。
    const UPGRADE_LOG: &str = "安装完成，退出并重启到新版本";
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
        // 反向护栏：用户手动停 / 托盘退出仍走清意图的 `svc_stop()`（升级路径不得把两处语义混回去）。
        assert!(
            ui.matches("svc_stop();").count() >= 1,
            "手动停/退出路径仍应调用 svc_stop()；若全仓不再有它，说明语义被合并了，请重审本条守卫"
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
}
