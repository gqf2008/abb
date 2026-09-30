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
//! 行为（**两段式**）：
//! 1. 第一段（本进程，高完整性）：取**当前会话 shell（explorer.exe）**的主令牌并复制一份 ——
//!    它代表"登录用户的普通权限身份"；把自己的环境写进临时文件，再用该令牌
//!    `CreateProcessWithTokenW` 拉起**本程序自己的中继模式**：
//!    `abb-spawner.exe --relay <envfile> -- <program> [args...]`，把**本进程的
//!    stdin/stdout/stderr** 原样交给它（bridge 侧看到的仍是同一条管道，ACP 的 stdio 协议不变）；
//! 2. 第二段（中继，已被降回普通用户）：读环境文件、删掉它，再用**普通 `CreateProcess`**
//!    以「环境全量接管 + stdio 继承」拉起 `<program>`，等它结束并把退出码透传给第一段。
//!
//! **为什么必须分两段**：`CreateProcessWithTokenW` / `CreateProcessAsUser` 在本机**拒收任何
//! 非 NULL 的 `lpEnvironment`**——实测矩阵：自建块、`GetEnvironmentStringsW` 的系统块、
//! `CreateEnvironmentBlock` 造的块、乃至单变量小块，一律 `0x80070057`（参数错误）；本机也没有
//! `SeAssignPrimaryTokenPrivilege`。唯一可用的 NULL env 形态会让子进程环境取自**令牌 profile**，
//! bridge 注入的 `BUZZ_AGENT_PROVIDER`/`PATH` 等全部丢失 ⇒ agent 一起来就退出、所有回合超时。
//! 所以环境只能由**已经降权的中继**用普通 `CreateProcess` 注入。
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

/// 中继模式的隐藏开关（只由第一段自己传，不对外暴露）。
const RELAY_FLAG: &str = "--relay";

/// 退出码：中继读不到 / 读坏环境文件。
///
/// `#[cfg(target_os = "windows")]`：唯一调用点在 `mod win` 内（`super::EXIT_RELAY_ENV`），
/// 不 gate 的话 macOS 构建里它就是死代码 —— 而 CI 的 `clippy -D warnings` 跑在 windows-latest，
/// macOS 本地门禁（`AGENTS.md` 的四条命令）会先红（2026-09-30 实测，见线程
/// `abb-macos-clippy-cfg-spawner-20260930`）。
#[cfg(target_os = "windows")]
const EXIT_RELAY_ENV: i32 = 5;

/// 中继环境文件路径：`%TEMP%\abb-relay-<pid>-<nanos>.env`（唯一名，避免并发撞车）。
///
/// `test` 也在 cfg 里：本函数是**平台无关的纯逻辑**（pid + 纳秒拼临时路径），既有的
/// `relay_tests::relay_env_path_is_unique_in_temp` 要能在 macOS 本地门禁上跑到（它测的是
/// 「名字唯一、落在临时目录」，与 Windows 无关）；生产侧的调用点仍只有 `mod win`。
#[cfg(any(target_os = "windows", test))]
fn relay_env_path() -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    std::env::temp_dir().join(format!("abb-relay-{}-{nanos}.env", std::process::id()))
}

/// 把 `(k, v)` 序列编码成环境文件内容：`k\0v\0k\0v\0…`。
///
/// 为什么用 NUL 分隔而不是 `k=v` 行：值里可能含空格/引号/`=`，NUL 是 Windows 环境值的
/// 合法边界（环境值本身不能含 NUL），编码无需转义、解码零歧义。
pub fn encode_env_pairs<I>(vars: I) -> Vec<u8>
where
    I: IntoIterator<Item = (String, String)>,
{
    let mut out = Vec::new();
    for (k, v) in vars {
        out.extend_from_slice(k.as_bytes());
        out.push(0);
        out.extend_from_slice(v.as_bytes());
        out.push(0);
    }
    out
}

/// 解码环境文件；字段数为奇数或含非法 UTF-8 一律判**损坏**（返回 None）。
///
/// 宁可不启动，也不要用半截环境把 agent 跑成「看起来起了、其实少了关键变量」——
/// 那正是这次要修的故障形态。
pub fn decode_env_pairs(bytes: &[u8]) -> Option<Vec<(String, String)>> {
    let mut fields: Vec<&[u8]> = bytes.split(|b| *b == 0).collect();
    // 结尾的 0 会切出一个空尾巴（正常现象），去掉它
    if fields.last().map(|f| f.is_empty()).unwrap_or(false) {
        fields.pop();
    }
    if !fields.len().is_multiple_of(2) {
        return None;
    }
    let mut out = Vec::with_capacity(fields.len() / 2);
    for pair in fields.chunks(2) {
        let k = std::str::from_utf8(pair[0]).ok()?.to_string();
        let v = std::str::from_utf8(pair[1]).ok()?.to_string();
        out.push((k, v));
    }
    Some(out)
}

/// 解析 `--relay <envfile> -- <program> [args...]`（argv[0] 是可执行文件）。
/// 不是中继形态返回 None（交回普通降权启动路径）。
pub fn parse_relay_args(argv: Vec<String>) -> Option<(String, String, Vec<String>)> {
    let mut it = argv.into_iter();
    let _exe = it.next()?;
    if it.next()? != RELAY_FLAG {
        return None;
    }
    let env_file = it.next()?;
    if it.next()? != "--" {
        return None;
    }
    let program = it.next()?;
    Some((env_file, program, it.collect()))
}

/// 含密钥的临时环境文件的兜底清理。
///
/// 正常路径由中继自己「读完即删」；这个 guard 覆盖「中继根本没起来」的失败分支——
/// 文件已被删时再删一次失败，无害。
///
/// `#[cfg(target_os = "windows")]`：与 [`EXIT_RELAY_ENV`] 同因 —— 只在 `mod win` 里构造，
/// 不 gate 会让 macOS 侧编译成死代码（`clippy -D warnings` 红）。
#[cfg(target_os = "windows")]
struct EnvFileGuard(std::path::PathBuf);

// impl 也是独立 item：type gate 了而 impl 没 gate 会直接编译错（E0425，本轮实测踩到）。
#[cfg(target_os = "windows")]
impl Drop for EnvFileGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
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

    /// 中继模式（第二段）：本进程已被降回**普通用户**（桌面 shell 令牌），负责用
    /// **普通 `CreateProcess`** 拉起真 agent —— 环境由第一段经环境文件交给它，
    /// 因为 `CreateProcessWithTokenW` 收不了非 NULL `lpEnvironment`（见模块头）。
    pub fn run_relay(env_file: &str, program: &str, args: &[String]) -> i32 {
        let bytes = match std::fs::read(env_file) {
            Ok(b) => b,
            Err(e) => {
                eprintln!("abb-spawner: 中继读环境文件失败（{env_file}）：{e}");
                return super::EXIT_RELAY_ENV;
            }
        };
        // 读到手就删：密钥不留盘（第一段另有 Drop 兜底，删不动属正常）。
        let _ = std::fs::remove_file(env_file);
        let Some(pairs) = super::decode_env_pairs(&bytes) else {
            eprintln!("abb-spawner: 中继环境文件损坏（{env_file}），拒绝以半截环境启动 {program}");
            return super::EXIT_RELAY_ENV;
        };
        // env_clear + envs = 子进程环境**恰好**是 bridge 交给我们的那一份；
        // stdin/stdout/stderr 继承 = bridge 的管道原样传下去（ACP 协议不变）。
        //
        // 必须走 crate::spawn 的唯一入口：agent 是控制台子系统程序，漏了 CREATE_NO_WINDOW
        // 就会凭空弹一个黑框（src/spawn.rs 的源码护栏专门守这条，本文件不得内联 creation_flags）。
        let status = agent_bridge::spawn::command(program)
            .args(args)
            .env_clear()
            .envs(pairs)
            .stdin(std::process::Stdio::inherit())
            .stdout(std::process::Stdio::inherit())
            .stderr(std::process::Stdio::inherit())
            .status();
        match status {
            Ok(st) => st.code().unwrap_or(1),
            Err(e) => {
                eprintln!("abb-spawner: 中继启动 {program} 失败：{e}");
                EXIT_SPAWN
            }
        }
    }

    pub fn run() -> i32 {
        let argv: Vec<String> = std::env::args().collect();
        // 中继模式（只由第一段自己传，见模块头）：普通权限、普通 CreateProcess。
        if let Some((env_file, program, args)) = super::parse_relay_args(argv.clone()) {
            return run_relay(&env_file, &program, &args);
        }
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
            // 两段式第一段：环境不能在 CreateProcessWithTokenW 里直接传（见模块头），
            // 所以写进临时文件，用桌面令牌拉起**自己**的中继模式去注入。
            let self_exe = std::env::current_exe()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_else(|_| "abb-spawner.exe".to_string());
            let env_file = super::relay_env_path();
            let env_pairs: Vec<(String, String)> = std::env::vars_os()
                .map(|(k, v)| {
                    (
                        k.to_string_lossy().into_owned(),
                        v.to_string_lossy().into_owned(),
                    )
                })
                .collect();
            if let Err(e) = std::fs::write(&env_file, super::encode_env_pairs(env_pairs)) {
                eprintln!(
                    "abb-spawner: 写中继环境文件失败（{}）：{e}",
                    env_file.display()
                );
                return EXIT_SPAWN;
            }
            // 中继没跑起来时别把含密钥的文件留盘上（中继读过即删，删不动属正常）。
            let _env_guard = super::EnvFileGuard(env_file.clone());
            let mut relay_args: Vec<String> = vec![
                super::RELAY_FLAG.to_string(),
                env_file.to_string_lossy().into_owned(),
                "--".to_string(),
                program.clone(),
            ];
            relay_args.extend(args.iter().cloned());
            let mut cmdline = build_command_line(&self_exe, &relay_args);
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
                    "abb-spawner: CreateProcessWithTokenW 拉起中继失败（{}），未启动 {program}",
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
            // 尾部反斜杠 + 引号：反斜杠必须加倍（否则会把结尾引号转义掉）。
            // 注意 `build_command_line` 返回的是**NUL 结尾**的 UTF-16 缓冲（`CommandLineToArgvW`
            // 的输入契约），所以比较前必须先切掉结尾的 0，否则等于拿 "…\"\0" 去比 "…\""。
            // 这条断言原先就是漏了这个 0：Windows 上必然红（Windows-only 测试，本机跑不到，
            // 直到 CI 才暴露 —— main CI run 36535165443 的 `test` job）。
            let buf = build_command_line("C:\\dir with space\\", &[]);
            assert_eq!(
                buf.last().copied(),
                Some(0),
                "命令行缓冲必须以 NUL 结尾（CommandLineToArgvW 的输入要求）"
            );
            let s = String::from_utf16_lossy(&buf[..buf.len() - 1]);
            assert_eq!(s, "\"C:\\dir with space\\\\\"");
        }
    }
}
/// 中继链路里**平台无关**的那半：环境文件编解码 + `--relay` 参数解析。
///
/// 为什么放顶层：这些是纯逻辑，mac 上的本地门禁也要跑到；真·两段 spawn 只有 Windows 能跑，
/// 端到端由提权验证脚本覆盖（本轮实测矩阵见模块头）。
#[cfg(test)]
mod relay_tests {
    use super::{decode_env_pairs, encode_env_pairs, parse_relay_args, relay_env_path, RELAY_FLAG};

    fn pairs() -> Vec<(String, String)> {
        vec![
            ("BUZZ_AGENT_PROVIDER".to_string(), "openai".to_string()),
            ("PATH".to_string(), "C:/a b;C:/中文 目录".to_string()),
            ("EMPTY".to_string(), String::new()),
            ("WITH_EQ".to_string(), "a=b=c".to_string()),
        ]
    }

    /// 编码 → 解码必须逐字还原（含空格 / 中文 / 空值 / 值里的 `=`）。
    #[test]
    fn env_pairs_round_trip() {
        let bytes = encode_env_pairs(pairs());
        assert_eq!(decode_env_pairs(&bytes), Some(pairs()));
    }

    /// 空环境合法（中继会先 env_clear 再全量设置）：编码为空、解码回空表。
    #[test]
    fn empty_env_round_trips() {
        let bytes = encode_env_pairs(Vec::new());
        assert!(bytes.is_empty());
        assert_eq!(decode_env_pairs(&bytes), Some(Vec::new()));
    }

    /// 字段数为奇数 = 损坏 ⇒ None（宁可不启动，也不用半截环境跑 agent）。
    #[test]
    fn odd_field_count_is_rejected() {
        assert_eq!(decode_env_pairs(b"K\0V\0K2"), None);
    }

    /// 非法 UTF-8 = 损坏 ⇒ None。
    #[test]
    fn invalid_utf8_is_rejected() {
        assert_eq!(decode_env_pairs(b"\xff\xfe\0V\0"), None);
    }

    /// `--relay` 形态能被识别；其它形态一律 None（交回普通降权启动路径）。
    #[test]
    fn parse_relay_args_accepts_only_relay_form() {
        let v = |s: &[&str]| s.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        assert_eq!(
            parse_relay_args(v(&[
                "abb-spawner.exe",
                RELAY_FLAG,
                "C:/e.env",
                "--",
                "buzz-agent.exe",
                "--acp"
            ])),
            Some((
                "C:/e.env".to_string(),
                "buzz-agent.exe".to_string(),
                v(&["--acp"])
            ))
        );
        // 普通形态（bridge 直接调用的那条）：不是中继
        assert_eq!(
            parse_relay_args(v(&["abb-spawner.exe", "--", "claude", "--acp"])),
            None
        );
        // 中继形态但缺 `--` 分隔 / 缺程序名：拒绝（宁可报用法错，也不要猜）
        assert_eq!(
            parse_relay_args(v(&[
                "abb-spawner.exe",
                RELAY_FLAG,
                "C:/e.env",
                "buzz-agent.exe"
            ])),
            None
        );
        assert_eq!(
            parse_relay_args(v(&["abb-spawner.exe", RELAY_FLAG, "C:/e.env", "--"])),
            None
        );
    }

    /// 环境文件落在临时目录、名字唯一（并发多个 agent 不撞车）。
    #[test]
    fn relay_env_path_is_unique_in_temp() {
        let a = relay_env_path();
        std::thread::sleep(std::time::Duration::from_millis(2));
        let b = relay_env_path();
        assert!(a.starts_with(std::env::temp_dir()), "{}", a.display());
        assert_ne!(a, b, "两次调用必须不同名（pid+纳秒）");
        assert!(a.to_string_lossy().ends_with(".env"));
    }
}
