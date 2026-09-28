//! Windows 子进程「抑制控制台窗口」的统一入口（唯一模块）。
//!
//! 本程序的 GUI/服务二进制是 Windows GUI 子系统（见 `main.rs` 顶部
//! `#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]`）：进程自身
//! **没有**控制台。此时 spawn 任何控制台子系统程序（cmd/reg/npm/git/taskkill/
//! explorer/schtasks…），Windows 都会为子进程新分配一个**可见**的控制台窗口——表现
//! 为闪一下黑框；长驻子进程（如 `git cat-file --batch`）窗口更会一直留在桌面上。
//! 唯一正确做法是构造 `Command` 时带上 `CREATE_NO_WINDOW`（`0x0800_0000`）。
//!
//! 历史上这段写法散落在 8 处内联 + 1 处重复实现（tokio 路径另写了一份私有
//! `configure_no_window`），漏一处就漏一个黑框，且新增 spawn 点极易再漏。故收口为
//! 本模块的唯一入口：所有 spawn 子进程的地方都从这里构造命令（[`command`] /
//! [`tokio_command`]）或显式施加（[`no_window`] / [`no_window_tokio`]）。
//!
//! 非 Windows 平台一律 no-op，保证跨平台同一份代码即可编译通过。

use std::process::Command;

/// Windows：`CREATE_NO_WINDOW` —— 子进程不新建控制台窗口。
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// 抑制 std 子进程的控制台窗口。Windows 上设置 `CREATE_NO_WINDOW`；非 Windows 为 no-op
/// （参数在此分支被消费掉，避免 unused 告警）。
pub fn no_window(cmd: &mut Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    #[cfg(not(windows))]
    let _ = cmd;
}

/// [`no_window`] 的 tokio 版本。tokio 的 `Command` 经 `as_std_mut()` 暴露内层 std 命令，
/// 施加同一套抑制语义。
pub fn no_window_tokio(cmd: &mut tokio::process::Command) {
    no_window(cmd.as_std_mut());
}

/// 便利构造：返回一个已抑制控制台窗口的 std 子进程命令。
///
/// 新代码优先用它——「构造即已抑制」，从源头杜绝漏设 `CREATE_NO_WINDOW`。
pub fn command(program: &str) -> Command {
    let mut cmd = Command::new(program);
    no_window(&mut cmd);
    cmd
}

/// 便利构造：[`command`] 的 tokio 版本。
pub fn tokio_command(program: &str) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new(program);
    no_window_tokio(&mut cmd);
    cmd
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 跨主机：在任意平台调用都不能 panic（非 Windows 走 `let _ = cmd` 的 no-op 分支，
    /// Windows 走 `creation_flags` 设置器；两者都不 panic 即契约成立）。
    #[test]
    fn no_window_is_a_noop_or_setter_without_panicking() {
        let mut raw = Command::new("true");
        no_window(&mut raw);

        let mut built = command("true");
        no_window(&mut built);

        let mut traw = tokio::process::Command::new("true");
        no_window_tokio(&mut traw);

        let mut tbuilt = tokio_command("true");
        no_window_tokio(&mut tbuilt);
    }

    /// Windows：`creation_flags(CREATE_NO_WINDOW)` 必须被接受，且不改变命令的可执行性契约。
    /// 设置器无 getter（std/tokio 都不提供），故「构造 + 设置不 panic、可继续链式配置」即
    /// 完整契约（对齐 `crates/buzz-agent/src/mcp.rs` 的两个既有用例）。
    #[cfg(windows)]
    #[test]
    fn no_window_sets_flag_on_windows() {
        use std::os::windows::process::CommandExt;
        let mut cmd = command("cmd.exe");
        cmd.arg("/c").arg("exit").creation_flags(CREATE_NO_WINDOW);

        let mut tcmd = tokio_command("cmd.exe");
        tcmd.as_std_mut()
            .args(["/c", "exit"])
            .creation_flags(CREATE_NO_WINDOW);
    }
}
