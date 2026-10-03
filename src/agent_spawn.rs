//! agent 子进程的构造入口（**只属于 bin**）。
//!
//! 与 spawn.rs 的分工：spawn.rs 是 lib/bin 共用的「抑制控制台窗口」叶子模块（纯构造）；
//! 本模块额外持有 Windows 的「句柄关闭即杀」job 守卫（杀 agent 树），并负责在「ABB 被以
//! 管理员身份启动」时**响亮告警**（新模型不允许那种形态）。只声明在 main.rs。
//!
//! ## 历史（2026-10-04 起删除降权层）
//! 以前 Windows 的 bridge 由 RunLevel=HighestAvailable 的计划任务拉起（高完整性），子进程会
//! 继承该完整性 ⇒ 本模块会把命令包一层 abb-spawner.exe（用桌面 shell 令牌把权限降回普通用户），
//! 外加一条 relay/环境文件链。owner 2026-10-04 决定「全部以普通用户运行，启动/停止统一由 ABB
//! 管理密码把关」⇒ 计划任务与 abb-spawner / abb-elev-helper / abb-helper 一并删除：agent 天然
//! 就是普通用户身份，这里不再需要任何降权包装（那条链也正是「agent 起不来 / 丢环境」类故障的温床）。
//!
//! ## 仍然保留的一条红线
//! 若有人**手动以管理员身份**启动 ABB，agent 会继承管理员权限 —— 这不是新模型允许的形态，故
//! tokio_command 会显式告警（bridge.out 留痕）。**不阻断**：阻断会让「以管理员跑 ABB」变成
//! 完全不可用，代价过大；一条响亮告警足以让维护者一眼看到根因。

/// 构造 agent 子进程命令（tokio 版，ACP 走这条）。
pub fn tokio_command(program: &str) -> tokio::process::Command {
    #[cfg(windows)]
    if agent_bridge::elev::win::is_elevated() {
        crate::log!(
            "[spawn] 本进程正以管理员权限运行：agent「{program}」会继承该权限。新模型要求 ABB 以普通用户运行（退出后用普通身份启动即可）"
        );
    }
    crate::spawn::tokio_command(program)
}

/// 「句柄关闭即杀」的 job 守卫（Windows）。
///
/// 持有 job 句柄本身；**Drop 时关闭 ⇒ 内核连带杀掉 job 里仍活着的整棵树** —— 也就是
/// agent 及其 spawn 出来的 MCP 服务等后代。语义 =「bridge 死，agent 树一起死」。
///
/// 非 Windows 恒为空壳（unix 用进程组 kill 表达同一语义，见 `buzz/acp.rs::kill_process_group`）。
pub struct KillOnCloseJob {
    #[cfg(windows)]
    handle: windows::Win32::Foundation::HANDLE,
}

// SAFETY: job 句柄是**进程级内核对象句柄**（本质是个整数），跨线程移动/共享不涉及别名内存；
// 关闭只在 Drop 里做一次。AcpClient 需要 Send（要进 JoinSet），故显式放开。
#[cfg(windows)]
unsafe impl Send for KillOnCloseJob {}
#[cfg(windows)]
unsafe impl Sync for KillOnCloseJob {}

impl Drop for KillOnCloseJob {
    fn drop(&mut self) {
        #[cfg(windows)]
        unsafe {
            let _ = windows::Win32::Foundation::CloseHandle(self.handle);
        }
    }
}

/// 把 `pid` 放进一个 `KILL_ON_JOB_CLOSE` job，返回守卫（拿不到就返回 None）。
///
/// **为什么需要**（2026-09-30 owner 实报「桌上堆了 10 个 wassette」）：Windows 上杀 agent
/// 只作用于**直接子进程**（`buzz/acp.rs` 的 `kill_process_group` 在非 unix 是 stub），而
/// agent 为每个 session 起的 MCP 服务（`wassette`、`mcp-events`）是**孙**进程 —— 于是一次
/// agent 崩溃/被杀就留下一个常驻孤儿。job 成员默认被后代继承，所以套一层就够。
///
/// 失败**不 fail-closed**：少一层「后代连带杀」保证而已，agent 本身照常能跑；记一行日志便于诊断。
#[cfg(windows)]
pub fn assign_kill_on_close_job(pid: u32) -> Option<KillOnCloseJob> {
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
        SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };
    use windows::Win32::System::Threading::{OpenProcess, PROCESS_SET_QUOTA, PROCESS_TERMINATE};

    // SAFETY: 全部句柄都在本函数内创建/校验，失败路径逐个关闭。
    unsafe {
        let job = match CreateJobObjectW(None, PCWSTR::null()) {
            Ok(j) => j,
            Err(_) => return None,
        };
        let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        if SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            &info as *const _ as *const core::ffi::c_void,
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        )
        .is_err()
        {
            let _ = CloseHandle(job);
            return None;
        }
        let Ok(proc) = OpenProcess(PROCESS_SET_QUOTA | PROCESS_TERMINATE, false, pid) else {
            let _ = CloseHandle(job);
            return None;
        };
        let assigned = AssignProcessToJobObject(job, proc);
        let _ = CloseHandle(proc);
        if assigned.is_err() {
            // 常见原因：本进程已在某个不允许嵌套的 job 里（Win7）或权限不足。
            crate::log!(
                "[spawn] 把 agent(pid={pid}) 放进 job 失败（它的后代不会被连带杀）：{}",
                windows::core::Error::from_win32()
            );
            let _ = CloseHandle(job);
            return None;
        }
        Some(KillOnCloseJob { handle: job })
    }
}

#[cfg(not(windows))]
pub fn assign_kill_on_close_job(_pid: u32) -> Option<KillOnCloseJob> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Windows：job 守卫 Drop 后，内核必须**立刻杀掉 job 里的进程** —— 这就是
    /// 「agent 死 ⇒ 它起的 MCP 服务（wassette 等后代）跟着死」的机制本身。
    ///
    /// 为什么要真起进程测：这是行为契约（KILL_ON_JOB_CLOSE + AssignProcessToJobObject 的组合
    /// 生效），纯逻辑断言证明不了；而「漏一层」的代价是每个会话留一个常驻孤儿（2026-09-30 实报）。
    #[cfg(windows)]
    #[test]
    fn kill_on_close_job_kills_member_on_drop() {
        // 走 crate::spawn::command（统一入口，顺带满足 spawn.rs 的源码护栏）
        let mut child = crate::spawn::command("cmd.exe")
            .args(["/c", "ping -n 30 127.0.0.1 >nul"])
            .spawn()
            .expect("spawn cmd");
        let pid = child.id();
        let job = assign_kill_on_close_job(pid).expect("把子进程放进 job");
        std::thread::sleep(std::time::Duration::from_millis(200));
        assert!(
            child.try_wait().expect("try_wait").is_none(),
            "前置：子进程应在运行"
        );
        drop(job); // 关句柄 ⇒ KILL_ON_JOB_CLOSE 生效
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if child.try_wait().expect("try_wait").is_some() {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "job 句柄关闭后子进程仍存活 —— KILL_ON_JOB_CLOSE 没生效"
            );
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }
}
