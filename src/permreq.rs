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

// ── 键鼠自动化前置权限（原 #129 锁屏按键注入）──
// 仿 ToDesk：agent 向锁屏 loginwindow 注入按键，需要 辅助功能（kTCCServicePostEvent，
// 注入键鼠事件必需）＋ 输入监控（kTCCServiceListenEvent，可选但建议）两项授权。
// 用 CoreGraphics 同步 API（CGRequest*EventAccess）：未决定时才弹系统授权框，
// 已授权/已拒绝不弹（返回当前布尔态），与 request_screen 同模式，无 block/信号量。

/// 触发「辅助功能」授权请求（kTCCServicePostEvent，CGEvent 注入必需）。
fn request_post_event() {
    #[link(name = "CoreGraphics", kind = "framework")]
    unsafe extern "C" {
        fn CGPreflightPostEventAccess() -> bool;
        fn CGRequestPostEventAccess() -> bool;
    }
    let r = std::panic::catch_unwind(|| unsafe {
        if CGPreflightPostEventAccess() {
            crate::log!("[perm] 辅助功能（按键注入）已授权，跳过");
        } else {
            crate::log!("[perm] 请求 辅助功能 授权（系统弹框中，请点允许）…");
            let granted = CGRequestPostEventAccess();
            crate::log!("[perm] 辅助功能 请求结果={granted}");
        }
    });
    if r.is_err() {
        crate::log!("[perm] ⚠️ 辅助功能 请求过程异常（已忽略，请到系统设置手动开）");
    }
}

/// 触发「输入监控」授权请求（kTCCServiceListenEvent）。
fn request_listen_event() {
    #[link(name = "CoreGraphics", kind = "framework")]
    unsafe extern "C" {
        fn CGPreflightListenEventAccess() -> bool;
        fn CGRequestListenEventAccess() -> bool;
    }
    let r = std::panic::catch_unwind(|| unsafe {
        if CGPreflightListenEventAccess() {
            crate::log!("[perm] 输入监控 已授权，跳过");
        } else {
            crate::log!("[perm] 请求 输入监控 授权（系统弹框中，请点允许）…");
            let granted = CGRequestListenEventAccess();
            crate::log!("[perm] 输入监控 请求结果={granted}");
        }
    });
    if r.is_err() {
        crate::log!("[perm] ⚠️ 输入监控 请求过程异常（已忽略，请到系统设置手动开）");
    }
}

/// 键鼠自动化前置权限：辅助功能 → 输入监控。
/// 与 request_media_permissions 同流程由 GUI「请求权限」按钮统一拉起。
pub fn request_lock_permissions() {
    crate::log!("[perm] 开始逐项请求键鼠自动化权限（辅助功能 → 输入监控）");
    request_post_event();
    request_listen_event();
    crate::log!("[perm] 键鼠自动化权限请求流程结束");
}

// ── #305 Step 0：相机探测（判定「ABB 派生的子进程是否继承 ABB 的 TCC 授权」）──

/// 首次抓帧用的帧率：多数 Mac 摄像头支持，且与历史行为一致（不做无谓的首轮失败）。
const CAMERA_PROBE_FRAMERATE: &str = "30";

/// 一次抓帧尝试（[`camera_probe`] 用它拼报告：成功那次，以及失败后被回退掉的那次）。
struct ProbeAttempt {
    framerate: String,
    exit: Option<i32>,
    bytes: u64,
    ok: bool,
    stderr: String,
}

/// `ffmpeg` 抓帧的参数（抽出来是为了单测能钉死这串 flag，防后续手滑改坏）。
/// `avfoundation` 的 `index` 形如 `"0"` / `"0:none"`（视频[:音频]）。
///
/// `framerate` 必须**精确等于**设备某个模式的 maxFrameRate：ffmpeg
/// （`libavdevice/avfoundation.m::configure_video_device`）按
/// `fabs(framerate - max_framerate) < 0.01` 匹配，不等就报
/// `Selected framerate ... is not supported by the device.`。**不能**靠省略 `-framerate`
/// 来「退化成设备默认」——该选项默认值就是 `ntsc`(29.97) 且恒生效，只支持 60fps 的机型照样失败
/// （回退见 [`parse_supported_framerates`] 与 [`camera_probe`]）。
///
/// 刻意**不带 `-y`**：本探测是诊断用，不该静默覆盖调用方指定的路径；抓帧前由
/// [`camera_probe`] 先删旧文件，仍存在时 ffmpeg 会报错——错误照样进报告，比误报成功好。
pub fn camera_probe_args(index: &str, out: &str, framerate: &str) -> Vec<String> {
    [
        "-hide_banner",
        "-loglevel",
        "error",
        "-f",
        "avfoundation",
        "-framerate",
        framerate,
        "-i",
        index,
        "-frames:v",
        "1",
        out,
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

/// 从 ffmpeg 打印的 `Supported modes:` 列表里解析设备**真实**支持的帧率（取每行 max）。
///
/// 匹配失败时 ffmpeg 会打印（`avfoundation.m::unsupported_format`）：
/// ```text
/// Selected framerate (30.000000) is not supported by the device.
/// Supported modes:
///   1920x1080@[60.000000 60.000000]fps
/// ```
/// 每行形如 `  宽x高@[min max]fps`（`%f`，6 位小数）。判定用的是 **maxFrameRate**，故回退值
/// 取 max；`"60.000000"` 归一成 `"60"` 再交回 ffmpeg。返回顺序 = ffmpeg 打印顺序（= 设备
/// mode 顺序），首个即首选。
pub fn parse_supported_framerates(stderr: &str) -> Vec<String> {
    stderr
        .lines()
        .filter_map(|line| {
            let range = line.split_once("@[")?.1.split_once("]fps")?.0;
            let max = range.split_whitespace().nth(1)?.parse::<f64>().ok()?;
            Some(format!("{max}"))
        })
        .collect()
}

/// 跑一次抓帧（含「先删旧文件」——保证 `bytes > 0` 只可能来自本次尝试）。
fn run_camera_probe_once(
    ffmpeg: &std::path::Path,
    index: &str,
    out: &str,
    framerate: &str,
) -> Result<ProbeAttempt, String> {
    let _ = std::fs::remove_file(out);
    let output = std::process::Command::new(ffmpeg)
        .args(camera_probe_args(index, out, framerate))
        .output()
        .map_err(|e| format!("启动 ffmpeg 失败：{e}"))?;
    let bytes = std::fs::metadata(out).map(|m| m.len()).unwrap_or(0);
    Ok(ProbeAttempt {
        framerate: framerate.to_string(),
        exit: output.status.code(),
        bytes,
        ok: camera_probe_ok(output.status.success(), bytes),
        stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
    })
}

/// 判定文案（纯函数，便于单测覆盖三条分支——防「分支永远走不到」这类回归：
/// 上一版就写了 `parent_state == "Restricted"`，而当时 `PermState` 根本没有该状态）。
fn camera_verdict(parent_state: &str, ok: bool) -> &'static str {
    if ok {
        return "✅ 抓到一帧：ABB 派生进程能拿到相机 → **TCC 继承成立**（#305 方向可行）。\n";
    }
    if parent_state == "Denied" || parent_state == "Restricted" {
        return "⚠️ 本次实验**无效**：ABB 的相机授权已是被拒/受限状态（不会弹框，也必然抓不到帧）。\n\
                先重置再重跑：`tccutil reset Camera com.sqb.abb`，然后重新执行本命令。\n";
    }
    "⚠️ 没抓到帧。请回看刚才是否弹过系统授权框：\n\
     - 弹框且归属方写着 **ABB** → 继承成立（点允许后重跑即出图）；\n\
     - 弹框写着 Terminal / iTerm / 其它 → 本次无效（必须用 `open -n -a ABB.app` 起）；\n\
     - 父进程相机态是 NotDetermined 且确实没弹框 → **继承不成立**，#305 需改方向。\n"
}

/// 判定：只有 ffmpeg **退出成功**且**确实产出了非空文件**才算成功。
/// （抓帧前由 [`camera_probe`] 先删旧文件，所以 `size > 0` 一定来自本次——不会拿残留文件
/// 冒充成功。）
fn camera_probe_ok(status_ok: bool, size: u64) -> bool {
    status_ok && size > 0
}

/// 报告落盘路径：`~/.agent-bridge/logs/camera-probe.log`。
///
/// **必须落盘**：用 `open -n -a ABB.app` 起的实例，stdout/stderr 都指向 `/dev/null`
/// （`src/platform.rs` / `src/ui.rs` 都有同样的已知事实），只 println 等于什么都没留下。
pub fn camera_probe_log_path() -> std::path::PathBuf {
    crate::bridge_dir().join("logs").join("camera-probe.log")
}

/// #305 Step 0：**在 ABB 自己的进程名下**派生 `ffmpeg` 抓一帧，据此判定 TCC 继承。
///
/// 关键用法（裸二进制从终端跑会把 TCC 归属算到终端，实验就无效）：
/// ```text
/// open -n -a /Applications/ABB.app --args --camera-probe 0 /tmp/abb-camera-probe.jpg
/// ```
/// `open -n` 让 LaunchServices 新起一个 ABB 实例（responsible process = ABB.app），
/// 该实例再派 `ffmpeg` 子进程 —— 正是 #305 要验证的那条链。
///
/// 返回 `(报告, 是否成功)`：报告同时写到 [`camera_probe_log_path`]（因上述 /dev/null），
/// 调用方按 bool 决定退出码。
pub fn camera_probe(index: &str, out: &str) -> Result<(String, bool), String> {
    let ffmpeg = crate::deps::find_in_path("ffmpeg")
        .ok_or_else(|| "找不到 ffmpeg（请先安装，如 brew install ffmpeg）".to_string())?;
    camera_probe_with_ffmpeg(&ffmpeg, index, out, &camera_probe_log_path())
}

/// [`camera_probe`] 的实现主体：ffmpeg 路径与报告路径可注入，单测能用 stub ffmpeg 走完整
/// 调用链，验证「帧率不被设备支持 → 按设备能力重试」这条回退真的跑通。
fn camera_probe_with_ffmpeg(
    ffmpeg: &std::path::Path,
    index: &str,
    out: &str,
    log: &std::path::Path,
) -> Result<(String, bool), String> {
    // **先看父进程（= ABB 自己）当前的相机 TCC 状态**：结论必须先看它。
    // 若已是 Denied/Restricted，则「不弹框也不出图」只说明授权记录陈旧，
    // **不能**据此判「继承不成立、#305 要改方向」——那是误判（同 LESSON 里
    // 设置面板/历史记录与当前 code identity 不一致的坑）。
    let parent_state = crate::deps::detect_permissions()
        .into_iter()
        .find(|p| p.id == "camera")
        .map(|p| format!("{:?}", p.state))
        .unwrap_or_else(|| "Unknown".to_string());

    let mut attempts = vec![run_camera_probe_once(
        ffmpeg,
        index,
        out,
        CAMERA_PROBE_FRAMERATE,
    )?];
    // 帧率不被设备支持时（实测：只支持 `1920x1080@60` 的机型上固定 30 必然失败），按 ffmpeg
    // 报出的设备能力重试一次；仍失败才判「抓不到帧」。
    if !attempts[0].ok {
        if let Some(fps) = parse_supported_framerates(&attempts[0].stderr)
            .into_iter()
            .find(|fps| fps != CAMERA_PROBE_FRAMERATE)
        {
            attempts.push(run_camera_probe_once(ffmpeg, index, out, &fps)?);
        }
    }
    let last = attempts
        .last()
        .expect("attempts 至少一条（上面刚 push 过）");
    let ok = last.ok;

    let mut r = String::new();
    r.push_str("# ABB camera-probe（#305 Step 0）\n");
    r.push_str(&format!("index        = {index}\n"));
    r.push_str(&format!("out          = {out}\n"));
    r.push_str(&format!(
        "父进程相机态 = {parent_state}（这是 ABB 自己的 TCC 状态）\n"
    ));
    r.push_str(&format!("ffmpeg       = {}\n", ffmpeg.display()));
    let attempt_lines: Vec<String> = attempts
        .iter()
        .map(|a| format!("帧率 {}（exit {:?} / {}B）", a.framerate, a.exit, a.bytes))
        .collect();
    r.push_str(&format!("尝试         = {}\n", attempt_lines.join(" → ")));
    r.push_str(&format!("exit         = {:?}\n", last.exit));
    r.push_str(&format!("bytes        = {}\n", last.bytes));
    if let Some(a) = attempts.iter().rev().find(|a| !a.stderr.is_empty()) {
        r.push_str(&format!(
            "ffmpeg stderr（帧率 {}）:\n{}\n",
            a.framerate, a.stderr
        ));
    }
    r.push_str("\n## 判定\n");
    r.push_str(camera_verdict(&parent_state, ok));
    if ok {
        r.push_str(&format!("（本次使用的父进程相机态：{parent_state}）\n"));
    }
    if !ok && attempts.len() > 1 {
        r.push_str("（已按设备报告的可用帧率重试过，仍抓不到帧 → 不是帧率不匹配的问题）\n");
    }
    if let Some(dir) = log.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    // 该文件是规定调用方式下**唯一**的交付通道（open -n 起的实例 stdout/stderr 都是
    // /dev/null），写失败必须显式失败，不能静默。
    if let Err(e) = std::fs::write(log, &r) {
        // 写盘失败 ≠ 抓帧失败：图可能已经成功落在 out 了，错误串必须带上结论与图片路径，
        // 否则调用方会误读成「实验失败」。同时把报告打到 stdout，保住终端直跑
        // （非 `open -n`）时仍能看到完整判定。
        print!("{r}");
        return Err(format!(
            "报告写入失败（{}）：{e}；本次抓帧 ok={ok}，图见 {out}",
            log.display()
        ));
    }
    r.push_str(&format!("\n（报告已写入 {}\n）", log.display()));
    Ok((r, ok))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// #305 Step 0：钉死探测用的 ffmpeg 参数串——设备类型、帧数与输出路径任一被改坏，
    /// 探测就会变成「跑了个别的命令却以为验过了」。刻意**不含 `-y`**（诊断命令不该静默
    /// 覆盖调用方指定路径；抓帧前由 camera_probe 先删旧文件）。
    #[test]
    fn camera_probe_args_pins_avfoundation_single_frame() {
        let a = camera_probe_args("0", "/tmp/x.jpg", "30");
        assert_eq!(
            a,
            vec![
                "-hide_banner",
                "-loglevel",
                "error",
                "-f",
                "avfoundation",
                "-framerate",
                "30",
                "-i",
                "0",
                "-frames:v",
                "1",
                "/tmp/x.jpg",
            ]
        );
        // 设备串支持 avfoundation 的 `视频[:音频]` 形态，必须原样透传给 -i
        let b = camera_probe_args("1:none", "/tmp/y.jpg", "60");
        let i = b.iter().position(|x| x == "-i").unwrap();
        assert_eq!(b[i + 1], "1:none");
        assert!(
            b.contains(&"/tmp/y.jpg".to_string()),
            "输出路径必须原样透传"
        );
        // 帧率必须原样落到 `-framerate`（回退值就是从这条通道传下去的）
        let j = b.iter().position(|x| x == "-framerate").unwrap();
        assert_eq!(b[j + 1], "60");
    }

    /// #305 Step 0：判定三条分支必须都能走到（上一版 `Restricted` 是死代码：当时
    /// `PermState` 没有该状态、`av_state` 把 AVAuthorizationStatus=1 折成了 NotDetermined）。
    #[test]
    fn camera_verdict_covers_all_cases() {
        assert!(camera_verdict("Granted", true).contains("继承成立"));
        assert!(camera_verdict("NotDetermined", true).contains("继承成立"));
        for st in ["Denied", "Restricted"] {
            let v = camera_verdict(st, false);
            assert!(v.contains("本次实验**无效**"), "{st} → {v}");
            assert!(v.contains("tccutil reset Camera"), "{st} 应给出重置指引");
        }
        let v = camera_verdict("NotDetermined", false);
        assert!(v.contains("继承不成立"), "未授权且无弹框才判继承不成立");
        assert!(
            !v.contains("本次实验**无效**"),
            "NotDetermined 不该被说成无效实验"
        );
    }

    /// #305 Step 0：成功判定必须「退出码绿 **且** 有非空产出」——防旧文件残留/空文件冒充成功。
    #[test]
    fn camera_probe_ok_requires_success_and_nonempty_file() {
        assert!(camera_probe_ok(true, 1));
        assert!(!camera_probe_ok(true, 0), "空文件不算成功");
        assert!(
            !camera_probe_ok(false, 1024),
            "ffmpeg 失败不算成功（哪怕有残留文件）"
        );
        assert!(!camera_probe_ok(false, 0));
    }

    /// 帧率回退的数据源：**本机真实 stderr**（只支持 `1920x1080@60` 的摄像头，2026-10-09
    /// 实测原文）。拿现场原文当 fixture，免得解析器只对「想象中的格式」成立。
    const REAL_FRAMERATE_STDERR: &str = "\
Selected framerate (30.000000) is not supported by the device.
Supported modes:
  1920x1080@[60.000000 60.000000]fps
[in#0 @ 0x9906c14000] Error opening input: Input/output error
Error opening input file 0.
";

    #[test]
    fn parse_supported_framerates_picks_max_of_each_mode() {
        assert_eq!(
            parse_supported_framerates(REAL_FRAMERATE_STDERR),
            vec!["60"]
        );
        // min≠max 取 max（ffmpeg 匹配用的是 maxFrameRate）；多 mode 保持打印顺序
        let multi = "\
Supported modes:
  640x480@[15.000000 30.000000]fps
  1920x1080@[29.970000 29.970000]fps
";
        assert_eq!(parse_supported_framerates(multi), vec!["30", "29.97"]);
        // 没有 modes 列表（或输出被裁剪）时不得凭空造出帧率
        assert!(parse_supported_framerates("Input/output error\n").is_empty());
        assert!(parse_supported_framerates("").is_empty());
    }

    /// 造一个 stub ffmpeg：把每次调用的参数追加到 `<dir>/calls.txt`；`-framerate` 不是 30
    /// 且 `always_fail=false` 时写一个非空假图并退出 0，否则按 `modes` 打印
    /// `Selected framerate ... Supported modes:` 后退 1。
    fn write_stub_ffmpeg(
        dir: &std::path::Path,
        modes: &str,
        always_fail: bool,
    ) -> std::path::PathBuf {
        let stub = dir.join("ffmpeg");
        let script = format!(
            r#"#!/bin/sh
printf '%s\n' "$*" >> "{calls}"
fps=""
prev=""
for a in "$@"; do
  if [ "$prev" = "-framerate" ]; then fps="$a"; fi
  prev="$a"
done
last=""
for a in "$@"; do last="$a"; done
if [ "$fps" != "30" ] && [ "{always_fail}" != "1" ]; then
  printf 'fake-jpeg' > "$last"
  exit 0
fi
printf 'Selected framerate (%s) is not supported by the device.\n' "$fps" >&2
printf 'Supported modes:\n{modes}' >&2
exit 1
"#,
            calls = dir.join("calls.txt").display(),
            always_fail = u8::from(always_fail),
            modes = modes,
        );
        std::fs::write(&stub, script).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();
        stub
    }

    /// 走 [`camera_probe_with_ffmpeg`] 完整调用链（stub ffmpeg，不用真相机）：
    /// ① 30 不被支持 → 用 modes 里的 60 重试并成功；② 无 modes 可回退 → 只跑一次；
    /// ③ 回退后仍失败 → 结论为「抓不到帧」且报告点明「不是帧率问题」。
    #[test]
    fn camera_probe_retries_with_device_framerate() {
        let dir = std::env::temp_dir().join(format!("abb-camprobe-stub-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let out = dir.join("frame.jpg");
        let log = dir.join("probe.log");
        let calls = |dir: &std::path::Path| -> Vec<String> {
            std::fs::read_to_string(dir.join("calls.txt"))
                .unwrap_or_default()
                .lines()
                .map(str::to_string)
                .collect()
        };

        // ① 本机真实情形：只支持 60fps → 回退后抓到帧
        let stub = write_stub_ffmpeg(&dir, "  1920x1080@[60.000000 60.000000]fps\n", false);
        let (report, ok) =
            camera_probe_with_ffmpeg(&stub, "0", out.to_str().unwrap(), &log).unwrap();
        assert!(ok, "回退到设备帧率后应成功：\n{report}");
        let c = calls(&dir);
        assert_eq!(c.len(), 2, "应恰好两次尝试：{c:?}");
        assert!(c[0].contains("-framerate 30"), "{:?}", c[0]);
        assert!(c[1].contains("-framerate 60"), "{:?}", c[1]);
        assert!(report.contains("帧率 60"), "{report}");
        assert!(log.exists(), "报告必须落盘");

        // ② 没有 Supported modes 可回退 → 只跑一次（不引入无谓的第二次 ffmpeg）
        let _ = std::fs::remove_file(dir.join("calls.txt"));
        let stub = write_stub_ffmpeg(&dir, "", false);
        let (report, ok) =
            camera_probe_with_ffmpeg(&stub, "0", out.to_str().unwrap(), &log).unwrap();
        assert!(!ok);
        assert_eq!(calls(&dir).len(), 1, "无可用帧率时不该重试");
        assert!(report.contains("尝试         = 帧率 30"), "{report}");

        // ③ 回退后仍失败 → 两次尝试，且报告把「不是帧率问题」说清楚
        let _ = std::fs::remove_file(dir.join("calls.txt"));
        let stub = write_stub_ffmpeg(&dir, "  1920x1080@[60.000000 60.000000]fps\n", true);
        let (report, ok) =
            camera_probe_with_ffmpeg(&stub, "0", out.to_str().unwrap(), &log).unwrap();
        assert!(!ok);
        assert_eq!(calls(&dir).len(), 2);
        assert!(report.contains("不是帧率不匹配的问题"), "{report}");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
