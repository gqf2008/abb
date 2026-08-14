//! 触发 macOS 权限请求 —— 屏幕录制 / 摄像头 / 麦克风。
//!
//! 背景：TCC 的这三项必须「先调用一次对应 API」才会把本 app 加进 系统设置列表（光打开面板
//! 看不到 agent-bridge，因为从未请求过）。完全磁盘/辅助功能可手动 ＋ 添加，这三项不行。
//! 故提供 `--request-permissions`：逐项触发请求（未决定时才弹系统授权框），等用户点完再查下一项。
//!
//! 手写 objc_msgSend FFI（零依赖原则，同 platform.rs 的 dock 图标），不引 objc crate。
//!
//! 摄像头/麦克风用 `+[AVCaptureDevice requestAccessForMediaType:completionHandler:]`：该 API
//! **异步**（completionHandler 在后台队列回调），不能同步空等（实测：无 block 直调触发 NSException
//! abort）。故用 `dispatch_semaphore` + completionHandler block 安全等待结果，全程 catch_unwind 兜底。

#![cfg(target_os = "macos")]

use std::ffi::c_void;

type Id = *mut c_void;
type Sel = *mut c_void;

unsafe extern "C" {
    fn objc_getClass(name: *const std::ffi::c_char) -> Id;
    fn sel_registerName(name: *const std::ffi::c_char) -> Sel;
    fn objc_msgSend();
    fn dlsym(handle: *mut c_void, symbol: *const std::ffi::c_char) -> *mut c_void;

    // ── libdispatch：信号量 + block 运行时 ──
    fn dispatch_semaphore_create(value: isize) -> *mut c_void;
    fn dispatch_semaphore_wait(sema: *mut c_void, timeout: u64) -> isize;
    fn dispatch_semaphore_signal(sema: *mut c_void) -> isize;
    fn _Block_copy(block: *const c_void) -> *mut c_void;
    fn _Block_release(block: *const c_void);
}

const RTLD_DEFAULT: *mut c_void = -2isize as *mut c_void;
const DISPATCH_TIME_FOREVER: u64 = u64::MAX;

/// completionHandler 用的 NSBlock 字面量（栈上分配，_Block_copy 提到堆）。
/// isa 指向 _NSConcreteStackBlock；invoke 是回调签名 void(^)(BOOL granted)。
#[repr(C)]
struct StackBlock {
    isa: *const c_void,
    flags: i32,
    reserved: i32,
    invoke: *const c_void,
    descriptor: *const c_void,
    // ── 捕获的变量（从 capture 开始按序）──
    sema: *mut c_void,
}

#[repr(C)]
struct BlockDescriptor {
    reserved: usize,
    size: usize,
}

unsafe extern "C" {
    static _NSConcreteStackBlock: c_void;
}

/// completionHandler: void (^)(BOOL granted)。收到结果 → signal 信号量放行等待线程。
unsafe extern "C" fn av_completion(block: &mut StackBlock, _granted: bool) {
    unsafe { dispatch_semaphore_signal(block.sema) };
}

/// 取 AVMediaType 常量（"vide"=摄像头 / "soun"=麦克风）。返回 NSString* 的 Id。
unsafe fn av_media_type(sym: &std::ffi::CStr) -> Id {
    let p = unsafe { dlsym(RTLD_DEFAULT, sym.as_ptr()) };
    if p.is_null() {
        return std::ptr::null_mut();
    }
    // AVMediaTypeXxx 是 `NSString* const` 全局变量：符号地址里存着 NSString*，须解引用一层。
    unsafe { *(p as *const Id) }
}

/// 触发摄像头/麦克风授权（异步 API + 信号量等待）。name 仅用于日志。
/// 任何 FFI/OC 异常都不应打挂本进程（catch_unwind 兜底；abort 才是真要防的）。
fn request_one(sym: &std::ffi::CStr, name: &str) {
    let r = std::panic::catch_unwind(|| unsafe {
        let cls = objc_getClass(c"AVCaptureDevice".as_ptr());
        if cls.is_null() {
            crate::log!("[perm] ⚠️ 拿不到 AVCaptureDevice 类");
            return;
        }
        let media = av_media_type(sym);
        if media.is_null() {
            crate::log!("[perm] ⚠️ 拿不到 AVMediaType 常量（{name}）");
            return;
        }
        let msg1: unsafe extern "C" fn(Id, Sel, Id) -> Id =
            std::mem::transmute(objc_msgSend as unsafe extern "C" fn());
        let status_fn: unsafe extern "C" fn(Id, Sel, Id) -> i64 =
            std::mem::transmute(objc_msgSend as unsafe extern "C" fn());
        let status_sel = sel_registerName(c"authorizationStatusForMediaType:".as_ptr());
        // AVAuthorizationStatus: 0 NotDetermined / 1 Restricted / 2 Denied / 3 Authorized
        match status_fn(cls, status_sel, media) {
            3 => crate::log!("[perm] {name} 已授权，跳过"),
            2 => crate::log!("[perm] {name} 之前被拒绝过——不会再弹框，请去系统设置手动开"),
            1 => crate::log!("[perm] {name} 受系统限制（家长控制/描述文件），无法请求"),
            _ => {
                crate::log!("[perm] 请求 {name} 授权（系统弹框中，请在弹框里点允许）…");
                let sema = dispatch_semaphore_create(0);
                let descriptor = BlockDescriptor {
                    reserved: 0,
                    size: std::mem::size_of::<StackBlock>(),
                };
                let mut block = StackBlock {
                    isa: &_NSConcreteStackBlock as *const c_void,
                    flags: 0,
                    reserved: 0,
                    invoke: av_completion as *const c_void,
                    descriptor: &descriptor as *const BlockDescriptor as *const c_void,
                    sema,
                };
                // 提到堆（completionHandler 异步回调时栈帧可能已不可用）
                let heap_block = _Block_copy(&mut block as *mut StackBlock as *const c_void);
                let req_sel =
                    sel_registerName(c"requestAccessForMediaType:completionHandler:".as_ptr());
                let req: unsafe extern "C" fn(Id, Sel, Id, *mut c_void) =
                    std::mem::transmute(objc_msgSend as unsafe extern "C" fn());
                req(cls, req_sel, media, heap_block);
                // 等用户在弹框里点完（永久等；GUI 侧有外层超时兜底）
                dispatch_semaphore_wait(sema, DISPATCH_TIME_FOREVER);
                _Block_release(heap_block);
                let after = status_fn(cls, status_sel, media);
                let _ = msg1; // 占位避免误用（未用到单参版本）
                crate::log!("[perm] {name} 请求后状态={after}（3=授权 2=拒绝）");
            }
        }
    });
    if r.is_err() {
        crate::log!("[perm] ⚠️ {name} 请求过程异常（已忽略，请到系统设置手动开）");
    }
}

/// 触发屏幕录制请求（CGRequestScreenCaptureAccess，未决定时弹框）。
fn request_screen() {
    #[link(name = "CoreGraphics", kind = "framework")]
    unsafe extern "C" {
        fn CGPreflightScreenCaptureAccess() -> bool;
        fn CGRequestScreenCaptureAccess() -> bool;
    }
    let r = std::panic::catch_unwind(|| unsafe {
        if CGPreflightScreenCaptureAccess() {
            crate::log!("[perm] 屏幕录制 已授权，跳过");
        } else {
            crate::log!("[perm] 请求 屏幕录制 授权（系统弹框/亮设置项，请允许）…");
            let granted = CGRequestScreenCaptureAccess();
            crate::log!("[perm] 屏幕录制 请求结果={granted}");
        }
    });
    if r.is_err() {
        crate::log!("[perm] ⚠️ 屏幕录制 请求过程异常（已忽略）");
    }
}

/// 逐项触发：屏幕录制 → 摄像头 → 麦克风。GUI 在独立子进程里调，不阻塞托盘。
/// 每项之间系统会串行弹框；都点完后进程退出，GUI 再 re-check 刷新状态。
pub fn request_media_permissions() {
    crate::log!("[perm] 开始逐项请求权限（screen → camera → microphone）");
    request_screen();
    request_one(c"AVMediaTypeVideo", "camera");
    request_one(c"AVMediaTypeAudio", "microphone");
    crate::log!("[perm] 权限请求流程结束");
}
