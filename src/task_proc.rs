//! `PayloadKind::Proc` 的 Unix 进程超管。
//!
//! 这里只负责“跑一个 argv 进程并把它停下来”：新进程组、stdout/stderr 流式落盘、
//! 超时/取消时 `SIGTERM → grace → 复查存活 → SIGKILL` 整组收尾，以及退出码回收。
//! Windows Job Object 尚未接入，本批在 CLI 与定义校验两处显式拒绝 proc。

#[cfg(unix)]
use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(unix)]
use std::sync::Arc;
#[cfg(unix)]
use std::time::Duration;

use crate::task_store::{Task, TaskRuntime, TaskStateKind, TaskStateStore};

const WINDOWS_PROC_REJECTION: &str =
    "Windows 暂不支持 proc 任务：Q15: Job Object 尚未接入（拒绝创建，避免进程树无法完整停止）";
#[cfg(unix)]
const PROC_POLL_INTERVAL: Duration = Duration::from_millis(20);
#[cfg(unix)]
const CANCEL_POLL_INTERVAL: Duration = Duration::from_millis(100);
#[cfg(unix)]
const DRAIN_JOIN_TIMEOUT: Duration = Duration::from_secs(2);
#[cfg(unix)]
const ORPHAN_GROUP_CLEANUP_LOG: &str = "leader 已退出，进程组仍有存活成员";

/// 纯函数便于单测直接覆盖 Windows 策略；生产入口用编译目标决定实际值。
pub(crate) fn platform_error_for_target(target_os: &str) -> Option<&'static str> {
    (target_os == "windows").then_some(WINDOWS_PROC_REJECTION)
}

/// 当前平台是否必须拒绝 proc。Windows 返回 Q15-D 的明确错误。
pub(crate) fn platform_error() -> Option<&'static str> {
    platform_error_for_target(std::env::consts::OS)
}

/// 判断是否处于 agent 上下文。
///
/// 真实 ACP 主路径的 `dev__shell` 看不到 `AGENT_BRIDGE_BOT_KEY` / `CHAT_ID` /
/// `SENDER_ROLE`：它们经 `crates/buzz-agent/src/devtools.rs` →
/// `mcp::apply_passthrough_env()` 的 `env_clear()` 白名单后被剥掉。因此 buzz-agent
/// 在该共享入口为每个派生 shell 注入 `ABB_AGENT_CONTEXT=1`；CLI 以它作为**主判据**。
/// 旧的 `AGENT_BRIDGE_*` 继续作为 legacy hook/手工调试路径的兜底判据。
///
/// 这不是对敌意 owner agent 的安全边界：owner 默认 FullAccess，本来就能任意执行、
/// 清除注入环境并直接改 `tasks.json`。它是纵深防御/合规闸，不能替代 OS sandbox。
pub(crate) const ACP_AGENT_CONTEXT_ENV: &str = "ABB_AGENT_CONTEXT";
const LEGACY_AGENT_CONTEXT_ENV_KEYS: &[&str] = &[
    "AGENT_BRIDGE_BOT_KEY",
    "AGENT_BRIDGE_CHAT_ID",
    "AGENT_BRIDGE_SENDER_ROLE",
];

fn first_nonempty_env<F>(mut get: F) -> Option<&'static str>
where
    F: FnMut(&str) -> Option<std::ffi::OsString>,
{
    if get(ACP_AGENT_CONTEXT_ENV).is_some_and(|value| !value.is_empty()) {
        return Some(ACP_AGENT_CONTEXT_ENV);
    }
    LEGACY_AGENT_CONTEXT_ENV_KEYS
        .iter()
        .copied()
        .find(|key| get(key).is_some_and(|value| !value.is_empty()))
}

fn agent_context_rejection_for(marker: Option<&str>) -> Option<String> {
    marker.map(|key| {
        format!("proc 只允许 GUI/人工入口（Q8）；检测到 agent 上下文环境变量 {key}，已拒绝创建")
    })
}

/// `run_task_cli` 的 proc 入口闸：agent 环境存在时返回可展示的拒绝原因。
pub(crate) fn agent_context_rejection() -> Option<String> {
    agent_context_rejection_for(first_nonempty_env(|key| std::env::var_os(key)))
}

/// 当前进程是否带 agent 上下文标记（ACP 主标记或 legacy 标记）。
pub(crate) fn agent_context_marker() -> Option<&'static str> {
    first_nonempty_env(|key| std::env::var_os(key))
}

#[cfg(unix)]
pub(crate) async fn run_proc_attempt(
    task: &Task,
    workspace: &str,
    states: &TaskStateStore,
    stop: &tokio_util::sync::CancellationToken,
) -> TaskRuntime {
    #[cfg(unix)]
    if task.payload.pty {
        return run_proc_attempt_pty(task, workspace, states, stop).await;
    }
    run_proc_attempt_with_grace(task, workspace, states, stop, None).await
}

/// PTY 默认尺寸（TUI 工具对 0×0 会异常；80×24 太窄，给 120×30）。
#[cfg(unix)]
const PTY_ROWS: u16 = 30;
#[cfg(unix)]
const PTY_COLS: u16 = 120;
/// 单条 stdin 请求的写入上界（字节）。PTY master 写可能阻塞（子进程不读就填满缓冲），
/// 所以既不能无界写、也不把超大 payload 塞进轮询循环里。
#[cfg(unix)]
const PTY_STDIN_MAX_BYTES: usize = 8192;

/// PTY 模式的轮询间隔（收 stdin 请求 / 看退出 / 看 stop）。
#[cfg(unix)]
const PTY_POLL_INTERVAL: Duration = Duration::from_millis(100);

/// 去掉终端控制序列（CSI/OSC 等），只留可读文本 —— PTY 输出会带颜色/光标控制，
/// 直接写日志既臃肿又难看。纯函数，便于单测。
///
/// `#[cfg(unix)]`：只有 PTY 路径用它；Windows 上 proc 本就显式拒绝，留在这里会变死代码。
#[cfg(unix)]
pub(crate) fn strip_ansi(input: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(input.len());
    let mut i = 0;
    while i < input.len() {
        let b = input[i];
        if b != 0x1b {
            // 去掉 CR（PTY 把行尾写成 CRLF）：保留 LF 即可
            if b != b'\r' {
                out.push(b);
            }
            i += 1;
            continue;
        }
        // ESC
        match input.get(i + 1) {
            Some(b'[') => {
                // CSI: ESC [ 参数 中间 最终字节（0x40..=0x7e）
                let mut j = i + 2;
                while j < input.len() && !(0x40..=0x7e).contains(&input[j]) {
                    j += 1;
                }
                i = (j + 1).min(input.len());
            }
            Some(b']') => {
                // OSC: ESC ] ... (BEL | ESC \)
                let mut j = i + 2;
                while j < input.len() {
                    if input[j] == 0x07 {
                        j += 1;
                        break;
                    }
                    if input[j] == 0x1b && input.get(j + 1) == Some(&b'\\') {
                        j += 2;
                        break;
                    }
                    j += 1;
                }
                i = j;
            }
            _ => i += 2, // 单个 ESC / 未知序列：丢掉转义本身
        }
    }
    out
}

/// 一条 stdin 输入请求（`task send` 写、PTY 会话读）。
#[cfg(unix)]
#[derive(Debug, Clone, serde::Deserialize)]
struct ProcStdinRequest {
    #[serde(default)]
    text: String,
    /// 写完是否补一个回车（默认 true；raw 模式下忽略）。
    #[serde(default = "default_true")]
    enter: bool,
    /// true = 原样写入（可带控制字符，如 \u0003 表示 Ctrl-C），不补回车。
    #[serde(default)]
    raw: bool,
}

#[cfg(unix)]
fn default_true() -> bool {
    true
}

/// 消费该任务的 stdin 请求（按文件名升序，处理完即删）。返回写入的字节数合计。
#[cfg(unix)]
fn drain_proc_stdin(
    paths: &crate::task_store::TaskPaths,
    id: &str,
    writer: &mut std::fs::File,
    log_max_bytes: u64,
    log_id: &str,
    log_lock: &std::sync::Mutex<()>,
) -> usize {
    use std::io::Write as _;
    let dir = paths.proc_stdin_dir(id);
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return 0;
    };
    let mut files: Vec<std::path::PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_file())
        .collect();
    files.sort();
    let mut written = 0;
    for f in files {
        let Ok(raw) = std::fs::read_to_string(&f) else {
            continue;
        };
        let Ok(req) = serde_json::from_str::<ProcStdinRequest>(&raw) else {
            crate::log!(
                "[proc:{}] stdin 请求解析失败，已丢弃：{}",
                short_id(log_id),
                f.display()
            );
            let _ = std::fs::remove_file(&f);
            continue;
        };
        let mut payload = req.text.clone().into_bytes();
        if !req.raw && req.enter {
            payload.push(b'\r');
        }
        if payload.len() > PTY_STDIN_MAX_BYTES {
            crate::log!(
                "[proc:{}] stdin 请求过大（{} 字节 > {PTY_STDIN_MAX_BYTES}），已丢弃：{}",
                short_id(log_id),
                payload.len(),
                f.file_name().unwrap_or_default().to_string_lossy()
            );
            let _ = std::fs::remove_file(&f);
            continue;
        }
        if !payload.is_empty() {
            if let Err(e) = writer.write_all(&payload) {
                crate::log!("[proc:{}] 写入 PTY 失败：{e:#}", short_id(log_id));
            } else {
                let _ = writer.flush();
                written += payload.len();
            }
        }
        // 只记「写了多少字节」，不记内容（可能是密码）
        let note = format!(
            "[proc] 收到 stdin 请求 {}（{} 字节）\n",
            f.file_name().unwrap_or_default().to_string_lossy(),
            payload.len()
        );
        {
            // 与读线程共用同一把日志锁（否则轮转/追加会交错）
            let _guard = log_lock.lock().unwrap_or_else(|e| e.into_inner());
            let _ = crate::task_store::append_task_log_record(
                paths,
                log_id,
                log_max_bytes,
                note.as_bytes(),
            );
        }
        let _ = std::fs::remove_file(&f);
    }
    written
}

/// PTY 模式：把 proc 载荷跑在伪终端里。
///
/// 与管道模式的区别只在「怎么起、怎么读、怎么喂 stdin」：收尾（SIGTERM → grace →
/// SIGKILL 进程组）、状态机、日志轮转全部复用既有实现。
#[cfg(unix)]
async fn run_proc_attempt_pty(
    task: &Task,
    workspace: &str,
    states: &TaskStateStore,
    stop: &tokio_util::sync::CancellationToken,
) -> TaskRuntime {
    use std::io::Read;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};

    let id = task.id.clone();
    let started = crate::chrono_lite::unix_secs();
    let paths = states.paths().clone();
    // 全新会话：清掉上一轮可能残留的输入请求（否则会被当成「用户早就想输入的内容」）
    let stale_dir = paths.proc_stdin_dir(&id);
    if let Ok(entries) = std::fs::read_dir(&stale_dir) {
        let mut n = 0;
        for e in entries.flatten() {
            if e.path().is_file() && std::fs::remove_file(e.path()).is_ok() {
                n += 1;
            }
        }
        if n > 0 {
            crate::log!(
                "[proc:{}] PTY 会话启动时清理了 {n} 条陈旧 stdin 请求",
                short_id(&id)
            );
        }
    }

    let ws = nix::pty::Winsize {
        ws_row: PTY_ROWS,
        ws_col: PTY_COLS,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let pty = match nix::pty::openpty(Some(&ws), None) {
        Ok(p) => p,
        Err(e) => {
            return terminal_runtime(
                states,
                &id,
                started,
                TaskStateKind::Failed,
                None,
                format!("PTY 创建失败：{e}"),
            );
        }
    };
    let master: OwnedFd = pty.master;
    let slave: OwnedFd = pty.slave;
    let stdin_fd = unsafe { OwnedFd::from_raw_fd(libc::dup(slave.as_raw_fd())) };
    let stdout_fd = unsafe { OwnedFd::from_raw_fd(libc::dup(slave.as_raw_fd())) };
    let stderr_fd = unsafe { OwnedFd::from_raw_fd(libc::dup(slave.as_raw_fd())) };

    let mut command = Command::new(&task.payload.cmd[0]);
    command
        .args(&task.payload.cmd[1..])
        .current_dir(workspace)
        .envs(&task.payload.env)
        .stdin(Stdio::from(stdin_fd))
        .stdout(Stdio::from(stdout_fd))
        .stderr(Stdio::from(stderr_fd));
    // TUI 需要 TERM；调用方没给就用一个通用值（否则 claude/codex/pi 会按 dumb 终端降级）
    if !task.payload.env.contains_key("TERM") {
        command.env("TERM", "xterm-256color");
    }
    // 新会话 + 把 pty 设为控制终端：pgid==pid（整组可杀），且子进程看到真 TTY。
    // pre_exec 里只调 libc（多线程 fork 后碰 allocator 会死锁）。
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::ioctl(0, libc::TIOCSCTTY as _, 0) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = match command.spawn() {
        Ok(c) => c,
        Err(e) => {
            return terminal_runtime(
                states,
                &id,
                started,
                TaskStateKind::Failed,
                None,
                format!("进程启动失败：{e}"),
            );
        }
    };
    let pid = child.id();

    let mut running = states.get(&id);
    running.kind = TaskStateKind::Running;
    running.pid = Some(pid);
    running.proc_identity = crate::task_identity::capture(pid);
    running.started_at = Some(started);
    running.finished_at = None;
    running.last_exit_code = None;
    running.last_error.clear();
    let _ = states.set(&id, running);
    crate::log!(
        "[task:{}] {} proc(pty) 已启动 pid={pid} size={PTY_ROWS}x{PTY_COLS}",
        task.bot_key,
        short_id(&id)
    );

    // 读线程：master → 去控制序列 → 日志（轮转交给 append_task_log_record）
    let mut reader = std::fs::File::from(master);
    let mut writer =
        std::fs::File::from(unsafe { OwnedFd::from_raw_fd(libc::dup(reader.as_raw_fd())) });
    let log_max_bytes = task.limits.log_max_bytes;
    let log_paths = paths.clone();
    let log_id = id.clone();
    // 读线程与主循环（drain 的「收到请求」注记）都会写同一个日志文件 → 共用一把锁，
    // 否则轮转/追加会交错（管道模式也是这么做的）。
    let log_lock = Arc::new(std::sync::Mutex::new(()));
    let reader_lock = log_lock.clone();
    std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let clean = strip_ansi(&buf[..n]);
                    if !clean.is_empty() {
                        let _guard = reader_lock.lock().unwrap_or_else(|e| e.into_inner());
                        let _ = crate::task_store::append_task_log_record(
                            &log_paths,
                            &log_id,
                            log_max_bytes,
                            &clean,
                        );
                    }
                }
            }
        }
    });

    // 等待线程：child.wait() 在阻塞线程里做，主循环只轮询（绝不在 wait 上无超时死等）
    let (exit_tx, exit_rx) = std::sync::mpsc::channel::<std::process::ExitStatus>();
    std::thread::spawn(move || {
        let _ = exit_tx.send(
            child
                .wait()
                .unwrap_or_else(|_| std::process::ExitStatus::default()),
        );
    });

    let deadline = if task.limits.timeout_secs == 0 {
        None
    } else {
        Some(tokio::time::Instant::now() + Duration::from_secs(task.limits.timeout_secs))
    };

    let mut stop_reason: Option<String> = None;
    let mut exited: Option<std::process::ExitStatus> = None;
    loop {
        if let Ok(st) = exit_rx.try_recv() {
            exited = Some(st);
            break;
        }
        if stop.is_cancelled() {
            stop_reason = Some("已取消（service 关停）".to_string());
            break;
        }
        // 用户 `task cancel`：与管道路径的 cancel watcher 同语义（consume_cancel_requests 对
        // Running 一律 continue，运行中这一轮必须由本循环自己认这个请求文件——
        // 否则 PTY 会话会「静默不受取消」，reviewer-34 反证实测 5s 不生效）。
        if paths.cancel_file(&id).exists() {
            stop_reason = Some("已取消（用户 task cancel）".to_string());
            break;
        }
        if deadline.is_some_and(|d| tokio::time::Instant::now() >= d) {
            stop_reason = Some(format!("执行超时（预算 {}s）", task.limits.timeout_secs));
            break;
        }
        let _ = drain_proc_stdin(&paths, &id, &mut writer, log_max_bytes, &id, &log_lock);
        tokio::time::sleep(PTY_POLL_INTERVAL).await;
    }

    let grace = Duration::from_secs(task.limits.grace_secs);
    let mut escalated = false;
    if stop_reason.is_some() || group_alive(nix::unistd::Pid::from_raw(pid as i32)) {
        // 停止链与管道模式一致：SIGTERM → grace → 复查 → SIGKILL（整组）
        match stop_orphan_process_group(pid, grace).await {
            Ok(stop) => escalated = stop.escalated,
            Err(e) => {
                stop_reason = Some(format!(
                    "{}；停止进程组失败：{e}",
                    stop_reason.unwrap_or_else(|| "进程组残留清理".to_string())
                ));
            }
        }
        if exited.is_none() {
            // 给等待线程一点时间把退出码送回来（SIGKILL 后应很快）
            for _ in 0..50 {
                if let Ok(st) = exit_rx.try_recv() {
                    exited = Some(st);
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }
    }
    let _ = std::fs::remove_file(paths.cancel_file(&id));

    let exit_code = exited.as_ref().and_then(|s| s.code());
    let note = match (&stop_reason, &exited) {
        (Some(reason), _) => reason.clone(),
        (None, Some(st)) if st.success() => String::new(),
        (None, Some(st)) => format!("进程退出码 {}", st.code().unwrap_or(-1)),
        (None, None) => "进程已退出（未取到退出码）".to_string(),
    };
    let kind = if stop_reason.is_some() {
        TaskStateKind::Cancelled
    } else if exited.map(|s| s.success()).unwrap_or(false) {
        TaskStateKind::Succeeded
    } else {
        TaskStateKind::Failed
    };
    let tail = format!("[proc] 结束：{note}\n");
    let _ = crate::task_store::append_task_log_record(&paths, &id, log_max_bytes, tail.as_bytes());
    if escalated {
        crate::log!(
            "[task:{}] {} proc(pty) 宽限后仍存活，已 SIGKILL 进程组",
            task.bot_key,
            short_id(&id)
        );
    }
    terminal_runtime(states, &id, started, kind, exit_code, note)
}

/// 测试可注入毫秒级宽限；生产走 `TaskLimits.grace_secs`。
#[cfg(unix)]
async fn run_proc_attempt_with_grace(
    task: &Task,
    workspace: &str,
    states: &TaskStateStore,
    stop: &tokio_util::sync::CancellationToken,
    grace_override: Option<Duration>,
) -> TaskRuntime {
    use std::process::Stdio;
    use tokio::process::Command;

    let id = task.id.clone();
    let started = crate::chrono_lite::unix_secs();
    let mut command = Command::new(&task.payload.cmd[0]);
    command
        .args(&task.payload.cmd[1..])
        .current_dir(workspace)
        .envs(&task.payload.env)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // 子进程成为新进程组组长（pgid == pid）；之后的孙进程默认继承该组，整组信号可覆盖。
    command.process_group(0);

    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(e) => {
            return terminal_runtime(
                states,
                &id,
                started,
                TaskStateKind::Failed,
                None,
                format!("进程启动失败：{e}"),
            );
        }
    };
    let pid = child.id().expect("spawned child must have pid");

    let mut running = states.get(&id);
    running.kind = TaskStateKind::Running;
    running.pid = Some(pid);
    // 进程执行细节保持不变；这里只在 spawn 成功后冻结代际身份，供 service 重启后的
    // fail-closed 恢复判定使用。
    running.proc_identity = crate::task_identity::capture(pid);
    running.started_at = Some(started);
    running.finished_at = None;
    running.last_exit_code = None;
    running.last_error.clear();
    let _ = states.set(&id, running);
    crate::log!(
        "[task:{}] {} proc 已启动 pid={pid}",
        task.bot_key,
        short_id(&id)
    );

    let log_lock = Arc::new(std::sync::Mutex::new(()));
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let stdout_task = stdout.map(|reader| {
        spawn_log_drain(
            reader,
            states.paths().clone(),
            id.clone(),
            task.limits.log_max_bytes,
            log_lock.clone(),
        )
    });
    let stderr_task = stderr.map(|reader| {
        spawn_log_drain(
            reader,
            states.paths().clone(),
            id.clone(),
            task.limits.log_max_bytes,
            log_lock.clone(),
        )
    });

    let attempt_cancel = stop.child_token();
    let user_cancelled = Arc::new(AtomicBool::new(false));
    let cancel_watch = {
        let token = attempt_cancel.clone();
        let user_cancelled = user_cancelled.clone();
        let cancel_file = states.paths().cancel_file(&id);
        tokio::spawn(async move {
            loop {
                if token.is_cancelled() {
                    return;
                }
                if cancel_file.exists() {
                    user_cancelled.store(true, Ordering::Relaxed);
                    token.cancel();
                    return;
                }
                tokio::select! {
                    _ = tokio::time::sleep(CANCEL_POLL_INTERVAL) => {}
                    _ = token.cancelled() => return,
                }
            }
        })
    };

    let deadline = task
        .timeout()
        .map(|budget| tokio::time::Instant::now() + budget);
    let mut exited = None;
    let mut stop_reason = None;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                exited = Some(status);
                break;
            }
            Ok(None) => {}
            Err(e) => {
                stop_reason = Some(format!("等待进程失败：{e}"));
                break;
            }
        }
        if attempt_cancel.is_cancelled() {
            stop_reason = Some(if user_cancelled.load(Ordering::Relaxed) {
                "已取消（用户 task cancel）".to_string()
            } else {
                "已取消（service 关停）".to_string()
            });
            break;
        }
        if deadline.is_some_and(|d| tokio::time::Instant::now() >= d) {
            stop_reason = Some(format!("执行超时（预算 {}s）", task.limits.timeout_secs));
            break;
        }
        tokio::time::sleep(PROC_POLL_INTERVAL).await;
    }

    let mut escalated = false;
    if stop_reason.is_some() {
        let grace = grace_override.unwrap_or_else(|| Duration::from_secs(task.limits.grace_secs));
        match stop_process_group(&mut child, pid, grace).await {
            Ok(stop) => {
                exited = stop.status.or(exited);
                escalated = stop.escalated;
            }
            Err(e) => {
                stop_reason = Some(format!(
                    "{}；停止进程组失败：{e}",
                    stop_reason.unwrap_or_default()
                ));
            }
        }
    } else if exited.is_some() && process_group_alive(pid) {
        // 组长先退出不等于进程组完成：后台孙进程可能继承 stdio 和 PGID 继续活着。
        // 这里必须复用同一停止链收掉整组，否则 stdout/stderr 的 EOF 永远不来，worker 卡死。
        let grace = grace_override.unwrap_or_else(|| Duration::from_secs(task.limits.grace_secs));
        let cleanup = match stop_process_group(&mut child, pid, grace).await {
            Ok(stop) => {
                escalated = stop.escalated;
                let how = if stop.escalated {
                    "宽限后 SIGKILL"
                } else {
                    "SIGTERM"
                };
                format!("{ORPHAN_GROUP_CLEANUP_LOG}；已按 {how} 收尾")
            }
            Err(e) => format!("{ORPHAN_GROUP_CLEANUP_LOG}；清理失败：{e}"),
        };
        let cleanup = if process_group_alive(pid) {
            format!("{cleanup}；复查后仍有存活成员")
        } else {
            cleanup
        };
        crate::log!(
            "[task:{}] {} proc leader pid={pid} 已退出，遗留进程组处理：{}",
            task.bot_key,
            short_id(&id),
            cleanup
        );
        let guard = log_lock.lock().unwrap_or_else(|e| e.into_inner());
        let note = format!("[proc] {cleanup}\n");
        let _ = crate::task_store::append_task_log_record(
            states.paths(),
            &id,
            task.limits.log_max_bytes,
            note.as_bytes(),
        );
        drop(guard);
    }
    if escalated {
        crate::log!(
            "[task:{}] {} proc pid={pid} 宽限后仍存活，已 SIGKILL 进程组",
            task.bot_key,
            short_id(&id)
        );
    }

    cancel_watch.abort();
    let _ = tokio::fs::remove_file(states.paths().cancel_file(&id)).await;

    let mut log_error = None;
    let drain_deadline = tokio::time::Instant::now() + DRAIN_JOIN_TIMEOUT;
    for mut handle in [stdout_task, stderr_task].into_iter().flatten() {
        let remaining = drain_deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            handle.abort();
            log_error = Some("日志 drain 超时，已放弃等待".to_string());
            continue;
        }
        match tokio::time::timeout(remaining, &mut handle).await {
            Ok(Ok(Ok(()))) => {}
            Ok(Ok(Err(e))) => log_error = Some(format!("日志写入失败：{e}")),
            Ok(Err(e)) => log_error = Some(format!("日志 drain 任务失败：{e}")),
            Err(_) => {
                handle.abort();
                log_error = Some(format!(
                    "日志 drain 超过 {}s 未 EOF，已放弃等待",
                    DRAIN_JOIN_TIMEOUT.as_secs()
                ));
            }
        }
    }

    let exit_code = exited.and_then(|status| status.code());
    let (kind, last_error) = if let Some(reason) = stop_reason {
        if reason.starts_with("已取消") {
            (TaskStateKind::Cancelled, reason)
        } else {
            (TaskStateKind::Failed, reason)
        }
    } else if exit_code == Some(0) {
        (TaskStateKind::Succeeded, String::new())
    } else if let Some(code) = exit_code {
        (TaskStateKind::Failed, format!("进程退出码 {code}"))
    } else {
        (TaskStateKind::Failed, "进程被信号终止".to_string())
    };
    let last_error = match (last_error.is_empty(), log_error) {
        (_, None) => last_error,
        (true, Some(e)) => e,
        (false, Some(e)) => format!("{last_error}；{e}"),
    };
    terminal_runtime(states, &id, started, kind, exit_code, last_error)
}

#[cfg(unix)]
fn spawn_log_drain<R>(
    mut reader: R,
    paths: crate::task_store::TaskPaths,
    id: String,
    max_bytes: u64,
    lock: Arc<std::sync::Mutex<()>>,
) -> tokio::task::JoinHandle<std::io::Result<()>>
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    use tokio::io::AsyncReadExt;

    tokio::spawn(async move {
        let mut buf = [0u8; 8192];
        loop {
            let n = reader.read(&mut buf).await?;
            if n == 0 {
                return Ok(());
            }
            let guard = lock.lock().unwrap_or_else(|e| e.into_inner());
            crate::task_store::append_task_log_bytes(&paths, &id, max_bytes, &buf[..n])?;
            drop(guard);
        }
    })
}

#[cfg(unix)]
struct GroupStop {
    status: Option<std::process::ExitStatus>,
    escalated: bool,
}

#[cfg(unix)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct OrphanGroupStop {
    pub escalated: bool,
    pub alive_after: bool,
}

/// 收掉一个不再由本进程 `Child` 持有的旧进程组。顺序与在线停止链一致：
/// SIGTERM → grace → 复查 → SIGKILL，并在升级后短暂等待组内成员消失。
///
/// 调用方必须先核验进程代际身份；本函数只按已验证的 pgid 发信号，避免 PID 复用误杀。
#[cfg(unix)]
pub(crate) async fn stop_orphan_process_group(
    pgid: u32,
    grace: Duration,
) -> std::io::Result<OrphanGroupStop> {
    use nix::sys::signal::{killpg, Signal};
    use nix::unistd::Pid;

    let pid = Pid::from_raw(pgid as i32);
    if !group_alive(pid) {
        return Ok(OrphanGroupStop {
            escalated: false,
            alive_after: false,
        });
    }

    let _ = killpg(pid, Signal::SIGTERM);
    let deadline = tokio::time::Instant::now() + grace;
    let mut escalated = false;
    loop {
        if !group_alive(pid) {
            break;
        }
        if tokio::time::Instant::now() >= deadline {
            if group_alive(pid) {
                let _ = killpg(pid, Signal::SIGKILL);
                escalated = true;
            }
            break;
        }
        tokio::time::sleep(PROC_POLL_INTERVAL).await;
    }

    if escalated {
        let wait_until = tokio::time::Instant::now() + Duration::from_secs(1);
        while group_alive(pid) && tokio::time::Instant::now() < wait_until {
            tokio::time::sleep(PROC_POLL_INTERVAL).await;
        }
    }
    Ok(OrphanGroupStop {
        escalated,
        alive_after: group_alive(pid),
    })
}

#[cfg(unix)]
async fn stop_process_group(
    child: &mut tokio::process::Child,
    pgid: u32,
    grace: Duration,
) -> std::io::Result<GroupStop> {
    use nix::sys::signal::{killpg, Signal};
    use nix::unistd::Pid;

    let pid = Pid::from_raw(pgid as i32);
    let mut status = child.try_wait()?;
    if group_alive(pid) {
        let _ = killpg(pid, Signal::SIGTERM);
    }
    let deadline = tokio::time::Instant::now() + grace;
    let mut escalated = false;
    loop {
        if status.is_none() {
            status = child.try_wait()?;
        }
        if !group_alive(pid) {
            break;
        }
        if tokio::time::Instant::now() >= deadline {
            if group_alive(pid) {
                let _ = killpg(pid, Signal::SIGKILL);
                escalated = true;
            }
            break;
        }
        tokio::time::sleep(PROC_POLL_INTERVAL).await;
    }
    if escalated {
        let wait_until = tokio::time::Instant::now() + Duration::from_secs(1);
        while group_alive(pid) && tokio::time::Instant::now() < wait_until {
            tokio::time::sleep(PROC_POLL_INTERVAL).await;
        }
    }
    if status.is_none() {
        status = Some(child.wait().await?);
    }
    Ok(GroupStop { status, escalated })
}

#[cfg(unix)]
fn group_alive(pid: nix::unistd::Pid) -> bool {
    use nix::errno::Errno;
    use nix::sys::signal::killpg;

    match killpg(pid, None::<nix::sys::signal::Signal>) {
        Ok(()) | Err(Errno::EPERM) => true,
        Err(_) => false,
    }
}

#[cfg(unix)]
fn process_group_alive(pgid: u32) -> bool {
    group_alive(nix::unistd::Pid::from_raw(pgid as i32))
}

#[cfg(not(unix))]
pub(crate) async fn run_proc_attempt(
    task: &Task,
    _workspace: &str,
    states: &TaskStateStore,
    _stop: &tokio_util::sync::CancellationToken,
) -> TaskRuntime {
    let reason = platform_error().unwrap_or("当前平台暂不支持 proc 任务");
    crate::log!(
        "[task:{}] {} 拒绝 proc：{reason}",
        task.bot_key,
        short_id(&task.id)
    );
    terminal_runtime(
        states,
        &task.id,
        crate::chrono_lite::unix_secs(),
        TaskStateKind::Failed,
        None,
        reason.to_string(),
    )
}

#[cfg(not(unix))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct OrphanGroupStop {
    pub escalated: bool,
    pub alive_after: bool,
}

/// Windows 尚无 Job Object，proc 在登记期已拒绝；保留同签名只为让跨平台恢复代码编译。
#[cfg(not(unix))]
pub(crate) async fn stop_orphan_process_group(
    _pgid: u32,
    _grace: std::time::Duration,
) -> std::io::Result<OrphanGroupStop> {
    Ok(OrphanGroupStop {
        escalated: false,
        alive_after: true,
    })
}

fn terminal_runtime(
    states: &TaskStateStore,
    id: &str,
    started: u64,
    kind: TaskStateKind,
    last_exit_code: Option<i32>,
    last_error: String,
) -> TaskRuntime {
    let prev = states.get(id);
    let runtime = TaskRuntime {
        kind,
        pid: None,
        started_at: Some(started),
        finished_at: Some(crate::chrono_lite::unix_secs()),
        last_exit_code,
        restarts: prev.restarts,
        consecutive_failures: prev.consecutive_failures,
        next_retry_at: prev.next_retry_at,
        last_error,
        last_fired_at: prev.last_fired_at,
        proc_identity: None,
    };
    let _ = states.set(id, runtime.clone());
    runtime
}

fn short_id(id: &str) -> &str {
    &id[..id.len().min(12)]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ANSI/控制序列清洗：颜色、光标控制、OSC 标题都要去掉，正文与换行保留。
    #[cfg(unix)]
    #[test]
    fn strip_ansi_removes_control_sequences() {
        let raw = b"\x1b[31mred\x1b[0m plain\r\n\x1b]0;title\x07tail";
        assert_eq!(strip_ansi(raw), b"red plain\ntail".to_vec());
        // 没有控制字符时逐字节不变
        assert_eq!(strip_ansi(b"hello\n"), b"hello\n".to_vec());
        // 非 UTF-8 字节原样保留（日志不因编码崩）
        assert_eq!(strip_ansi(&[0xff, 0xfe]), vec![0xff, 0xfe]);
    }

    /// PTY 端到端：跑一条需要 **TTY + 交互输入** 的 proc 任务，从输入通道喂文本，
    /// 断言日志里出现交互回显，再取消收尾。这条锁住「claude/codex/pi 这类 TUI 能跑」的核心。
    #[cfg(unix)]
    #[tokio::test]
    async fn pty_proc_session_is_interactive() {
        let root = tmp_root("pty-e2e");
        let store = crate::task_store::TaskStore::new_at(&root, "b");
        // TaskStateStore 不可 Clone：跑会话用 Arc，读日志另开一个实例（盘是真相）
        let states = std::sync::Arc::new(crate::task_store::TaskStateStore::new_at(&root, "b"));
        let reader_states = crate::task_store::TaskStateStore::new_at(&root, "b");
        let task = crate::task_store::Task {
            schema_version: crate::task_store::TASK_SCHEMA_VERSION,
            id: "tk_pty_e2e".to_string(),
            name: "pty".to_string(),
            legacy_job_id: String::new(),
            bot_key: "b".to_string(),
            created_by: crate::task_store::CreatedBy {
                role: crate::config::SenderRole::Owner,
                bot_key: "b".to_string(),
                chat_id: "c".to_string(),
            },
            payload: TaskPayload {
                kind: PayloadKind::Proc,
                pty: true,
                cmd: vec![
                    "/bin/sh".into(),
                    "-c".into(),
                    "test -t 0 && printf 'TTY=yes\n' || printf 'TTY=no\n'; printf 'READY\n'; read x; printf 'GOT=%s\n' \"$x\"; while :; do sleep 0.2; done".into(),
                ],
                ..Default::default()
            },
            trigger: Default::default(),
            resume_on_boot: crate::task_store::DEFAULT_RESUME_ON_BOOT,
            delivery: Default::default(),
            limits: TaskLimits {
                timeout_secs: 0,
                grace_secs: 1,
                ..Default::default()
            },
        };
        task.validate().expect("pty proc 任务应合法");
        store.add(task.clone()).unwrap();

        let workspace = root.join("ws");
        std::fs::create_dir_all(&workspace).unwrap();
        let stop = tokio_util::sync::CancellationToken::new();
        let handle = {
            let task = task.clone();
            let states = std::sync::Arc::clone(&states);
            let stop = stop.clone();
            let ws = workspace.to_string_lossy().to_string();
            tokio::spawn(async move { run_proc_attempt_pty(&task, &ws, &states, &stop).await })
        };

        let log_path = reader_states.paths().log_file(&task.id);
        let read_log = |p: &std::path::Path| std::fs::read_to_string(p).unwrap_or_default();
        let mut log = String::new();
        for _ in 0..100 {
            log = read_log(&log_path);
            if log.contains("READY") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(
            log.contains("TTY=yes"),
            "proc(pty) 下 stdin 必须是 TTY：{log:?}"
        );
        assert!(log.contains("READY"), "未在 5s 内看到 READY：{log:?}");

        // 通过输入通道喂文本（与 `task send` 写的是同一个文件约定）
        let dir = reader_states.paths().proc_stdin_dir(&task.id);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            reader_states.paths().proc_stdin_file(&task.id, 1),
            r#"{"text":"hello","enter":true}"#,
        )
        .unwrap();
        for _ in 0..100 {
            log = read_log(&log_path);
            if log.contains("GOT=hello") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(log.contains("GOT=hello"), "输入未生效：{log:?}");

        // 取消收尾：脚本在 GOT 之后长驻（模拟 claude/codex/pi 那种交互会话），
        // 所以这里必须由 cancel 收尾 —— 断言 Cancelled 同时证明「整组杀 + 不挂」
        stop.cancel();
        let rt = tokio::time::timeout(Duration::from_secs(10), handle)
            .await
            .expect("PTY 会话取消后应迅速收尾")
            .expect("join");
        assert_eq!(
            rt.kind,
            crate::task_store::TaskStateKind::Cancelled,
            "{rt:?}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    use crate::task_store::{PayloadKind, TaskLimits, TaskPayload};
    #[cfg(unix)]
    use std::path::{Path, PathBuf};

    #[cfg(unix)]
    fn tmp_root(tag: &str) -> PathBuf {
        let root =
            std::env::temp_dir().join(format!("abb-proc-test-{tag}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    #[cfg(unix)]
    fn proc_task(id: &str, script: &str, log_max_bytes: u64, grace_secs: u64) -> Task {
        Task {
            schema_version: crate::task_store::TASK_SCHEMA_VERSION,
            id: id.to_string(),
            name: String::new(),
            legacy_job_id: String::new(),
            bot_key: "b".to_string(),
            created_by: Default::default(),
            payload: TaskPayload {
                kind: PayloadKind::Proc,
                cmd: vec!["/bin/sh".to_string(), "-c".to_string(), script.to_string()],
                ..Default::default()
            },
            trigger: Default::default(),
            resume_on_boot: crate::task_store::DEFAULT_RESUME_ON_BOOT,
            delivery: Default::default(),
            limits: TaskLimits {
                log_max_bytes,
                grace_secs,
                ..Default::default()
            },
        }
    }

    #[cfg(unix)]
    fn read_log(paths: &crate::task_store::TaskPaths, id: &str) -> String {
        crate::task_store::read_task_logs(paths, id, false).unwrap_or_default()
    }

    #[cfg(unix)]
    fn pid_alive(pid: u32) -> bool {
        use nix::errno::Errno;
        use nix::sys::signal::kill;
        use nix::unistd::Pid;

        match kill(Pid::from_raw(pid as i32), None::<nix::sys::signal::Signal>) {
            Ok(()) | Err(Errno::EPERM) => true,
            Err(_) => false,
        }
    }

    #[cfg(unix)]
    async fn wait_for<F>(timeout: Duration, mut cond: F)
    where
        F: FnMut() -> bool,
    {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            if cond() {
                return;
            }
            assert!(tokio::time::Instant::now() < deadline, "等待测试条件超时");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    #[test]
    fn windows_proc_rejection_is_explicit() {
        let reason = platform_error_for_target("windows").unwrap();
        assert!(
            reason.contains("Q15: Job Object 尚未接入"),
            "Windows 拒绝原因必须点名 Q15 缺口：{reason}"
        );
        assert_eq!(platform_error_for_target("macos"), None);
    }

    #[test]
    fn agent_context_gate_covers_acp_and_legacy_markers() {
        for expected in [
            ACP_AGENT_CONTEXT_ENV,
            "AGENT_BRIDGE_BOT_KEY",
            "AGENT_BRIDGE_CHAT_ID",
            "AGENT_BRIDGE_SENDER_ROLE",
        ] {
            let found = first_nonempty_env(|key| {
                (key == expected).then(|| std::ffi::OsString::from("set"))
            });
            assert_eq!(
                found,
                Some(expected),
                "每个注入变量都必须单独构成 agent 上下文"
            );
        }
        assert_eq!(
            first_nonempty_env(|_| Some(std::ffi::OsString::new())),
            None,
            "空值不应误判为 agent 上下文"
        );
        assert_eq!(first_nonempty_env(|_| None), None);

        let reason = agent_context_rejection_for(Some(ACP_AGENT_CONTEXT_ENV)).unwrap();
        assert!(
            reason.contains("proc 只允许 GUI/人工入口") && reason.contains(ACP_AGENT_CONTEXT_ENV),
            "拒绝原因必须说明入口限制与命中的 agent 标记：{reason}"
        );
        assert_eq!(agent_context_rejection_for(None), None);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn exit_code_is_recorded_and_nonzero_is_failed() {
        let root = tmp_root("exit7");
        let states = TaskStateStore::new_at(&root, "b");
        let task = proc_task("tk_exit7", "exit 7", 1024, 1);
        let stop = tokio_util::sync::CancellationToken::new();
        let rt = run_proc_attempt_with_grace(
            &task,
            root.to_str().unwrap(),
            &states,
            &stop,
            Some(Duration::from_millis(300)),
        )
        .await;

        assert_eq!(rt.kind, TaskStateKind::Failed);
        assert_eq!(rt.last_exit_code, Some(7));
        assert_eq!(rt.pid, None);
        assert_eq!(states.get(&task.id).last_exit_code, Some(7));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn stdout_is_readable_before_process_exit() {
        let root = tmp_root("stream");
        let states = TaskStateStore::new_at(&root, "b");
        let task = proc_task("tk_stream", "/bin/echo stream-visible; sleep 10", 4096, 1);
        let paths = states.paths().clone();
        let stop = tokio_util::sync::CancellationToken::new();
        let run = run_proc_attempt_with_grace(
            &task,
            root.to_str().unwrap(),
            &states,
            &stop,
            Some(Duration::from_millis(300)),
        );
        tokio::pin!(run);
        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        loop {
            tokio::select! {
                rt = &mut run => panic!("进程提前退出，未证明流式可达：{rt:?}"),
                _ = tokio::time::sleep(Duration::from_millis(20)) => {
                    if read_log(&paths, &task.id).contains("stream-visible") {
                        break;
                    }
                    assert!(
                        tokio::time::Instant::now() < deadline,
                        "等待流式日志超时；当前日志={:?}",
                        read_log(&paths, &task.id)
                    );
                }
            }
        }
        assert_eq!(states.get(&task.id).kind, TaskStateKind::Running);
        assert!(states.get(&task.id).pid.is_some(), "运行中应暴露 pid");
        stop.cancel();
        let rt = run.await;
        assert_eq!(rt.kind, TaskStateKind::Cancelled);
        assert_eq!(rt.pid, None);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cancel_request_kills_whole_process_group() {
        let root = tmp_root("pgid");
        let states = TaskStateStore::new_at(&root, "b");
        let parent_file = root.join("parent.pid");
        let child_file = root.join("child.pid");
        let mut task = proc_task(
            "tk_pgid",
            "sleep 60 & echo $! > \"$ABB_CHILD_PID\"; echo $$ > \"$ABB_PARENT_PID\"; wait",
            4096,
            1,
        );
        task.payload.env.insert(
            "ABB_PARENT_PID".to_string(),
            parent_file.display().to_string(),
        );
        task.payload.env.insert(
            "ABB_CHILD_PID".to_string(),
            child_file.display().to_string(),
        );
        let stop = tokio_util::sync::CancellationToken::new();
        let run = run_proc_attempt_with_grace(
            &task,
            root.to_str().unwrap(),
            &states,
            &stop,
            Some(Duration::from_millis(300)),
        );
        tokio::pin!(run);
        let ready_deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        loop {
            tokio::select! {
                rt = &mut run => panic!("进程组探针提前退出：{rt:?}"),
                _ = tokio::time::sleep(Duration::from_millis(20)) => {
                    if parent_file.exists() && child_file.exists() {
                        break;
                    }
                    assert!(
                        tokio::time::Instant::now() < ready_deadline,
                        "等待进程组探针写 pid 超时"
                    );
                }
            }
        }
        let parent: u32 = std::fs::read_to_string(&parent_file)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let child: u32 = std::fs::read_to_string(&child_file)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert!(pid_alive(parent));
        assert!(pid_alive(child));

        std::fs::create_dir_all(states.paths().cancel_requests_dir()).unwrap();
        std::fs::write(states.paths().cancel_file(&task.id), "{}").unwrap();
        let rt = run.await;
        assert_eq!(rt.kind, TaskStateKind::Cancelled);
        wait_for(Duration::from_secs(3), || {
            !pid_alive(parent) && !pid_alive(child)
        })
        .await;
        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn normal_leader_exit_cleans_orphan_group_and_returns() {
        let root = tmp_root("orphan-group");
        let states = TaskStateStore::new_at(&root, "b");
        let parent_file = root.join("parent.pid");
        let child_file = root.join("child.pid");
        let mut task = proc_task(
            "tk_orphan",
            "sleep 60 & echo $! > \"$ABB_CHILD_PID\"; echo $$ > \"$ABB_PARENT_PID\"; exit 0",
            4096,
            1,
        );
        task.payload.env.insert(
            "ABB_PARENT_PID".to_string(),
            parent_file.display().to_string(),
        );
        task.payload.env.insert(
            "ABB_CHILD_PID".to_string(),
            child_file.display().to_string(),
        );
        let stop = tokio_util::sync::CancellationToken::new();
        let rt = tokio::time::timeout(
            Duration::from_secs(6),
            run_proc_attempt_with_grace(
                &task,
                root.to_str().unwrap(),
                &states,
                &stop,
                Some(Duration::from_millis(300)),
            ),
        )
        .await
        .expect("leader 正常退出后必须由有界 drain 兜底返回");

        assert_eq!(rt.kind, TaskStateKind::Succeeded);
        assert_eq!(rt.last_exit_code, Some(0));
        assert_eq!(rt.pid, None);
        let parent: u32 = std::fs::read_to_string(&parent_file)
            .expect("探针应先写出组长 pid")
            .trim()
            .parse()
            .unwrap();
        let child: u32 = std::fs::read_to_string(&child_file)
            .expect("探针应先写出孙进程 pid")
            .trim()
            .parse()
            .unwrap();
        wait_for(Duration::from_secs(3), || !pid_alive(child)).await;
        assert!(!pid_alive(parent), "组长应已被 wait 回收");
        assert!(
            !process_group_alive(parent),
            "leader 退出后的遗留进程组必须被收掉"
        );
        let log = read_log(states.paths(), &task.id);
        assert!(
            log.contains(ORPHAN_GROUP_CLEANUP_LOG),
            "运行日志必须留下遗留进程组清理痕迹：{log}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn sigterm_ignoring_process_is_killed_after_injected_grace() {
        use tokio::process::Command;

        let root = tmp_root("ignore-term");
        let ready = root.join("ready");
        let mut child = Command::new("/bin/sh")
            .arg("-c")
            .arg("trap '' TERM; echo ready > \"$ABB_READY\"; while :; do :; done")
            .env("ABB_READY", &ready)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .process_group(0)
            .spawn()
            .unwrap();
        let pid = child.id().unwrap();
        wait_for(Duration::from_secs(2), || ready.exists()).await;
        let started = tokio::time::Instant::now();
        let stop = stop_process_group(&mut child, pid, Duration::from_millis(300))
            .await
            .unwrap();
        assert!(stop.escalated, "忽略 SIGTERM 必须在宽限后升级 SIGKILL");
        assert!(
            started.elapsed() >= Duration::from_millis(250),
            "升级不能早于注入的 300ms 宽限"
        );
        assert!(!pid_alive(pid), "SIGKILL 后进程必须消失");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn process_exiting_within_grace_is_not_sent_sigkill() {
        use tokio::process::Command;

        let root = tmp_root("term-exit");
        let ready = root.join("ready");
        let mut child = Command::new("/bin/sh")
            .arg("-c")
            .arg("trap 'exit 0' TERM; echo ready > \"$ABB_READY\"; while :; do sleep 1; done")
            .env("ABB_READY", &ready)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .process_group(0)
            .spawn()
            .unwrap();
        let pid = child.id().unwrap();
        wait_for(Duration::from_secs(2), || ready.exists()).await;
        let grace = Duration::from_secs(10);
        let started = tokio::time::Instant::now();
        let stop = stop_process_group(&mut child, pid, grace).await.unwrap();
        assert!(!stop.escalated, "宽限内已退出的进程组不得再收到第二次信号");
        assert!(
            started.elapsed() < grace,
            "进程已退出时不得空等完整个宽限（不作为延迟 SLO，只区分早返回与耗尽 grace）"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn timeout_uses_stop_semantics() {
        let root = tmp_root("timeout");
        let states = TaskStateStore::new_at(&root, "b");
        let ready = root.join("ready");
        let mut task = proc_task(
            "tk_timeout",
            "trap '' TERM; echo ready > \"$ABB_READY\"; while :; do :; done",
            4096,
            1,
        );
        task.limits.timeout_secs = 2;
        task.payload
            .env
            .insert("ABB_READY".to_string(), ready.display().to_string());
        let stop = tokio_util::sync::CancellationToken::new();
        let rt = run_proc_attempt_with_grace(
            &task,
            root.to_str().unwrap(),
            &states,
            &stop,
            Some(Duration::from_millis(300)),
        )
        .await;
        assert_eq!(rt.kind, TaskStateKind::Failed);
        assert!(rt.last_error.contains("执行超时"), "{}", rt.last_error);
        assert_eq!(rt.pid, None);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn streaming_log_rotates_with_bounded_current_segment() {
        let root = tmp_root("rotate");
        let states = TaskStateStore::new_at(&root, "b");
        let max = 64;
        let task = proc_task(
            "tk_rotate",
            "i=0; while [ $i -lt 40 ]; do echo 0123456789; i=$((i+1)); done",
            max,
            1,
        );
        let stop = tokio_util::sync::CancellationToken::new();
        let rt = run_proc_attempt_with_grace(
            &task,
            root.to_str().unwrap(),
            &states,
            &stop,
            Some(Duration::from_millis(300)),
        )
        .await;
        assert_eq!(rt.kind, TaskStateKind::Succeeded);

        let current = states.paths().log_file(&task.id);
        let len = std::fs::metadata(&current).unwrap().len();
        assert!(len < max, "当前日志段应小于上限，实际 {len} >= {max}");
        assert!(Path::new(&format!("{}.1", current.display())).exists());
        assert!(Path::new(&format!("{}.2", current.display())).exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cwd_and_env_are_applied() {
        let root = tmp_root("cwd-env");
        let workspace = root.join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        let states = TaskStateStore::new_at(&root, "b");
        let mut task = proc_task(
            "tk_cwd_env",
            "/bin/pwd; printf '%s\\n' \"$ABB_PROC_TEST\"",
            4096,
            1,
        );
        task.payload
            .env
            .insert("ABB_PROC_TEST".to_string(), "env-ok".to_string());
        let stop = tokio_util::sync::CancellationToken::new();
        let rt = run_proc_attempt_with_grace(
            &task,
            workspace.to_str().unwrap(),
            &states,
            &stop,
            Some(Duration::from_millis(300)),
        )
        .await;
        assert_eq!(rt.kind, TaskStateKind::Succeeded);
        let log = read_log(states.paths(), &task.id);
        assert!(log.contains(workspace.to_str().unwrap()), "{log}");
        assert!(log.contains("env-ok"), "{log}");
        let _ = std::fs::remove_dir_all(&root);
    }
}
