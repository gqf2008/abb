//! ABB Windows「降权启动器」（批 `abb-svc-persist-password-gate` 的 B2）。
//!
//! 背景：Windows 上 bridge 由计划任务以 `RunLevel=HighestAvailable` 拉起（高完整性，这样
//! 普通权限的 `taskkill`/任务管理器杀不掉它——owner 要的「登录后服务不能被随便杀死」）。
//! 代价是它 spawn 的子进程会**继承高完整性**：claude/codex 这类 agent 若被诱导，后果就从
//! 用户级升到管理员级。本程序负责把这部分降回来。
//!
//! 用法（由 bridge 侧 `spawn::tokio_command` 在「本进程已提权」时自动加这层包装）：
//! ```text
//! abb-spawner.exe -- <program> [args...]
//! ```
//! 行为：
//! 1. 取**当前会话 shell（explorer.exe）**的主令牌并复制一份 —— 它代表"登录用户的普通权限身份"；
//! 2. 用该令牌 `CreateProcessWithTokenW` 拉起 `<program>`，把**本进程的 stdin/stdout/stderr**
//!    原样交给它（bridge 侧看到的仍是同一条管道，ACP 的 stdio 协议不变）；
//! 3. 等子进程结束，**透传退出码**。
//!
//! fail-closed：找不到 explorer 的令牌（无桌面会话/被安全软件拦）时**拒绝启动并报错**，
//! 绝不"退回用继承来的管理员身份启动"——那正是本程序要防的事。
//!
//! 非 Windows 平台为占位 main（拒绝运行），保证全平台构建一致（与 `abb-helper` 同风格）。

#[cfg(not(target_os = "windows"))]
fn main() {
    eprintln!("abb-spawner 仅 Windows 可用（其它平台的 bridge 就是普通用户身份，无需降权启动器）");
    std::process::exit(1);
}

#[cfg(target_os = "windows")]
fn main() {
    std::process::exit(win::run());
}

#[cfg(target_os = "windows")]
mod win {
    use windows::core::{PCWSTR, PWSTR};
    use windows::Win32::Foundation::{CloseHandle, HANDLE};
    use windows::Win32::Security::{
        DuplicateTokenEx, SecurityImpersonation, TokenPrimary, TOKEN_ALL_ACCESS, TOKEN_DUPLICATE,
        TOKEN_QUERY,
    };
    use windows::Win32::System::Console::{
        GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
    };
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
        TH32CS_SNAPPROCESS,
    };
    use windows::Win32::System::Threading::{
        CreateProcessWithTokenW, GetExitCodeProcess, OpenProcess, OpenProcessToken,
        WaitForSingleObject, CREATE_NO_WINDOW, LOGON_WITH_PROFILE, PROCESS_INFORMATION,
        PROCESS_QUERY_LIMITED_INFORMATION, STARTF_USESTDHANDLES, STARTUPINFOW,
    };

    /// 退出码：用法错（没给 `--`/程序）。
    const EXIT_USAGE: i32 = 2;
    /// 退出码：拿不到桌面 shell 的令牌（fail-closed，拒绝以高完整性启动 agent）。
    const EXIT_NO_TOKEN: i32 = 3;
    /// 退出码：创建子进程失败。
    const EXIT_SPAWN: i32 = 4;

    /// argv → (program, args)；要求 `--` 分隔（bridge 侧统一这么传，避免参数被本程序误吃）。
    pub fn split_args(argv: Vec<String>) -> Option<(String, Vec<String>)> {
        let mut it = argv.into_iter();
        let _exe = it.next()?;
        // 跳过可能存在的第一个 `--`
        let mut rest: Vec<String> = it.collect();
        if rest.first().map(String::as_str) == Some("--") {
            rest.remove(0);
        }
        if rest.is_empty() {
            return None;
        }
        let program = rest.remove(0);
        Some((program, rest))
    }

    /// 当前进程 pid 对应的会话里，找到 explorer.exe 的 pid。
    ///
    /// 为什么要 explorer：它由系统在用户登录会话里以**普通（中）完整性**启动，是"这台机器上
    /// 该用户的普通权限身份"的权威代表（UAC 提权进程的令牌不是）。
    fn explorer_pid() -> Option<u32> {
        // SAFETY: 只读快照 + 固定大小结构体；句柄在本函数内关闭。
        unsafe {
            let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0).ok()?;
            let mut entry = PROCESSENTRY32W {
                dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
                ..Default::default()
            };
            let mut found = None;
            if Process32FirstW(snap, &mut entry).is_ok() {
                loop {
                    let name = String::from_utf16_lossy(
                        &entry.szExeFile[..entry
                            .szExeFile
                            .iter()
                            .position(|c| *c == 0)
                            .unwrap_or(entry.szExeFile.len())],
                    );
                    if name.eq_ignore_ascii_case("explorer.exe") {
                        found = Some(entry.th32ProcessID);
                        break;
                    }
                    if Process32NextW(snap, &mut entry).is_err() {
                        break;
                    }
                }
            }
            let _ = CloseHandle(snap);
            found
        }
    }

    /// 取 explorer 的**主令牌副本**（可跨进程用于 CreateProcessWithTokenW）。
    fn shell_token() -> Option<HANDLE> {
        // SAFETY: 句柄在失败路径逐个关闭；成功时返回的令牌句柄由调用方关闭。
        unsafe {
            let pid = explorer_pid()?;
            let proc = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
            let mut token = HANDLE::default();
            let opened = OpenProcessToken(proc, TOKEN_DUPLICATE | TOKEN_QUERY, &mut token);
            let _ = CloseHandle(proc);
            opened.ok()?;
            let mut dup = HANDLE::default();
            let r = DuplicateTokenEx(
                token,
                // 主令牌 + 全权（新进程要能用它 CreateProcessWithTokenW）
                TOKEN_ALL_ACCESS,
                None,
                SecurityImpersonation,
                TokenPrimary,
                &mut dup,
            );
            let _ = CloseHandle(token);
            r.ok()?;
            Some(dup)
        }
    }

    /// 把 argv 拼成 Windows 命令行（CreateProcessWithTokenW 收的是**单个字符串**）。
    ///
    /// 按 MSVCRT 规则加引号：含空白/引号/反斜杠结尾时包引号，内部 `"` 用 `\"`，反斜杠按
    /// 其后是否跟引号决定是否加倍（与 Rust std 的 `append_quoted` 同源做法）。
    pub fn build_command_line(program: &str, args: &[String]) -> Vec<u16> {
        fn quote_into(s: &str, out: &mut String) {
            let needs = s.is_empty() || s.chars().any(|c| c == ' ' || c == '\t' || c == '"');
            if !needs {
                out.push_str(s);
                return;
            }
            out.push('"');
            let mut backslashes = 0usize;
            for c in s.chars() {
                match c {
                    '\\' => {
                        backslashes += 1;
                        out.push('\\');
                    }
                    '"' => {
                        for _ in 0..backslashes + 1 {
                            out.push('\\');
                        }
                        backslashes = 0;
                        out.push('"');
                    }
                    _ => {
                        backslashes = 0;
                        out.push(c);
                    }
                }
            }
            for _ in 0..backslashes {
                out.push('\\');
            }
            out.push('"');
        }
        let mut line = String::new();
        quote_into(program, &mut line);
        for a in args {
            line.push(' ');
            quote_into(a, &mut line);
        }
        line.encode_utf16().chain(std::iter::once(0)).collect()
    }

    pub fn run() -> i32 {
        let argv: Vec<String> = std::env::args().collect();
        let Some((program, args)) = split_args(argv) else {
            eprintln!("用法：abb-spawner.exe -- <program> [args...]");
            return EXIT_USAGE;
        };
        let Some(token) = shell_token() else {
            // fail-closed：拿不到桌面 shell 的令牌就拒绝启动（绝不回退成"继承管理员身份启动"）。
            eprintln!(
                "abb-spawner: 找不到当前会话桌面 shell（explorer.exe）的令牌，拒绝以高完整性启动 {program}"
            );
            return EXIT_NO_TOKEN;
        };

        // SAFETY: 句柄来自 GetStdHandle（可能是 INVALID/0，交给 Windows 判断）；令牌在本函数内关闭。
        unsafe {
            let si = STARTUPINFOW {
                cb: std::mem::size_of::<STARTUPINFOW>() as u32,
                dwFlags: STARTF_USESTDHANDLES,
                hStdInput: GetStdHandle(STD_INPUT_HANDLE).unwrap_or_default(),
                hStdOutput: GetStdHandle(STD_OUTPUT_HANDLE).unwrap_or_default(),
                hStdError: GetStdHandle(STD_ERROR_HANDLE).unwrap_or_default(),
                ..Default::default()
            };
            let mut pi = PROCESS_INFORMATION::default();
            let mut cmdline = build_command_line(&program, &args);
            let r = CreateProcessWithTokenW(
                token,
                LOGON_WITH_PROFILE,
                PCWSTR::null(),
                PWSTR(cmdline.as_mut_ptr()),
                CREATE_NO_WINDOW,
                None,
                PCWSTR::null(),
                &si,
                &mut pi,
            );
            let _ = CloseHandle(token);
            if r.is_err() {
                eprintln!(
                    "abb-spawner: CreateProcessWithTokenW 失败（{}），未启动 {program}",
                    windows::core::Error::from_win32()
                );
                return EXIT_SPAWN;
            }
            // 等它结束并透传退出码：bridge 侧看到的仍是同一条管道 + 同一个"子进程"。
            let _ = WaitForSingleObject(pi.hProcess, u32::MAX);
            let mut code = 0u32;
            let _ = GetExitCodeProcess(pi.hProcess, &mut code);
            let _ = CloseHandle(pi.hThread);
            let _ = CloseHandle(pi.hProcess);
            code as i32
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn split_args_requires_program_after_dashdash() {
            let v = |s: &[&str]| s.iter().map(|x| x.to_string()).collect::<Vec<_>>();
            assert_eq!(
                split_args(v(&["abb-spawner.exe", "--", "claude", "--acp"])),
                Some(("claude".to_string(), v(&["--acp"])))
            );
            // 没有 `--` 也接受（第一个参数就是程序），但不能是空
            assert_eq!(
                split_args(v(&["abb-spawner.exe", "codex"])),
                Some(("codex".to_string(), v(&[])))
            );
            assert_eq!(split_args(v(&["abb-spawner.exe", "--"])), None);
            assert_eq!(split_args(v(&["abb-spawner.exe"])), None);
        }

        #[test]
        fn command_line_quotes_like_msvcrt() {
            let w = |s: &str| build_command_line(s, &[]);
            // 无空白 → 原样
            assert_eq!(
                String::from_utf16_lossy(&w("C:\\a\\b.exe")[..8]),
                "C:\\a\\b.e"
            );
            // 含空格 → 包引号
            let s = String::from_utf16_lossy(&build_command_line(
                "C:\\Program Files\\x.exe",
                &["--flag".to_string(), "a b".to_string()],
            ));
            assert!(
                s.starts_with("\"C:\\Program Files\\x.exe\" --flag \"a b\""),
                "{s}"
            );
            // 尾部反斜杠 + 引号：反斜杠必须加倍（否则会把结尾引号转义掉）
            let s = String::from_utf16_lossy(&build_command_line("C:\\dir with space\\", &[]));
            assert_eq!(s, "\"C:\\dir with space\\\\\"");
        }
    }
}
