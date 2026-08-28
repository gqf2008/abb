//! Windows：隐藏控制台 spawn（#153）。
//!
//! 背景：ABB 是 GUI 子系统进程（无控制台）。过去用 `CREATE_NO_WINDOW`（0x08000000）启动
//! agent（claude/codex/pi，node 控制台程序）→ agent **无控制台** → 其 Bash 工具 spawn 的
//! 孙进程（git.exe / node.exe / cmd.exe 等控制台程序）**没有父控制台可继承**，Windows 会为
//! 它们**新建可见控制台窗口** → 聊天中 agent 执行外部命令就闪一个黑框然后消失（#153 现象）。
//!
//! 修复：改用 `CreateProcessW` + `CREATE_NEW_CONSOLE` + `STARTUPINFO.wShowWindow=SW_HIDE`：
//! agent 持有**隐藏控制台**，其 Bash 子进程**继承同一隐藏控制台** → 不再新建可见窗口。
//! stdout/stderr 仍走管道（输出/退出码/超时/watchdog 行为不受影响）；kill 由上层
//! `taskkill /T /F` 杀进程树（见 agent.rs::kill_agent_tree）。
//!
//! Rust std `Command` 不暴露 STARTUPINFO，故用 windows-sys 自建 spawn：
//! 管道（CreatePipe）+ 环境块（父 env + 显式覆盖）+ cwd + 命令行（std 同款引号规则）。

#![cfg(windows)]

use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle, RawHandle};
use std::path::Path;
use std::sync::Arc;

use windows_sys::Win32::Foundation::{
    CloseHandle, SetHandleInformation, HANDLE, HANDLE_FLAG_INHERIT, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
    SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
};
use windows_sys::Win32::System::Pipes::CreatePipe;
use windows_sys::Win32::System::Threading::{
    CreateProcessW, GetExitCodeProcess, TerminateProcess, WaitForSingleObject, CREATE_NEW_CONSOLE,
    CREATE_UNICODE_ENVIRONMENT, INFINITE, PROCESS_INFORMATION, STARTF_USESHOWWINDOW,
    STARTF_USESTDHANDLES, STARTUPINFOW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::SW_HIDE;

/// 隐藏控制台 spawn 出的 agent 子进程（stdin/stdout/stderr 走管道，可异步读写）。
/// 接口刻意对齐 `tokio::process::Child`（id/kill/wait/wait_with_output），
/// 便于 run_once / teambuilder 跨平台共用同一套消费代码。
pub(crate) struct HiddenChild {
    pub(crate) stdin: Option<tokio::fs::File>,
    pub(crate) stdout: Option<tokio::fs::File>,
    pub(crate) stderr: Option<tokio::fs::File>,
    /// 进程句柄（wait/kill 用；Drop 时未回收则终止防孤儿）。
    proc: Arc<OwnedHandle>,
    /// #158 Job Object（KILL_ON_JOB_CLOSE）：作业句柄全部关闭（ABB 退出/崩溃/异常
    /// Drop）→ OS 自动终止作业内 agent 及全部子孙进程，零残留。正常退出时作业内
    /// 已无进程，关闭无害。
    job: Option<OwnedHandle>,
    pid: u32,
}

impl HiddenChild {
    pub(crate) fn id(&self) -> Option<u32> {
        Some(self.pid)
    }

    /// 终止主进程（子树由上层 taskkill /T 负责）。进程已退出时无害失败。
    pub(crate) async fn kill(&mut self) -> io::Result<()> {
        let proc = self.proc.clone();
        tokio::task::spawn_blocking(move || unsafe {
            let _ = TerminateProcess(proc.as_raw_handle() as HANDLE, 1);
        })
        .await
        .map_err(io::Error::other)?;
        Ok(())
    }

    /// 阻塞等待进程退出并取退出码（WaitForSingleObject + GetExitCodeProcess）。
    pub(crate) async fn wait(&mut self) -> io::Result<std::process::ExitStatus> {
        use std::os::windows::process::ExitStatusExt;
        let proc = self.proc.clone();
        let code = tokio::task::spawn_blocking(move || unsafe {
            WaitForSingleObject(proc.as_raw_handle() as HANDLE, INFINITE);
            let mut code: u32 = 0;
            GetExitCodeProcess(proc.as_raw_handle() as HANDLE, &mut code);
            code
        })
        .await
        .map_err(io::Error::other)?;
        Ok(std::process::ExitStatus::from_raw(code))
    }

    /// 并发收满 stdout/stderr 后等退出（对齐 tokio::process::Child::wait_with_output）。
    pub(crate) async fn wait_with_output(&mut self) -> io::Result<std::process::Output> {
        use tokio::io::AsyncReadExt;
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let (mut so, mut se) = match (self.stdout.take(), self.stderr.take()) {
            (Some(so), Some(se)) => (so, se),
            _ => return Err(io::Error::other("stdout/stderr 管道缺失")),
        };
        let (r1, r2) = tokio::join!(so.read_to_end(&mut stdout), se.read_to_end(&mut stderr));
        r1?;
        r2?;
        let status = self.wait().await?;
        Ok(std::process::Output {
            status,
            stdout,
            stderr,
        })
    }
}

impl Drop for HiddenChild {
    fn drop(&mut self) {
        // 未 wait 即丢弃（异常提前返回）时终止进程防孤儿；正常路径 wait/kill 已回收，无害。
        unsafe {
            let _ = TerminateProcess(self.proc.as_raw_handle() as HANDLE, 1);
        }
    }
}

#[async_trait::async_trait]
impl crate::agent::KillableChild for HiddenChild {
    async fn kill(&mut self) -> std::io::Result<()> {
        self.kill().await
    }
    async fn wait(&mut self) -> std::io::Result<std::process::ExitStatus> {
        self.wait().await
    }
}

/// 以「隐藏控制台」方式 spawn 子进程（#153）。参数从 std/tokio Command 提取：
/// - `program`：可执行文件（Windows 下 .cmd shim 场景为 `cmd`，首参 `/c` 跟脚本路径）
/// - `args`：程序参数（原样，内部按 std 同款规则引号化拼命令行）
/// - `cwd`：工作目录（None = 继承 ABB 当前目录）
/// - `envs`：环境覆盖（(KEY, Some(v)) 设置 / (KEY, None) 删除；基底=父进程完整环境）
pub(crate) fn spawn_hidden(
    program: &OsStr,
    args: &[OsString],
    cwd: Option<&Path>,
    envs: &[(OsString, Option<OsString>)],
) -> io::Result<HiddenChild> {
    // #158 Job Object（KILL_ON_JOB_CLOSE）：创建即失败则整体报错（agent 无资源防护
    // 不裸奔——挂起残留正是本机制要根治的）。作业句柄随 HiddenChild 生命周期，
    // ABB 退出/崩溃 → OS 终止作业内 agent 及全部子孙，零残留。
    let job = create_kill_on_close_job()?;
    // ── 1. 管道：stdin 父写子读，stdout/stderr 子写父读（两端都先可继承）──
    let mut stdin_read: HANDLE = INVALID_HANDLE_VALUE;
    let mut stdin_write: HANDLE = INVALID_HANDLE_VALUE;
    let mut stdout_read: HANDLE = INVALID_HANDLE_VALUE;
    let mut stdout_write: HANDLE = INVALID_HANDLE_VALUE;
    let mut stderr_read: HANDLE = INVALID_HANDLE_VALUE;
    let mut stderr_write: HANDLE = INVALID_HANDLE_VALUE;
    unsafe {
        let sa = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: std::ptr::null_mut(),
            bInheritHandle: 1,
        };
        let mut made = 0u8;
        let r = CreatePipe(&mut stdin_read, &mut stdin_write, &sa, 0);
        if r != 0 {
            made = 1;
        }
        let r2 = if r != 0 {
            CreatePipe(&mut stdout_read, &mut stdout_write, &sa, 0)
        } else {
            0
        };
        if r2 != 0 {
            made = 2;
        }
        let r3 = if r2 != 0 {
            CreatePipe(&mut stderr_read, &mut stderr_write, &sa, 0)
        } else {
            0
        };
        if r3 != 0 {
            made = 3;
        }
        if r3 == 0 {
            // 失败：关闭已建管道，返回错误
            let close = |h: HANDLE| {
                if h != INVALID_HANDLE_VALUE {
                    CloseHandle(h);
                }
            };
            close(stdin_read);
            close(stdin_write);
            if made >= 2 {
                close(stdout_read);
                close(stdout_write);
            }
            return Err(io::Error::last_os_error());
        }
        // 父端句柄清可继承位：即便 bInheritHandles 语义变化也不泄漏给子进程。
        SetHandleInformation(stdin_write, HANDLE_FLAG_INHERIT, 0);
        SetHandleInformation(stdout_read, HANDLE_FLAG_INHERIT, 0);
        SetHandleInformation(stderr_read, HANDLE_FLAG_INHERIT, 0);
    }

    // ── 2. 命令行（std 同款引号规则，CreateProcessW 会改写缓冲区 → 需 mut）──
    let mut cmdline = build_command_line(program, args);
    cmdline.push(0);

    // ── 3. 环境块：父进程完整环境 + 显式覆盖（CreateProcessW 需 UTF-16 双 NUL 块）──
    let env_block = build_env_block(envs);

    // ── 4. STARTUPINFO：隐藏控制台 + 标准句柄直通管道 ──
    let mut si: STARTUPINFOW = unsafe { std::mem::zeroed() };
    si.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
    si.dwFlags = STARTF_USESTDHANDLES | STARTF_USESHOWWINDOW;
    si.wShowWindow = SW_HIDE as u16;
    si.hStdInput = stdin_read;
    si.hStdOutput = stdout_write;
    si.hStdError = stderr_write;

    let cwd_w: Vec<u16> = match cwd {
        Some(p) => p
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect(),
        None => Vec::new(),
    };

    let mut pi: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
    let ok = unsafe {
        CreateProcessW(
            std::ptr::null(), // lpApplicationName：走 PATH 解析（与 std Command 同语义）
            cmdline.as_mut_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            1, // bInheritHandles = TRUE：与 std 同款——仅「可继承」句柄被继承，
            //   父端句柄已在上方 SetHandleInformation 清可继承位，不会泄漏给子进程；
            //   管道创建→CreateProcessW 之间暴露窗口仅本函数内（并发窗口与 std 一致）。
            CREATE_NEW_CONSOLE | CREATE_UNICODE_ENVIRONMENT,
            env_block.as_ptr() as *const core::ffi::c_void,
            if cwd_w.is_empty() {
                std::ptr::null()
            } else {
                cwd_w.as_ptr()
            },
            &si,
            &mut pi,
        )
    };
    if ok == 0 {
        unsafe {
            CloseHandle(stdin_read);
            CloseHandle(stdin_write);
            CloseHandle(stdout_read);
            CloseHandle(stdout_write);
            CloseHandle(stderr_read);
            CloseHandle(stderr_write);
        }
        return Err(io::Error::last_os_error());
    }

    // ── 4.5 Job Object：把 agent 主进程分配进作业（其子孙进程默认继承作业）──
    unsafe {
        let ok = AssignProcessToJobObject(job.as_raw_handle() as HANDLE, pi.hProcess);
        if ok == 0 {
            // 极端情况（进程已退出/作业权限异常）：关闭作业句柄避免泄漏，返回错误。
            // agent 未受保护即拒绝启动，与「无资源防护不裸奔」一致。
            let _ = TerminateProcess(pi.hProcess, 1);
            CloseHandle(pi.hThread);
            CloseHandle(pi.hProcess);
            CloseHandle(stdin_read);
            CloseHandle(stdin_write);
            CloseHandle(stdout_read);
            CloseHandle(stdout_write);
            CloseHandle(stderr_read);
            CloseHandle(stderr_write);
            return Err(io::Error::last_os_error());
        }
    }

    // ── 5. 收尾：关子端句柄，父端句柄包装成 tokio File ──
    unsafe {
        CloseHandle(stdin_read);
        CloseHandle(stdout_write);
        CloseHandle(stderr_write);
        CloseHandle(pi.hThread);
    }
    let stdin_file = unsafe { std::fs::File::from_raw_handle(stdin_write as RawHandle) };
    let stdout_file = unsafe { std::fs::File::from_raw_handle(stdout_read as RawHandle) };
    let stderr_file = unsafe { std::fs::File::from_raw_handle(stderr_read as RawHandle) };
    let proc = unsafe { OwnedHandle::from_raw_handle(pi.hProcess as RawHandle) };

    Ok(HiddenChild {
        stdin: Some(tokio::fs::File::from_std(stdin_file)),
        stdout: Some(tokio::fs::File::from_std(stdout_file)),
        stderr: Some(tokio::fs::File::from_std(stderr_file)),
        proc: Arc::new(proc),
        job: Some(job),
        pid: pi.dwProcessId,
    })
}

/// 创建 Job Object 并设 KILL_ON_JOB_CLOSE：作业句柄全部关闭时 OS 终止作业内全部进程。
/// ABB 进程退出/崩溃（句柄随进程回收）→ agent 及全部子孙零残留——确定性兜底，
/// 不依赖 ABB 自身清理逻辑（#158）。
fn create_kill_on_close_job() -> io::Result<OwnedHandle> {
    unsafe {
        let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
        if job.is_null() {
            return Err(io::Error::last_os_error());
        }
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let ok = SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            &info as *const _ as *const core::ffi::c_void,
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        );
        if ok == 0 {
            CloseHandle(job);
            return Err(io::Error::last_os_error());
        }
        Ok(OwnedHandle::from_raw_handle(job as RawHandle))
    }
}

/// 拼命令行：`program args...`，每段按 Windows 规则引号化（与 std::process::Command
/// 同款算法——空格/制表/引号触发引号包裹，引号前反斜杠翻倍，尾部反斜杠翻倍）。
fn build_command_line(program: &OsStr, args: &[OsString]) -> Vec<u16> {
    let mut out = Vec::new();
    push_quoted(&mut out, program);
    for a in args {
        out.push(b' ' as u16);
        push_quoted(&mut out, a);
    }
    out
}

/// 单段参数引号化（std 同款）：
/// - 空串 → `""`
/// - 不含空白/双引号 → 原样
/// - 否则 `"..."`，其中 `"` 转义为 `\"`，其前的反斜杠翻倍；串尾反斜杠翻倍
fn push_quoted(out: &mut Vec<u16>, arg: &OsStr) {
    let wide: Vec<u16> = arg.encode_wide().collect();
    if wide.is_empty() {
        out.extend_from_slice(&[b'"' as u16, b'"' as u16]);
        return;
    }
    let needs_quotes = wide
        .iter()
        .any(|&c| c == b' ' as u16 || c == b'\t' as u16 || c == b'\n' as u16 || c == b'"' as u16);
    if !needs_quotes {
        out.extend_from_slice(&wide);
        return;
    }
    out.push(b'"' as u16);
    let mut num_backslashes = 0usize;
    for &c in &wide {
        if c == b'\\' as u16 {
            num_backslashes += 1;
            continue;
        }
        if c == b'"' as u16 {
            for _ in 0..num_backslashes * 2 {
                out.push(b'\\' as u16);
            }
            out.push(b'\\' as u16);
            out.push(b'"' as u16);
        } else {
            for _ in 0..num_backslashes {
                out.push(b'\\' as u16);
            }
            out.push(c);
        }
        num_backslashes = 0;
    }
    for _ in 0..num_backslashes * 2 {
        out.push(b'\\' as u16);
    }
    out.push(b'"' as u16);
}

/// 构造 UTF-16 双 NUL 结尾环境块：父进程完整环境 + 显式覆盖（None = 删除）。
/// 键大小写不敏感去重（Windows 环境变量语义）。
fn build_env_block(envs: &[(OsString, Option<OsString>)]) -> Vec<u16> {
    let mut map: HashMap<String, (OsString, OsString)> = HashMap::new();
    for (k, v) in std::env::vars_os() {
        map.entry(key_lower(&k)).or_insert((k, v));
    }
    for (k, v) in envs {
        let lk = key_lower(k);
        match v {
            Some(val) => {
                map.insert(lk, (k.clone(), val.clone()));
            }
            None => {
                map.remove(&lk);
            }
        }
    }
    let mut block = Vec::new();
    for (_lk, (k, v)) in map {
        block.extend(k.encode_wide());
        block.push(b'=' as u16);
        block.extend(v.encode_wide());
        block.push(0);
    }
    block.push(0); // 双 NUL 结尾
    block
}

fn key_lower(k: &OsStr) -> String {
    k.to_string_lossy().to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use std::ffi::OsString;

    /// 枚举顶层窗口（同步回调，闭包以裸指针传入，EnumWindows 返回前回调必已执行完）。
    unsafe extern "system" fn enum_cb(
        hwnd: windows_sys::Win32::Foundation::HWND,
        lparam: isize,
    ) -> windows_sys::core::BOOL {
        let cb = &mut *(lparam as *mut &mut dyn FnMut(windows_sys::Win32::Foundation::HWND));
        cb(hwnd);
        1
    }
    fn enum_windows(mut cb: &mut dyn FnMut(windows_sys::Win32::Foundation::HWND)) {
        // 传「&mut dyn 引用」的地址（thin 指针）；回调内经 lparam 还原引用后调用。
        // EnumWindows 同步返回，回调执行期间引用必然存活。
        let p = &mut cb as *mut &mut dyn FnMut(windows_sys::Win32::Foundation::HWND) as isize;
        unsafe {
            windows_sys::Win32::UI::WindowsAndMessaging::EnumWindows(Some(enum_cb), p);
        }
    }
    fn visible_console_windows() -> HashSet<windows_sys::Win32::Foundation::HWND> {
        let mut set = HashSet::new();
        enum_windows(&mut |hwnd| unsafe {
            if is_console_window(hwnd) && is_window_visible(hwnd) {
                set.insert(hwnd);
            }
        });
        set
    }
    unsafe fn is_console_window(hwnd: windows_sys::Win32::Foundation::HWND) -> bool {
        let mut buf = [0u16; 64];
        let len = windows_sys::Win32::UI::WindowsAndMessaging::GetClassNameW(
            hwnd,
            buf.as_mut_ptr(),
            buf.len() as i32,
        );
        if len <= 0 {
            return false;
        }
        String::from_utf16_lossy(&buf[..len as usize]) == "ConsoleWindowClass"
    }
    unsafe fn is_window_visible(hwnd: windows_sys::Win32::Foundation::HWND) -> bool {
        windows_sys::Win32::UI::WindowsAndMessaging::IsWindowVisible(hwnd) != 0
    }

    #[test]
    fn command_line_quotes_like_std() {
        // 覆盖 std 引号规则的关键边界（空格/制表/空串/双引号/反斜杠/尾斜杠/unicode）。
        let cases: &[(&str, &str)] = &[
            ("abc", "abc"),
            ("a b", "\"a b\""),
            ("", "\"\""),
            ("a\tb", "\"a\tb\""),
            ("a\"b", "\"a\\\"b\""),
            ("a\\b", "a\\b"),         // 无空白不引号化
            ("a b\\", "\"a b\\\\\""), // 引号内尾部反斜杠翻倍
            ("a\\ b", "\"a\\ b\""),   // 反斜杠后跟非引号 → 原样保留（仅引号前翻倍）
            ("C:\\Program Files\\x.exe", "\"C:\\Program Files\\x.exe\""),
            ("中文 路径", "\"中文 路径\""),
        ];
        for (input, expected) in cases {
            let mut out = Vec::new();
            push_quoted(&mut out, OsStr::new(input));
            let got = String::from_utf16_lossy(&out);
            assert_eq!(got, *expected, "push_quoted({input:?})");
        }
    }

    #[test]
    fn command_line_roundtrips_via_commandline_to_argvw() {
        // 金标准：拼出的命令行经 CommandLineToArgvW 解析必须还原原参数。
        // （cmd/node 等真实子进程就用这套规则解析 argv。）
        let tricky = [
            "simple",
            "with space",
            "",
            "quote\"inside",
            "trail\\",
            "back\\slash\\ path",
            "C:\\Program Files\\agent.exe",
            "uni中文 dir",
            "tab\there",
        ];
        let program = OsStr::new("C:\\Program Files\\agent.exe");
        let args: Vec<OsString> = tricky.iter().map(|s| OsString::from(*s)).collect();
        let mut line = build_command_line(program, &args);
        line.push(0);

        let mut n_args = 0i32;
        let argv = unsafe {
            windows_sys::Win32::UI::Shell::CommandLineToArgvW(line.as_ptr(), &mut n_args)
        };
        assert!(!argv.is_null(), "CommandLineToArgvW 失败");
        let parsed: Vec<String> = (0..n_args as usize)
            .map(|i| {
                let p = unsafe { *argv.add(i) };
                // 手动扫描 NUL 求宽字符串长度（避免额外 Globalization 特性）
                let mut len = 0usize;
                unsafe {
                    while *p.add(len) != 0 {
                        len += 1;
                    }
                }
                let slice = unsafe { std::slice::from_raw_parts(p, len) };
                String::from_utf16_lossy(slice)
            })
            .collect();
        unsafe {
            windows_sys::Win32::Foundation::LocalFree(argv as _);
        }

        assert_eq!(parsed.len(), 1 + tricky.len(), "argv: {parsed:?}");
        assert_eq!(parsed[0], "C:\\Program Files\\agent.exe");
        for (i, s) in tricky.iter().enumerate() {
            assert_eq!(&parsed[i + 1], s, "argv[{i}]");
        }
    }

    fn run_capture(
        program: &str,
        args: &[&str],
        envs: &[(OsString, Option<OsString>)],
    ) -> (std::process::ExitStatus, String, String) {
        let program_os = OsString::from(program);
        let args_os: Vec<OsString> = args.iter().map(|s| OsString::from(*s)).collect();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let mut child = spawn_hidden(&program_os, &args_os, None, envs).expect("spawn 失败");
            let out = child.wait_with_output().await.expect("wait 失败");
            (
                out.status,
                String::from_utf8_lossy(&out.stdout).into_owned(),
                String::from_utf8_lossy(&out.stderr).into_owned(),
            )
        })
    }

    #[test]
    fn spawn_hidden_runs_cmd_and_captures_output() {
        // 管道 + CreateProcessW + 隐藏控制台最小冒烟：cmd /c echo 输出可达、退出码正确。
        let (status, stdout, stderr) = run_capture("cmd", &["/c", "echo", "hello-hidden"], &[]);
        assert!(status.success(), "exit={status:?} stderr={stderr}");
        assert!(stdout.contains("hello-hidden"), "stdout={stdout:?}");
    }

    #[test]
    fn spawn_hidden_applies_env_override() {
        // 环境块：父 env 基底 + 显式覆盖（设置/删除）。
        let overrides = vec![
            (
                OsString::from("PROBE_HIDDEN_VAR"),
                Some(OsString::from("probe-value")),
            ),
            (OsString::from("PROBE_DELETED_VAR"), None),
        ];
        let (status, stdout, _) = run_capture(
            "cmd",
            &[
                "/c",
                "echo",
                "%PROBE_HIDDEN_VAR%",
                "%PROBE_DELETED_VAR%",
                "%COMSPEC%",
            ],
            &overrides,
        );
        assert!(status.success());
        assert!(stdout.contains("probe-value"), "stdout={stdout:?}");
        // 删除的变量不展开（%PROBE_DELETED_VAR% 原样保留）
        assert!(!stdout.contains("probe-deleted"), "stdout={stdout:?}");
        // 父 env 基底仍在（COMSPEC 来自父进程环境）
        assert!(stdout.contains("cmd.exe"), "stdout={stdout:?}");
    }

    #[test]
    fn spawn_hidden_respects_cwd() {
        // cwd 直通：cmd /c cd 打印当前目录。
        let dir = std::env::temp_dir().join("winproc-cwd-test");
        let _ = std::fs::create_dir_all(&dir);
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let dir2 = dir.clone();
        let (status, stdout, _) = rt.block_on(async {
            let mut child = spawn_hidden(
                &OsString::from("cmd"),
                &[OsString::from("/c"), OsString::from("cd")],
                Some(&dir2),
                &[],
            )
            .expect("spawn 失败");
            let out = child.wait_with_output().await.expect("wait 失败");
            (
                out.status,
                String::from_utf8_lossy(&out.stdout).into_owned(),
                String::from_utf8_lossy(&out.stderr).into_owned(),
            )
        });
        assert!(status.success());
        assert!(stdout.contains("winproc-cwd-test"), "stdout={stdout:?}");
    }

    #[test]
    fn spawn_hidden_stdin_eof_reaches_child() {
        // stdin 管道：写入后 drop（EOF）→ cmd /c more 读到内容输出。
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let (status, stdout, stderr) = rt.block_on(async {
            use tokio::io::AsyncWriteExt;
            let mut child = spawn_hidden(
                &OsString::from("cmd"),
                &[OsString::from("/c"), OsString::from("more")],
                None,
                &[],
            )
            .expect("spawn 失败");
            if let Some(mut stdin) = child.stdin.take() {
                let _ = stdin.write_all(b"piped-input-line\r\n").await;
                drop(stdin);
            }
            let out = child.wait_with_output().await.expect("wait 失败");
            (
                out.status,
                String::from_utf8_lossy(&out.stdout).into_owned(),
                String::from_utf8_lossy(&out.stderr).into_owned(),
            )
        });
        assert!(status.success(), "stderr={stderr}");
        assert!(stdout.contains("piped-input-line"), "stdout={stdout:?}");
    }

    /// #153 回归：agent（node）spawn 孙进程（cmd）不得新建**可见**控制台窗口。
    /// 旧行为（CREATE_NO_WINDOW）：agent 无控制台 → cmd 孙进程新建可见控制台 → 闪框。
    /// 新行为（隐藏控制台）：孙进程继承隐藏控制台 → 无新可见窗口。
    /// #158 Job Object（KILL_ON_JOB_CLOSE）回归：HiddenChild drop（作业句柄关闭）→
    /// OS 自动终止作业内 agent 及全部子孙进程——确定性零残留，不依赖自身清理逻辑。
    #[test]
    fn job_object_kills_grandchildren_on_drop() {
        // 持久孙进程构造（同 #156 测试）：`start ping` 独立进程 + `& ping` 阻塞保持主进程
        let mut child = spawn_hidden(
            &OsString::from("cmd"),
            &[
                OsString::from("/C"),
                OsString::from("start ping -n 120 127.0.0.1 & ping -n 120 127.0.0.1"),
            ],
            None,
            &[],
        )
        .expect("spawn 应成功");
        let pid = child.id().unwrap();
        // 轮询等孙进程出现
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(8);
        while ping_count_winproc() < 1 && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(200));
        }
        assert!(
            ping_count_winproc() >= 1,
            "前置：孙进程应存在（构造/断言失效则本测试无意义）"
        );
        let _ = pid;
        // drop → 作业句柄关闭 → KILL_ON_JOB_CLOSE → 作业内全部进程被 OS 终止
        drop(child);
        let deadline2 = std::time::Instant::now() + std::time::Duration::from_secs(4);
        loop {
            if ping_count_winproc() == 0 {
                break;
            }
            if std::time::Instant::now() >= deadline2 {
                panic!(
                    "Job Object 关闭后孙进程应被 OS 终止（KILL_ON_JOB_CLOSE），残留 {} 个",
                    ping_count_winproc()
                );
            }
            std::thread::sleep(std::time::Duration::from_millis(200));
        }
        // 清理保险（正常应无残留）
        let _ = std::process::Command::new("taskkill")
            .args(["/IM", "ping.exe", "/F"])
            .status();
    }

    fn ping_count_winproc() -> usize {
        let out = std::process::Command::new("tasklist")
            .args(["/FI", "IMAGENAME eq ping.exe"])
            .output()
            .expect("tasklist 应可执行");
        String::from_utf8_lossy(&out.stdout)
            .to_ascii_lowercase()
            .matches("ping.exe")
            .count()
    }

    fn hidden_console_no_new_visible_window_for_grandchildren() {
        // node 缺失时跳过（CI/精简环境）。
        let Some(node) = crate::deps::find_in_path("node") else {
            eprintln!("node 未安装，跳过孙进程控制台回归测试");
            return;
        };
        let script = "require('child_process').spawnSync('cmd',['/c','ping','-n','5','127.0.0.1'],{stdio:'inherit'});";
        let before = visible_console_windows();

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let (status, new_visible, stderr) = rt.block_on(async {
            let mut child = spawn_hidden(
                node.as_os_str(),
                &[OsString::from("-e"), OsString::from(script)],
                None,
                &[],
            )
            .expect("spawn node 失败");
            // 孙进程 cmd/ping 运行期间（ping -n 5 ≈ 4s）轮询枚举：若新建可见控制台会被捕获
            let mut new_visible = HashSet::new();
            for _ in 0..6 {
                tokio::time::sleep(std::time::Duration::from_millis(400)).await;
                let now = visible_console_windows();
                for h in &now {
                    if !before.contains(h) {
                        new_visible.insert(*h);
                    }
                }
            }
            let out = child.wait_with_output().await.expect("wait 失败");
            (
                out.status,
                new_visible,
                String::from_utf8_lossy(&out.stderr).into_owned(),
            )
        });
        assert!(status.success(), "node 脚本失败 stderr={stderr}");
        assert!(
            new_visible.is_empty(),
            "#153 回归：agent 孙进程新建了可见控制台窗口 {new_visible:?}"
        );
    }
}
