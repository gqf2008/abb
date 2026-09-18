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
    run_proc_attempt_with_grace(task, workspace, states, stop, None).await
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
        last_error,
        last_fired_at: prev.last_fired_at,
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
            bot_key: "b".to_string(),
            created_by: Default::default(),
            payload: TaskPayload {
                kind: PayloadKind::Proc,
                cmd: vec!["/bin/sh".to_string(), "-c".to_string(), script.to_string()],
                ..Default::default()
            },
            trigger: Default::default(),
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
        let started = tokio::time::Instant::now();
        let stop = stop_process_group(&mut child, pid, Duration::from_secs(2))
            .await
            .unwrap();
        assert!(!stop.escalated, "宽限内已退出的进程组不得再收到第二次信号");
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "已退出应快速返回，不空等完宽限"
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
