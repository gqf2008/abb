//! `proc` 的进程代际身份：PID 会被系统复用，单看 PID 不能证明旧进程还活着。
//!
//! 身份由操作系统提供的启动标记 + 命令行快照组成。恢复时必须两者同时匹配才把
//! 旧进程视为仍存活；查不到身份时调用方应 fail-closed（不 adopt、不重复拉起）。

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcIdentity {
    pub pid: u32,
    /// OS 进程启动标记。Linux 用 `/proc/<pid>/stat` 的 starttime；macOS 用
    /// `sysctl(KERN_PROC_PID)` 取到的 `p_starttime`（`macos-<sec>.<usec>`）。
    ///
    /// ⚠️ macOS 这一栏的格式在卡 `abb-task-identity-setuid-20261028` 里**变过一次**：
    /// 旧值是 `ps -o lstart=` 的文本（`Fri Nov 15 10:23:45 2024`，秒粒度 + 本地时区），
    /// 新值是 epoch 微秒（不受时区影响、能区分同一秒内的 PID 复用）。老记录与新采集
    /// 必然逐字不符 ⇒ **不能**直接逐字比对（那会得到一个无根据的 `Dead`），因此
    /// `verify()` 先把「不是本代格式」的老值截成 `Unknown`；旧进程真不在了则仍走上游的
    /// `Dead` 自动恢复。见 `proc_sysctl::start_token` 的迁移说明与 `verify()` 的守门注释。
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
    // 升级迁移守门（macOS）：`start_token` 的采集格式在卡 `abb-task-identity-setuid-20261028`
    // 里从 `ps -o lstart=` 文本换成了 `macos-<sec>.<usec>`（见 `proc_sysctl::start_token`）。
    // 老记录与新采集**无法互译**（旧值只有秒粒度且带本地时区），逐字比对必不符。
    //
    // 若不在这里截住，比对结果会是 `Dead`——而 `Dead` 的语义是「旧代际**确实**不存在」。
    // 那可说不得：此刻 pid 还在、只是我们验不了它的代际，正是契约里 `Unknown`
    //（「进程可能活着，但无法核验」）的教科书情形。误判 `Dead` 的代价是
    // `recover_keepalives` 会退避后重新拉起，而旧 keepalive 是 `setsid` 独立进程组、
    // 可能仍然活着 → **双实例**（重复投递/重复计费），正是本模块存在的理由。
    //
    // 顺序很关键：上面两条 `Dead` 已经在最前面——旧记录对应的进程若真的没了（升级前就
    // 崩掉了，这是最常见的情形），照样走 `Dead` 自动恢复，升级零感知。
    #[cfg(target_os = "macos")]
    if !identity
        .start_token
        .starts_with(crate::proc_sysctl::TOKEN_PREFIX)
    {
        return IdentityStatus::Unknown;
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

#[cfg(target_os = "macos")]
fn process_start_token(pid: u32) -> Option<String> {
    crate::proc_sysctl::start_token(pid)
}

/// 非 macOS / 非 Linux 的 unix（FreeBSD 之类：本仓不构建、也没有原生实现）→ 一律 `None`，
/// 由 `verify()` 判 `Unknown`（fail-closed：不 adopt、不重拉）。**不**回落到 `/bin/ps`：
/// macOS 上那是 setuid 二进制、沙箱内 exec 必失败，留一条「看起来能用、实际必坏」的路径
/// 只会掩盖问题（卡 `abb-task-identity-setuid-20261028`）。
#[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
fn process_start_token(_pid: u32) -> Option<String> {
    None
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

#[cfg(target_os = "macos")]
fn process_command_line(pid: u32) -> Option<String> {
    crate::proc_sysctl::command_line(pid)
}

#[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
fn process_command_line(_pid: u32) -> Option<String> {
    None
}

#[cfg(target_os = "macos")]
fn process_is_zombie(pid: u32) -> bool {
    // 与旧实现（`ps stat` 以 'Z' 开头）同语义：读不到 → `false`（不判僵尸），
    // 让 `verify()` 继续走后面的判据、必要时落到 `Unknown`。
    crate::proc_sysctl::is_zombie(pid)
}

#[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
fn process_is_zombie(_pid: u32) -> bool {
    false
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

    /// 升级迁移守门：老记录（`ps -o lstart=` 文本）必须判 `Unknown`，**不能**判 `Dead`。
    ///
    /// 为什么这条是关键：`Dead` 会让 `recover_keepalives` 退避后重拉，而旧 keepalive
    /// 是 `setsid` 独立进程组、可能仍活着 → 双实例。`Unknown` 的语义（「可能活着，无法
    /// 核验」）才是事实。
    ///
    /// 判别力（均实测过）：
    /// - 删掉 `verify()` 里那道守门 → 第一条断言拿到 `Dead` ⇒ 红。
    /// - 把守门**提到** `pid_exists`/僵尸两条 `Dead` 之前（顺序错了）→ 第二条断言拿到
    ///   `Unknown` ⇒ 红。这道用例因此同时钉住了「判据存在」与「判据位置」。
    /// - 单独把守门写成【恒 Unknown】不会红第二条（`pid_exists` 在上游先返回 `Dead`）——
    ///   这是好事：说明守门遮蔽不了自动恢复路径。
    #[cfg(target_os = "macos")]
    #[test]
    fn legacy_ps_lstart_token_verifies_as_unknown_not_dead() {
        let mut child = std::process::Command::new("/bin/sleep")
            .arg("2")
            .spawn()
            .unwrap();
        let pid = child.id();
        let mut identity = capture(pid).expect("live child should have identity");
        // 模拟升级前落盘的记录：旧 `ps -o lstart=` 文本 + 旧空格连接的命令行。
        identity.start_token = "Fri Nov 15 10:23:45 2024".to_string();
        identity.command_line = "/bin/sleep 2".to_string();
        assert_eq!(
            verify(&identity),
            IdentityStatus::Unknown,
            "老格式记录 + 进程仍存活 ⇒ 无法核验（Unknown），不得判 Dead（会造双实例）"
        );

        // 反向：进程真的不在了，照样得走 Dead 自动恢复（守门不能把升级变成一律 Unknown）。
        let _ = child.kill();
        let _ = child.wait();
        assert_eq!(
            verify(&identity),
            IdentityStatus::Dead,
            "老记录但进程已不在 ⇒ 必须 Dead（否则 keepalive 升级后再也不自动恢复）"
        );
    }
}
