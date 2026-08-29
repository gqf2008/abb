//! 平台抽象 —— 跨平台差异都收敛在这里（打开文件夹 / 开机自启 / 一次性数据迁移）。
//! 设计目标：本体（WS/协议/agent/定时）全平台同码；平台特有只在 macOS 实现，Win/Linux 留 stub。
//! 「服务监控 + 开机自启」不依赖 launchd/systemd/任务计划：托盘 GUI 自启（登录项），GUI 当 service 的看门。

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
        use std::os::windows::process::CommandExt;
        let _ = std::process::Command::new("explorer")
            .arg(path)
            .creation_flags(0x0800_0000)
            .spawn();
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
        use std::os::windows::process::CommandExt;
        // start 把首个带引号参数当窗口标题，故先给空标题再给 url；
        // CREATE_NO_WINDOW 避免 cmd 闪控制台。
        let _ = std::process::Command::new("cmd")
            .args(["/c", "start", "", url])
            .creation_flags(0x0800_0000)
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
    let mut cmd = {
        use std::os::windows::process::CommandExt;
        let mut c = std::process::Command::new("clip");
        c.creation_flags(0x0800_0000);
        c
    };
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

/// 是否已设为「登录时自动启动」。
#[cfg(target_os = "macos")]
pub fn autostart_enabled() -> bool {
    login_item_plist().exists()
}

/// 开启/关闭「登录时自动启动」。
/// 实现：往 ~/Library/LaunchAgents 写一个 LaunchAgent plist（以 LaunchServices 登录项方式拉起 GUI）。
/// 注意：这是「开机自启 GUI」，不是用 launchd 管 service 生命周期——service 由 GUI 监控。
#[cfg(target_os = "macos")]
pub fn set_autostart(enable: bool) -> Result<()> {
    let plist = login_item_plist();
    if enable {
        let exe = current_exe()?;
        if let Some(parent) = plist.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let content = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>com.sqb.agent-bridge.gui</string>
  <key>ProgramArguments</key>
  <array>
    <string>{}</string>
  </array>
  <key>RunAtLoad</key>
  <true/>
</dict>
</plist>
"#,
            exe.display()
        );
        std::fs::write(&plist, content)
            .with_context(|| format!("写登录项失败: {}", plist.display()))?;
    } else if plist.exists() {
        std::fs::remove_file(&plist).with_context(|| "删除登录项失败")?;
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn login_item_plist() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_default()
        .join("Library/LaunchAgents/com.sqb.agent-bridge.gui.plist")
}

// ── Windows：登录自启 = HKCU Run 键 ──
// 值名 ABB，值 = "C:\...\agent-bridge.exe"（带引号；文件名由 current_exe() 决定）。
// GUI 每次刷新读 Run 键决定菜单显「开/关」。
#[cfg(target_os = "windows")]
const AUTOSTART_RUN_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";
#[cfg(target_os = "windows")]
const AUTOSTART_VALUE: &str = "ABB";

/// 跑 reg.exe（CREATE_NO_WINDOW：GUI 进程 spawn 控制台程序不弹窗）。
#[cfg(target_os = "windows")]
fn run_reg(args: &[&str]) -> std::io::Result<std::process::Output> {
    use std::os::windows::process::CommandExt;
    std::process::Command::new("reg")
        .args(args)
        .creation_flags(0x0800_0000)
        .output()
}

#[cfg(target_os = "windows")]
pub fn autostart_enabled() -> bool {
    run_reg(&["query", AUTOSTART_RUN_KEY, "/v", AUTOSTART_VALUE])
        .map(|o| o.status.success())
        .unwrap_or(false)
}

#[cfg(target_os = "windows")]
pub fn set_autostart(enable: bool) -> Result<()> {
    let exe = current_exe()?;
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
    if out.status.success() {
        Ok(())
    } else {
        let msg = String::from_utf8_lossy(&out.stderr);
        anyhow::bail!("设置开机自启失败: {}", msg.trim())
    }
}
#[cfg(target_os = "linux")]
pub fn autostart_enabled() -> bool {
    false // TODO: ~/.config/autostart/agent-bridge.desktop
}
#[cfg(target_os = "linux")]
pub fn set_autostart(_enable: bool) -> Result<()> {
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

pub fn migrate_legacy_state(bot_key: &str) {
    let base = crate::bridge_dir();
    let dest = crate::workspace_dir(bot_key);
    // #178：只有确有旧数据要搬时才建 dest——无条件预建空目录会抢跑 service 的
    // 隔离键迁移（migrate_keys 见目标已存在跳过 rename），旧工作区数据搁浅
    //（2026-08-29 老板真机：GUI 启动预建空目录致庆小丰整目录未迁移）。
    let old_ws = base.join("workspace");
    let flat_pending = LEGACY_FLAT_FILES.iter().any(|f| base.join(f).exists());
    if old_ws.is_dir() || flat_pending {
        let _ = std::fs::create_dir_all(&dest);
    }

    // 旧 workspace/ 目录内容搬入 workspaces/<key>/
    let old_ws = base.join("workspace");
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
    crate::log!("[migrate] 旧单 bot 数据已并入 workspaces/{bot_key}/（幂等）");
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
