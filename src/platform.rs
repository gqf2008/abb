//! 平台抽象 —— 跨平台差异都收敛在这里（打开文件夹 / 开机自启 / 一次性数据迁移）。
//! 设计目标：本体（WS/协议/agent/定时）全平台同码；平台特有只在 macOS 实现，Win/Linux 留 stub。
//! 「服务监控 + 开机自启」：service 生命周期由 GUI 看门（不经 launchd/systemd/任务计划）；
//! 自启在 macOS 用 LaunchAgent，且改完必须 `launchctl` reload——只写文件时 launchd
//! 用的仍是内存里那份旧定义（历史上正是这个让"改了不生效"）。

use anyhow::{Context, Result};
use std::path::PathBuf;

/// 用系统默认方式「打开」一个路径（访达/资源管理器）。
pub fn open_path(path: &std::path::Path) {
    #[cfg(target_os = "macos")]
    {
        let _ = std::process::Command::new("open").arg(path).spawn();
    }
    #[cfg(target_os = "windows")]
    {
        let _ = crate::spawn::command("explorer").arg(path).spawn();
    }
    #[cfg(target_os = "linux")]
    {
        let _ = std::process::Command::new("xdg-open").arg(path).spawn();
    }
}

/// 用系统默认浏览器打开一个 URL（依赖安装文档等）。三平台各自的起手式。
pub fn open_url(url: &str) {
    #[cfg(target_os = "macos")]
    {
        let _ = std::process::Command::new("open").arg(url).spawn();
    }
    #[cfg(target_os = "windows")]
    {
        // start 把首个带引号参数当窗口标题，故先给空标题再给 url；
        // CREATE_NO_WINDOW（统一走 crate::spawn）避免 cmd 闪控制台。
        let _ = crate::spawn::command("cmd")
            .args(["/c", "start", "", url])
            .spawn();
    }
    #[cfg(target_os = "linux")]
    {
        let _ = std::process::Command::new("xdg-open").arg(url).spawn();
    }
}

/// 复制文本到系统剪贴板（授权码等）。pbcopy / clip / wl-copy+xclip 尽力而为；失败返回 false。
/// 命令失败（如无头环境无 xclip）只回 false 不 panic，调用方给用户提示。
/// 注意：写完 stdin 必须关闭（drop）让子进程读到 EOF 才结束，否则 wait() 会死锁。
pub fn copy_to_clipboard(text: &str) -> bool {
    use std::io::Write;
    use std::process::Stdio;
    #[cfg(target_os = "macos")]
    let mut cmd = std::process::Command::new("pbcopy");
    #[cfg(target_os = "windows")]
    let mut cmd = crate::spawn::command("clip");
    #[cfg(target_os = "linux")]
    let mut cmd = {
        // Wayland 用 wl-copy，X11 用 xclip，都缺则失败
        if std::process::Command::new("wl-copy")
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
        {
            std::process::Command::new("wl-copy")
        } else if std::process::Command::new("xclip")
            .arg("-version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
        {
            let mut c = std::process::Command::new("xclip");
            c.args(["-selection", "clipboard"]);
            c
        } else {
            return false;
        }
    };
    let mut child = match cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(c) => c,
        Err(_) => return false,
    };
    let mut stdin = match child.stdin.take() {
        Some(s) => s,
        None => return false,
    };
    if stdin.write_all(text.as_bytes()).is_err() {
        return false;
    }
    drop(stdin); // 关闭管道 → 子进程读到 EOF 才退出
    child.wait().map(|s| s.success()).unwrap_or(false)
}

/// 当前可执行文件路径（GUI 本体）。
pub fn current_exe() -> Result<PathBuf> {
    std::env::current_exe().context("拿不到当前可执行文件路径")
}

// ─────────────────────────── macOS：激活策略（dock 图标 + 置前）───────────────────────────

/// 打开窗口前调用：accessory → regular（dock 出图标），并 activate 抢前台聚焦。
/// 这是「点托盘设置 → dock 有图标、窗口置前可输入」的关键。accessory 进程的窗口既不置前也没 dock 图标。
/// 实现：直接调 AppKit（项目零依赖原则，手写 objc_msgSend FFI，不引 objc crate）。
/// 须在 Slint/winit 事件循环线程（即主线程）调——NSApplication 非线程安全。
#[cfg(target_os = "macos")]
pub fn set_dock_visible(visible: bool) {
    macos_activate(visible);
    if visible {
        // 提升为 regular 后，Dock 图标默认是「终端/控制台」占位图（裸二进制无 bundle 图标）。
        // 显式把 AppIcon 设到 NSApplication，让 Dock 显示应用图标（debug 裸跑也对；
        // 打包成 .app 后 CFBundleIconFile 本就生效，这里是无害的双保险）。
        set_app_icon();
    }
}

/// 把应用图标设到 NSApplication.applicationIconImage。
/// 图标来源：优先 .app bundle 的 Resources/AppIcon.icns（打包后）；否则 app-assets/icon-1024.png（debug 源码树）。
/// 手写 objc_msgSend FFI（零依赖原则，同 macos_activate）。须在主线程调。
#[cfg(target_os = "macos")]
fn set_app_icon() {
    #![allow(non_snake_case)]
    use std::ffi::c_void;
    use std::sync::OnceLock;

    // 解析图标文件路径（只需一次）。优先 .icns（内含多尺寸，系统自挑档 + 套 macOS 圆角模板）；
    // 找不到才退回 PNG（需手动设 size，否则按像素尺寸当点尺寸画 → Dock 图标巨大）。
    static ICON_PATH: OnceLock<Option<std::path::PathBuf>> = OnceLock::new();
    let path = ICON_PATH.get_or_init(|| {
        let exe = std::env::current_exe().ok();
        let profile = exe.as_ref().and_then(|e| e.parent()); // …/Contents/MacOS 或 …/target/{debug,release}
        let candidates = [
            // .app bundle（打包后）
            profile.map(|p| p.join("../Resources/AppIcon.icns")),
            // debug：源码树 app-assets（先 icns 后 png）
            profile.map(|p| p.join("../../app-assets/AppIcon.icns")),
            profile.map(|p| p.join("../../app-assets/icon-1024.png")),
        ];
        for c in candidates.into_iter().flatten() {
            if let Some(p) = c.canonicalize().ok().filter(|p| p.exists()) {
                return Some(p);
            }
        }
        None
    });
    let Some(path) = path else { return };
    let is_icns = path.extension().is_some_and(|e| e == "icns");

    type Id = *mut c_void;
    type Sel = *mut c_void;
    unsafe extern "C" {
        fn objc_getClass(name: *const std::ffi::c_char) -> Id;
        fn sel_registerName(name: *const std::ffi::c_char) -> Sel;
        fn objc_msgSend();
    }
    // objc selector 缓存（usize 存储，原始指针非 Send/Sync；进程内全局常量地址，缓存安全）。
    type Sels = (usize, usize, usize, usize, usize, usize, usize, usize);
    static SELS: OnceLock<Sels> = OnceLock::new();
    let (
        shared_app,
        alloc,
        init_with_data,
        data_with_file,
        init_with_file,
        set_size,
        set_app_icon,
        release,
    ) = *SELS.get_or_init(|| unsafe {
        (
            sel_registerName(c"sharedApplication".as_ptr()) as usize,
            sel_registerName(c"alloc".as_ptr()) as usize,
            sel_registerName(c"initWithData:".as_ptr()) as usize,
            sel_registerName(c"dataWithContentsOfFile:".as_ptr()) as usize,
            sel_registerName(c"initWithContentsOfFile:".as_ptr()) as usize,
            sel_registerName(c"setSize:".as_ptr()) as usize,
            sel_registerName(c"setApplicationIconImage:".as_ptr()) as usize,
            sel_registerName(c"release".as_ptr()) as usize,
        )
    });
    let (
        shared_app,
        alloc,
        init_with_data,
        data_with_file,
        init_with_file,
        set_size,
        set_app_icon,
        release,
    ) = (
        shared_app as Sel,
        alloc as Sel,
        init_with_data as Sel,
        data_with_file as Sel,
        init_with_file as Sel,
        set_size as Sel,
        set_app_icon as Sel,
        release as Sel,
    );

    // NSSize 是 {f64, f64} 值类型；objc_msgSend 按位传。
    #[repr(C)]
    struct NSSize {
        width: f64,
        height: f64,
    }

    unsafe {
        let msg0: unsafe extern "C" fn(Id, Sel) -> Id =
            std::mem::transmute(objc_msgSend as unsafe extern "C" fn());
        let msg1: unsafe extern "C" fn(Id, Sel, Id) -> Id =
            std::mem::transmute(objc_msgSend as unsafe extern "C" fn());
        let msg_size: unsafe extern "C" fn(Id, Sel, NSSize) =
            std::mem::transmute(objc_msgSend as unsafe extern "C" fn());

        // NSString *pathStr = [NSString stringWithUTF8String:cpath]
        let cpath = match std::ffi::CString::new(path.to_string_lossy().as_bytes()) {
            Ok(c) => c,
            Err(_) => return,
        };
        let nsstring_cls = objc_getClass(c"NSString".as_ptr());
        let path_str = msg1(
            nsstring_cls,
            sel_registerName(c"stringWithUTF8String:".as_ptr()),
            cpath.as_ptr() as Id,
        );
        if path_str.is_null() {
            return;
        }
        let nsimage_cls = objc_getClass(c"NSImage".as_ptr());
        let img = if is_icns {
            // icns：initWithContentsOfFile 直接读（内含多尺寸，系统自挑档 + 套圆角模板）
            msg1(msg0(nsimage_cls, alloc), init_with_file, path_str)
        } else {
            // PNG：initWithContentsOfFile 常返回 nil（只认 icns），改走 NSData + initWithData
            let nsdata_cls = objc_getClass(c"NSData".as_ptr());
            let data = msg1(nsdata_cls, data_with_file, path_str);
            if data.is_null() {
                crate::log!("[platform] ⚠️ NSData nil: {}", path.display());
                return;
            }
            let img = msg1(msg0(nsimage_cls, alloc), init_with_data, data);
            if !img.is_null() {
                // 不设 size 会按 PNG 像素(1024)当点尺寸画 → Dock 图标巨大。设成 Dock 标准点尺寸。
                msg_size(
                    img,
                    set_size,
                    NSSize {
                        width: 512.0,
                        height: 512.0,
                    },
                );
            }
            img
        };
        if img.is_null() {
            crate::log!("[platform] ⚠️ NSImage 加载 nil: {}", path.display());
            return;
        }
        // [[NSApplication sharedApplication] setApplicationIconImage:img]
        let app = msg0(objc_getClass(c"NSApplication".as_ptr()), shared_app);
        if !app.is_null() {
            msg1(app, set_app_icon, img);
        }
        msg0(img, release);
    }
}

/// 窗口全关后调用：降回 accessory（dock 图标消失，回到纯托盘态）。
#[cfg(target_os = "macos")]
pub fn hide_dock() {
    macos_activate(false);
}

/// Dock 图标点击（applicationShouldHandleReopen）回调：重新显示主窗口。
/// no-frame 自绘窗口后，系统标题栏窗口的「Dock 点击自动恢复」默认行为失效——
/// 需实现 NSApplicationDelegate 的 shouldHandleReopen 手动恢复（2026-08-18 回归修复）。
/// 手写 objc runtime FFI（零依赖原则，同 macos_activate）：动态类 ABBDockDelegate
/// 挂到 NSApplication，reopen 时调用注册的回调（Rust 侧显示设置窗）。
#[cfg(target_os = "macos")]
pub fn install_dock_reopen(on_reopen: Box<dyn Fn()>) {
    #![allow(non_snake_case)]
    use std::ffi::c_void;
    use std::sync::OnceLock;

    type Id = *mut c_void;
    type Sel = *mut c_void;

    unsafe extern "C" {
        fn objc_getClass(name: *const std::ffi::c_char) -> Id;
        fn objc_msgSend();
        fn sel_registerName(name: *const std::ffi::c_char) -> Sel;
        fn object_getClass(obj: Id) -> Id;
        fn class_addMethod(
            cls: Id,
            sel: Sel,
            imp: *const c_void,
            types: *const std::ffi::c_char,
        ) -> bool;
        fn imp_implementationWithBlock(block: *const c_void) -> *const c_void;
    }

    /// ObjC block 字面量（imp_implementationWithBlock 需要）。
    #[repr(C)]
    struct BlockLiteral {
        isa: *const c_void,
        flags: i32,
        reserved: i32,
        invoke: *const c_void,
    }
    extern "C" fn reopen_invoke(
        _block: *const BlockLiteral,
        _self: Id,
        _cmd: Sel,
        _app: Id,
        _has_visible: bool,
    ) -> bool {
        if let Some(cb) = REOPEN_CB.get() {
            cb.lock().unwrap().0();
        }
        true // 已处理（恢复窗口由回调做）
    }
    /// 全局回调（reopen 静态函数无法捕获闭包，走全局存取）。
    /// AppKit delegate 回调恒在主线程执行——跨线程 Send 标记是安全的（unsafe impl
    /// 只在主线程调用；Mutex 仅为 OnceLock 的 Sync 要求）。
    struct MainThreadCb(Box<dyn Fn()>);
    unsafe impl Send for MainThreadCb {}
    static REOPEN_CB: OnceLock<std::sync::Mutex<MainThreadCb>> = OnceLock::new();
    let _ = REOPEN_CB.set(std::sync::Mutex::new(MainThreadCb(on_reopen))); // 首次设置；重复调用幂等忽略

    unsafe {
        // 拿 NSApp 当前 delegate（winit 注册的）——**不能 setDelegate 替换**（winit
        // app_state.rs:182 一致性检查会 panic），改为给 winit delegate 的类动态添加
        // applicationShouldHandleReopen:hasVisibleWindows:（类方法表添加，实例自动响应，
        // delegate 指针不变）。
        let app: Id = {
            let shared = sel_registerName(c"sharedApplication".as_ptr());
            let shared_fn: unsafe extern "C" fn(Id, Sel) -> Id =
                std::mem::transmute(objc_msgSend as unsafe extern "C" fn());
            shared_fn(objc_getClass(c"NSApplication".as_ptr()), shared)
        };
        if app.is_null() {
            return;
        }
        let delegate_sel = sel_registerName(c"delegate".as_ptr());
        let get_delegate_fn: unsafe extern "C" fn(Id, Sel) -> Id =
            std::mem::transmute(objc_msgSend as unsafe extern "C" fn());
        let delegate = get_delegate_fn(app, delegate_sel);
        if delegate.is_null() {
            return;
        }
        let cls = object_getClass(delegate);
        if cls.is_null() {
            return;
        }
        // 全局 block（BLOCK_IS_GLOBAL：runtime 不 copy，无 copy/dispose 需求）+ 泄漏保活
        let block = Box::leak(Box::new(BlockLiteral {
            isa: &_NSConcreteGlobalBlock as *const c_void,
            flags: 1 << 28, // BLOCK_IS_GLOBAL
            reserved: 0,
            invoke: reopen_invoke as *const c_void,
        }));
        extern "C" {
            static _NSConcreteGlobalBlock: c_void;
        }
        let imp = imp_implementationWithBlock(block as *mut BlockLiteral as *const c_void);
        class_addMethod(
            cls,
            sel_registerName(c"applicationShouldHandleReopen:hasVisibleWindows:".as_ptr()),
            imp,
            c"c@:@@".as_ptr(),
        );
        std::mem::forget(Box::from_raw(block)); // block 泄漏保活（IMP 引用它）
    }
}

#[cfg(not(target_os = "macos"))]
#[allow(dead_code)] // stub：仅占位对齐 API，非 macOS 无调用方
pub fn install_dock_reopen(_on_reopen: Box<dyn Fn()>) {}

#[cfg(target_os = "macos")]
fn macos_activate(front: bool) {
    #![allow(non_snake_case)]
    use std::ffi::c_void;
    use std::sync::OnceLock;

    type Id = *mut c_void;
    type Sel = *mut c_void;
    unsafe extern "C" {
        fn objc_getClass(name: *const std::ffi::c_char) -> Id;
        fn sel_registerName(name: *const std::ffi::c_char) -> Sel;
        fn objc_msgSend();
    }
    // 各 selector 只解析一次。usize 存储（原始指针非 Send/Sync，不能放 OnceLock）；
    // ObjC selector 是进程内全局注册的常量地址，缓存安全。
    static SELS: OnceLock<(usize, usize, usize)> = OnceLock::new();
    let (shared_app, set_policy, activate) = *SELS.get_or_init(|| unsafe {
        (
            sel_registerName(c"sharedApplication".as_ptr()) as usize,
            sel_registerName(c"setActivationPolicy:".as_ptr()) as usize,
            sel_registerName(c"activateIgnoringOtherApps:".as_ptr()) as usize,
        )
    });
    let (shared_app, set_policy, activate) =
        (shared_app as Sel, set_policy as Sel, activate as Sel);

    unsafe {
        let shared_app_fn: unsafe extern "C" fn(Id, Sel) -> Id =
            std::mem::transmute(objc_msgSend as unsafe extern "C" fn());
        let app = shared_app_fn(objc_getClass(c"NSApplication".as_ptr()), shared_app);
        if app.is_null() {
            return;
        }
        // NSApplicationActivationPolicy: Regular=0, Accessory=1
        let policy: isize = if front { 0 } else { 1 };
        let set_policy_fn: unsafe extern "C" fn(Id, Sel, isize) =
            std::mem::transmute(objc_msgSend as unsafe extern "C" fn());
        set_policy_fn(app, set_policy, policy);
        if front {
            // 提到前台并获得焦点（让刚 show 的窗口成为 key window）
            let activate_fn: unsafe extern "C" fn(Id, Sel, bool) =
                std::mem::transmute(objc_msgSend as unsafe extern "C" fn());
            activate_fn(app, activate, true);
        }
    }
}

/// 非 macOS 无 dock 概念，留空调用即可。
#[cfg(not(target_os = "macos"))]
pub fn set_dock_visible(_visible: bool) {}
/// 非 macOS 无 dock 概念，留空调用即可。
#[cfg(not(target_os = "macos"))]
pub fn hide_dock() {}

// ─────────────────────────── macOS：登录项自启 ───────────────────────────

/// LaunchAgent 标签（同时是 plist 文件名）。launchd 按标签记账，改这个字符串会让
/// 存量用户的旧 agent 变孤儿——只能新增、不能更名。
#[cfg(target_os = "macos")]
const LOGIN_ITEM_LABEL: &str = "com.sqb.agent-bridge.gui";

/// bridge 常驻 job 的标签（**新增**：与托盘 job 分开记账）。
///
/// 为什么要拆两个 job：本批要求「登录后 bridge 独立于托盘存活、托盘退出不受影响」。
/// 旧形态是托盘拉起 `--service` 子进程，托盘一退（`on_quit_app`）子进程就被杀；改成
/// launchd 直接托管 `--service` 之后，托盘只是客户端。launchd 按标签记账，**只能新增
/// 不能更名**（更名会让存量用户的旧 job 变孤儿）。
#[cfg(target_os = "macos")]
const SERVICE_ITEM_LABEL: &str = "com.sqb.agent-bridge.service";

/// 自启 plist 与当前二进制的关系（判定见 [`login_item_state_at`]）。
#[cfg(target_os = "macos")]
#[derive(Debug, PartialEq, Eq)]
enum LoginItem {
    /// 没有 plist（用户从未开自启），或内容不是我们的 schema（读不出参数）——不猜。
    Absent,
    /// plist 登记的就是当前这个可执行文件。
    Matches,
    /// plist 在，但登记的二进制已不存在（App 被移动/删过），或指向另一份副本。
    /// 前者 launchd 首次 exec 即判 `EX_CONFIG(78)` 并静默停手（实测不重试、二进制
    /// 补回也不拉），托盘却仍显示「开」；后者会在登录时拉起旧版本。
    Drifted,
}

/// plist 字符串值的 XML 转义（写侧）。不转义时含 `&` 的路径会产出非法 plist，
/// launchd 直接拒载——而本 PR 的立身之本就是「回显说真话」，故写转义、读还原。
#[cfg(target_os = "macos")]
fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[cfg(target_os = "macos")]
fn xml_unescape(s: &str) -> String {
    // 还原顺序与写侧相反：最后才还原 `&amp;`，否则 `&amp;lt;` 会被二次解码成 `<`。
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
}

/// 自启 plist 的唯一模板（纯函数，便于单测锁 schema）。这些键不是装饰：
/// `KeepAlive`/`ThrottleInterval` 承载本机实测结论，被人顺手删掉就等于把
/// 「崩溃不复活」的老毛病改回来，所以有测试断言它们必须存在。
///
/// `StandardOutPath`/`StandardErrorPath` 是 GUI 日志的唯一出路：`crate::log!` 只写
/// stdout（main.rs:147），而 GUI 由 `open`/LaunchServices 起来时 stdio 全指
/// /dev/null（本机实测 lsof），不重定向就等于什么都不留——`heal_autostart` 这类
/// 只在 GUI 里跑的诊断信息本来会彻底不可见。现存的 logs/gui.out（8/10 那份手写
/// plist 留下的）就是这个键的产物，自愈重写 plist 时不能把它弄丢。
#[cfg(target_os = "macos")]
fn build_login_plist(exe: &std::path::Path, logs: &std::path::Path) -> String {
    build_plist(LOGIN_ITEM_LABEL, exe, logs, &[], "gui")
}

/// bridge 常驻 job 的 plist：`ProgramArguments = [exe, --service]`。
///
/// 与托盘 job 的差别只有参数（以及日志文件名）：`KeepAlive` 两边都带，这正是本批
/// 「登录后 bridge 不能被随便杀死」在 macOS 侧的答案——被 `kill -9` 也会被 launchd 拉回
/// （`ThrottleInterval` 10s 内不重拉，防抖）。
#[cfg(target_os = "macos")]
fn build_service_plist(exe: &std::path::Path, logs: &std::path::Path) -> String {
    build_plist(SERVICE_ITEM_LABEL, exe, logs, &["--service"], "service")
}

/// LaunchAgent plist 模板（托盘 job 与 bridge job 共用）。
///
/// `args` = `ProgramArguments` 里 exe 之后的参数；`log_stem` 决定 stdout/stderr 落到
/// `logs/<stem>.out|.err`（托盘 gui.*、bridge service.*，便于分开排查）。
#[cfg(target_os = "macos")]
fn build_plist(
    label: &str,
    exe: &std::path::Path,
    logs: &std::path::Path,
    args: &[&str],
    log_stem: &str,
) -> String {
    let extra: String = args
        .iter()
        .map(|a| format!("    <string>{}</string>\n", xml_escape(a)))
        .collect();
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>{label}</string>
  <key>ProgramArguments</key>
  <array>
    <string>{exe}</string>
{extra}  </array>
  <key>RunAtLoad</key>
  <true/>
  <key>KeepAlive</key>
  <dict>
    <key>SuccessfulExit</key>
    <false/>
  </dict>
  <key>ThrottleInterval</key>
  <integer>10</integer>
  <key>StandardOutPath</key>
  <string>{out}</string>
  <key>StandardErrorPath</key>
  <string>{err}</string>
</dict>
</plist>
"#,
        label = xml_escape(label),
        exe = xml_escape(&exe.display().to_string()),
        out = xml_escape(&logs.join(format!("{log_stem}.out")).display().to_string()),
        err = xml_escape(&logs.join(format!("{log_stem}.err")).display().to_string()),
    )
}

/// 取 `ProgramArguments` 里的**全部** `<string>`（已 XML 还原）。
///
/// 与 [`plist_program_argument`] 的区别：它只取第一个（判「登记的是不是当前二进制」），
/// 本函数用于判 bridge job 的**参数**是否为 `--service`——只认自己的 schema，读不出就
/// 返回空向量（调用方据此判 Drifted/Absent，绝不猜）。
#[cfg(target_os = "macos")]
fn plist_program_arguments(text: &str) -> Vec<String> {
    let Some(rest) = text.split("<key>ProgramArguments</key>").nth(1) else {
        return Vec::new();
    };
    let Some(arr) = rest
        .split("<array>")
        .nth(1)
        .and_then(|a| a.split("</array>").next())
    else {
        return Vec::new();
    };
    arr.split("<string>")
        .skip(1)
        .filter_map(|s| s.split("</string>").next())
        .map(|s| {
            s.replace("&lt;", "<")
                .replace("&gt;", ">")
                .replace("&amp;", "&")
        })
        .collect()
}

/// 取 plist 里 `ProgramArguments` 数组的第一个 `<string>`（已 XML 还原）。自己写的
/// schema 自己解析（不为此引一个 plist 依赖）；读不到就判 [`LoginItem::Absent`]，绝不猜。
#[cfg(target_os = "macos")]
fn plist_program_argument(text: &str) -> Option<String> {
    text.split("<key>ProgramArguments</key>")
        .nth(1)?
        .split("<array>")
        .nth(1)?
        .split("<string>")
        .nth(1)
        .and_then(|s| s.split("</string>").next())
        .map(str::trim)
        .map(xml_unescape)
}

/// 判定自启项状态。`plist`/`current` 参数化是为单测（不碰真实 ~/Library）。
/// 两侧都 canonicalize：`/var` → `/private/var`、软链、大小写不敏感卷都可能让
/// 字符串不等而路径实际相等。
#[cfg(target_os = "macos")]
fn login_item_state_at(plist: &std::path::Path, current: &std::path::Path) -> LoginItem {
    let Ok(text) = std::fs::read_to_string(plist) else {
        return LoginItem::Absent;
    };
    let Some(arg) = plist_program_argument(&text) else {
        return LoginItem::Absent;
    };
    let registered = std::path::Path::new(&arg);
    if !registered.exists() {
        return LoginItem::Drifted;
    }
    match (
        std::fs::canonicalize(registered),
        std::fs::canonicalize(current),
    ) {
        (Ok(a), Ok(b)) if a == b => LoginItem::Matches,
        _ => LoginItem::Drifted,
    }
}

/// 自启的**合并状态**：托盘 job 与 bridge job 必须都在且都指向当前二进制。
///
/// 刻意把「一侧在、另一侧缺」判成 [`LoginItem::Drifted`]（而不是 Absent）：存量用户只有
/// 托盘 job，本批要给他补 bridge job —— 归到 Drifted 正好落进 `heal_autostart` 的自愈路径
/// （「用户已经开过自启 → 按当前二进制重建」），开关也继续显示「开」，不会回显说谎。
#[cfg(target_os = "macos")]
fn autostart_state() -> LoginItem {
    let Ok(exe) = current_exe() else {
        return LoginItem::Absent;
    };
    autostart_state_at(&login_item_plist(), &service_item_plist(), &exe)
}

/// [`autostart_state`] 的路径可注入版（单测不碰真实 `~/Library/LaunchAgents`）。
#[cfg(target_os = "macos")]
fn autostart_state_at(
    tray_plist: &std::path::Path,
    svc_plist: &std::path::Path,
    exe: &std::path::Path,
) -> LoginItem {
    let svc = |p: &std::path::Path| -> LoginItem {
        match login_item_state_at(p, exe) {
            LoginItem::Matches => {
                let text = std::fs::read_to_string(p).unwrap_or_default();
                if plist_program_arguments(&text)
                    .iter()
                    .any(|a| a == "--service")
                {
                    LoginItem::Matches
                } else {
                    LoginItem::Drifted
                }
            }
            other => other,
        }
    };
    match (login_item_state_at(tray_plist, exe), svc(svc_plist)) {
        (LoginItem::Matches, LoginItem::Matches) => LoginItem::Matches,
        (LoginItem::Absent, LoginItem::Absent) => LoginItem::Absent,
        _ => LoginItem::Drifted,
    }
}

/// 是否已设为「登录时自动启动」。
/// 老实现只判 plist 文件存在，于是 App 换过位置后（`scripts/build.sh` 装
/// `~/Applications`、正式包拖进 `/Applications`，而 plist 里写的是写入当时的
/// `current_exe()`）launchd 首次 exec 就静默失败、再不自理，托盘却仍显示「开」——
/// 回显说谎。
#[cfg(target_os = "macos")]
pub fn autostart_enabled() -> bool {
    autostart_state() == LoginItem::Matches
}

/// GUI 启动时自愈：plist 在但指向失效路径/旧副本 → 按当前二进制重建并 reload。
/// 只动「用户已经开过自启」的配置——没有 plist 就是没开过，绝不擅自给人开。
#[cfg(target_os = "macos")]
pub fn heal_autostart() {
    if autostart_state() != LoginItem::Drifted {
        return;
    }
    crate::log!("[autostart] 自启项漂移/缺 bridge job，按当前二进制重建两个 job");
    // 只记「检测到漂移并发起重建」这一条事实；成没成由 `set_autostart` 单点自己记
    // （两处各写一遍早晚分叉，且这里写「已重载」在本会话跳过 bootout 时并不成立）。
    log_autostart_event(
        "自愈：自启项漂移或缺 bridge job（App 移动过/旧版只有托盘 job），发起按当前二进制重建",
    );
    let _ = set_autostart(true);
}

/// 非 macOS、非 Windows（Linux）自启未实现，无自愈可言。
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
pub fn heal_autostart() {}

/// Linux：尚未实现「平台级托管」（没有 launchd/计划任务的对应物），如实 stub。
///
/// 与 [`heal_autostart`] 的 Linux stub 同一惯例：**宁可显式说未实现**，也不让调用方
/// 在非 mac/win 上编译不过或静默走错分支（评审 P7）。
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
pub fn service_supervised() -> bool {
    false
}

/// Linux：无托管则无托管重启。
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
pub fn restart_service_supervised() -> Result<()> {
    anyhow::bail!("Linux 暂未实现平台级托管（bridge 由托盘看门狗代管）")
}

/// Linux：无托管则无「托管启动」。
///
/// 评审 R23 §3.1：`install::svc_start` 无条件调本函数，缺这个 stub 会让非 mac/win 目标编译红。
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
pub fn start_service_supervised() -> Result<()> {
    anyhow::bail!("Linux 暂未实现平台级托管（bridge 由托盘看门狗代管）")
}

/// Linux：授权停止未实现——**不静默降级成无授权停止**。
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
pub fn stop_service_authorized() -> Result<()> {
    anyhow::bail!("Linux 暂未实现「授权停止服务」（bridge 由托盘生命周期管理）")
}

/// 自愈判据（纯函数，便于单测）：**只在**「用户意图为开」且「注册表实际缺失」时为真。
/// 两个方向都不能判错：意图开+实际也在 = 无需动作；意图关 = 用户自己关的，绝不能替他
/// 打开。任一方向判反，都会把「用户主动关掉自启」变成「每次启动被偷偷打开」。
#[cfg(target_os = "windows")]
fn autostart_heal_needed(desired: bool, registered: bool) -> bool {
    desired && !registered
}

/// Windows GUI 启动时自愈：用户开过自启（有意图标记）但 Run 键不见了 → 按意图补写。
/// 只动「用户开过」的配置：没有标记就是没开过，绝不擅自给人开（与 macOS 侧同一条纪律）。
///
/// 为什么 Run 键会「自己不见」：`HKCU\...\Run` 是每用户可写的启动项位置，安全软件 /
/// 系统清理工具会把它当可疑启动项**静默**删除（本机实测：火绒 HIPS 在跑时 `reg add`
/// 当场成功、重启后该值消失），而 GUI 每次刷新读的是 Run 键真值 → 托盘显示「关」、
/// 用户以为设置没生效。注册表本身无法区分「被删」与「用户关掉」，故用意图标记区分。
///
/// 已知边界：只判 Run 值**在不在**，不解析它指向哪个二进制（见上面 Windows 段注释）；
/// 升级器原地替换二进制、安装位置由 Inno 固定，故不存在 macOS 那种「指向旧副本」的漂移。
#[cfg(target_os = "windows")]
pub fn heal_autostart() {
    let logs = crate::bridge_dir().join("logs");
    let desired = autostart_desired_at(&logs);
    let registered = autostart_enabled();
    // 服务常驻（计划任务）状态如实入审计：注册/删除任务都需要管理员授权，**自愈不弹授权框**
    // （否则每次启动都弹 UAC）。所以这里只记录「用户想开自启、但常驻任务还没生效」，让他从
    // autostart.log 看得出下一步是「开一次开关（会走一次 UAC）」。
    if desired && service_persist_state() != "matches" {
        log_autostart_event(&format!(
            "服务常驻未生效（计划任务 {}）：注册需管理员授权，开一次「开机自启」开关即可补齐",
            service_persist_state()
        ));
    }
    // 存量用户（升级前就开了自启、没留下意图标记）：只要键还在就补记意图，让以后被
    // 回滚时能自愈。此处只在「键在」时补记，不写注册表，无副作用。
    if !desired && registered {
        let _ = set_autostart_desired_at(&logs, true);
        log_autostart_event("自愈：Run 键已开但无意图记录（升级前存量），补记用户意图");
        return;
    }
    if !autostart_heal_needed(desired, registered) {
        return;
    }
    crate::log!("[autostart] Run 键缺失（疑被安全软件回滚），按用户意图补写");
    log_autostart_event(
        "自愈：用户意图为开但 HKCU Run 键缺失（疑被安全软件/清理工具回滚），发起补写",
    );
    let _ = set_autostart(true);
}

/// 设置开机自启的**唯一公开入口**：成败一律在这里落审计。
///
/// 记录点做在单点而不是各调用方（同 #263 把防自杀守卫下沉到 `unload_login_agent` 的
/// 理由）：将来新增第三个调用点（CLI 之类）不会静默漏记——「漏记」本身没有任何症状。
pub fn set_autostart(enable: bool) -> Result<()> {
    let want = if enable { "开" } else { "关" };
    let r = set_autostart_impl(enable);
    match &r {
        Ok(()) => log_autostart_event(&format!(
            "开机自启已设为{want}（本会话是否已重载 launchd，见相邻记录）",
        )),
        Err(e) => log_autostart_event(&format!("开机自启设为{want} 失败: {e:#}")),
    }
    r
}

/// 开启/关闭「登录时自动启动」。
/// 实现：往 ~/Library/LaunchAgents 写一个 LaunchAgent plist（launchd 登录时拉起
/// GUI；不是 LaunchServices 登录项 API）。注意：这是「开机自启 GUI」，不是用
/// launchd 管 service 生命周期——service 由 GUI 看门（见 install.rs）。
/// 写完必须 reload：只 `fs::write` 时 launchd 用的仍是内存里那份旧定义，改动
/// 到下次登录都不生效（历史上正是这个让「重开自启」也修不好）。
///
/// plist 里 `KeepAlive = {SuccessfulExit = false}` + `ThrottleInterval = 10` 是
/// 本机实测选的形态（macOS 26.5.2，gui/501 域，36s 观察窗，探针标签跑完即拆）：
/// - 被信号杀 / 非 0 退出 → 重启（约 10s 一次，节流由 ThrottleInterval 管）；
/// - 退出码 0（托盘「退出」走 `quit_event_loop`）→ **不**重启，用户意图得到尊重；
/// - 不选 `Crashed = true` 的原因：release 配置未设 `panic = "abort"`，Rust panic 是
///   **exit 101 且无信号**，`Crashed` 只认信号死，恰好救不活我们要救的那一类。
/// - 可执行文件缺失时 launchd 判 `EX_CONFIG(78)` 后**静默停手**（实测 250s 内只试 1 次，
///   把二进制装回原路径也不补拉）：既不会刷屏重试，也不能指望 launchd 自己恢复——
///   后者正是 [`heal_autostart`] 存在的理由。
#[cfg(target_os = "macos")]
fn set_autostart_impl(enable: bool) -> Result<()> {
    let plist = login_item_plist();
    let svc_plist = service_item_plist();
    if !enable {
        // 顺序要紧：先删 plist（「下次登录不再自启」的硬判据，必须落定），再尽力摘掉
        // 本会话的 job。反过来先 bootout 会在「launchd 拉起的实例里点关」时当场把自己
        // 杀掉，删文件永远轮不到——用户看到 App 凭空退出而自启照旧（审查必修项）。
        for p in [&plist, &svc_plist] {
            if p.exists() {
                std::fs::remove_file(p)
                    .with_context(|| format!("删除登录项失败: {}", p.display()))?;
            }
        }
        // 再尽力摘本会话的 job；跑的是我们自己时 unload 内部会跳过（见其文档）。
        // 已知窄窗：跳过意味着 launchd 内存里那份定义还在——若它带 KeepAlive（本会话
        // 是被新模板拉起过的），此后 App 非 0 退出/被信号杀仍会被复活**一次**；正常
        // 退出码 0 不复活，且 plist 已删 → 下次登录起彻底干净。宁可留这个窄窗，也不
        // 为改配置杀掉用户正在用的 App。
        unload_login_agent();
        // bridge job 不是本进程（本进程是托盘）→ 直接 bootout，无自杀风险。
        unload_agent_at(SERVICE_ITEM_LABEL, false);
        return Ok(());
    }
    let exe = current_exe()?;
    if let Some(parent) = plist.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // logs/ 先建好：launchd 若打不开 StandardOutPath 目标可能不起 job（man 页只说
    // 「文件不存在则创建」，对父目录缺失的行为沉默——这里按最保守做法先建目录）。
    let logs = crate::bridge_dir().join("logs");
    let _ = std::fs::create_dir_all(&logs);
    std::fs::write(&plist, build_login_plist(&exe, &logs))
        .with_context(|| format!("写登录项失败: {}", plist.display()))?;
    // 本批新增：bridge 常驻 job（`--service`，KeepAlive）——托盘退出不再影响 bridge。
    std::fs::write(&svc_plist, build_service_plist(&exe, &logs))
        .with_context(|| format!("写 bridge 登录项失败: {}", svc_plist.display()))?;
    // reload 内部同样带防自杀保护：是我们自己就不重装载，新增的保活键下次登录生效。
    reload_login_agent(&plist)?;
    reload_agent_at(SERVICE_ITEM_LABEL, &svc_plist, false)
}

/// 由 launchd 拉起的自启 job 是否就是本进程（**自杀保护的唯一判据**）。
#[cfg(target_os = "macos")]
fn login_job_is_self() -> bool {
    let target = format!("gui/{}/{}", uid(), LOGIN_ITEM_LABEL);
    let Ok(out) = std::process::Command::new("launchctl")
        .args(["print", &target])
        .output()
    else {
        return false;
    };
    if !out.status.success() {
        return false;
    }
    parse_launchd_job_pid(&String::from_utf8_lossy(&out.stdout)) == Some(std::process::id())
}

/// 从 `launchctl print` 输出里取 job 主进程 pid。本机实测形态是一个 TAB 缩进的
/// `pid = 61995`，且全输出含 `pid` 子串的行**仅此一条**（`last exit code`、
/// `exit timeout` 都不以 `pid` 开头；真出现 `responsible pid` 也会因 trim 后前缀
/// 不符而被跳过）。容忍 `pid=1` 这类空白差异。
/// 解析不出（job 未运行/格式变了）→ `None` → 判「不是自己」→ 落回原行为：**宁可
/// 保护失效，也不要把用户正在用的 App 误判成该跳过**。格式漂移是显式风险，故留了
/// 单测锁住这行解析。
#[cfg(target_os = "macos")]
fn parse_launchd_job_pid(print_out: &str) -> Option<u32> {
    print_out.lines().find_map(|l| {
        let rest = l.trim().strip_prefix("pid")?.trim_start();
        rest.strip_prefix('=')?.trim().parse::<u32>().ok()
    })
}

/// 让 launchd 改用磁盘上最新定义（bootout 旧 job → bootstrap 新 plist）。
/// 域是 per-user 的 `gui/<uid>`；uid 取本进程，不用 `$UID` 环境变量（由 launchd
/// 起的进程未必带它）。
#[cfg(target_os = "macos")]
fn reload_login_agent(plist: &std::path::Path) -> Result<()> {
    reload_agent_at(LOGIN_ITEM_LABEL, plist, login_job_is_self())
}

/// 按标签重载一个 LaunchAgent（托盘 job 与 bridge job 共用）。
///
/// `is_self` = 「正在跑的那个 job 的进程就是本进程」——为真时跳过 bootout（摘它会杀掉
/// 自己）。托盘 job 用 [`login_job_is_self`] 判；bridge job 由托盘调用时恒为 false。
#[cfg(target_os = "macos")]
fn reload_agent_at(label: &str, plist: &std::path::Path, is_self: bool) -> Result<()> {
    if !unload_agent_at(label, is_self) {
        // 没摘成（正在跑的就是我们自己）→ 也别 bootstrap：同一 label 重复 bootstrap
        // 会失败。新写的文件已在磁盘上，下次登录 launchd 重读即生效。
        return Ok(());
    }
    let domain = format!("gui/{}", uid());
    let out = std::process::Command::new("launchctl")
        .args(["bootstrap", &domain, &plist.display().to_string()])
        .output()
        .with_context(|| "执行 launchctl bootstrap 失败")?;
    if out.status.success() {
        Ok(())
    } else {
        anyhow::bail!(
            "launchctl bootstrap 失败: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
}

/// 从 launchd 摘掉本 App 的 agent。返回 `false` = **因为那个 job 进程就是本进程而
/// 跳过**（`bootout` 会终止 job 进程，实测 `kill -0` 拿 rc=1、pgrep/ps 全空；摘自己
/// 等于让 ABB 凭空退出），调用方据此别再去 bootstrap。未加载过时报错属正常路径
/// （首次开启前），按「已摘」处理。
///
/// 保护做在这里而不是各调用点：以后谁再加一条 reload/unload 路径，不会绕开它。
#[cfg(target_os = "macos")]
fn unload_login_agent() -> bool {
    unload_agent_at(LOGIN_ITEM_LABEL, login_job_is_self())
}

/// 按标签 bootout（`is_self` 语义见 [`reload_agent_at`]）。
#[cfg(target_os = "macos")]
fn unload_agent_at(label: &str, is_self: bool) -> bool {
    if is_self {
        crate::log!("[autostart] 跳过 bootout：该 job 正在运行的进程就是本进程（摘它会杀掉自己）");
        log_autostart_event(
            "跳过 launchctl bootout：正在跑的 job 进程就是本进程（摘它会杀掉自己）",
        );
        return false;
    }
    let target = format!("gui/{}/{}", uid(), label);
    let _ = std::process::Command::new("launchctl")
        .args(["bootout", &target])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    true
}

/// 本进程 uid（libc 已在直接依赖里，不必 spawn `id -u`）。
#[cfg(target_os = "macos")]
fn uid() -> u32 {
    // SAFETY: getuid(2) 无参数、无内存交互，任意线程可调。
    unsafe { libc::getuid() }
}

#[cfg(target_os = "macos")]
fn login_item_plist() -> PathBuf {
    // 文件名 == 标签名（launchd 按标签记账），别把字符串写两遍以免分叉。
    dirs::home_dir()
        .unwrap_or_default()
        .join("Library/LaunchAgents")
        .join(format!("{LOGIN_ITEM_LABEL}.plist"))
}

/// bridge job 的 plist 路径（文件名 == 标签名，同 [`login_item_plist`] 的约定）。
#[cfg(target_os = "macos")]
fn service_item_plist() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_default()
        .join("Library/LaunchAgents")
        .join(format!("{SERVICE_ITEM_LABEL}.plist"))
}

/// 停止由 launchd 托管的 bridge —— **用户级，不再要管理员授权**（2026-10-04 新模型，P3）。
///
/// 历史：2026-09-28 owner 要求「停止必须授权」，于是这里走 `osascript … with administrator
/// privileges`（弹密码/Touch ID）。2026-10-04 owner 统一为「全部以普通用户运行，启动/停止
/// 统一由 ABB 管理密码把关」⇒ 提权那层退出：这里直接用当前用户的 `launchctl bootout` 摘掉
/// 自己的 LaunchAgent（技术上本来就允许）；要不要停由 UI 层的管理密码门决定（`admin_pass`）。
///
/// **诚实标注**（保留原注释的这份诚实）：这不是「更安全」的写法 —— 同用户进程仍能停掉该
/// agent。「不能被随便停」现在由管理密码门（防误操作/防随手停）与用户的登录会话边界共同
/// 表达，不再由 OS 授权表达。别把这条读成「停止变安全了」。
#[cfg(target_os = "macos")]
pub fn stop_service_authorized() -> Result<()> {
    let target = format!("gui/{}/{}", uid(), SERVICE_ITEM_LABEL);
    let out = std::process::Command::new("launchctl")
        .args(["bootout", &target])
        .output()
        .context("执行 launchctl bootout 失败")?;
    // 评审 P8 的教训必须保留：**不看退出码**，回读 job 是否还在域里 —— 否则会出现
    // 「用户被告知已停止、服务还在跑」。job 本来就没加载时 bootout 会非 0，这算成功。
    if !out.status.success() && job_loaded(&target) {
        anyhow::bail!(
            "launchctl bootout 失败，服务未停止：{}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    if job_loaded(&target) {
        anyhow::bail!("launchctl bootout 未生效（job 仍在域里），服务未停止");
    }
    log_autostart_event(&format!("已 bootout {target}（bridge 已停止，用户级）"));
    Ok(())
}

/// bridge 是否已交给平台级 supervisor 托管（macOS=launchd job 存在；Windows=计划任务，B1b）。
///
/// 托管状态下**托盘不再自己起/杀 bridge**：起/杀都交给 supervisor，否则两边会抢
/// `logs/service.pid` 与单实例锁（表现为反复拉起-退出）。
#[cfg(target_os = "macos")]
pub fn service_supervised() -> bool {
    service_item_plist().exists()
}

/// 让 supervisor 重启 bridge（不经过托盘自己的 stop+start 路径）。
///
/// `kickstart -k` = 杀掉当前实例并立刻重起；这是「重启」这个动作在托管形态下的正确实现
/// （用户没有要求给重启加授权，故不加）。
///
/// 评审 P5 的两点修正：① job 已不在域里（例如刚被授权停止过）时 `kickstart -k` 会失败 ⇒
/// 退回 [`start_service_supervised`]；② 「启动」与「重启」是两件事，看门狗只该调前者。
#[cfg(target_os = "macos")]
pub fn restart_service_supervised() -> Result<()> {
    let target = format!("gui/{}/{}", uid(), SERVICE_ITEM_LABEL);
    if !job_loaded(&target) {
        return start_service_supervised();
    }
    let out = std::process::Command::new("launchctl")
        .args(["kickstart", "-k", &target])
        .output()
        .context("执行 launchctl kickstart 失败")?;
    if !out.status.success() {
        anyhow::bail!(
            "launchctl kickstart 失败: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

/// job 在 launchd 里的形态。**「在域里」与「在跑」是两件事**，混为一谈会造出
/// 「谁都以为它在跑」的静止态（见 [`supervised_start_action`]）。
#[cfg(target_os = "macos")]
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum JobState {
    /// `launchctl print` 失败：不在域里（没 bootstrap 过，或被 `bootout` 摘掉了）。
    Absent,
    /// 在域里但**没有运行中的进程**（`print` 输出里没有 `pid` 行）。
    Stopped,
    /// 在域里且有主进程 pid。
    Running,
}

/// 托管形态下的「启动」该做什么（纯函数，便于单测锁死判据）。
#[cfg(target_os = "macos")]
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum SupervisedStart {
    /// 已经在跑：什么都不做（看门狗每 2s 走一次，绝不能顺手 `kickstart -k`）。
    Noop,
    /// 在域里但没在跑：`kickstart`（**不带 `-k`**）把它拉回来。
    Kickstart,
    /// 不在域里：按 plist `bootstrap`。
    Bootstrap,
}

/// 「启动」的判据：**已加载 ≠ 在跑**。
///
/// 为什么必须有这条分支（2026-09-30 owner 实报「最新的版本 mac 上也启动不起来了」）：
/// bridge job 的 plist 是 `KeepAlive = {SuccessfulExit: false}` —— **优雅退出（exit 0）
/// 的 job launchd 不会重拉**。升级流程恰恰是「先优雅停 service（SIGTERM ⇒ 正常关停、
/// exit 0）→ 换包 → 拉起新托盘」，于是 job 停在「在域里、not running、last exit = 0」；
/// 托盘的看门狗虽然每 2s 判活失败，但旧实现只判 `job_loaded`（`launchctl print` 成功）
/// 就直接 `Ok(())`，既不 kickstart 也不 bootstrap ⇒ **永久静止态**，机器人全离线而界面上
/// 只看到「自动重拉」的日志（GUI 由 `open` 拉起时 stdout 还是 /dev/null）。
#[cfg(target_os = "macos")]
fn supervised_start_action(state: JobState) -> SupervisedStart {
    match state {
        JobState::Running => SupervisedStart::Noop,
        JobState::Stopped => SupervisedStart::Kickstart,
        JobState::Absent => SupervisedStart::Bootstrap,
    }
}

/// `launchctl` 调用的上界。
///
/// 本机实测（2026-09-30，评审 R1 的 B1 独立复现）：正常路径都是**毫秒级**（`print` 0.01s；
/// `kickstart` 对「在跑」/「优雅停下」两种 job 都是瞬时返回），但 job 处于 `EX_CONFIG` 形态
/// （程序不存在：`state = spawn scheduled`、`runs = 1`、`last exit code = 78: EX_CONFIG`、
/// **无 pid 行**）时 `launchctl kickstart` **>20s 不返回**（同批 `print`/`bootout` 仍瞬时）。
///
/// 已知不精确处（评审 R2 实测，如实记下）：plist 带 `ThrottleInterval=10` 时，**会成功**的
/// `kickstart` 也可能要等到节流窗口过去才返回（实测 9.006s；客户端被我们 kill 后，launchd 仍会
/// 在 +10s 完成那次 spawn）。所以 5s 在「被节流的成功」这一支上会先报一次 `TimedOut` —— 表现为
/// 一条 `[watchdog] 自动重拉失败` 日志 + 下一拍（2s）复验时 job 已在跑；**没有功能损失**
/// （看门狗判活读的是 pid 文件），只是日志噪声。改成 >10s 会把「真挂住」那条路的线程寿命拉长
/// （2s tick ⇒ 并发数 = 上界/tick），故取 5s 换更小的资源占用。
#[cfg(target_os = "macos")]
const LAUNCHCTL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// 有界跑一个子进程：到点就 `kill` 掉这个客户端并按 `TimedOut` 返回 —— **绝不把「没返回」当成功**。
///
/// 为什么不能直接用 `Command::output()`：它一直等到进程退出（上面那条实测就是 >20s），
/// 而 `launchctl` 的调用点包括托盘 2s 看门狗（UI 线程）——挂住就等于托盘冻死。
/// stdout/stderr 交给排水线程读，免得子进程写满管道缓冲后自己卡住。
///
/// 独立可测：传一个 `sleep` 进去就能验证「到点必回 Err」（见单测）。
#[cfg(target_os = "macos")]
fn run_bounded(
    mut cmd: std::process::Command,
    timeout: std::time::Duration,
) -> std::io::Result<std::process::Output> {
    fn drain<R: std::io::Read + Send + 'static>(
        pipe: Option<R>,
    ) -> Option<std::thread::JoinHandle<Vec<u8>>> {
        pipe.map(|mut p| {
            std::thread::spawn(move || {
                let mut buf = Vec::new();
                let _ = std::io::Read::read_to_end(&mut p, &mut buf);
                buf
            })
        })
    }
    let mut child = cmd.spawn()?;
    let out_h = drain(child.stdout.take());
    let err_h = drain(child.stderr.take());
    let deadline = std::time::Instant::now() + timeout;
    let status = loop {
        if let Some(st) = child.try_wait()? {
            break st;
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            // kill 之后管道 EOF，排水线程会自己收尾；join 只为不留悬空线程。
            if let Some(h) = out_h {
                let _ = h.join();
            }
            if let Some(h) = err_h {
                let _ = h.join();
            }
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                format!("子进程超过 {timeout:?} 未退出，已终止"),
            ));
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    };
    Ok(std::process::Output {
        status,
        stdout: out_h
            .map(|h| h.join().unwrap_or_default())
            .unwrap_or_default(),
        stderr: err_h
            .map(|h| h.join().unwrap_or_default())
            .unwrap_or_default(),
    })
}

/// `launchctl` 的**唯一**调用点：所有需要可注入的实现都从这里出去，单测才能用假 runner
/// 驱动真实逻辑而不 spawn 真的 launchctl。**走 [`run_bounded`]**（有界，见上）。
#[cfg(target_os = "macos")]
fn launchctl(args: &[&str]) -> std::io::Result<std::process::Output> {
    let mut cmd = std::process::Command::new("launchctl");
    cmd.args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    run_bounded(cmd, LAUNCHCTL_TIMEOUT)
}

/// launchctl 调用缝（单测注入假 runner 用；生产走 [`launchctl`]）。
#[cfg(target_os = "macos")]
type LaunchctlRunner<'a> = dyn FnMut(&[&str]) -> std::io::Result<std::process::Output> + 'a;

/// 读 job 形态：`launchctl print` 成功 = 在域里，输出里能解析出 pid 才算在跑。
///
/// pid 行复用 [`parse_launchd_job_pid`]（同一份格式假设，改格式时一处生效）。
#[cfg(target_os = "macos")]
fn job_state_with(run: &mut LaunchctlRunner<'_>, target: &str) -> JobState {
    match run(&["print", target]) {
        Ok(o) if o.status.success() => {
            if parse_launchd_job_pid(&String::from_utf8_lossy(&o.stdout)).is_some() {
                JobState::Running
            } else {
                JobState::Stopped
            }
        }
        _ => JobState::Absent,
    }
}

/// job 是否已在 launchd 域里（`launchctl print <target>` 成功 = 在）。
///
/// **不要**把它当「在跑」用：判「要不要拉起来」见 [`supervised_start_action`]。
#[cfg(target_os = "macos")]
fn job_loaded(target: &str) -> bool {
    job_state_with(&mut launchctl, target) != JobState::Absent
}

/// 托管形态下的「启动」：确保 bridge **真的在跑**。
///
/// 与 [`restart_service_supervised`] 的区别是**不杀**正在跑的实例——看门狗「意图=运行但判活失败」
/// 时走这条（评审 P5：以前走 kickstart -k，pid 文件滞后或冷启动 >2s 就形成抖动）；
/// 「已加载但没在跑」则按 [`supervised_start_action`] 补一次 `kickstart`（不带 `-k`：
/// 本机实测对正在跑的 job 是 rc=0、pid 不变，不会打断实例）。
///
/// 这里**不做多秒轮询**：本函数会被托盘看门狗的 2s tick（UI 线程）调用，阻塞它等于卡界面；
/// 「起没起来」由下一拍判活复验。
#[cfg(target_os = "macos")]
pub fn start_service_supervised() -> Result<()> {
    let plist = service_item_plist();
    start_service_supervised_with(&mut launchctl, &plist)
}

/// [`start_service_supervised`] 的可注入实现（单测用假 runner + 临时 plist 路径驱动）。
#[cfg(target_os = "macos")]
fn start_service_supervised_with(
    run: &mut LaunchctlRunner<'_>,
    plist: &std::path::Path,
) -> Result<()> {
    let target = format!("gui/{}/{}", uid(), SERVICE_ITEM_LABEL);
    match supervised_start_action(job_state_with(run, &target)) {
        SupervisedStart::Noop => Ok(()),
        SupervisedStart::Kickstart => {
            let out = run(&["kickstart", &target]).context("执行 launchctl kickstart 失败")?;
            if !out.status.success() {
                anyhow::bail!(
                    "launchctl kickstart 失败: {}",
                    String::from_utf8_lossy(&out.stderr).trim()
                );
            }
            Ok(())
        }
        SupervisedStart::Bootstrap => {
            if !plist.exists() {
                anyhow::bail!("bridge 登录项不存在（未开自启？）：{}", plist.display());
            }
            let domain = format!("gui/{}", uid());
            let out = run(&["bootstrap", &domain, &plist.display().to_string()])
                .context("执行 launchctl bootstrap 失败")?;
            if !out.status.success() {
                anyhow::bail!(
                    "launchctl bootstrap 失败: {}",
                    String::from_utf8_lossy(&out.stderr).trim()
                );
            }
            Ok(())
        }
    }
}

// ─────────────────────────── 自启变更审计（全平台） ───────────────────────────

/// 把 `include_str!` 进来的源码按 LF 归一。
///
/// **为什么必须有这一层**：本仓**没有** `.gitattributes` 强制换行（规则文档里那句「强制源码 LF」
/// 与实际不符），Windows 检出（GitHub Actions `windows-latest`，`core.autocrlf=true`）里源码是
/// **CRLF**。于是任何按 `\n` 拼接的片段匹配——例如 `find("\n}\n")` 取函数体——在 Windows 上匹配
/// 不到：CI run `36396786020` 实测 `updater::tests::verify_sha256_production_seam_wires_the_file_log`
/// 因此 panic（`.expect("函数体结束")`），而 macOS 本地全绿。
///
/// 按行扫描（`str::lines()`）本身对 CRLF 是安全的（它会吃掉行尾 `\r`）；**只有**「把 `\n` 写进
/// 模式串」或「按字节切片跨行」的守卫需要先过这一层。
///
/// 仅在测试构建里存在（所有源码守卫都在 `#[cfg(test)]` 模块内）。
#[cfg(test)]
pub(crate) fn src_lf(s: &'static str) -> std::borrow::Cow<'static, str> {
    if s.contains("\r\n") {
        std::borrow::Cow::Owned(s.replace("\r\n", "\n"))
    } else {
        std::borrow::Cow::Borrowed(s)
    }
}

/// 把一次自启配置变更/自愈结果追加到 `<bridge_dir>/logs/autostart.log`。
///
/// 为什么不能只靠 `crate::log!`：它只写 stdout（`main.rs:147`），而 GUI 由
/// `open`/Finder 拉起时 0/1/2 全指 /dev/null（本机实测 `lsof -p <gui pid>`），
/// 「自愈成没成、为什么失败」这条最需要留证据的信息会当场蒸发。plist 里的
/// `StandardOutPath` 只对 **launchd 拉起的那一次** 生效，救不了手工启动这条路。
/// 与 plist 那两个键互补：一个管以后，一个管当下。
///
/// 内容只有动作、路径与 launchctl 回显文本，绝不含密钥（同类行早已落在 bridge.out）。
/// best-effort：写失败只丢审计，绝不影响自启动作本身。
pub fn log_autostart_event(msg: &str) {
    append_event_log(&crate::bridge_dir().join("logs"), "autostart.log", msg);
}

/// 把一条审计事件追加到 `<logs_dir>/<file>`（时间戳 + 压平换行的单行记录）。
///
/// 与 [`log_autostart_event`] 同一套机制、同一份 [`autostart_record`] 格式，供其它
/// 「GUI 进程里发生、但消息可能蒸发」的链路复用（当前：升级动作 → `logs/update.log`）。
/// 目录作为参数传入是为了让单测用临时目录，不写真实 `~/.agent-bridge`
/// （见 `LESSON_单测不得写用户真实运行数据须拆出注入缝.md`）。
///
/// best-effort：写失败只丢审计，不影响调用方的动作本身。
pub fn append_event_log(logs_dir: &std::path::Path, file: &str, msg: &str) {
    use std::io::Write;
    let _ = std::fs::create_dir_all(logs_dir);
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(logs_dir.join(file))
    {
        // `autostart_record` 自己已经带结尾换行（它的单测就断言「一条记录只允许一个换行」），
        // 这里必须用 `write!`：早先用 `writeln!` 会在每条记录之间多落一个空行（本批新加的
        // 「一行一记录」断言把它抓出来了）。
        let _ = write!(f, "{}", autostart_record(&crate::chrono_lite::now(), msg));
    }
}

/// 审计行格式（纯函数，便于单测；时间戳由调用方给，避免测试依赖时钟）。
///
/// msg 一律压平换行：错误链里会拼进 `launchctl` 的 stderr 与 plist 路径（都可含
/// 换行），不清洗就会一条事件落成多行，甚至伪造出带时间戳样子的假记录。
fn autostart_record(ts: &str, msg: &str) -> String {
    // CRLF 先归一（否则一个换行会落成两个 ⏎）；再统一把独立 CR/LF 显式压成 ⏎——
    // 直接删掉 \r 会把 "a\rb" 无声粘成 "ab"，丢掉分隔语义。
    let flat = msg.replace("\r\n", "\n").replace(['\r', '\n'], "⏎");
    format!("[{ts}] {flat}\n")
}

// ── Windows：登录自启 = HKCU Run 键 ──
// 值名 ABB，值 = "C:\...\agent-bridge.exe"（带引号；文件名由 current_exe() 决定）。
// GUI 每次刷新读 Run 键决定菜单显「开/关」。
// 注意：这里只判值**存在**，不像 macOS 那样校验路径是否还是当前二进制——安装位置
// 由 Inno 固定、升级器原地替换二进制，键值不会漂移；不做未经实测的注册表解析。
#[cfg(target_os = "windows")]
const AUTOSTART_RUN_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";
#[cfg(target_os = "windows")]
const AUTOSTART_VALUE: &str = "ABB";

/// 跑 reg.exe（CREATE_NO_WINDOW：GUI 进程 spawn 控制台程序不弹窗）。
#[cfg(target_os = "windows")]
fn run_reg(args: &[&str]) -> std::io::Result<std::process::Output> {
    crate::spawn::command("reg").args(args).output()
}

/// Windows：新模型（2026-10-04 owner 决定）下**没有 OS 级托管** —— 服务由托盘以普通用户身份
/// 拉起并看守（`install::svc_start` + 2 秒看门狗），用户意图仍由 `service.desired` 表达。
///
/// 为什么要拆掉旧形态：以前是 `RunLevel=HighestAvailable` 的常驻计划任务（高完整性，普通权限
/// 杀不掉）+ 一套提权件（`abb-spawner` / `abb-elev-helper` / `abb-helper`）。owner 2026-10-04
/// 决定「全部以普通用户运行，启动/停止统一由管理密码把关」⇒ 计划任务与提权件一并退出历史舞台。
/// 所以这里**恒 false**（调用方 `install::svc_start` 据此走托盘子进程那条路）。
#[cfg(target_os = "windows")]
pub fn service_supervised() -> bool {
    false
}

/// Windows：重启服务 = 让托盘那条用户级路径重拉（`install::svc_start` 自带「先停旧实例」）。
///
/// 新模型下没有计划任务可 `/end` `/run`：服务就是托盘的子进程（普通用户身份），
/// 重启即「杀掉 + 重拉」，与「启动」走同一条代码，不再有两套语义。
#[cfg(target_os = "windows")]
pub fn restart_service_supervised() -> Result<()> {
    crate::install::svc_start()
}

/// Windows：停止服务 —— 新模型下**不需要提权**，就是真停掉那个用户级进程。
///
/// 历史：以前走提权 helper 的白名单 op `stop-bridge-task`（UAC + 审计），因为服务当时跑在
/// `HighestAvailable` 计划任务下、普通权限杀不掉。owner 2026-10-04 决定「全部以普通用户运行，
/// 启动/停止统一由 ABB 管理密码把关」⇒ 提权那层退出，这里只负责**真停掉**；
/// 「要不要停」由 UI 层的管理密码门决定（`admin_pass`）——两道门职责分开，别混。
#[cfg(target_os = "windows")]
pub fn stop_service_authorized() -> Result<()> {
    crate::install::svc_stop_keep_desired()
}

/// Windows：托管形态下的「启动」—— 新模型下等价于让托盘那条用户级路径重拉
///（`install::svc_start` 自带「已在跑就先停」，故与重启同一入口，不再有两套语义）。
#[cfg(target_os = "windows")]
pub fn start_service_supervised() -> Result<()> {
    crate::install::svc_start()
}

#[cfg(target_os = "windows")]
pub fn service_persist_state() -> &'static str {
    // 新模型（2026-10-04）：「服务常驻」不再由计划任务表达 —— 自启是**托盘的 Run 键**
    // （见 autostart_enabled），服务由托盘的看门狗拉起。故恒 absent。
    "absent"
}

/// 从 `reg query` 输出里取出 Run 值指向的路径（取不到返回 None）。
///
/// 值名与类型之间是**多个空格**，故不按空白切分（会切出空段），而是锚定 `REG_SZ` 取其后内容。
#[cfg(any(target_os = "windows", test))]
fn parse_run_value(reg_query_stdout: &str) -> Option<String> {
    for line in reg_query_stdout.lines() {
        if let Some(i) = line.find("REG_SZ") {
            let v = line[i + "REG_SZ".len()..].trim().trim_matches('"').trim();
            if !v.is_empty() {
                return Some(v.to_string());
            }
        }
    }
    None
}

/// Run 值指向的可执行路径是不是**当前这份**二进制。
///
/// 为什么必须比对（2026-10-05 审计 #10）：per-user 装法（`{localappdata}` 时代）迁到
/// per-machine 后，旧 Run 值仍指着 `%LOCALAPPDATA%` 下已经不存在的 exe —— 而原先只看
/// `reg query` **成功** ⇒ 托盘显示「自启：开」、登录却什么都不发生，`heal_autostart` 又
/// 认为「已注册」而不修（本机 2026-10-04 实测就是这种指向不存在路径的值）。
#[cfg(any(target_os = "windows", test))]
fn run_value_matches_exe(value: &str, exe: &std::path::Path) -> bool {
    let norm = |s: &str| {
        s.replace('/', "\\")
            .trim_matches('"')
            .trim()
            .to_ascii_lowercase()
    };
    norm(value) == norm(&exe.display().to_string())
}

#[cfg(target_os = "windows")]
pub fn autostart_enabled() -> bool {
    let Ok(out) = run_reg(&["query", AUTOSTART_RUN_KEY, "/v", AUTOSTART_VALUE]) else {
        return false;
    };
    if !out.status.success() {
        return false;
    }
    // 有值还不够：必须**指向当前这份二进制**，否则就是「显示开、登录没反应」的漂移态。
    let Ok(exe) = current_exe() else {
        return false;
    };
    match parse_run_value(&String::from_utf8_lossy(&out.stdout)) {
        Some(v) => run_value_matches_exe(&v, &exe),
        None => false,
    }
}

/// 「用户意图开自启」持久标记（与 `logs/service.desired` 同款：存在 = 开，不存在 = 关/
/// 没开过）。Run 键没有「谁改的」信息，被安全软件静默删除时与「用户自己关掉」不可
/// 区分；没有这个标记就没法既自愈、又不擅自替人开自启。
#[cfg(target_os = "windows")]
fn autostart_desired_flag_at(logs: &std::path::Path) -> std::path::PathBuf {
    logs.join("autostart.desired")
}

/// 写 / 清意图标记。base 可注入，单测不碰真实 `~/.agent-bridge`。
#[cfg(target_os = "windows")]
fn set_autostart_desired_at(logs: &std::path::Path, enable: bool) -> Result<()> {
    let f = autostart_desired_flag_at(logs);
    if enable {
        std::fs::create_dir_all(logs)
            .with_context(|| format!("建自启标记目录失败: {}", logs.display()))?;
        std::fs::write(&f, b"1").with_context(|| format!("写自启意图标记失败: {}", f.display()))?;
    } else {
        match std::fs::remove_file(&f) {
            Ok(()) => {}
            // 本来就不存在 = 已经是「关」，幂等，不算错。
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(anyhow::anyhow!("删自启意图标记失败 {}: {e}", f.display()));
            }
        }
    }
    Ok(())
}

#[cfg(target_os = "windows")]
fn autostart_desired_at(logs: &std::path::Path) -> bool {
    autostart_desired_flag_at(logs).exists()
}

#[cfg(target_os = "windows")]
fn set_autostart_impl(enable: bool) -> Result<()> {
    let exe = current_exe()?;
    // 新模型（2026-10-04）：自启 = 只写用户的 Run 键（拉起**托盘**），服务由托盘的看门狗
    // 以普通用户身份拉起 —— 不再注册常驻计划任务、不再要管理员授权。
    let out = if enable {
        let val = format!("\"{}\"", exe.display());
        run_reg(&[
            "add",
            AUTOSTART_RUN_KEY,
            "/v",
            AUTOSTART_VALUE,
            "/t",
            "REG_SZ",
            "/d",
            &val,
            "/f",
        ])
    } else {
        run_reg(&["delete", AUTOSTART_RUN_KEY, "/v", AUTOSTART_VALUE, "/f"])
    }
    .context("reg 命令执行失败")?;
    if !out.status.success() {
        let msg = String::from_utf8_lossy(&out.stderr);
        anyhow::bail!("设置开机自启失败: {}", msg.trim())
    }
    // 注册表动作成功后再落意图标记。顺序要紧：标记是「用户想要开/关」的唯一判据，
    // 若标记先落而注册表失败，下次自愈就会拿一个未兑现的意图去改用户配置。
    // 标记写失败不回滚注册表（用户要的效果已达成），但响亮留痕——否则自愈会静默失效。
    if let Err(e) = set_autostart_desired_at(&crate::bridge_dir().join("logs"), enable) {
        log_autostart_event(&format!("⚠️ 自启意图标记写入失败（自愈将失效）: {e:#}"));
    }
    Ok(())
}
#[cfg(target_os = "linux")]
pub fn autostart_enabled() -> bool {
    false // TODO: ~/.config/autostart/agent-bridge.desktop
}
#[cfg(target_os = "linux")]
fn set_autostart_impl(_enable: bool) -> Result<()> {
    anyhow::bail!("Linux autostart 尚未实现")
}

// ─────────────────────────── 一次性数据迁移 ───────────────────────────

/// 把旧的「平铺单 bot」运行时数据迁到「workspaces/<key>/」结构。幂等、best-effort：
///   ~/.agent-bridge/workspace/     → workspaces/<key>/   （目录内容整体搬入）
///   ~/.agent-bridge/sessions.json  → workspaces/<key>/sessions.json
///   ~/.agent-bridge/jobs.json      → workspaces/<key>/jobs.json
/// 旧「单 bot 平铺」数据文件名（#178 闸门与迁移循环共用单一事实源——两侧分写会
/// 在新增平铺文件时静默分叉：闸门漏判 → dest 不建 → rename 静默失败数据搁浅）。
const LEGACY_FLAT_FILES: [&str; 2] = ["sessions.json", "jobs.json"];

pub fn migrate_legacy_state(cfg: &crate::config::Config) {
    // flat 遗留是单 bot 时代产物，归首 bot；bot2+ 的隔离键迁移由 service 的
    // migrate_keys 负责（职责边界）。
    if let Some(b) = cfg.bots.first() {
        migrate_legacy_state_at(&crate::bridge_dir(), &cfg.bots, b);
    }
}

/// 内部实现（base 可注入，单测不碰真实 ~/.agent-bridge）。
fn migrate_legacy_state_at(
    base: &std::path::Path,
    bots: &[crate::config::BotConfig],
    bot: &crate::config::BotConfig,
) {
    let new_key = bot.key();
    let dest = base.join("workspaces").join(&new_key);
    // #187：旧数据候选有三处——平铺 json、旧 workspace/ 目录、workspaces/<legacy_key>/
    //（#174 旧隔离键布局）。任一存在才建 dest（#178：无条件预建空目录会搁浅数据）。
    let old_ws = base.join("workspace");
    let flat_pending = LEGACY_FLAT_FILES.iter().any(|f| base.join(f).exists());
    let legacy_dir = base.join("workspaces").join(bot.legacy_key());
    // #187 审查 F5：legacy_key 被别的 bot 留守（其 key==legacy_key，同名双 bot 且
    // 首位带 app_id 的形态）时**不 fold**——该目录归留守 bot（service 的 contested
    // 规则同样判给它），两序归属才一致；否则 GUI 折给首 bot、service 判给留守者，
    // 同一目录两序发散。
    let contested = bots
        .iter()
        .any(|o| !std::ptr::eq(o, bot) && o.key() == bot.legacy_key());
    let legacy_dir_pending = legacy_dir.is_dir() && legacy_dir != dest && !contested;
    if old_ws.is_dir() || flat_pending || legacy_dir_pending {
        let _ = std::fs::create_dir_all(&dest);
    }

    // 旧 workspace/ 目录内容搬入 workspaces/<key>/
    if old_ws.is_dir() {
        if let Ok(entries) = std::fs::read_dir(&old_ws) {
            for e in entries.flatten() {
                let to = dest.join(e.file_name());
                if !to.exists() {
                    let _ = std::fs::rename(e.path(), &to);
                }
            }
        }
        let _ = std::fs::remove_dir(&old_ws); // 仅当空了才成功
    }
    // 旧平铺 json 搬入
    for f in LEGACY_FLAT_FILES {
        let from = base.join(f);
        let to = dest.join(f);
        if from.exists() && !to.exists() {
            let _ = std::fs::rename(&from, &to);
        }
    }
    // #187：隔离键旧目录折入——GUI 先于 service 跑时，把 workspaces/<legacy_key>/
    // 逐项并入 dest（绝不覆盖已有项），service 的 migrate_keys 不再被非空 dest 拦住
    //（#178 修复后只响亮跳过，数据照样搁浅）；service 先跑时旧目录已被 rename 成
    // dest，此步自然 no-op。两序皆收敛。contested（留守 bot 占用旧键）时不折入，
    // 归属两序一致（见上 F5 注释）。逐项搬移天然幂等且不覆盖，可安全重入。
    if legacy_dir_pending {
        if let Ok(entries) = std::fs::read_dir(&legacy_dir) {
            for e in entries.flatten() {
                let to = dest.join(e.file_name());
                if !to.exists() {
                    let _ = std::fs::rename(e.path(), &to);
                }
            }
        }
        let _ = std::fs::remove_dir(&legacy_dir); // 仅当空了才成功
                                                  // #187 审查 F7：目标同名冲突会让部分条目滞留旧目录（每次 GUI 启动静默
                                                  // 重试）——响亮落日志，别把搁浅伪装成成功。
        if legacy_dir.is_dir() && matches!(crate::config::Config::dir_empty(&legacy_dir), Ok(false))
        {
            crate::log!(
                "[migrate] ⚠️ 旧目录 {} 部分内容因同名未折入 {}，已保留原位，请人工处理",
                legacy_dir.display(),
                dest.display()
            );
        }
    }
    crate::log!("[migrate] 旧单 bot 数据已并入 workspaces/{new_key}/（幂等）");
}

/// 一次性退役：删除**旧的高权限常驻计划任务**（2026-10-04 新模型不再需要它）。
///
/// 为什么必须做：旧任务以 `RunLevel=HighestAvailable` 运行 ⇒ 它拉起的是**高完整性** service，
/// 而新模型的 service 是托盘的普通用户子进程。两者抢同一个 `service` 单实例锁 ⇒ 谁先拿到谁跑，
/// 表现为「有时是提权实例、有时不是」的随机状态（也会让「全部以普通用户运行」这条承诺失真）。
/// 删掉任务后语义唯一：**服务只由托盘以普通用户身份看守**。
///
/// 权限：任务属当前用户（RunLevel 只影响启动后的完整性），`/delete` 不需要提权。
/// best-effort：绝大多数机器本来就没有旧任务，静默返回；只在「确实存在但删不掉」时响亮留痕。
#[cfg(target_os = "windows")]
pub fn retire_legacy_bridge_task() {
    // 名字写字面量：新模型下已没有任务定义模块（`src/svc_task.rs` 已删），而**退役旧任务**
    // 恰恰需要这个历史名字 —— 它是这段迁移代码唯一的用途，故就地固定。
    let name = "ABB-Bridge";
    let exists = crate::spawn::command("schtasks")
        .args(["/query", "/tn", name])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if !exists {
        return;
    }
    match crate::spawn::command("schtasks")
        .args(["/delete", "/tn", name, "/f"])
        .output()
    {
        Ok(o) if o.status.success() => crate::log!(
            "[migrate] 已退役旧常驻计划任务 {name}（新模型：服务由托盘以普通用户身份看守）"
        ),
        Ok(o) => crate::log!(
            "[migrate] ⚠️ 删除旧常驻计划任务失败（它可能仍会拉起高完整性实例）：{}",
            String::from_utf8_lossy(&o.stderr).trim()
        ),
        Err(e) => crate::log!("[migrate] schtasks 执行失败（跳过旧任务退役）：{e:#}"),
    }
}

#[cfg(not(target_os = "windows"))]
pub fn retire_legacy_bridge_task() {}
/// 当前进程是否以管理员身份运行（**不再依赖已删的 elev lib**）。
///
/// 直接 extern 声明 IsUserAnAdmin（shell32）：本函数只要一个布尔答案，不值得引一整套 Win32
/// 绑定 —— 与 macOS 侧「零依赖 raw FFI」的风格一致。
///
/// 用途只有一个：**告警**。新模型（2026-10-04）要求 ABB 以普通用户运行；若维护者手动提权
/// 启动，agent 会继承管理员权限，这里让 spawn/服务启动时能响亮留痕。
#[cfg(windows)]
pub fn is_elevated() -> bool {
    #[link(name = "shell32")]
    extern "system" {
        fn IsUserAnAdmin() -> i32;
    }
    // SAFETY: 无参数、无副作用，返回值按 BOOL 解释。
    unsafe { IsUserAnAdmin() != 0 }
}

#[cfg(not(windows))]
pub fn is_elevated() -> bool {
    false
}

/// 一次性数据迁移：改名 feishu-bridge → agent-bridge，数据目录 `~/feishu-bridge` → `~/.agent-bridge`。
/// 在 main() 最顶（args 解析、任何加锁/读写之前）调用。幂等、best-effort。
///
/// 逐条目 rename（同卷原子，保留 0600 权限与中文目录名），**不是整目录 mv**——
/// 旧目录里的 Python 时代遗留（bridge.py/venv/tenants/…）当时原地留作档案（现 Python 已整体删除，
/// WS 协议参考存于 feishu-bridge-rs/reference/feishu_ws_protocol.py）。回滚 = 反向 rename。
///   config.json / workspaces/ / logs/   →  ~/.agent-bridge/
/// 两个坑（Plan 阶段查实）：
///   - logs/service.desired 必须随迁：GUI 看门（ui.rs）依它自动拉起 service，不迁则 service 静默停摆。
///   - logs/service.pid 绝不能迁：stale pid 可能被系统复用，看门 svc_stop 会误杀无辜进程 → 迁后删掉。
///   - .gui.lock/.service.lock 不迁：flock 是 fd 锚点，旧进程死后锁已释放，新位置由 single_instance 自建。
pub fn migrate_to_agent_bridge() {
    // 自定义运行数据目录：不探测或搬动真实用户目录里的旧数据，避免测试/多实例串数据。
    if crate::bridge_home_override().is_some() {
        return;
    }
    let old = dirs::home_dir().unwrap_or_default().join("feishu-bridge");
    let new = crate::bridge_dir(); // ~/.agent-bridge
    if !old.is_dir() {
        return; // 快速路径：已迁过或全新机器，零成本
    }
    let mut moved_any = false;
    for entry in ["config.json", "workspaces", "logs"] {
        let from = old.join(entry);
        let to = new.join(entry);
        if from.exists() && !to.exists() {
            let _ = std::fs::create_dir_all(&new);
            if std::fs::rename(&from, &to).is_ok() {
                moved_any = true;
            }
        }
    }
    // 关键：只在「这次真的搬了数据」时才做收尾（删 stale pid、重写指引）。
    // 若无条件删 service.pid：迁移只跑一次，之后旧目录只剩 Python 遗留、三条目都已在
    // 新位置 → moved_any=false，但每次 GUI 看门拉起 service 仍会把刚写的 service.pid
    // 删掉 → 看门 status() 读不到 pid 误判 service 死了 → 每 2s 狂重启（flock 兜住不并发）。
    if !moved_any {
        return; // 无可迁条目（早已迁完）：什么都不做，尤其别碰 service.pid
    }
    // stale service.pid 必删（看门误杀风险）；service.desired 保留（看门自动拉起语义）
    let _ = std::fs::remove_file(new.join("logs").join("service.pid"));
    rewrite_workspace_guides(&new.join("workspaces"));
    crate::log!("[migrate] ~/feishu-bridge → ~/.agent-bridge 完成（幂等）");
}

/// 迁移后把每个 workspace 的 CLAUDE.md / AGENTS.md（agent 工作区指引）里的旧命名 in-place 更新。
/// 这些文件教 agent 调 `feishu-bridge job` CLI + 读 FEISHU_* env；改名后旧文案会让 agent 调不存在的命令。
/// ensure_workspace_guide 只在文件不存在时写，所以存量文件必须在这里就地改。
/// 替换顺序：先换 FEISHU_* 全大写（与 feishu-bridge 无交集，防子串误伤），再换产品名。
fn rewrite_workspace_guides(workspaces: &std::path::Path) {
    let Ok(entries) = std::fs::read_dir(workspaces) else {
        return;
    };
    for ws in entries.flatten() {
        if !ws.path().is_dir() {
            continue;
        }
        for name in ["CLAUDE.md", "AGENTS.md"] {
            let p = ws.path().join(name);
            let Ok(text) = std::fs::read_to_string(&p) else {
                continue;
            };
            let new = text
                .replace("FEISHU_CHAT_ID", "AGENT_BRIDGE_CHAT_ID")
                .replace("FEISHU_BOT_KEY", "AGENT_BRIDGE_BOT_KEY")
                .replace("feishu-bridge", "agent-bridge")
                .replace("飞书桥", "Agent Bridge");
            // 内容相同跳过写盘（共享 helper，避免无谓重写）
            let _ = crate::atomic_write_text_if_changed(&p, &new);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 自启判定必须「有值 **且** 指向当前二进制」才算开（2026-10-05 审计 #10）。
    ///
    /// 判别力：把 `run_value_matches_exe` 从 `autostart_enabled()` 去掉（退回「只看 reg query
    /// 成功」）⇒ 最后一条断言的语义消失，用户看到「自启：开」但登录什么都不发生 —— 本机
    /// 2026-10-04 真实遇到过指向不存在路径的值。
    #[test]
    fn run_value_parses_and_matches_only_the_current_exe() {
        let sample = concat!(
            "HKEY_CURRENT_USER\\Software\\Microsoft\\Windows\\CurrentVersion\\Run\r\n",
            "    ABB    REG_SZ    C:\\Program Files\\ABB\\agent-bridge.exe\r\n\r\n"
        );
        assert_eq!(
            parse_run_value(sample).as_deref(),
            Some(r"C:\Program Files\ABB\agent-bridge.exe"),
            "必须能从 reg query 输出取出路径（值名与类型之间是多空格）"
        );
        assert!(parse_run_value("HKEY_CURRENT_USER\\...\\Run\r\n").is_none());
        let cur = std::path::Path::new(r"C:\Program Files\ABB\agent-bridge.exe");
        assert!(run_value_matches_exe(
            r"C:\Program Files\ABB\agent-bridge.exe",
            cur
        ));
        assert!(
            !run_value_matches_exe(
                r"C:\Users\gxh\AppData\Local\Programs\ABB\agent-bridge.exe",
                cur
            ),
            "旧 per-user 路径必须判「不匹配」—— 这正是本机真实出现过的漂移态"
        );
    }

    /// Windows 自启自愈判据：只在「意图为开 + 注册表缺失」时为真。这条纯函数是
    /// 「既不擅自替人开、又能补回被回滚的项」的唯一闸门，两个方向都要锁死。
    #[cfg(target_os = "windows")]
    #[test]
    fn autostart_heal_only_when_desired_and_missing() {
        assert!(autostart_heal_needed(true, false), "意图开 + 缺失 → 补写");
        assert!(!autostart_heal_needed(true, true), "意图开 + 也在 → 不动作");
        assert!(
            !autostart_heal_needed(false, true),
            "无意图 → 别动（可能是用户自己关的）"
        );
        assert!(!autostart_heal_needed(false, false), "没开过 → 绝不擅自开");
    }

    /// 意图标记落盘 / 清除（base 注入，不碰真实 ~/.agent-bridge）；开启幂等、关闭幂等。
    #[cfg(target_os = "windows")]
    #[test]
    fn autostart_desired_flag_roundtrip() {
        let dir =
            std::env::temp_dir().join(format!("abb-autostart-desired-{}", uuid::Uuid::new_v4()));
        let logs = dir.join("logs");
        assert!(!autostart_desired_at(&logs), "初始无标记 = 没开过");
        // 父目录不存在也要能建（首次开启场景）
        set_autostart_desired_at(&logs, true).unwrap();
        assert!(autostart_desired_at(&logs));
        set_autostart_desired_at(&logs, true).unwrap();
        assert!(autostart_desired_at(&logs), "重复开启幂等");
        set_autostart_desired_at(&logs, false).unwrap();
        assert!(!autostart_desired_at(&logs));
        set_autostart_desired_at(&logs, false).unwrap();
        assert!(!autostart_desired_at(&logs), "重复关闭幂等（不存在不算错）");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 审计行格式：一行一条、带时间戳前缀，供 logs/autostart.log 事后排查
    /// （GUI 由 open 起时 stdout 指 /dev/null，这是唯一留得下的证据）。
    #[test]
    fn autostart_record_is_one_timestamped_line() {
        let r = autostart_record("2026-09-08 20:31:02", "自愈：已按当前二进制重建");
        assert_eq!(r, "[2026-09-08 20:31:02] 自愈：已按当前二进制重建\n");
        assert_eq!(r.matches('\n').count(), 1, "一条记录只允许一个换行");
        // msg 里的换行必须被压平：错误链会拼进 launchctl 的 stderr（可含换行）与
        // plist 路径，不清洗就是一条事件落多行，甚至伪造出带时间戳样子的假记录。
        let dirty = autostart_record(
            "T",
            "bootstrap 失败: Bootstrap failed: 125\n[T2099-01-01 00:00:00] 伪造条目",
        );
        assert_eq!(dirty.matches('\n').count(), 1, "{dirty}");
        assert!(dirty.contains("⏎"), "{dirty}");
        assert!(
            !dirty.contains("\n[T2099"),
            "被压平的内容不得另起一行冒充独立记录: {dirty}"
        );
        assert_eq!(
            autostart_record("T", "a\r\nb"),
            "[T] a⏎b\n",
            "CRLF 只算一个分隔"
        );
        // 独立 CR（不带 LF）也得留痕：直接剥掉会把 "a\rb" 无声粘成 "ab"，丢分隔语义。
        assert_eq!(autostart_record("T", "a\rb"), "[T] a⏎b\n");
    }

    /// 事件落盘（升级审计复用同一机制）：目录不存在要自建、两次调用要**追加**（不覆盖）、
    /// 每行都是一个带时间戳的单行记录——这是 GUI 进程里唯一留得下的证据
    /// （`crate::log!` 只写 stdout，GUI 由安装器/资源管理器拉起时它会蒸发）。
    #[test]
    fn append_event_log_creates_appends_and_stays_single_line() {
        let dir = std::env::temp_dir().join(format!("abb-update-log-{}", uuid::Uuid::new_v4()));
        let logs = dir.join("logs");
        assert!(!logs.exists(), "前置：目标目录不存在，用来验「自建」");
        append_event_log(&logs, "update.log", "开始安装升级包 ABB-Setup-2.23.75.exe");
        append_event_log(&logs, "update.log", "多行错误链\n第二行不得另起一条");
        let text = std::fs::read_to_string(logs.join("update.log")).expect("日志应写出");
        assert_eq!(
            text.lines().count(),
            2,
            "两次调用 = 两条单行记录（追加而非覆盖；空行也不许有）：{text}"
        );
        assert!(
            !text.contains("\n\n"),
            "记录之间不得夹空行（`autostart_record` 自带结尾换行，写入时不能再用 writeln!）：{text:?}"
        );
        assert!(text.contains("开始安装升级包"), "第一条记录应保留：{text}");
        assert!(
            text.contains("第二行不得另起一条"),
            "多行内容应被压平进同一条记录，而不是丢掉：{text}"
        );
        assert!(
            text.lines().all(|l| l.starts_with('[')),
            "每行都要有时间戳前缀：{text}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 自启 plist 解析（macOS）：只认自己写的 schema；读不出参数一律 None（调用方
    /// 判 Absent），宁可当作"不是我们的配置"也不猜——历史上现网那份 plist 就是带
    /// 代码从不产出的键的手写遗留。
    #[cfg(target_os = "macos")]
    #[test]
    fn plist_program_argument_reads_registered_exe() {
        assert_eq!(
            plist_program_argument(
                "<dict>\n  <key>ProgramArguments</key>\n  <array>\n    <string>/Applications/ABB.app/Contents/MacOS/agent-bridge</string>\n  </array>\n</dict>"
            ),
            Some("/Applications/ABB.app/Contents/MacOS/agent-bridge".to_string())
        );
        // 无 ProgramArguments（畸形/手写遗留）
        assert_eq!(
            plist_program_argument("<dict><key>Label</key><string>x</string></dict>"),
            None
        );
        // 有 key 但数组里没有 <string>
        assert_eq!(
            plist_program_argument("<key>ProgramArguments</key><array></array>"),
            None
        );
        // 后面还跟着别的 array（如 StandardPaths 之流）也不误伤：只取本 key 之后第一个
        assert_eq!(
            plist_program_argument(
                "<key>ProgramArguments</key><array><string>/a/b</string></array><key>X</key><array><string>/c/d</string></array>"
            ),
            Some("/a/b".to_string())
        );
    }

    /// plist 模板 ↔ 解析的往返，以及模板里承载实测结论的那几个键必须存在。
    /// 抽成 `build_login_plist` 就是为了让这条测试能锁住 schema：谁把 `KeepAlive`
    /// 顺手删了（=「崩溃自动拉起」这条承诺静默失效），这里就红。
    #[cfg(target_os = "macos")]
    #[test]
    fn login_plist_round_trips_and_keeps_measured_keys() {
        // 含 XML 特殊字符的路径：写侧必须转义，否则产出非法 plist（launchd 直接拒载），
        // 而解析器又得还原回原路径，不然回显再次说谎。
        for p in [
            "/Applications/ABB.app/Contents/MacOS/agent-bridge",
            "/Users/x/My Apps&Co/ABB.app/Contents/MacOS/agent-bridge",
            "/tmp/a<b>c/agent-bridge",
        ] {
            let built = build_login_plist(std::path::Path::new(p), std::path::Path::new("/logs"));
            assert!(
                !built.contains("&Co") && built.contains("&amp;Co") == p.contains('&'),
                "转义不符: {built}"
            );
            assert_eq!(plist_program_argument(&built).as_deref(), Some(p));
        }
        let built = build_login_plist(
            std::path::Path::new("/x/agent-bridge"),
            std::path::Path::new("/y/logs"),
        );
        for key in [
            "<key>Label</key>",
            "<key>ProgramArguments</key>",
            "<key>RunAtLoad</key>",
            "<key>KeepAlive</key>",
            "<key>SuccessfulExit</key>",
            "<key>ThrottleInterval</key>",
            "<key>StandardOutPath</key>",
            "<key>StandardErrorPath</key>",
        ] {
            assert!(built.contains(key), "plist 模板缺键 {key}：{built}");
        }
        // GUI 日志重定向必须落在 logs/ 下（crate::log! 只写 stdout，不重定向=零证据）
        assert!(
            built.contains("<string>/y/logs/gui.out</string>")
                && built.contains("<string>/y/logs/gui.err</string>"),
            "{built}"
        );
        // 三个实测选型值也锁住：RunAtLoad 翻 false = 登录根本不拉起（功能静默死亡）；
        // SuccessfulExit 翻 true = 托盘「退出」会被复活；ThrottleInterval = 重试节奏。
        assert!(built.contains("<key>RunAtLoad</key>\n  <true/>"), "{built}");
        assert!(
            built.contains("<key>SuccessfulExit</key>\n    <false/>"),
            "{built}"
        );
        assert!(built.contains("<integer>10</integer>"), "{built}");
        // plist 文件名与标签必须同源（launchd 按标签记账，两者分叉=写了个没人加载的文件）
        assert_eq!(
            login_item_plist()
                .file_name()
                .map(|n| n.to_string_lossy().into_owned()),
            Some(format!("{LOGIN_ITEM_LABEL}.plist"))
        );
    }

    /// 防自杀保护的唯一数据来源是 `launchctl print` 里那一行主进程 pid。格式一旦
    /// 漂移，保护就静默失效（退回「点关 → 闪退」那个必修 bug），所以锁住实测形态：
    /// 一个 TAB + `pid = <n>`，且不被 `last exit code`/`exit timeout` 之类干扰。
    #[cfg(target_os = "macos")]
    #[test]
    fn parse_launchd_job_pid_matches_real_print_output() {
        assert_eq!(
            parse_launchd_job_pid(
                "\tstate = running\n\texit timeout = 5\n\tpid = 61995\n\tlast exit code = (never exited)\n"
            ),
            Some(61995)
        );
        // 无空格形态（未来格式变化的容错）
        assert_eq!(parse_launchd_job_pid("        pid=42\n"), Some(42));
        // 本机实测：那份 EX_CONFIG 的 job 输出里根本没有 pid 行 → None（可安全 bootout）
        assert_eq!(
            parse_launchd_job_pid(
                "\tstate = spawn scheduled\n\truns = 1\n\tlast exit code = 78: EX_CONFIG\n"
            ),
            None
        );
        // 含 `pid` 字样但不是主进程行的，不误配
        assert_eq!(parse_launchd_job_pid("\trespawn count = 3\n"), None);
    }

    /// 本机实测的两种 `launchctl print` 形态（2026-09-30 现场抄录，去掉易漂移字段）。
    /// `job_state_with` 的判据全部落在这两份文本上：**只有 pid 行才算在跑**。
    #[cfg(target_os = "macos")]
    const PRINT_NOT_RUNNING: &str = "\tactive count = 0\n\tpath = /Users/x/Library/LaunchAgents/com.sqb.agent-bridge.service.plist\n\ttype = LaunchAgent\n\tstate = not running\n\truns = 1\n\tlast exit code = 0\n";
    /// 同一 job 被 `kickstart` 拉起来之后（真实输出里 `state = running` + `pid = <n>`）。
    #[cfg(target_os = "macos")]
    const PRINT_RUNNING: &str = "\tactive count = 1\n\tpath = /Users/x/Library/LaunchAgents/com.sqb.agent-bridge.service.plist\n\ttype = LaunchAgent\n\tstate = running\n\tpid = 13603\n\tlast exit code = 0\n";

    /// 假 `launchctl` 的签名与「收到的命令」账本（拆成别名只为压掉 clippy 的 type_complexity）。
    #[cfg(target_os = "macos")]
    type FakeRunner = Box<dyn FnMut(&[&str]) -> std::io::Result<std::process::Output>>;
    #[cfg(target_os = "macos")]
    type RecordedCalls = std::rc::Rc<std::cell::RefCell<Vec<Vec<String>>>>;

    /// 构造一个假的 `launchctl`：按**动词**前缀匹配预置应答，同时把收到的完整命令记下来。
    /// 这样测的是真实判据与命令序，而不是 spawn 真的 launchctl（那会动到用户的 launchd 域）。
    #[cfg(target_os = "macos")]
    fn fake_launchctl(
        plan: Vec<(&'static str, i32, &'static str, &'static str)>,
    ) -> (FakeRunner, RecordedCalls) {
        let calls = std::rc::Rc::new(std::cell::RefCell::new(Vec::<Vec<String>>::new()));
        let seen = std::rc::Rc::clone(&calls);
        let runner = Box::new(move |args: &[&str]| {
            seen.borrow_mut()
                .push(args.iter().map(|s| s.to_string()).collect::<Vec<String>>());
            let verb = args.first().copied().unwrap_or("");
            let (_, code, stdout, stderr) = plan
                .iter()
                .find(|(v, ..)| *v == verb)
                .unwrap_or_else(|| panic!("测试没预置 `launchctl {verb}` 的应答：{args:?}"));
            Ok(fake_status(*code, stdout, stderr))
        });
        (runner, calls)
    }

    /// 假 `Output`（macOS 测试专用：unix 可以用 from_raw 造退出码）。
    #[cfg(target_os = "macos")]
    fn fake_status(code: i32, stdout: &str, stderr: &str) -> std::process::Output {
        use std::os::unix::process::ExitStatusExt;
        std::process::Output {
            status: std::process::ExitStatus::from_raw(code << 8),
            stdout: stdout.as_bytes().to_vec(),
            stderr: stderr.as_bytes().to_vec(),
        }
    }

    /// 「启动」的判据三态：在跑=不动、**在域里没在跑=必须 kickstart**、不在域里=bootstrap。
    ///
    /// 中间那条是 2026-09-30 owner 实报（升级到 v2.23.77 后 mac 上 bridge 再没起来）的回归锁：
    /// 旧实现只有「在域里 ⇒ Ok」一档，于是这个形态永远没人拉。
    #[cfg(target_os = "macos")]
    #[test]
    fn supervised_start_action_kickstarts_loaded_but_stopped_job() {
        assert_eq!(
            supervised_start_action(JobState::Running),
            SupervisedStart::Noop
        );
        assert_eq!(
            supervised_start_action(JobState::Stopped),
            SupervisedStart::Kickstart,
            "已加载但没在跑 ⇒ 必须 kickstart（否则是永久静止态）"
        );
        assert_eq!(
            supervised_start_action(JobState::Absent),
            SupervisedStart::Bootstrap
        );
    }

    /// 形态读取落在那两份实测文本上：`not running`/无 pid → Stopped；有 pid → Running；
    /// `print` 非 0 → Absent。
    #[cfg(target_os = "macos")]
    #[test]
    fn job_state_distinguishes_loaded_from_running() {
        let (mut run, _) = fake_launchctl(vec![("print", 0, PRINT_NOT_RUNNING, "")]);
        assert_eq!(job_state_with(&mut *run, "gui/501/x"), JobState::Stopped);

        let (mut run, _) = fake_launchctl(vec![("print", 0, PRINT_RUNNING, "")]);
        assert_eq!(job_state_with(&mut *run, "gui/501/x"), JobState::Running);

        let (mut run, _) = fake_launchctl(vec![("print", 1, "", "Could not find service")]);
        assert_eq!(job_state_with(&mut *run, "gui/501/x"), JobState::Absent);
    }

    /// 已是「在域里没在跑」时，「启动」必须发一次 `kickstart`——**不带 `-k`**（本机实测
    /// 不带 -k 对正在跑的 job 是 rc=0、pid 不变；带 -k 会打断实例，看门狗每 2s 一次就成抖动）。
    #[cfg(target_os = "macos")]
    #[test]
    fn start_service_supervised_kickstarts_stopped_job_without_kill() {
        let (mut run, calls) = fake_launchctl(vec![
            ("print", 0, PRINT_NOT_RUNNING, ""),
            ("kickstart", 0, "", ""),
        ]);
        // plist 故意给一个不存在的路径：kickstart 这一支不该依赖它（依赖了说明走到了 bootstrap）。
        let plist = std::env::temp_dir().join("abb-not-exist-svc.plist");
        start_service_supervised_with(&mut *run, &plist).expect("kickstart 应成功");

        let calls = calls.borrow();
        let target = format!("gui/{}/{}", uid(), SERVICE_ITEM_LABEL);
        assert_eq!(calls.len(), 2, "只该 print + kickstart，实得 {calls:?}");
        assert_eq!(calls[0], vec!["print".to_string(), target.clone()]);
        assert_eq!(calls[1], vec!["kickstart".to_string(), target]);
        assert!(
            !calls.iter().flatten().any(|a| a == "-k"),
            "绝不能带 -k（会杀掉正在跑的实例）：{calls:?}"
        );
        assert!(
            !calls
                .iter()
                .any(|c| c.first().map(String::as_str) == Some("bootstrap")),
            "已加载的 job 不该再 bootstrap：{calls:?}"
        );
    }

    /// 已在跑 ⇒ 只查一次、不发任何命令（看门狗 2s 一拍，这里多发一条就是抖动源头）。
    #[cfg(target_os = "macos")]
    #[test]
    fn start_service_supervised_noops_when_running() {
        let (mut run, calls) = fake_launchctl(vec![("print", 0, PRINT_RUNNING, "")]);
        let plist = std::env::temp_dir().join("abb-not-exist-svc.plist");
        start_service_supervised_with(&mut *run, &plist).expect("在跑 ⇒ Ok");
        assert_eq!(calls.borrow().len(), 1, "在跑就不该再发命令");
    }

    /// 不在域里才 `bootstrap`（argv 形态一并锁住）；plist 缺失时如实报错且不发 bootstrap。
    #[cfg(target_os = "macos")]
    #[test]
    fn start_service_supervised_bootstraps_only_when_job_absent() {
        let base = std::env::temp_dir().join(format!("abb-svc-start-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&base).unwrap();
        let plist = base.join("com.sqb.agent-bridge.service.plist");
        std::fs::write(&plist, b"<plist/>").unwrap();

        let (mut run, calls) = fake_launchctl(vec![
            ("print", 1, "", "Could not find service"),
            ("bootstrap", 0, "", ""),
        ]);
        start_service_supervised_with(&mut *run, &plist).expect("bootstrap 应成功");
        {
            let calls = calls.borrow();
            assert_eq!(calls.len(), 2, "print + bootstrap，实得 {calls:?}");
            assert_eq!(
                calls[1],
                vec![
                    "bootstrap".to_string(),
                    format!("gui/{}", uid()),
                    plist.display().to_string()
                ]
            );
        }

        // 没开自启（plist 不在）⇒ 如实报错，别静默 Ok 也别乱 bootstrap。
        let (mut run, calls) = fake_launchctl(vec![("print", 1, "", "Could not find service")]);
        let missing = base.join("absent.plist");
        let err = start_service_supervised_with(&mut *run, &missing)
            .expect_err("plist 缺失必须报错")
            .to_string();
        assert!(err.contains("bridge 登录项不存在"), "{err}");
        assert_eq!(calls.borrow().len(), 1, "报错前不该发 bootstrap");

        let _ = std::fs::remove_dir_all(&base);
    }

    /// `kickstart` 自己失败（rc≠0）必须如实 Err —— 「拉不起来」不能报成成功。
    #[cfg(target_os = "macos")]
    #[test]
    fn start_service_supervised_reports_kickstart_failure() {
        let (mut run, _) = fake_launchctl(vec![
            ("print", 0, PRINT_NOT_RUNNING, ""),
            ("kickstart", 1, "", "Operation not permitted"),
        ]);
        let plist = std::env::temp_dir().join("abb-not-exist-svc.plist");
        let err = start_service_supervised_with(&mut *run, &plist)
            .expect_err("kickstart rc≠0 必须 Err")
            .to_string();
        assert!(err.contains("Operation not permitted"), "{err}");
    }

    /// **生产接线守卫**：`pub fn start_service_supervised()` 必须委托给上面那批用例覆盖的
    /// `_with` 实现，并且把真实 `launchctl` 传进去（否则测试测的是旁路副本：判据改回
    /// 「只看 job_loaded」时单测照样绿，而线上照旧起不来）。
    #[cfg(target_os = "macos")]
    #[test]
    fn service_start_prod_entry_delegates_to_tested_impl() {
        // 必须过 `src_lf`：Windows 检出是 CRLF，按 `\n` 取函数体会匹配不到（CI 实测过）。
        let src = src_lf(include_str!("platform.rs"));
        let impl_at = src
            .find("fn start_service_supervised_with(")
            .expect("可注入实现存在");
        // macOS 那一份生产入口在实现之前（文件后面还有 Linux 的 no-op stub，同名）
        let head = src[..impl_at]
            .rfind("pub fn start_service_supervised()")
            .expect("生产入口存在");
        let tail = src[head..]
            .find("\n}\n")
            .map(|i| head + i)
            .expect("函数体结束");
        let body = &src[head..tail];
        assert!(
            body.contains("start_service_supervised_with(") && body.contains("launchctl"),
            "pub fn start_service_supervised 必须调 start_service_supervised_with 并传真实 launchctl\n{body}"
        );
        // 反向（评审 R1 的 O1 / R2 复测 M5）：保留上面两个受检子串、却塞回「已加载就早退」
        // 的旧判据时，子串断言照样绿 —— 那正好是本批要修的 bug，故这里把旁路也钉住。
        assert!(
            !body.contains("job_loaded"),
            "生产入口不得再判 job_loaded（「已加载」≠「在跑」，那是本批修掉的静止态根因）\n{body}"
        );
    }

    /// `run_bounded` 必须**到点就回 Err**，而不是陪着子进程挂住：用真 `sleep` 量时间。
    ///
    /// 判别力（评审 R1 的 B1）：把超时分支去掉/绕过，`sleep 5` 会跑完并以 `Ok` 返回 ⇒ 本用例红。
    /// 另一半是正例，防「一律 Err」的假实现。
    #[cfg(target_os = "macos")]
    #[test]
    fn run_bounded_reports_timeout_instead_of_hanging() {
        let mut slow = std::process::Command::new("/bin/sleep");
        slow.arg("5")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        let t0 = std::time::Instant::now();
        let err = run_bounded(slow, std::time::Duration::from_millis(300))
            .expect_err("超时必须 Err（绝不能陪着挂住）");
        assert_eq!(err.kind(), std::io::ErrorKind::TimedOut, "{err}");
        assert!(
            t0.elapsed() < std::time::Duration::from_secs(3),
            "应在 ~300ms 内返回，实得 {:?}",
            t0.elapsed()
        );

        let mut fast = std::process::Command::new("/bin/echo");
        fast.arg("abb-run-bounded")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        let out = run_bounded(fast, std::time::Duration::from_secs(5)).expect("echo 应成功");
        assert!(out.status.success());
        assert_eq!(
            String::from_utf8_lossy(&out.stdout).trim(),
            "abb-run-bounded",
            "有界执行不能吞掉输出"
        );
    }

    /// 接线守卫：`launchctl()` 必须走有界执行（B1 的修复点），不得退回无界的 `Command::output()`。
    #[cfg(target_os = "macos")]
    #[test]
    fn launchctl_calls_go_through_the_bounded_runner() {
        let src = src_lf(include_str!("platform.rs"));
        let head = src
            .find("fn launchctl(args: &[&str])")
            .expect("launchctl 调用点存在");
        let tail = src[head..]
            .find("\n}\n")
            .map(|i| head + i)
            .expect("函数体结束");
        let body = &src[head..tail];
        assert!(
            body.contains("run_bounded("),
            "launchctl 必须走 run_bounded\n{body}"
        );
        assert!(
            !body.contains(".output()"),
            "不得退回无界的 .output()\n{body}"
        );
    }

    /// 漂移判定四态：无 plist / 指向当前二进制 / 指向已消失的旧路径 / 指向另一份
    /// 仍存在的旧副本。后两种都必须判 Drifted——前者让 launchd 静默 EX_CONFIG(78)，
    /// 后者会在登录时拉起旧版本，都不能被 `autostart_enabled()` 报成「正常开着」。
    #[cfg(target_os = "macos")]
    #[test]
    fn login_item_state_distinguishes_matches_and_drift() {
        let base = std::env::temp_dir().join(format!("abb-autostart-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&base).unwrap();
        let plist = base.join("gui.plist");
        let current = base.join("agent-bridge");
        std::fs::write(&current, b"bin").unwrap();
        let write_plist = |exe: &std::path::Path| {
            // 用真实模板造夹具（而不是手写一小段 XML），这样模板与解析器一起被锁。
            std::fs::write(&plist, build_login_plist(exe, &base.join("logs"))).unwrap();
        };

        // 没有 plist = 用户从未开自启（自愈绝不能顺手给他开）
        assert_eq!(login_item_state_at(&plist, &current), LoginItem::Absent);
        write_plist(&current);
        assert_eq!(
            login_item_state_at(&plist, &current),
            LoginItem::Matches,
            "登记的就是当前二进制"
        );
        write_plist(&base.join("gone").join("agent-bridge"));
        assert_eq!(
            login_item_state_at(&plist, &current),
            LoginItem::Drifted,
            "App 被移动过：旧路径已不存在"
        );
        let other = base.join("agent-bridge-old");
        std::fs::write(&other, b"bin").unwrap();
        write_plist(&other);
        assert_eq!(
            login_item_state_at(&plist, &current),
            LoginItem::Drifted,
            "指向另一份仍存在的副本（旧版本残留）也算漂移"
        );
        std::fs::write(&plist, "<dict/>").unwrap();
        assert_eq!(
            login_item_state_at(&plist, &current),
            LoginItem::Absent,
            "读不出参数不猜"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    fn legacy_bot() -> crate::config::BotConfig {
        crate::config::BotConfig::for_test("旧名bot", "cli_newkey123456")
    }

    /// #187 回归护栏（GUI 先行序）：平铺遗留 + 隔离键旧目录同时存在 → GUI 的
    /// legacy 迁移把两路数据都并入 dest、旧目录消失——service 的隔离键迁移不再
    /// 被非空 dest 拦住（#178 修复后只会响亮跳过，数据照样搁浅）。
    #[test]
    fn migrate_legacy_state_folds_isolated_legacy_dir_gui_first() {
        let base = std::env::temp_dir().join(format!("abb-legacy-gui-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(base.join("workspaces/旧名bot")).unwrap();
        std::fs::write(base.join("workspaces/旧名bot/pending.json"), "[]").unwrap();
        std::fs::write(base.join("sessions.json"), r#"{"a":1}"#).unwrap();

        let b = legacy_bot();
        migrate_legacy_state_at(&base, std::slice::from_ref(&b), &b);

        // dest 同时收到两路数据（隔离键旧目录 + 平铺）
        assert!(
            base.join("workspaces/cli_newkey123456/pending.json")
                .exists(),
            "隔离键旧目录内容必须折入 dest"
        );
        assert!(
            base.join("workspaces/cli_newkey123456/sessions.json")
                .exists(),
            "平铺遗留必须落入 dest"
        );
        // 旧目录已折入消失：service 的 migrate_keys 无事可做，无搁浅
        //（service 序的收敛性由 config::migrate_keys 幂等测试覆盖）
        assert!(!base.join("workspaces/旧名bot").exists());
        let _ = std::fs::remove_dir_all(&base);
    }

    /// #187 回归护栏（service 先行序）：旧目录已被 service rename 成 dest →
    /// GUI 的 fold 自然 no-op（不重建旧目录），平铺数据直接落 dest。两序收敛。
    #[test]
    fn migrate_legacy_state_after_service_rename_noop_fold() {
        let base = std::env::temp_dir().join(format!("abb-legacy-svc-{}", uuid::Uuid::new_v4()));
        // 模拟 service 已完成隔离键迁移：dest 已带数据、旧目录不存在
        std::fs::create_dir_all(base.join("workspaces/cli_newkey123456")).unwrap();
        std::fs::write(base.join("workspaces/cli_newkey123456/pending.json"), "[]").unwrap();
        // 平铺遗留还在（GUI 从未跑过）
        std::fs::write(base.join("jobs.json"), r#"[]"#).unwrap();

        migrate_legacy_state_at(&base, std::slice::from_ref(&legacy_bot()), &legacy_bot());

        assert!(
            base.join("workspaces/cli_newkey123456/jobs.json").exists(),
            "平铺遗留必须落 dest"
        );
        assert!(
            !base.join("workspaces/旧名bot").exists(),
            "fold 不得重建旧目录（service 已 rename 走，no-op）"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    /// #187 审查 F5 回归护栏（contested fold 跳过）：首 bot 带 app_id、第二 bot
    /// 同名留守（key==首 bot 的 legacy_key）→ GUI fold 必须跳过（目录归留守 bot，
    /// 与 service 的 contested 规则两序一致，不折给首 bot）。
    #[test]
    fn migrate_legacy_state_skips_fold_when_legacy_key_contested() {
        let base = std::env::temp_dir().join(format!("abb-legacy-ct-{}", uuid::Uuid::new_v4()));
        // 隔离键旧目录 workspaces/x1（被留守 bot2 的 key 占用）
        std::fs::create_dir_all(base.join("workspaces/x1")).unwrap();
        std::fs::write(base.join("workspaces/x1/data.json"), "d").unwrap();

        let bots = vec![
            crate::config::BotConfig::for_test("x1", "a1"),
            crate::config::BotConfig::for_test("x1", ""),
        ];
        let first = &bots[0];
        assert_eq!(first.legacy_key(), "x1", "首 bot legacy=name");
        assert_eq!(first.key(), "a1", "首 bot 新键=app_id");
        assert_eq!(
            bots[1].key(),
            "x1",
            "留守 bot 的 key 即首 bot 的 legacy_key"
        );
        migrate_legacy_state_at(&base, &bots, first);

        // contested：目录不得被折入首 bot 的 dest，原位保留归留守 bot
        assert!(
            base.join("workspaces/x1/data.json").exists(),
            "留守占用的旧目录不得被 fold 折走"
        );
        assert!(
            !base.join("workspaces/a1").exists(),
            "contested 时不得为首 bot 建 dest（无其他可搬数据）"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    /// 本批新增的 **bridge 常驻 job**：`ProgramArguments` 必须带 `--service`，且与托盘 job
    /// 一样带 `KeepAlive`（被 kill 也拉回——这是「登录后服务不能被随便杀死」的 macOS 侧答案）。
    #[cfg(target_os = "macos")]
    #[test]
    fn service_plist_runs_service_with_keepalive() {
        let built = build_service_plist(
            std::path::Path::new("/Applications/ABB.app/Contents/MacOS/agent-bridge"),
            std::path::Path::new("/logs"),
        );
        let args = plist_program_arguments(&built);
        assert_eq!(
            args,
            vec![
                "/Applications/ABB.app/Contents/MacOS/agent-bridge".to_string(),
                "--service".to_string()
            ],
            "bridge job 必须带 --service：{built}"
        );
        assert!(built.contains("<key>KeepAlive</key>"), "必须保活：{built}");
        assert!(built.contains(SERVICE_ITEM_LABEL), "必须用新标签：{built}");
        assert!(
            built.contains("service.out") && built.contains("service.err"),
            "bridge 日志要与托盘分开：{built}"
        );
        // 托盘 job 不受影响：仍然无参数、仍然保活。
        let tray = build_login_plist(
            std::path::Path::new("/Applications/ABB.app/Contents/MacOS/agent-bridge"),
            std::path::Path::new("/logs"),
        );
        assert_eq!(plist_program_arguments(&tray).len(), 1, "{tray}");
        assert!(tray.contains(LOGIN_ITEM_LABEL));
        assert!(tray.contains("gui.out"), "{tray}");
    }

    /// 自启的**合并状态**：两个 job 都在才是「开」；只有托盘 job（存量用户）判 Drifted
    /// ——归到 Drifted 才会落进 `heal_autostart` 自愈路径，给老用户补上 bridge job，
    /// 同时托盘开关继续显示「开」（不回显说谎）。
    #[cfg(target_os = "macos")]
    #[test]
    fn autostart_state_requires_both_jobs_and_heals_legacy() {
        let base = std::env::temp_dir().join(format!("abb-autostart-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&base).unwrap();
        let exe = base.join("agent-bridge");
        std::fs::write(&exe, b"x").unwrap();
        let tray = base.join("tray.plist");
        let svc = base.join("svc.plist");
        let logs = base.join("logs");

        assert_eq!(autostart_state_at(&tray, &svc, &exe), LoginItem::Absent);

        std::fs::write(&tray, build_login_plist(&exe, &logs)).unwrap();
        assert_eq!(autostart_state_at(&tray, &svc, &exe), LoginItem::Drifted);

        std::fs::write(&svc, build_service_plist(&exe, &logs)).unwrap();
        assert_eq!(autostart_state_at(&tray, &svc, &exe), LoginItem::Matches);

        std::fs::write(&svc, build_login_plist(&exe, &logs)).unwrap();
        assert_eq!(autostart_state_at(&tray, &svc, &exe), LoginItem::Drifted);

        let _ = std::fs::remove_dir_all(&base);
    }

    /// `plist_program_arguments` 是多参解析 + XML 还原（`--service` 的判定依赖它）。
    #[cfg(target_os = "macos")]
    #[test]
    fn plist_program_arguments_parses_all_and_unescapes() {
        let text = "<key>ProgramArguments</key>\n  <array>\n    <string>/a &amp; b/ab</string>\n    <string>--service</string>\n  </array>\n";
        assert_eq!(
            plist_program_arguments(text),
            vec!["/a & b/ab".to_string(), "--service".to_string()]
        );
        assert!(
            plist_program_arguments("<dict/>").is_empty(),
            "读不出就空，别猜"
        );
    }
}
