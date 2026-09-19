//! `proc` 的进程代际身份：PID 会被系统复用，单看 PID 不能证明旧进程还活着。
//!
//! 身份由操作系统提供的启动标记 + 命令行快照组成。恢复时必须两者同时匹配才把
//! 旧进程视为仍存活；查不到身份时调用方应 fail-closed（不 adopt、不重复拉起）。

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcIdentity {
    pub pid: u32,
    /// OS 进程启动标记。Linux 用 `/proc/<pid>/stat` 的 starttime；macOS 用 `ps lstart`。
    pub start_token: String,
    /// 启动时采集的命令行快照；后续必须逐字匹配。
    pub command_line: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(not(unix), allow(dead_code))]
pub enum IdentityStatus {
    Alive,
    Dead,
    /// 进程可能活着，但权限/系统工具导致无法核验；调用方必须按“可能活着”处理。
    Unknown,
}

/// 采集当前进程身份。进程刚退出/无法读取命令行时返回 `None`。
#[cfg(unix)]
pub fn capture(pid: u32) -> Option<ProcIdentity> {
    let start_token = process_start_token(pid)?;
    let command_line = process_command_line(pid)?;
    Some(ProcIdentity {
        pid,
        start_token,
        command_line,
    })
}

#[cfg(not(unix))]
#[cfg_attr(not(unix), allow(dead_code))]
pub fn capture(_pid: u32) -> Option<ProcIdentity> {
    None
}

/// 核验保存的身份是否仍指向同一个进程。任何读取失败都返回 `Unknown`，不返回
/// `Dead`——后者会在旧进程仍活着但 `ps/proc` 暂时不可读时制造双实例。
#[cfg(unix)]
pub fn verify(identity: &ProcIdentity) -> IdentityStatus {
    if !pid_exists(identity.pid) {
        return IdentityStatus::Dead;
    }
    if process_is_zombie(identity.pid) {
        return IdentityStatus::Dead;
    }
    let Some(start_token) = process_start_token(identity.pid) else {
        return IdentityStatus::Unknown;
    };
    if start_token != identity.start_token {
        return IdentityStatus::Dead; // PID 已复用，旧代际确实不存在。
    }
    let Some(command_line) = process_command_line(identity.pid) else {
        return IdentityStatus::Unknown;
    };
    if command_line != identity.command_line {
        return IdentityStatus::Dead; // 同一 PID 已换命令，旧代际不存在。
    }
    IdentityStatus::Alive
}

#[cfg(not(unix))]
pub fn verify(_identity: &ProcIdentity) -> IdentityStatus {
    IdentityStatus::Unknown
}

#[cfg(unix)]
fn pid_exists(pid: u32) -> bool {
    use nix::errno::Errno;
    use nix::sys::signal::kill;
    use nix::unistd::Pid;

    match kill(Pid::from_raw(pid as i32), None::<nix::sys::signal::Signal>) {
        Ok(()) | Err(Errno::EPERM) => true,
        Err(_) => false,
    }
}

#[cfg(target_os = "linux")]
fn process_start_token(pid: u32) -> Option<String> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // comm 可能含空格/右括号，字段 3 从最后一个 ')' 之后开始；starttime 是字段 22。
    let tail = stat.rsplit_once(')')?.1;
    tail.split_whitespace().nth(19).map(str::to_string)
}

#[cfg(all(unix, not(target_os = "linux")))]
fn process_start_token(pid: u32) -> Option<String> {
    ps_field(pid, "lstart")
}

#[cfg(target_os = "linux")]
fn process_command_line(pid: u32) -> Option<String> {
    let bytes = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    if bytes.is_empty() {
        return None;
    }
    let args: Vec<String> = bytes
        .split(|b| *b == 0)
        .filter(|arg| !arg.is_empty())
        .map(|arg| String::from_utf8_lossy(arg).into_owned())
        .collect();
    (!args.is_empty()).then(|| args.join("\u{1f}"))
}

#[cfg(all(unix, not(target_os = "linux")))]
fn process_command_line(pid: u32) -> Option<String> {
    ps_field(pid, "command")
}

#[cfg(all(unix, not(target_os = "linux")))]
fn process_is_zombie(pid: u32) -> bool {
    ps_field(pid, "stat")
        .map(|state| state.trim_start().starts_with('Z'))
        .unwrap_or(false)
}

#[cfg(target_os = "linux")]
fn process_is_zombie(pid: u32) -> bool {
    let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
        return false;
    };
    stat.rsplit_once(')')
        .and_then(|(_, tail)| tail.split_whitespace().next())
        .is_some_and(|state| state.starts_with('Z'))
}

#[cfg(all(unix, not(target_os = "linux")))]
fn ps_field(pid: u32, field: &str) -> Option<String> {
    let output = std::process::Command::new("/bin/ps")
        .args(["-p", &pid.to_string(), "-o", &format!("{field}=")])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let value = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!value.is_empty()).then_some(value)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn captured_identity_matches_live_process_and_rejects_stale_token() {
        let mut child = std::process::Command::new("/bin/sleep")
            .arg("2")
            .spawn()
            .unwrap();
        let pid = child.id();
        let mut identity = capture(pid).expect("live child should have identity");
        assert_eq!(verify(&identity), IdentityStatus::Alive);

        identity.start_token.push_str("-stale");
        assert_eq!(verify(&identity), IdentityStatus::Dead);

        let _ = child.kill();
        let _ = child.wait();
    }
}
