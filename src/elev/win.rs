//! Windows 专有实现：命名管道（含 DACL）、Winlogon 注册表读写、`runas` 拉起 helper。
//!
//! 只在 `#[cfg(target_os = "windows")]` 下编译（`elev/mod.rs` 里条件 `mod`），所以
//! 本文件里的 `unsafe` FFI **不需要**再做平台门控。三个部分：
//!
//! 1. 注册表：`HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Winlogon` 的三个值；
//! 2. 命名管道：服务端 DACL 只放「当前用户 SID + SYSTEM + Administrators」；
//! 3. `ShellExecuteExW "runas"`：调用方（非提权）拉起短命 helper 并发一个请求。
//!
//! # 为什么 helper 当**服务端**（而不是调用方建管道、helper 连过来）
//! - 白名单匹配与令牌校验必须发生在**提权进程里**：调用方是非提权进程，它建的管道管不着
//!   别的进程往提权进程里灌什么；
//! - 「提权方当管道服务端、用户态进程当客户端」是成熟形态（如 Mozilla Maintenance
//!   Service），中完整性级别的客户端连高完整性级别的管道服务端没有额外障碍；
//! - 调用方要处理 UAC（用户可能直接取消），「等 helper 把管道建出来」这种带重试的活儿
//!   本来就该落在调用方一侧。
//!
//! # 令牌的残余风险（与 `elev` 模块头一致）
//! `--token` 经命令行传给提权 helper，同用户进程可读它的命令行；令牌只防跨用户冒用、
//! 抬高同用户冒用成本，真正的边界是管道 DACL。
//!
//! # 实测坑：服务端写完必须 `FlushFileBuffers` 再断开
//! `DisconnectNamedPipe` 会**丢弃客户端还没读走的数据**。helper 是「写完响应就退出」的
//! 短命进程，所以 [`PipeServer::write_frame`] 里写完立刻 `FlushFileBuffers`（它在客户端
//! 读走全部数据前阻塞），否则客户端只会看到 EOF、一个字节都读不到——审计里却记着 `ok`。
//!
//! # 本文件不做「真机执行」
//! 验收只跑单测（纯逻辑在 `elev` 模块）。`call_helper` / `WinlogonBackend` 的**真机**
//! 验证（含 UAC 交互）留待 owner 手动走一遍——本机无 UAC 交互环境，且真写 HKLM 会改
//! 系统自动登录状态（红线：本批不许真执行提权 helper / 真写 HKLM）。

use std::ffi::{c_void, OsStr};
use std::os::windows::ffi::OsStrExt;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use super::{AutoLoginState, SysBackend, SysError, FRAME_HEADER, MAX_BODY};

/// 句柄/键句柄（Win32 的 `HANDLE` 与 `HKEY` 都是指针宽度）。
type Handle = *mut c_void;
type Hkey = *mut c_void;

// ─────────────────────────── FFI 声明 ───────────────────────────
// 与仓库既有风格一致：手写 `#[link]`，不引入 `windows-sys` / `winapi`。

mod ffi {
    use std::ffi::c_void;

    /// `SECURITY_ATTRIBUTES`。
    #[repr(C)]
    pub struct SecurityAttributes {
        pub n_length: u32,
        pub lp_security_descriptor: *mut c_void,
        pub b_inherit_handle: i32,
    }

    /// `ShellExecuteExW` 的 `SHELLEXECUTEINFOW`（字段顺序/大小按 Windows SDK）。
    #[repr(C)]
    pub struct ShellExecuteInfoW {
        pub cb_size: u32,
        pub f_mask: u32,
        pub hwnd: *mut c_void,
        pub lp_verb: *const u16,
        pub lp_file: *const u16,
        pub lp_parameters: *const u16,
        pub lp_directory: *const u16,
        pub n_show: i32,
        pub h_inst_app: *mut c_void,
        pub lp_id_list: *mut c_void,
        pub lp_class: *const u16,
        pub h_key_class: *mut c_void,
        pub dw_hot_key: u32,
        pub h_icon_or_monitor: *mut c_void,
        pub h_process: *mut c_void,
    }

    #[link(name = "kernel32")]
    unsafe extern "system" {
        pub fn GetLastError() -> u32;
        pub fn CloseHandle(h: *mut c_void) -> i32;
        pub fn GetCurrentProcess() -> *mut c_void;
        pub fn GetCurrentProcessId() -> u32;
        pub fn CreateNamedPipeW(
            name: *const u16,
            open_mode: u32,
            pipe_mode: u32,
            max_instances: u32,
            out_buffer_size: u32,
            in_buffer_size: u32,
            default_timeout: u32,
            security_attributes: *const SecurityAttributes,
        ) -> *mut c_void;
        pub fn ConnectNamedPipe(h: *mut c_void, overlapped: *mut c_void) -> i32;
        pub fn DisconnectNamedPipe(h: *mut c_void) -> i32;
        pub fn CreateFileW(
            name: *const u16,
            desired_access: u32,
            share_mode: u32,
            security_attributes: *const SecurityAttributes,
            creation_disposition: u32,
            flags_and_attributes: u32,
            template_file: *mut c_void,
        ) -> *mut c_void;
        pub fn ReadFile(
            h: *mut c_void,
            buf: *mut c_void,
            to_read: u32,
            read: *mut u32,
            overlapped: *mut c_void,
        ) -> i32;
        pub fn WriteFile(
            h: *mut c_void,
            buf: *const c_void,
            to_write: u32,
            written: *mut u32,
            overlapped: *mut c_void,
        ) -> i32;
        pub fn FlushFileBuffers(h: *mut c_void) -> i32;
        pub fn WaitForSingleObject(h: *mut c_void, ms: u32) -> u32;
        pub fn GetExitCodeProcess(h: *mut c_void, code: *mut u32) -> i32;
        pub fn GetNamedPipeClientProcessId(pipe: *mut c_void, client_pid: *mut u32) -> i32;
        pub fn LocalFree(mem: *mut c_void) -> *mut c_void;
    }

    #[link(name = "advapi32")]
    unsafe extern "system" {
        pub fn RegOpenKeyExW(
            key: *mut c_void,
            sub_key: *const u16,
            options: u32,
            sam_desired: u32,
            result: *mut *mut c_void,
        ) -> u32;
        pub fn RegSetValueExW(
            key: *mut c_void,
            value_name: *const u16,
            reserved: u32,
            dw_type: u32,
            data: *const u8,
            cb_data: u32,
        ) -> u32;
        pub fn RegDeleteValueW(key: *mut c_void, value_name: *const u16) -> u32;
        pub fn RegQueryValueExW(
            key: *mut c_void,
            value_name: *const u16,
            reserved: *mut u32,
            dw_type: *mut u32,
            data: *mut u8,
            cb_data: *mut u32,
        ) -> u32;
        pub fn RegCloseKey(key: *mut c_void) -> u32;
        pub fn OpenProcessToken(
            process: *mut c_void,
            desired_access: u32,
            token: *mut *mut c_void,
        ) -> i32;
        pub fn GetTokenInformation(
            token: *mut c_void,
            class: u32,
            info: *mut c_void,
            info_len: u32,
            return_len: *mut u32,
        ) -> i32;
        pub fn ConvertSidToStringSidW(sid: *mut c_void, out: *mut *mut u16) -> i32;
        pub fn ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl: *const u16,
            revision: u32,
            sd: *mut *mut c_void,
            sd_len: *mut u32,
        ) -> i32;
    }

    #[link(name = "shell32")]
    unsafe extern "system" {
        pub fn IsUserAnAdmin() -> i32;
        pub fn ShellExecuteExW(info: *mut c_void) -> i32;
    }
}

// ─────────────────────────── 常量 ───────────────────────────

/// Winlogon 子键（读取时**必须**带 `KEY_WOW64_64KEY`，见 [`open_winlogon`]）。
const WINLOGON_SUBKEY: &str = r"SOFTWARE\Microsoft\Windows NT\CurrentVersion\Winlogon";
const VAL_AUTO_ADMIN_LOGON: &str = "AutoAdminLogon";
const VAL_DEFAULT_USERNAME: &str = "DefaultUserName";
const VAL_DEFAULT_PASSWORD: &str = "DefaultPassword";

/// `KEY_QUERY_VALUE`。
const KEY_QUERY_VALUE: u32 = 0x0001;
/// `KEY_SET_VALUE`。
const KEY_SET_VALUE: u32 = 0x0002;
/// `KEY_WOW64_64KEY`：强制访问 64 位视图，防 WOW64 重定向把读写落到 `Wow6432Node`
/// 下（那样 Windows 根本读不到，自动登录静默失效）。
const KEY_WOW64_64KEY: u32 = 0x0100;

/// `REG_SZ`。
const REG_SZ: u32 = 1;
/// `REG_EXPAND_SZ`。
const REG_EXPAND_SZ: u32 = 2;
/// `REG_DWORD`。
const REG_DWORD: u32 = 4;

const ERROR_SUCCESS: u32 = 0;
const ERROR_FILE_NOT_FOUND: u32 = 2;
const ERROR_ACCESS_DENIED: u32 = 5;

/// `TOKEN_QUERY`。
const TOKEN_QUERY: u32 = 0x0008;
/// `TokenUser`。
const TOKEN_USER: u32 = 1;
/// `SDDL_REVISION_1`。
const SDDL_REVISION_1: u32 = 1;

/// `PIPE_ACCESS_DUPLEX`。
const PIPE_ACCESS_DUPLEX: u32 = 0x0000_0003;
/// `FILE_FLAG_FIRST_PIPE_INSTANCE`：抢注即失败，防「我先占住这个名字」。
const FILE_FLAG_FIRST_PIPE_INSTANCE: u32 = 0x0008_0000;
/// `PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT`（阻塞式字节流）。
const PIPE_BYTE_BLOCKING: u32 = 0x0000_0000;

/// `GENERIC_READ | GENERIC_WRITE`。
const GENERIC_READ_WRITE: u32 = 0x8000_0000 | 0x4000_0000;
/// `OPEN_EXISTING`。
const OPEN_EXISTING: u32 = 3;
/// `ERROR_PIPE_CONNECTED`：客户端在 `ConnectNamedPipe` 之前就连上了，算连接成功。
const ERROR_PIPE_CONNECTED: u32 = 535;
/// `ERROR_CANCELLED`：用户在 UAC 弹框上点了「否」。
const ERROR_CANCELLED: u32 = 1223;

/// `SEE_MASK_NOCLOSEPROCESS`：拿回进程句柄，用于等待退出 + 取退出码。
const SEE_MASK_NOCLOSEPROCESS: u32 = 0x0000_0040;
/// `SW_HIDE`：helper 是控制台程序，用隐藏窗口拉起，避免闪一个黑框（等价于仓库其它
/// spawn 用的 `CREATE_NO_WINDOW`——`ShellExecute` 没有那个标志位）。
const SW_HIDE: i32 = 0;
/// `WAIT_OBJECT_0`。
const WAIT_OBJECT_0: u32 = 0;
/// `STILL_ACTIVE`（进程还在跑时 `GetExitCodeProcess` 的返回值）。
const STILL_ACTIVE: u32 = 259;

/// `INVALID_HANDLE_VALUE`。
fn invalid_handle() -> Handle {
    -1isize as Handle
}

/// `HKEY_LOCAL_MACHINE`：预定义键值 `(HKEY)(LONG)0x80000002`，64 位需符号扩展。
fn hkey_local_machine() -> Hkey {
    // 先按 `LONG`（i32）解释成负数，再符号扩展到指针宽度——照抄 SDK 的宏语义，
    // 直接写 `0x8000_0002i32` 会溢出字面量（deny(overflowing_literals)）。
    (0x8000_0002u32 as i32 as isize) as Hkey
}

// ─────────────────────────── 小工具 ───────────────────────────

/// `&str` → NUL 结尾的 UTF-16。
fn wide(s: &str) -> Vec<u16> {
    OsStr::new(s)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

/// 覆写一段 `u16`（交给 Win32 的密码副本，调用返回后必须自己擦掉）。
fn wipe_wide(buf: &mut [u16]) {
    for u in buf.iter_mut() {
        // SAFETY: `u` 指向我们自己刚分配的 `Vec<u16>` 里的一个元素，写 0 良定义；
        // `write_volatile` 保证优化不会把这段「写完就没人读」的覆写删掉。
        unsafe { std::ptr::write_volatile(u, 0u16) };
    }
    std::sync::atomic::compiler_fence(std::sync::atomic::Ordering::SeqCst);
}

/// 当前进程是否以管理员身份运行。
pub fn is_elevated() -> bool {
    // SAFETY: `IsUserAnAdmin` 无参数、无副作用，返回值按 BOOL 解释。
    unsafe { ffi::IsUserAnAdmin() != 0 }
}

/// Win32 错误码 → 后端失败分类（访问被拒单列，便于把「没提权」与真故障分开）。
fn map_rc(rc: u32) -> SysError {
    if rc == ERROR_ACCESS_DENIED {
        SysError::Denied
    } else {
        SysError::Failed
    }
}

/// 内部诊断：只打**数值**错误码与**常量**值名，绝不打印值内容（密码可能在里面）。
fn log_fail(what: &str, rc: u32) {
    eprintln!("[elev] {what} 失败 rc={rc}");
}

// ─────────────────────────── 注册表：Winlogon 后端 ───────────────────────────

/// 打开 Winlogon 键（固定 `KEY_WOW64_64KEY`）。
fn open_winlogon(access: u32) -> Result<Hkey, u32> {
    let sub = wide(WINLOGON_SUBKEY);
    let mut h: Hkey = std::ptr::null_mut();
    // SAFETY: 子键名是 NUL 结尾 UTF-16；`h` 是合法出参。
    let rc = unsafe {
        ffi::RegOpenKeyExW(
            hkey_local_machine(),
            sub.as_ptr(),
            0,
            access | KEY_WOW64_64KEY,
            &mut h,
        )
    };
    if rc == ERROR_SUCCESS {
        Ok(h)
    } else {
        log_fail("RegOpenKeyExW(Winlogon)", rc);
        Err(rc)
    }
}

/// 写一个 `REG_SZ` 值。值内容可能是密码，返回前把本地 UTF-16 缓冲擦掉。
fn write_string(h: Hkey, name: &str, value: &str) -> Result<(), u32> {
    let name_w = wide(name);
    let mut data: Vec<u16> = OsStr::new(value).encode_wide().collect();
    data.push(0);
    // SAFETY: `data` 是刚建的 `Vec<u16>`，长度按字节算 `len*size_of::<u16>()`，指针有效。
    let rc = unsafe {
        ffi::RegSetValueExW(
            h,
            name_w.as_ptr(),
            0,
            REG_SZ,
            data.as_ptr() as *const u8,
            (data.len() * std::mem::size_of::<u16>()) as u32,
        )
    };
    wipe_wide(&mut data);
    if rc == ERROR_SUCCESS {
        Ok(())
    } else {
        log_fail("RegSetValueExW", rc);
        Err(rc)
    }
}

/// 删一个值；本来就不存在算成功（`clear` 因此幂等）。
fn delete_value(h: Hkey, name: &str) -> Result<(), u32> {
    let name_w = wide(name);
    // SAFETY: 键句柄来自 `open_winlogon`；值名是 NUL 结尾 UTF-16。
    let rc = unsafe { ffi::RegDeleteValueW(h, name_w.as_ptr()) };
    if rc == ERROR_SUCCESS || rc == ERROR_FILE_NOT_FOUND {
        Ok(())
    } else {
        log_fail("RegDeleteValueW", rc);
        Err(rc)
    }
}

/// 读一个值：`REG_SZ`/`REG_EXPAND_SZ` 按 UTF-16 解（**不展开**环境变量）、`REG_DWORD`
/// 按十进制字符串返回；其它类型一律 `None`（不猜）。
fn read_string(h: Hkey, name: &str) -> Result<Option<String>, u32> {
    let name_w = wide(name);
    let mut ty = 0u32;
    let mut cb = 0u32;
    // SAFETY: 第一次调用只问长度（数据指针给 null，缓冲容量 0）。
    let rc = unsafe {
        ffi::RegQueryValueExW(
            h,
            name_w.as_ptr(),
            std::ptr::null_mut(),
            &mut ty,
            std::ptr::null_mut(),
            &mut cb,
        )
    };
    if rc == ERROR_FILE_NOT_FOUND {
        return Ok(None);
    }
    if rc != ERROR_SUCCESS {
        log_fail("RegQueryValueExW(len)", rc);
        return Err(rc);
    }
    if cb == 0 {
        return Ok(Some(String::new()));
    }
    // 多留 2 字节：值可能没按 REG_SZ 约定带结尾 NUL。
    let mut buf = vec![0u8; cb as usize + 2];
    let mut cb2 = cb;
    // SAFETY: 缓冲至少 `cb` 字节，`cb2` 同步给出实际容量（in/out 参数）。
    let rc = unsafe {
        ffi::RegQueryValueExW(
            h,
            name_w.as_ptr(),
            std::ptr::null_mut(),
            &mut ty,
            buf.as_mut_ptr(),
            &mut cb2,
        )
    };
    if rc != ERROR_SUCCESS {
        log_fail("RegQueryValueExW(data)", rc);
        return Err(rc);
    }
    buf.truncate(cb2 as usize);
    match ty {
        REG_SZ | REG_EXPAND_SZ => {
            let units: Vec<u16> = buf
                .as_chunks::<2>()
                .0
                .iter()
                .map(|c| u16::from_le_bytes(*c))
                .collect();
            let s = String::from_utf16_lossy(&units);
            Ok(Some(s.trim_end_matches('\0').to_string()))
        }
        REG_DWORD if buf.len() >= 4 => Ok(Some(
            u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]).to_string(),
        )),
        _ => Ok(None),
    }
}

/// Winlogon 后端：三个白名单操作的**唯一**系统实现。
///
/// `get` **刻意不读** `DefaultPassword`：本操作只回答「开着吗、给谁开」，没必要把密码
/// 从注册表里捞出来（少一次接触就少一次泄漏面）。
pub struct WinlogonBackend;

impl WinlogonBackend {
    /// 未提权时写操作必然被系统拒绝——提前返回 `denied`，让调用方看到「UAC 没点 /
    /// helper 没提权」这个真实原因，而不是一个含糊的系统错误。
    fn require_elevated() -> Result<(), SysError> {
        if is_elevated() {
            Ok(())
        } else {
            Err(SysError::Denied)
        }
    }
}

impl SysBackend for WinlogonBackend {
    fn set_auto_login(&self, username: &str, password: &str) -> Result<(), SysError> {
        Self::require_elevated()?;
        let h = open_winlogon(KEY_QUERY_VALUE | KEY_SET_VALUE).map_err(map_rc)?;
        // 三值一起写。顺序：先开开关、再用户名、最后密码——中途失败也不会留下
        // 「开关=1 但用户名还是空的」这种可用组合；失败如实返回，由调用方决定重试/清除。
        let mut out = Ok(());
        for (name, value) in [
            (VAL_AUTO_ADMIN_LOGON, "1"),
            (VAL_DEFAULT_USERNAME, username),
            (VAL_DEFAULT_PASSWORD, password),
        ] {
            if let Err(rc) = write_string(h, name, value) {
                out = Err(map_rc(rc));
                break;
            }
        }
        // SAFETY: `h` 是 `open_winlogon` 返回的合法键句柄，且此后不再使用。
        unsafe { ffi::RegCloseKey(h) };
        out
    }

    fn clear_auto_login(&self) -> Result<(), SysError> {
        Self::require_elevated()?;
        let h = open_winlogon(KEY_QUERY_VALUE | KEY_SET_VALUE).map_err(map_rc)?;
        // 选「删三个值」而不是「写 AutoAdminLogon=0」：前者不留残留——尤其不给
        // `DefaultPassword` 留一个还在盘上的旧密码；删不存在 = 幂等成功。
        let mut out = Ok(());
        for name in [
            VAL_AUTO_ADMIN_LOGON,
            VAL_DEFAULT_USERNAME,
            VAL_DEFAULT_PASSWORD,
        ] {
            if let Err(rc) = delete_value(h, name) {
                out = Err(map_rc(rc));
                break;
            }
        }
        // SAFETY: 同上，句柄合法且此后不再使用。
        unsafe { ffi::RegCloseKey(h) };
        out
    }

    fn get_auto_login(&self) -> Result<AutoLoginState, SysError> {
        let h = open_winlogon(KEY_QUERY_VALUE).map_err(map_rc)?;
        let read = read_state(h);
        // SAFETY: 同上，句柄合法且此后不再使用。
        unsafe { ffi::RegCloseKey(h) };
        read.map_err(map_rc)
    }
}

/// 读两个值（不碰密码）。
fn read_state(h: Hkey) -> Result<AutoLoginState, u32> {
    let enabled = matches!(read_string(h, VAL_AUTO_ADMIN_LOGON)?.as_deref(), Some("1"));
    let username = read_string(h, VAL_DEFAULT_USERNAME)?;
    Ok(AutoLoginState {
        enabled,
        username: username.filter(|u| !u.is_empty()),
    })
}

// ─────────────────────────── 命名管道 ───────────────────────────

/// 当前用户 SID 的字符串形式（`S-1-5-21-…`），用于拼 SDDL。
fn current_user_sid_string() -> Result<String, String> {
    // SAFETY: 全程只用真实的 Win32 句柄与足够大、按 `usize` 对齐的缓冲。
    unsafe {
        let mut token: Handle = std::ptr::null_mut();
        if ffi::OpenProcessToken(ffi::GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return Err(format!("OpenProcessToken rc={}", ffi::GetLastError()));
        }
        let mut len = 0u32;
        // 第一次只为拿所需长度（按约定这次调用失败并填 `len`）。
        ffi::GetTokenInformation(token, TOKEN_USER, std::ptr::null_mut(), 0, &mut len);
        // 用 usize 缓冲保证指针对齐（TOKEN_USER 的第一个字段就是指针）。
        let words = (len as usize).div_ceil(std::mem::size_of::<usize>());
        let mut buf = vec![0usize; words];
        let ok = ffi::GetTokenInformation(
            token,
            TOKEN_USER,
            buf.as_mut_ptr() as *mut c_void,
            len,
            &mut len,
        );
        ffi::CloseHandle(token);
        if ok == 0 {
            return Err(format!(
                "GetTokenInformation(TokenUser) rc={}",
                ffi::GetLastError()
            ));
        }
        // `TOKEN_USER { User: SID_AND_ATTRIBUTES { Sid: PSID, Attributes: DWORD } }`
        let sid = *(buf.as_ptr() as *const *mut c_void);
        let mut sid_str: *mut u16 = std::ptr::null_mut();
        if ffi::ConvertSidToStringSidW(sid, &mut sid_str) == 0 {
            return Err(format!("ConvertSidToStringSidW rc={}", ffi::GetLastError()));
        }
        let mut n = 0usize;
        while *sid_str.add(n) != 0 {
            n += 1;
        }
        let s = String::from_utf16_lossy(std::slice::from_raw_parts(sid_str, n));
        ffi::LocalFree(sid_str as *mut c_void);
        Ok(s)
    }
}

/// 造一份 DACL 并执行 `f`，退出后立刻释放安全描述符。
///
/// SDDL `D:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;GA;;;<当前用户 SID>)`：
/// `P` = 保护型 DACL（不继承父对象的 ACE），ACE 只有三个主体——SYSTEM、内置
/// Administrators、**当前用户**。也就是说**其它用户连不上这条管道**，这是本通道的
/// 边界；令牌是它之上的第二道（见模块头）。
fn with_pipe_security<R>(f: impl FnOnce(&ffi::SecurityAttributes) -> R) -> Result<R, String> {
    let sid = current_user_sid_string()?;
    let sddl = format!("D:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;GA;;;{sid})");
    let sddl_w = wide(&sddl);
    let mut sd: *mut c_void = std::ptr::null_mut();
    // SAFETY: 输入是 NUL 结尾 UTF-16；输出是 LocalAlloc 出来的自相对安全描述符。
    let ok = unsafe {
        ffi::ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl_w.as_ptr(),
            SDDL_REVISION_1,
            &mut sd,
            std::ptr::null_mut(),
        )
    };
    if ok == 0 || sd.is_null() {
        // SAFETY: `GetLastError` 无参数。
        let rc = unsafe { ffi::GetLastError() };
        return Err(format!(
            "ConvertStringSecurityDescriptorToSecurityDescriptorW rc={rc}"
        ));
    }
    let sa = ffi::SecurityAttributes {
        n_length: std::mem::size_of::<ffi::SecurityAttributes>() as u32,
        lp_security_descriptor: sd,
        b_inherit_handle: 0,
    };
    let out = f(&sa);
    // SAFETY: `sd` 是上面那次调用分配的，只释放一次。
    unsafe { ffi::LocalFree(sd) };
    Ok(out)
}

/// 阻塞式「4 字节大端长度 + JSON」收发（服务端/客户端共用）。
struct PipeIo {
    handle: Handle,
}

// SAFETY: `PipeIo` 持有一个**独占**的 Win32 管道句柄。Windows 句柄属于进程而不是线程，
// 可以在任意线程上收发；本类型不可 `Clone`、内部没有共享可变状态，任一时刻只有一个持有
// 者能碰它，因此跨线程移动不会造成别名访问（`PipeServer` / `PipeClient` 因此也是 `Send`）。
unsafe impl Send for PipeIo {}

impl PipeIo {
    fn read_frame(&self, max: usize) -> Result<Vec<u8>, String> {
        let mut header = [0u8; FRAME_HEADER];
        self.read_exact(&mut header)?;
        let len = super::decode_frame_len(header);
        if !super::frame_len_ok(len) || len > max {
            return Err(format!("帧长非法：{len}"));
        }
        let mut body = vec![0u8; len];
        self.read_exact(&mut body)?;
        Ok(body)
    }

    fn write_frame(&self, payload: &[u8]) -> Result<(), String> {
        if payload.len() > MAX_BODY {
            return Err(format!("待发帧过长：{}", payload.len()));
        }
        let framed = super::encode_frame(payload);
        self.write_all(&framed)
    }

    fn read_exact(&self, buf: &mut [u8]) -> Result<(), String> {
        let mut off = 0usize;
        while off < buf.len() {
            let mut got = 0u32;
            // SAFETY: 目标缓冲是 `buf` 的剩余部分，长度按字节给出。
            let ok = unsafe {
                ffi::ReadFile(
                    self.handle,
                    buf[off..].as_mut_ptr() as *mut c_void,
                    (buf.len() - off) as u32,
                    &mut got,
                    std::ptr::null_mut(),
                )
            };
            if ok == 0 || got == 0 {
                // SAFETY: 无参数。
                let rc = unsafe { ffi::GetLastError() };
                return Err(format!("管道读取失败（对端可能已退出）rc={rc}"));
            }
            off += got as usize;
        }
        Ok(())
    }

    fn write_all(&self, buf: &[u8]) -> Result<(), String> {
        let mut off = 0usize;
        while off < buf.len() {
            let mut wrote = 0u32;
            // SAFETY: 源缓冲是 `buf` 的剩余部分，长度按字节给出。
            let ok = unsafe {
                ffi::WriteFile(
                    self.handle,
                    buf[off..].as_ptr() as *const c_void,
                    (buf.len() - off) as u32,
                    &mut wrote,
                    std::ptr::null_mut(),
                )
            };
            if ok == 0 || wrote == 0 {
                // SAFETY: 无参数。
                let rc = unsafe { ffi::GetLastError() };
                return Err(format!("管道写入失败 rc={rc}"));
            }
            off += wrote as usize;
        }
        Ok(())
    }
}

/// 管道**服务端**（helper 侧）：创建 + 等一个客户端。
pub struct PipeServer {
    io: PipeIo,
}

impl PipeServer {
    /// 创建单实例管道（`FILE_FLAG_FIRST_PIPE_INSTANCE`）+ 只放当前用户的 DACL。
    pub fn create(pipe: &str) -> Result<PipeServer, String> {
        let name_w = wide(pipe);
        let handle = with_pipe_security(|sa| {
            // SAFETY: 名字是 NUL 结尾 UTF-16；`sa` 指向本次调用期间有效的安全描述符。
            unsafe {
                ffi::CreateNamedPipeW(
                    name_w.as_ptr(),
                    PIPE_ACCESS_DUPLEX | FILE_FLAG_FIRST_PIPE_INSTANCE,
                    PIPE_BYTE_BLOCKING,
                    1,
                    MAX_BODY as u32,
                    MAX_BODY as u32,
                    0,
                    sa,
                )
            }
        })?;
        if handle == invalid_handle() || handle.is_null() {
            // SAFETY: 无参数。
            let rc = unsafe { ffi::GetLastError() };
            return Err(format!("CreateNamedPipeW 失败 rc={rc}"));
        }
        Ok(PipeServer {
            io: PipeIo { handle },
        })
    }

    /// 等客户端连上（阻塞）。兜底超时由 helper 的看门线程负责（见二进制入口）。
    pub fn accept(&self) -> Result<(), String> {
        // SAFETY: 句柄来自 `create`；同步模式下不传 OVERLAPPED。
        let ok = unsafe { ffi::ConnectNamedPipe(self.io.handle, std::ptr::null_mut()) };
        if ok == 0 {
            // SAFETY: 无参数。
            let rc = unsafe { ffi::GetLastError() };
            if rc != ERROR_PIPE_CONNECTED {
                return Err(format!("ConnectNamedPipe 失败 rc={rc}"));
            }
        }
        Ok(())
    }

    /// 对端进程 pid（审计 `source` 用）；取不到返回 `None`（有权限限制时会失败）。
    pub fn client_pid(&self) -> Option<u32> {
        let mut pid = 0u32;
        // SAFETY: 句柄有效；`pid` 是合法出参。
        let ok = unsafe { ffi::GetNamedPipeClientProcessId(self.io.handle, &mut pid) };
        if ok == 0 || pid == 0 {
            None
        } else {
            Some(pid)
        }
    }

    /// 读一个请求帧。
    pub fn read_frame(&self, max: usize) -> Result<Vec<u8>, String> {
        self.io.read_frame(max)
    }

    /// 写一个响应帧。
    pub fn write_frame(&self, payload: &[u8]) -> Result<(), String> {
        self.io.write_frame(payload)?;
        // 关键一步：服务端写完必须 `FlushFileBuffers` —— 它会在**客户端读走全部数据**
        // 之前阻塞；否则 helper 紧接着 `DisconnectNamedPipe` 会把还没被读走的响应**丢掉**
        // （客户端只看到 EOF，拿不到任何响应）。本批 E2E 实测踩到：审计里明明记了
        // `ok`，客户端却读不到一个字节。代价是若客户端不再读，这里会阻塞到看门超时。
        // SAFETY: 句柄来自 `create`，且已处于已连接状态。
        let ok = unsafe { ffi::FlushFileBuffers(self.io.handle) };
        if ok == 0 {
            // SAFETY: 无参数。
            let rc = unsafe { ffi::GetLastError() };
            return Err(format!("FlushFileBuffers 失败 rc={rc}"));
        }
        Ok(())
    }
}

impl Drop for PipeServer {
    fn drop(&mut self) {
        // SAFETY: 句柄由本结构独占，只在这里断开 + 关闭一次。
        unsafe {
            ffi::DisconnectNamedPipe(self.io.handle);
            ffi::CloseHandle(self.io.handle);
        }
    }
}

/// 管道**客户端**（调用方侧）：等 helper 把管道建出来后连上。
pub struct PipeClient {
    io: PipeIo,
}

impl PipeClient {
    /// 轮询连接（helper 要先过 UAC、启动、建管道，所以这里必须能等）。
    ///
    /// `give_up` 每次失败都会被问一次「还要不要继续等」——调用方用它检测「helper 已经
    /// 退出」（例如用户取消了 UAC），从而立刻失败，而不是白等到超时。
    pub fn connect(
        pipe: &str,
        timeout: Duration,
        mut give_up: impl FnMut() -> bool,
    ) -> Result<PipeClient, String> {
        let name_w = wide(pipe);
        let start = Instant::now();
        loop {
            // SAFETY: 名字是 NUL 结尾 UTF-16；不请求继承、不共享。
            let handle = unsafe {
                ffi::CreateFileW(
                    name_w.as_ptr(),
                    GENERIC_READ_WRITE,
                    0,
                    std::ptr::null(),
                    OPEN_EXISTING,
                    0,
                    std::ptr::null_mut(),
                )
            };
            if handle != invalid_handle() && !handle.is_null() {
                return Ok(PipeClient {
                    io: PipeIo { handle },
                });
            }
            // SAFETY: 无参数。
            let last = unsafe { ffi::GetLastError() };
            if give_up() {
                return Err(format!("提权 helper 在连上管道前就退出了（rc={last}）"));
            }
            if start.elapsed() >= timeout {
                return Err(format!("等待提权 helper 连接超时（最后 rc={last}）"));
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// 读一个响应帧。
    pub fn read_frame(&self, max: usize) -> Result<Vec<u8>, String> {
        self.io.read_frame(max)
    }

    /// 写一个请求帧。
    pub fn write_frame(&self, payload: &[u8]) -> Result<(), String> {
        self.io.write_frame(payload)
    }
}

impl Drop for PipeClient {
    fn drop(&mut self) {
        // SAFETY: 句柄由本结构独占，只关闭一次。
        unsafe {
            ffi::CloseHandle(self.io.handle);
        }
    }
}

// ─────────────────────────── runas 拉起 helper ───────────────────────────

/// helper 可执行文件路径：与当前进程同目录（与 `abb-helper` 同款打包约定）。
pub fn helper_exe_path() -> Result<PathBuf, String> {
    let exe = std::env::current_exe().map_err(|e| format!("拿不到当前可执行文件路径：{e}"))?;
    let dir = exe
        .parent()
        .ok_or_else(|| "当前可执行文件没有父目录".to_string())?;
    Ok(dir.join("abb-elev-helper.exe"))
}

/// 拉起提权 helper 的结果。
pub struct HelperOutcome {
    /// 响应帧内容（JSON）。
    pub response: Vec<u8>,
    /// helper 进程退出码（0 = 正常处理完一个请求）。**必须一起看**：退出码非 0 意味着
    /// 响应即使解析得出来也不可信（提权包装必须透传子进程退出码）。
    pub exit_code: u32,
}

/// 调用方侧：`runas` 拉起 helper → 连管道 → 发一个请求 → 读回响应 → 等 helper 退出。
///
/// 本批**不接线**到 GUI/CLI（只在库内可用，尚无调用点）；真机执行需要 UAC 交互，留给
/// owner 验证——本机不跑。
pub fn call_helper(
    op: super::Op,
    params: &serde_json::Value,
    timeout: Duration,
) -> Result<HelperOutcome, String> {
    let exe = helper_exe_path()?;
    if !exe.is_file() {
        return Err(format!("提权 helper 不存在：{}", exe.display()));
    }
    // 管道名带调用方 pid + 一次性随机串：并发调用互不干扰，也不给对方猜名字的机会。
    // SAFETY: `GetCurrentProcessId` 无参数。
    let pid = unsafe { ffi::GetCurrentProcessId() };
    let pipe = super::pipe_name(&format!("{pid}-{}", super::gen_token()));
    let token = super::gen_token();
    let args = format!("--pipe \"{pipe}\" --token {token}");

    // 先建管道再提权：helper 一启动就能连上，省掉「谁先谁后」的竞态。
    let server = PipeServer::create(&pipe)?;

    let exe_w = wide(&exe.to_string_lossy());
    let verb_w = wide("runas");
    let args_w = wide(&args);
    let mut sei = ffi::ShellExecuteInfoW {
        cb_size: std::mem::size_of::<ffi::ShellExecuteInfoW>() as u32,
        f_mask: SEE_MASK_NOCLOSEPROCESS,
        hwnd: std::ptr::null_mut(),
        lp_verb: verb_w.as_ptr(),
        lp_file: exe_w.as_ptr(),
        lp_parameters: args_w.as_ptr(),
        lp_directory: std::ptr::null(),
        n_show: SW_HIDE,
        h_inst_app: std::ptr::null_mut(),
        lp_id_list: std::ptr::null_mut(),
        lp_class: std::ptr::null(),
        h_key_class: std::ptr::null_mut(),
        dw_hot_key: 0,
        h_icon_or_monitor: std::ptr::null_mut(),
        h_process: std::ptr::null_mut(),
    };
    // SAFETY: `sei` 是完整的 SHELLEXECUTEINFOW，`cb_size` 已按结构大小填好。
    let ok =
        unsafe { ffi::ShellExecuteExW(&mut sei as *mut ffi::ShellExecuteInfoW as *mut c_void) };
    if ok == 0 {
        // SAFETY: 无参数。
        let rc = unsafe { ffi::GetLastError() };
        // 用户在 UAC 弹框上点了「否」：这是明确的拒绝，不是故障。
        if rc == ERROR_CANCELLED {
            return Err("提权被取消（用户在 UAC 弹框上拒绝）".to_string());
        }
        return Err(format!("ShellExecuteExW(runas) 失败 rc={rc}"));
    }
    let process = sei.h_process;

    // 等 helper 连上来；同时用进程句柄判断「它已经退出了」（例如 UAC 被取消 / 启动失败）。
    let client = PipeClient::connect(&pipe, timeout, || {
        // SAFETY: `process` 来自 ShellExecuteExW（SEE_MASK_NOCLOSEPROCESS）；null 视为「不知道」。
        !process.is_null() && unsafe { ffi::WaitForSingleObject(process, 0) } == WAIT_OBJECT_0
    });
    let client = match client {
        Ok(c) => c,
        Err(e) => {
            if !process.is_null() {
                // SAFETY: 句柄来自 ShellExecuteExW，所有权在我们手里，只关一次。
                unsafe { ffi::CloseHandle(process) };
            }
            return Err(e);
        }
    };

    // 发请求。缓冲里有密码 ⇒ 发完立刻擦掉我们这一侧的副本。
    let request = serde_json::json!({
        "token": token,
        "op": op.name(),
        "params": params,
    });
    let mut payload = serde_json::to_vec(&request).map_err(|e| format!("请求序列化失败：{e}"))?;
    let sent = client.write_frame(&payload);
    super::wipe_bytes(&mut payload);
    sent?;

    let response = client.read_frame(MAX_BODY);
    drop(client); // 先断开管道，再等 helper 退出

    // 无论读成功与否都要给子进程收尸 + 关句柄（否则句柄泄漏，且退出码丢失）。
    let exit_code = if process.is_null() {
        STILL_ACTIVE
    } else {
        // SAFETY: 句柄有效；超时按「还在跑」处理（返回 STILL_ACTIVE）。
        unsafe { ffi::WaitForSingleObject(process, timeout.as_millis() as u32) };
        let mut code = STILL_ACTIVE;
        // SAFETY: `code` 是合法出参。
        unsafe { ffi::GetExitCodeProcess(process, &mut code) };
        // SAFETY: 只关一次。
        unsafe { ffi::CloseHandle(process) };
        code
    };
    // 服务端句柄显式释放（helper 已退出，管道随后会被系统回收）。
    drop(server);

    let response = response?;
    Ok(HelperOutcome {
        response,
        exit_code,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    /// 回归（2026-09-24 E2E 实测的坑）：helper 是「写完响应就退出」的短命进程，服务端
    /// 析构时 `DisconnectNamedPipe` 会**丢弃客户端还没读走的数据**——实测表现是审计里记着
    /// `ok`，客户端一个字节都读不到。
    ///
    /// 本用例用真实的命名管道对（`PipeServer` + `PipeClient`）钉住这条性质：服务端写完帧
    /// **立刻析构**，客户端仍必须完整收到这一帧。去掉 `PipeServer::write_frame` 里的
    /// `FlushFileBuffers`（它在客户端读走全部数据前不返回），本用例会红。
    #[test]
    fn server_write_frame_survives_immediate_disconnect() {
        let name = format!(
            r"\\.\pipe\abb-elev-helper-test-{}-{}",
            std::process::id(),
            super::super::gen_token()
        );
        let server = PipeServer::create(&name).expect("建管道");
        let stop = Arc::new(AtomicBool::new(false));
        let stop_for_client = Arc::clone(&stop);
        let name_for_client = name.clone();
        let client_thread = std::thread::spawn(move || {
            PipeClient::connect(&name_for_client, Duration::from_secs(10), || {
                stop_for_client.load(Ordering::SeqCst)
            })
        });
        server.accept().expect("接受连接");
        let client = client_thread
            .join()
            .expect("客户端线程不应 panic")
            .expect("客户端应连上");

        // 客户端先进读等待；服务端写（内部 flush 会等客户端读走），随后立刻析构。
        let reader = std::thread::spawn(move || client.read_frame(MAX_BODY));
        let frame = br#"{"ok":true,"code":"ok"}"#;
        server.write_frame(frame).expect("写响应");
        drop(server);
        stop.store(true, Ordering::SeqCst);

        let got = reader
            .join()
            .expect("读线程不应 panic")
            .expect("服务端断开前必须把响应送达客户端");
        assert_eq!(got, frame);
    }

    /// `hkey_local_machine()` 必须与 SDK 的 `(HKEY)(LONG)0x80000002` 同值（64 位符号扩展）。
    #[test]
    fn hklm_handle_matches_sdk_macro() {
        let expected = 0x8000_0002u32 as i32 as isize as usize;
        assert_eq!(hkey_local_machine() as usize, expected);
    }
}
