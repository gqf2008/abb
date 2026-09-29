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
    use windows::Win32::Foundation::{
        CloseHandle, SetHandleInformation, HANDLE, HANDLE_FLAG_INHERIT,
    };
    use windows::Win32::Security::{
        DuplicateTokenEx, GetTokenInformation, SecurityImpersonation, TokenPrimary, TokenUser,
        TOKEN_ALL_ACCESS, TOKEN_DUPLICATE, TOKEN_QUERY,
    };
    use windows::Win32::System::Console::{
        GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
    };
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
        TH32CS_SNAPPROCESS,
    };
    use windows::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
        SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };
    use windows::Win32::System::Threading::{
        CreateProcessWithTokenW, GetCurrentProcess, GetExitCodeProcess, OpenProcess,
        OpenProcessToken, ResumeThread, TerminateProcess, WaitForSingleObject, CREATE_NO_WINDOW,
        CREATE_SUSPENDED, LOGON_WITH_PROFILE, PROCESS_INFORMATION,
        PROCESS_QUERY_LIMITED_INFORMATION, STARTF_USESTDHANDLES, STARTUPINFOW,
    };
    use windows::Win32::UI::WindowsAndMessaging::{GetShellWindow, GetWindowThreadProcessId};

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

    /// toolhelp 扫全机找第一个 explorer.exe（`GetShellWindow` 的兜底路径）。
    fn explorer_pid() -> Option<u32> {
        // SAFETY: 只读快照 + 固定大小结构体；句柄在函数内关闭。
        unsafe {
            let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0).ok()?;
            let mut entry = PROCESSENTRY32W {
                dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
                ..Default::default()
            };
            let mut found = None;
            if Process32FirstW(snap, &mut entry).is_ok() {
                loop {
                    let end = entry
                        .szExeFile
                        .iter()
                        .position(|c| *c == 0)
                        .unwrap_or(entry.szExeFile.len());
                    let name = String::from_utf16_lossy(&entry.szExeFile[..end]);
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

    /// 当前会话的**桌面 shell** pid：优先 `GetShellWindow()`（就是我们自己会话的 shell），
    /// 取不到再退回 toolhelp 里第一个 explorer.exe。
    ///
    /// 评审 R23 B2-c：只按进程名找 explorer 会在多用户/快速切换的机器上选中**别人**的 shell，
    /// 于是 agent 会以那个人的身份跑（读到别人的 `~/.claude`）。故这里①优先 GetShellWindow，
    /// ②拿到令牌后**再校验 SID**（见 [`shell_token`]）。
    fn shell_pid() -> Option<u32> {
        // SAFETY: GetShellWindow 无参数；GetWindowThreadProcessId 只读。
        unsafe {
            let hwnd = GetShellWindow();
            if !hwnd.is_invalid() {
                let mut pid = 0u32;
                GetWindowThreadProcessId(hwnd, Some(&mut pid));
                if pid != 0 {
                    return Some(pid);
                }
            }
        }
        explorer_pid()
    }

    /// 取某个**令牌句柄**里的用户 SID 的**原始字节**（用于「令牌是不是本用户」的硬校验）。
    ///
    /// 直接比字节而不是转字符串：少一次 LocalAlloc/格式转换（评审 R23 的「句柄生命周期最易错」），
    /// SID 的内存布局是固定的（Revision/SubAuthorityCount/IdentifierAuthority/SubAuthority）。
    fn token_user_sid_bytes(token: HANDLE) -> Option<Vec<u8>> {
        // SAFETY: 两次 GetTokenInformation（第一次问长度），缓冲按 usize 对齐。
        unsafe {
            let mut len = 0u32;
            let _ = GetTokenInformation(token, TokenUser, None, 0, &mut len);
            if len == 0 {
                return None;
            }
            let words = (len as usize).div_ceil(std::mem::size_of::<usize>());
            let mut buf = vec![0usize; words];
            GetTokenInformation(
                token,
                TokenUser,
                Some(buf.as_mut_ptr() as *mut core::ffi::c_void),
                len,
                &mut len,
            )
            .ok()?;
            // TOKEN_USER { User: SID_AND_ATTRIBUTES { Sid: PSID, .. } }
            let sid = *(buf.as_ptr() as *const *const u8);
            if sid.is_null() {
                return None;
            }
            let count = *sid.add(1) as usize;
            let size = 8 + 4 * count; // SID 头 8 字节 + 4 字节/子认证机构
            Some(std::slice::from_raw_parts(sid, size).to_vec())
        }
    }

    /// 本进程（当前用户）的 SID 字节。
    fn own_sid() -> Option<Vec<u8>> {
        // SAFETY: 打开自身令牌后立即关闭。
        unsafe {
            let mut token = HANDLE::default();
            OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).ok()?;
            let sid = token_user_sid_bytes(token);
            let _ = CloseHandle(token);
            sid
        }
    }

    /// 取当前会话 shell 的**主令牌副本**，并校验它确实属于**本用户**（评审 R23 B2-c）。
    fn shell_token() -> Option<HANDLE> {
        // SAFETY: 句柄在失败路径逐个关闭；成功时返回的令牌句柄由调用方关闭。
        unsafe {
            let pid = shell_pid()?;
            let proc = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
            let mut token = HANDLE::default();
            let opened = OpenProcessToken(proc, TOKEN_DUPLICATE | TOKEN_QUERY, &mut token);
            let _ = CloseHandle(proc);
            opened.ok()?;
            // 硬校验：shell 令牌的用户必须与本进程一致 —— 否则在多用户机器上会把 agent
            // 跑成别人的身份（能读到别人的凭据/配置）。不等就 fail-closed。
            let shell_sid = token_user_sid_bytes(token);
            let mine = own_sid();
            if shell_sid.is_none() || mine.is_none() || shell_sid != mine {
                let _ = CloseHandle(token);
                return None;
            }
            let mut dup = HANDLE::default();
            let r = DuplicateTokenEx(
                token,
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
            // 三个 std 句柄必须是**可继承**的，子进程才拿得到（否则 agent 的 stdio 全空）。
            // 评审 R23 B2-a：`CreateProcessWithTokenW` 在 STARTF_USESTDHANDLES 下把这三个字段
            // **原样拷给子进程**，只要句柄可继承即可（.NET 的 Process、wez/EleDo 的 deelevate
            // 都这么用）—— 不需要 `CreateProcessAsUser*` 那条需要额外特权（SeAssignPrimaryToken）
            // 的路。句柄可继承位在此显式设置，别依赖调用方恰好设过。
            for h in [si.hStdInput, si.hStdOutput, si.hStdError] {
                if !h.is_invalid() {
                    let _ = SetHandleInformation(h, HANDLE_FLAG_INHERIT.0, HANDLE_FLAG_INHERIT);
                }
            }
            let r = CreateProcessWithTokenW(
                token,
                LOGON_WITH_PROFILE,
                PCWSTR::null(),
                PWSTR(cmdline.as_mut_ptr()),
                // CREATE_SUSPENDED：先挂起创建，等 job 指派完成再放它跑 —— 否则 agent 可能
                // 在 AssignProcessToJobObject 之前就 fork 出孙进程，那些孙进程不属 job，
                // shim 死后不会被连带杀掉（复评 R24 §7-G3）。
                CREATE_NO_WINDOW | CREATE_SUSPENDED,
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
            // 评审 R23 §3.2：把 agent 放进「句柄关闭即杀」的 Job Object —— bridge 侧 kill 的是
            // **本 shim**，shim 一死 job 句柄由内核关闭 ⇒ agent 随之被杀，不会变孤儿。
            // 这也是 ACP 的 kill_on_drop 语义在多一层进程后仍成立的关键。
            match CreateJobObjectW(None, PCWSTR::null()) {
                Ok(job) => {
                    let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
                    info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
                    let set_ok = SetInformationJobObject(
                        job,
                        JobObjectExtendedLimitInformation,
                        &info as *const _ as *const core::ffi::c_void,
                        std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                    );
                    let assign_ok = AssignProcessToJobObject(job, pi.hProcess);
                    if set_ok.is_err() || assign_ok.is_err() {
                        // 复评 R24 §7-G3：job 没设成/没指派成功 ⇒ 杀树语义不成立，宁可明确失败
                        // （杀掉刚创建的挂起进程）也不留一个"以为会被连带杀"的 agent。
                        eprintln!(
                            "abb-spawner: Job Object 设置/指派失败（set={set_ok:?} assign={assign_ok:?}），已终止 {program}"
                        );
                        let _ = TerminateProcess(pi.hProcess, 1);
                        let _ = CloseHandle(pi.hThread);
                        let _ = CloseHandle(pi.hProcess);
                        return EXIT_SPAWN;
                    }
                    // job 句柄**故意不关**（HANDLE 是 Copy）：本进程退出或被 kill 时内核回收
                    // 句柄表项 ⇒ KILL_ON_JOB_CLOSE 生效 ⇒ agent 随之被杀。
                    let _ = job;
                }
                Err(e) => {
                    eprintln!("abb-spawner: CreateJobObjectW 失败（{e}），已终止 {program}");
                    let _ = TerminateProcess(pi.hProcess, 1);
                    let _ = CloseHandle(pi.hThread);
                    let _ = CloseHandle(pi.hProcess);
                    return EXIT_SPAWN;
                }
            }
            // 放它跑（创建时是挂起的）。
            if ResumeThread(pi.hThread) == u32::MAX {
                eprintln!("abb-spawner: ResumeThread 失败，已终止 {program}");
                let _ = TerminateProcess(pi.hProcess, 1);
                let _ = CloseHandle(pi.hThread);
                let _ = CloseHandle(pi.hProcess);
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

        /// B2-f：`--` 之后的参数里再出现 `--`、以及空串参数，都必须原样保留。
        #[test]
        fn split_args_keeps_after_dashdash_verbatim() {
            let v = |s: &[&str]| s.iter().map(|x| x.to_string()).collect::<Vec<_>>();
            assert_eq!(
                split_args(v(&["abb-spawner.exe", "--", "claude", "--", "-p", ""])),
                Some(("claude".to_string(), v(&["--", "-p", ""])))
            );
        }

        /// B2-f：用系统自己的 `CommandLineToArgvW` 做**构造性**往返校验（不是自比对期望串）。
        #[test]
        fn command_line_round_trips_through_commandlinetoargvw() {
            use windows::Win32::UI::Shell::CommandLineToArgvW;
            let cases: Vec<Vec<String>> = vec![
                vec![
                    "C:\\Program Files\\ABB\\agent-bridge.exe".into(),
                    "--service".into(),
                ],
                vec!["claude".into(), "--acp".into(), "a b".into()],
                vec!["x".into(), "".into(), "尾反斜杠\\".into(), "引\"号".into()],
            ];
            for (i, args) in cases.iter().enumerate() {
                let prog = args[0].clone();
                let mut line = build_command_line(&prog, &args[1..]);
                // SAFETY: line 是 NUL 结尾的 UTF-16；返回的 argv 由 LocalFree 归还。
                let mut n = 0i32;
                let argv = unsafe { CommandLineToArgvW(PCWSTR(line.as_ptr()), &mut n) };
                assert!(!argv.is_null(), "case {i}: CommandLineToArgvW 失败");
                let got: Vec<String> = (0..n as isize)
                    .map(|k| unsafe { (*argv.offset(k)).to_string().unwrap_or_default() })
                    .collect();
                unsafe {
                    // LocalFree 收 HLOCAL（新类型）：直接构造，不再包 Option（0.58 的签名是
                    // `LocalFree(hlocal: Option<HLOCAL>)`? —— 这里按编译错误给出的 Param 形态调）
                    let _ = windows::Win32::Foundation::LocalFree(
                        windows::Win32::Foundation::HLOCAL(argv as *mut core::ffi::c_void),
                    );
                }
                assert_eq!(
                    got, *args,
                    "case {i}: 往返不一致（构造的命令行解析回来不同）"
                );
                line.clear();
            }
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
