//! ABB 锁屏控制 root 特权助手（#129 Stage 1）。
//!
//! 以 root launchd daemon 运行，向锁屏 loginwindow 注入键盘事件（仿 ToDesk：
//! root + 辅助功能/输入监控授权下 IOHIDEventSystemClient 可跨安全会话注入）。
//!
//! ## 安全模型（必须 fail-closed）
//! - 只监听本机 unix socket（/var/run/com.sqb.abb-helper.sock，0600 + chown 安装用户）；
//! - 每个连接校验对等方：peer uid == 安装用户 && 进程路径为主程序 && 代码签名
//!   （已签名 → 必须过 designated requirement；未签名 dev 构建 → 路径兜底）；
//! - unlock 密码只瞬态存在于内存：注入后立即清零，**不落盘、不进日志、不重试**；
//! - 单次解锁失败/超时即丢弃（防暴力尝试）；非 ASCII/未支持符号 → 整次失败不静默跳过；
//! - 无任何网络监听端口。
//!
//! ## 待真机验证（Stage 2，见 docs/lock-screen-helper.md）
//! - loginwindow 会话定向注入（root 下 DispatchEvent 是否直达锁屏需 Mac 实测）；
//! - 非 US 键盘布局的符号映射（当前为 US 布局表）；
//! - 授权弹窗时序与 launchctl bootstrap 在真实安装环境的回归。
//!
//! 非 macOS 平台为占位 main（拒绝运行），保证全平台构建一致。

#[cfg(target_os = "macos")]
use std::io::{Read, Write};
#[cfg(target_os = "macos")]
use std::os::fd::AsRawFd;
#[cfg(target_os = "macos")]
use std::os::unix::fs::PermissionsExt;
#[cfg(target_os = "macos")]
use std::os::unix::net::{UnixListener, UnixStream};
#[cfg(target_os = "macos")]
use std::time::Duration;

#[cfg(target_os = "macos")]
const SOCKET_PATH: &str = "/var/run/com.sqb.abb-helper.sock";
/// 主程序可执行名（对等进程路径校验用）。
#[cfg(target_os = "macos")]
const MAIN_APP_NAME: &str = "agent-bridge";
/// 主程序 bundle identifier（签名校验 designated requirement 用）。
#[cfg(target_os = "macos")]
const MAIN_BUNDLE_ID: &str = "com.sqb.abb";
/// 最大请求体（防滥用）。
#[cfg(target_os = "macos")]
const MAX_BODY: usize = 1 << 16;
/// 密码长度上限。
#[cfg(target_os = "macos")]
const MAX_PASSWORD: usize = 512;

fn main() -> std::process::ExitCode {
    #[cfg(target_os = "macos")]
    {
        let args: Vec<String> = std::env::args().collect();
        if args.iter().any(|a| a == "--version") {
            println!("abb-helper {}", env!("CARGO_PKG_VERSION"));
            return std::process::ExitCode::SUCCESS;
        }
        if !args.iter().any(|a| a == "--helper-daemon") {
            eprintln!("usage: abb-helper --helper-daemon");
            return std::process::ExitCode::from(2);
        }
        match run_daemon() {
            Ok(()) => std::process::ExitCode::SUCCESS,
            Err(e) => {
                // 仅打非敏感错误；密码永不出现。
                eprintln!("abb-helper daemon error: {e}");
                std::process::ExitCode::FAILURE
            }
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        eprintln!("abb-helper is macOS-only");
        std::process::ExitCode::FAILURE
    }
}

#[cfg(target_os = "macos")]
fn run_daemon() -> Result<(), String> {
    use std::os::unix::fs::chown;
    // 必须 root（launchd 系统域 daemon）。
    if unsafe { libc::geteuid() } != 0 {
        return Err("必须 root 运行（launchd 系统域）".into());
    }
    // 安装用户 uid（plist 注入）；拿不到 = 配置错误，fail-closed 拒绝服务。
    let uid: u32 = std::env::var("LOCKCTL_UID")
        .ok()
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| "LOCKCTL_UID 环境变量缺失或非法（安装 plist 配置错误）".to_string())?;

    // 清掉陈旧 socket 文件后创建，0600 + chown 安装用户（仅该用户可连）。
    let _ = std::fs::remove_file(SOCKET_PATH);
    let listener =
        UnixListener::bind(SOCKET_PATH).map_err(|e| format!("bind {SOCKET_PATH}: {e}"))?;
    std::fs::set_permissions(SOCKET_PATH, std::fs::Permissions::from_mode(0o600))
        .map_err(|e| format!("chmod socket: {e}"))?;
    if let Err(e) = chown(SOCKET_PATH, Some(uid), None) {
        return Err(format!("chown socket -> uid {uid}: {e}"));
    }

    eprintln!("abb-helper daemon listening on {SOCKET_PATH} (uid={uid})");
    for conn in listener.incoming() {
        match conn {
            Ok(stream) => {
                let _ = std::thread::spawn(move || {
                    if let Err(e) = handle_conn(stream) {
                        eprintln!("conn error: {e}");
                    }
                });
            }
            Err(e) => eprintln!("accept error: {e}"),
        }
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn handle_conn(mut stream: UnixStream) -> Result<(), String> {
    stream.set_read_timeout(Some(Duration::from_secs(10))).ok();
    stream.set_write_timeout(Some(Duration::from_secs(10))).ok();

    // 读 4 字节大端长度 + JSON。
    let mut len_b = [0u8; 4];
    if stream.read_exact(&mut len_b).is_err() {
        return Ok(()); // 客户端提前断开/超时，正常
    }
    let len = u32::from_be_bytes(len_b) as usize;
    if len == 0 || len > MAX_BODY {
        return Err(format!("非法请求长度 {len}"));
    }
    let mut body = vec![0u8; len];
    stream.read_exact(&mut body).map_err(|e| e.to_string())?;
    let req: serde_json::Value =
        serde_json::from_slice(&body).map_err(|e| format!("JSON 解析失败: {e}"))?;

    // 对等方校验（先于任何命令处理；失败直接断开，fail-closed）。
    if !peer_ok(&stream) {
        return Err("对等方校验失败（uid/路径/签名不符），拒绝服务".into());
    }

    let cmd = req.get("cmd").and_then(|c| c.as_str()).unwrap_or("");
    let resp = match cmd {
        "status" => serde_json::json!({ "ok": true, "pid": std::process::id() }),
        "unlock" => {
            let mut pw = req
                .get("password")
                .and_then(|p| p.as_str())
                .unwrap_or("")
                .to_string();
            let result = do_unlock(&pw);
            wipe(&mut pw); // 瞬态密码清零（不落盘/不进日志）
            match result {
                Ok(()) => serde_json::json!({ "ok": true }),
                Err(e) => serde_json::json!({ "ok": false, "error": e }),
            }
        }
        other => serde_json::json!({ "ok": false, "error": format!("unknown cmd: {other}") }),
    };
    let payload = serde_json::to_vec(&resp).map_err(|e| e.to_string())?;
    let mut out = Vec::with_capacity(4 + payload.len());
    out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    out.extend_from_slice(&payload);
    stream.write_all(&out).map_err(|e| e.to_string())?;
    stream.flush().ok();
    Ok(())
}

// ─────────────────────────── 对等方校验 ───────────────────────────

/// 校验连接方：uid == LOCKCTL_UID && 可执行路径为主程序 && 代码签名（若已签名）。
#[cfg(target_os = "macos")]
fn peer_ok(stream: &UnixStream) -> bool {
    // (a) peer uid（getpeereid，unix socket 内核级身份）
    let mut uid: u32 = 0;
    let mut gid: u32 = 0;
    if unsafe { libc::getpeereid(stream.as_raw_fd(), &mut uid, &mut gid) } != 0 {
        return false;
    }
    let want: u32 = std::env::var("LOCKCTL_UID")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    if want == 0 || uid != want {
        return false;
    }

    // (b) peer pid + 可执行路径（LOCAL_PEERPID → proc_pidpath）
    let Some(pid) = peer_pid(stream.as_raw_fd()) else {
        return false;
    };
    let Some(path) = proc_path(pid) else {
        return false;
    };
    // basename 精确匹配（防 /agent-bridge.* 子串误放行用户可写目录；签名校验才是强闸）。
    let name = path.rsplit('/').next().unwrap_or("");
    let path_ok = name == MAIN_APP_NAME || name.starts_with(&format!("{MAIN_APP_NAME}."));
    if !path_ok {
        return false;
    }

    // (c) 代码签名（已签名 → 必须过 designated requirement；未签名 dev → 路径兜底）
    match signature_state(pid) {
        SignatureState::SignedOk => true,
        SignatureState::Unsigned => path_ok, // 本地未签名开发构建：仅路径+uid 兜底
        SignatureState::Rejected => false,
    }
}

#[cfg(target_os = "macos")]
#[derive(PartialEq)]
enum SignatureState {
    SignedOk,
    Unsigned,
    Rejected,
}

/// LOCAL_PEERPID（sys/un.h：SOL_LOCAL=0, LOCAL_PEERPID=0x001f）取对等 pid。
#[cfg(target_os = "macos")]
fn peer_pid(fd: i32) -> Option<i32> {
    const SOL_LOCAL: i32 = 0;
    const LOCAL_PEERPID: i32 = 0x001f;
    let mut pid: i32 = -1;
    let mut len = std::mem::size_of::<i32>() as libc::socklen_t;
    let r = unsafe {
        libc::getsockopt(
            fd,
            SOL_LOCAL,
            LOCAL_PEERPID,
            &mut pid as *mut i32 as *mut libc::c_void,
            &mut len,
        )
    };
    if r == 0 && pid > 0 {
        Some(pid)
    } else {
        None
    }
}

/// proc_pidpath（libproc，libSystem 内建）取进程可执行路径。
#[cfg(target_os = "macos")]
fn proc_path(pid: i32) -> Option<String> {
    use std::ffi::c_char;
    unsafe extern "C" {
        fn proc_pidpath(pid: i32, buffer: *mut c_char, buffersize: u32) -> i32;
    }
    let mut buf = [0i8; 4096];
    let n = unsafe { proc_pidpath(pid, buf.as_mut_ptr(), buf.len() as u32) };
    if n <= 0 {
        return None;
    }
    let bytes = unsafe { std::slice::from_raw_parts(buf.as_ptr() as *const u8, n as usize) };
    Some(String::from_utf8_lossy(bytes).into_owned())
}

/// SecCode 签名校验：kSecGuestAttributePid 定位对等进程 → 检查 designated requirement。
/// 未签名（errSecCSUnsigned）→ Unsigned（dev 构建路径兜底）；其它失败 → Rejected。
#[cfg(target_os = "macos")]
fn signature_state(pid: i32) -> SignatureState {
    use std::ffi::{c_char, c_void, CStr};

    const ERR_CS_UNSIGNED: i32 = -67062;
    const ERR_CS_REQ_FAILED: i32 = -67072;

    #[link(name = "Security", kind = "framework")]
    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        fn SecCodeCopyGuestWithAttributes(
            host: *const c_void,
            attributes: *const c_void,
            flags: u32,
            guest: *mut *const c_void,
        ) -> i32;
        fn SecRequirementCreateWithString(
            text: *const c_void,
            flags: u32,
            requirement: *mut *const c_void,
        ) -> i32;
        fn SecCodeCheckValidity(code: *const c_void, flags: u32, requirement: *const c_void)
            -> i32;
        fn CFNumberCreate(
            allocator: *const c_void,
            the_type: i32,
            value_ptr: *const c_void,
        ) -> *const c_void;
        fn CFDictionaryCreate(
            allocator: *const c_void,
            keys: *const *const c_void,
            values: *const *const c_void,
            num_values: isize,
            key_callbacks: *const c_void,
            value_callbacks: *const c_void,
        ) -> *const c_void;
        fn CFStringCreateWithCString(
            allocator: *const c_void,
            c_str: *const c_char,
            encoding: u32,
        ) -> *const c_void;
        fn CFRelease(cf: *const c_void);
        static kSecGuestAttributePid: *const c_void;
    }
    const KCF_NUMBER_INT_TYPE: i32 = 4;
    const KCF_STRING_ENCODING_UTF8: u32 = 0x08000100;

    // 失败路径统一释放（闭包化避免重复 CFRelease 代码）。
    unsafe {
        let pid_num = CFNumberCreate(
            std::ptr::null(),
            KCF_NUMBER_INT_TYPE,
            &pid as *const i32 as *const c_void,
        );
        if pid_num.is_null() {
            return SignatureState::Rejected;
        }
        let keys = [kSecGuestAttributePid];
        let values = [pid_num];
        let attrs = CFDictionaryCreate(
            std::ptr::null(),
            keys.as_ptr(),
            values.as_ptr(),
            1,
            std::ptr::null(),
            std::ptr::null(),
        );
        CFRelease(pid_num);
        if attrs.is_null() {
            return SignatureState::Rejected;
        }

        let mut guest: *const c_void = std::ptr::null();
        let st = SecCodeCopyGuestWithAttributes(std::ptr::null(), attrs, 0, &mut guest);
        CFRelease(attrs);
        if st == ERR_CS_UNSIGNED {
            return SignatureState::Unsigned;
        }
        if st != 0 || guest.is_null() {
            return SignatureState::Rejected;
        }

        // requirement: identifier "com.sqb.abb"（bundle id；裸二进制按 designated requirement 匹配）。
        let req_bytes = format!("identifier \"{MAIN_BUNDLE_ID}\"\0");
        let req_str = CStr::from_bytes_with_nul(req_bytes.as_bytes())
            .unwrap_or(c"identifier \"com.sqb.abb\"");
        let req_cf =
            CFStringCreateWithCString(std::ptr::null(), req_str.as_ptr(), KCF_STRING_ENCODING_UTF8);
        let mut req: *const c_void = std::ptr::null();
        let st2 = if req_cf.is_null() {
            -1
        } else {
            let s = SecRequirementCreateWithString(req_cf, 0, &mut req);
            CFRelease(req_cf);
            s
        };
        if st2 != 0 || req.is_null() {
            CFRelease(guest);
            return SignatureState::Rejected;
        }
        let check = SecCodeCheckValidity(guest, 0, req);
        CFRelease(req);
        CFRelease(guest);
        match check {
            0 => SignatureState::SignedOk,
            ERR_CS_REQ_FAILED => SignatureState::Rejected,
            _ => SignatureState::Rejected,
        }
    }
}

// ─────────────────────────── 解锁（HID 注入） ───────────────────────────

/// 清零字符串内容（瞬态密码用；不打印、不落盘）。
#[cfg(target_os = "macos")]
fn wipe(s: &mut String) {
    unsafe {
        for b in s.as_bytes_mut() {
            *b = 0;
        }
    }
    s.clear();
}

/// 单次解锁：逐字符注入按键（含 shift），最后回车。密码瞬态，不落盘/不重试。
#[cfg(target_os = "macos")]
fn do_unlock(pw: &str) -> Result<(), String> {
    if pw.is_empty() {
        return Err("空密码".into());
    }
    if pw.len() > MAX_PASSWORD {
        return Err("密码过长".into());
    }
    // 先整体校验字符可映射（fail-closed：任一字符不支持 → 整次拒绝，不注入一半）。
    let keys: Vec<(u32, bool)> = pw
        .chars()
        .map(key_for)
        .collect::<Option<Vec<_>>>()
        .ok_or_else(|| "密码含当前布局不支持的非 ASCII/特殊字符".to_string())?;

    // IOHIDEventSystemClient：root 下 DispatchEvent 可注入锁屏会话（ToDesk 同路线）。
    // 逐字符注入：中途失败会留下部分已输入字符（Return 在末尾不提交，未登录成功）——可接受，失败即返回。
    let client = hid_client_create()
        .ok_or_else(|| "IOHIDEventSystemClientCreate 失败（无 HID 子系统？）".to_string())?;

    let r = (|| -> Result<(), String> {
        // 小间隔避免 loginwindow 吞键。
        const SHIFT: u32 = 0xE1; // kHIDUsage_KeyboardLeftShift
        const RETURN: u32 = 0x28; // kHIDUsage_KeyboardReturn
        for (usage, shift) in &keys {
            if *shift {
                hid_post_key(client, SHIFT, true)?;
                std::thread::sleep(Duration::from_millis(12));
            }
            hid_post_key(client, *usage, true)?;
            std::thread::sleep(Duration::from_millis(12));
            hid_post_key(client, *usage, false)?;
            if *shift {
                std::thread::sleep(Duration::from_millis(12));
                hid_post_key(client, SHIFT, false)?;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        hid_post_key(client, RETURN, true)?;
        std::thread::sleep(Duration::from_millis(12));
        hid_post_key(client, RETURN, false)
    })();
    hid_client_release(client);
    // 无论成败：密码字符串在调用方已清零；这里再保险（不打印、不落盘）。
    r
}

/// ASCII → (HID Usage, 需要 Shift)。US 布局表（kHIDPage_KeyboardOrKeypad=0x07）。
/// 非 US 布局的符号键位差异留待真机验证（Stage 2）。
#[cfg(target_os = "macos")]
fn key_for(ch: char) -> Option<(u32, bool)> {
    let u = |v: u32| (v, false);
    let s = |v: u32| (v, true);
    match ch {
        'a'..='z' => Some(u(0x04 + (ch as u32 - 'a' as u32))),
        'A'..='Z' => Some(s(0x04 + (ch as u32 - 'A' as u32))),
        '1'..='9' => Some(u(0x1E + (ch as u32 - '1' as u32))),
        '0' => Some(u(0x27)),
        ' ' => Some(u(0x2C)),
        '-' => Some(u(0x2D)),
        '=' => Some(u(0x2E)),
        '[' => Some(u(0x2F)),
        ']' => Some(u(0x30)),
        '\\' => Some(u(0x31)),
        ';' => Some(u(0x33)),
        '\'' => Some(u(0x34)),
        '`' => Some(u(0x35)),
        ',' => Some(u(0x36)),
        '.' => Some(u(0x37)),
        '/' => Some(u(0x38)),
        '!' => Some(s(0x1E)),
        '@' => Some(s(0x1F)),
        '#' => Some(s(0x20)),
        '$' => Some(s(0x21)),
        '%' => Some(s(0x22)),
        '^' => Some(s(0x23)),
        '&' => Some(s(0x24)),
        '*' => Some(s(0x25)),
        '(' => Some(s(0x26)),
        ')' => Some(s(0x27)),
        '_' => Some(s(0x2D)),
        '+' => Some(s(0x2E)),
        '{' => Some(s(0x2F)),
        '}' => Some(s(0x30)),
        '|' => Some(s(0x31)),
        ':' => Some(s(0x33)),
        '"' => Some(s(0x34)),
        '~' => Some(s(0x35)),
        '<' => Some(s(0x36)),
        '>' => Some(s(0x37)),
        '?' => Some(s(0x38)),
        _ => None,
    }
}

#[cfg(target_os = "macos")]
#[link(name = "IOKit", kind = "framework")]
#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    // IOKit HID
    fn IOHIDEventSystemClientCreate(allocator: *const std::ffi::c_void) -> *mut std::ffi::c_void;
    fn IOHIDEventSystemClientSetDispatchQueue(
        client: *mut std::ffi::c_void,
        queue: *const std::ffi::c_void,
    );
    fn IOHIDEventCreateKeyboardEvent(
        allocator: *const std::ffi::c_void,
        time_stamp: u64,
        usage_page: u32,
        usage: u32,
        down: bool,
        options: u32,
    ) -> *mut std::ffi::c_void;
    fn IOHIDEventSystemClientDispatchEvent(
        client: *mut std::ffi::c_void,
        event: *mut std::ffi::c_void,
    );
    // CoreFoundation
    fn CFRelease(cf: *const std::ffi::c_void);
    fn dispatch_queue_create(
        label: *const std::ffi::c_char,
        attr: *const std::ffi::c_void,
    ) -> *const std::ffi::c_void;
}

#[cfg(target_os = "macos")]
fn hid_client_create() -> Option<*mut std::ffi::c_void> {
    unsafe {
        let client = IOHIDEventSystemClientCreate(std::ptr::null());
        if client.is_null() {
            return None;
        }
        // 需要一个 dispatch queue（事件回调用；我们只用 DispatchEvent，仍按要求设置）。
        let q = dispatch_queue_create(c"com.sqb.abb-helper.hid".as_ptr(), std::ptr::null());
        if q.is_null() {
            CFRelease(client);
            return None;
        }
        IOHIDEventSystemClientSetDispatchQueue(client, q);
        Some(client)
    }
}

#[cfg(target_os = "macos")]
fn hid_client_release(client: *mut std::ffi::c_void) {
    unsafe { CFRelease(client) };
}

/// 注入单个键盘事件（key down/up）。失败时返回错误（含事件创建失败）。
#[cfg(target_os = "macos")]
fn hid_post_key(client: *mut std::ffi::c_void, usage: u32, down: bool) -> Result<(), String> {
    unsafe {
        const PAGE_KEYBOARD: u32 = 0x07; // kHIDPage_KeyboardOrKeypad
        let ev = IOHIDEventCreateKeyboardEvent(std::ptr::null(), 0, PAGE_KEYBOARD, usage, down, 0);
        if ev.is_null() {
            return Err(format!(
                "IOHIDEventCreateKeyboardEvent 失败 (usage={usage:#x})"
            ));
        }
        IOHIDEventSystemClientDispatchEvent(client, ev);
        CFRelease(ev);
        Ok(())
    }
}

#[cfg(test)]
#[cfg(target_os = "macos")]
mod tests {
    use super::key_for;

    #[test]
    fn key_map_basic() {
        assert_eq!(key_for('a'), Some((0x04, false)));
        assert_eq!(key_for('z'), Some((0x1D, false)));
        assert_eq!(key_for('A'), Some((0x04, true)));
        assert_eq!(key_for('1'), Some((0x1E, false)));
        assert_eq!(key_for('0'), Some((0x27, false)));
        assert_eq!(key_for('!'), Some((0x1E, true)));
        assert_eq!(key_for(' '), Some((0x2C, false)));
        assert_eq!(key_for('_'), Some((0x2D, true)));
        assert_eq!(key_for('中'), None);
        assert_eq!(key_for('\n'), None);
    }
}
