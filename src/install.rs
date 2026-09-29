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
    // 托管形态（macOS launchd / Windows 计划任务）：起停交给 supervisor，托盘不再自己
    // spawn —— 两边同时管会抢单实例锁，表现为反复拉起-退出（见 platform::service_supervised）。
    if crate::platform::service_supervised() {
        set_desired(true);
        // 「启动」不等于「重启」：托管形态下只确保它在跑（job 没加载才 bootstrap / `/run`），
        // 不能用 kickstart -k —— 那会杀掉正在跑的实例，看门狗每 2s 判活失败就再来一次，
        // 形成「拉起即被杀」的抖动（评审 P5）。
        return crate::platform::start_service_supervised();
    }
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
    if crate::platform::service_supervised() {
        // 托管形态：交给 supervisor 重启（不经过「先杀后起」的托盘子进程路径）。
        set_desired(true);
        if let Err(e) = crate::platform::restart_service_supervised() {
            crate::log!("[watchdog] 托管重启失败: {e:#}");
        }
        return;
    }
    svc_stop();
    // 稍等子进程退出再拉
    std::thread::sleep(std::time::Duration::from_millis(300));
    if let Err(e) = svc_start() {
        crate::log!("[watchdog] 重启失败: {e:#}");
    }
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
    if platform.is_ok() && st.running && st.pid != 0 {
        terminate(st.pid);
        crate::log!(
            "[watchdog] 授权停止：已终止 pid={}（非托管形态兜底）",
            st.pid
        );
    }
    if let Err(e) = platform {
        // 授权被取消 / 平台侧失败：只有确认进程真的没了才算停成功，否则如实报错。
        let still = status();
        if still.running {
            return Err(e);
        }
        crate::log!("[watchdog] 平台侧返回失败，但 service 已不在运行：按成功处理（{e:#}）");
    }
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

    /// 托盘必须用 `runasoriginaluser` 拉起：安装器已是管理员，否则管理员令牌会传染给托盘
    /// spawn 的 claude/codex（agent 带管理员权限）。bridge 的高完整性由计划任务单独负责。
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
            assert!(
                l.contains("runasoriginaluser"),
                "每条拉起都必须带 runasoriginaluser（否则托盘以管理员身份跑）：{l}"
            );
        }
    }

    /// **随包清单的守卫（系统性）**：`Cargo.toml` 里每个**非 macOS-only** 的 bin，
    /// 都必须在安装脚本的 `[Files]` 里出现同名 `.exe`。
    ///
    /// 为什么做成通用规则：这个缺口连着被抓两轮 —— 先是 B2 的 `abb-spawner.exe`（复评 R24），
    /// 再是授权停止要用的 `abb-elev-helper.exe`（复评 R25）。两者都是「代码里 spawn 同目录的
    /// helper，但打包清单没带」⇒ 安装版上对应功能必然报「helper 不存在」。硬编码两条只能防这两
    /// 个名字，通用规则能防下一个。
    ///
    /// 例外面（必须写清理由）：`abb-helper` 是 macOS 锁屏助手（非 macOS 平台只有占位 main），
    /// Windows 包不需要它。
    #[test]
    fn installer_ships_every_non_macos_bin() {
        const MACOS_ONLY_BINS: [&str; 1] = ["abb-helper"];
        let manifest = include_str!("../Cargo.toml");
        let mut checked = 0usize;
        for block in manifest.split("[[bin]]").skip(1) {
            let name = block
                .lines()
                .find_map(|l| l.trim().strip_prefix("name = "))
                .map(|v| v.trim().trim_matches('"').to_string())
                .expect("[[bin]] 段必须有 name");
            if MACOS_ONLY_BINS.contains(&name.as_str()) {
                continue;
            }
            checked += 1;
            let want_src = format!("release\\{name}.exe");
            assert!(
                code_lines()
                    .iter()
                    .any(|l| l.contains(&format!("{name}.exe")) && l.contains("DestDir")),
                "安装包必须随带 {name}.exe（代码里会 spawn 同目录的这个二进制）"
            );
            assert!(
                code_lines().iter().any(|l| l.contains(&want_src)),
                "源路径必须是 target\\release\\{name}.exe"
            );
        }
        assert!(
            checked >= 2,
            "至少应检查 abb-elev-helper 与 abb-spawner，实得 {checked}"
        );
    }

    /// 装机即登记常驻任务，且**不在 Pascal 里重写 XML**（XML 单一定义在 src/svc_task.rs）：
    /// 这里只允许出现一次 `--install-bridge-task` 调用与一次卸载删除。
    #[test]
    fn installer_registers_and_removes_the_bridge_task_via_our_binary() {
        assert!(
            code_lines()
                .iter()
                .any(|l| l.contains("'--install-bridge-task'")),
            "安装器必须调用我们自己的隐藏子命令登记任务（XML 单一定义在 svc_task.rs）"
        );
        assert!(
            !ISS.contains("<RunLevel>HighestAvailable</RunLevel>"),
            "安装脚本里不得重写一份任务 XML（会与 src/svc_task.rs 漂移）"
        );
        assert!(
            code_lines()
                .iter()
                .any(|l| l.contains("/delete /tn ' + BridgeTaskName")),
            "卸载时必须删掉常驻任务，别留孤儿"
        );
    }
}
