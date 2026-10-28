//! ✂ 截屏（卡 `abb-v11-screenshot`）：点功能区的 ✂ → 选区/选窗截一张 → 落进待发附件。
//!
//! **为什么走 `screencapture` 子进程**（卡里把「原生 API」列为首选、子进程列为退路）：
//! 交互式**选区/选窗**本身就是这个系统工具的既有能力（`-i`：拖框选区、空格切窗口、Esc 取消），
//! 而手写 CoreGraphics 路径要么只能拍全屏（`CGWindowListCreateImage` 拿不到「用户想截的那块」），
//! 要么得自己再写一套覆盖全屏的选区 UI。TCC 归属与原生路径**同一条责任链**（子进程的
//! 「责任进程」是派它的 app ⇒ 首屏授权框仍记在 ABB 名下），所以「探权限 → 引导」这套照旧。
//!
//! **为什么先探权限再拍**：没有「屏幕录制」授权时，macOS 要么拒绝、要么**静默**把窗口内容
//! 从图里去掉——两种都会让用户拿到一张错图，而错图比报错更难发现。所以未授权时**不拍**，
//! 直接给可读引导（并顺手触发一次系统授权请求，那是唯一的授权入口）。
//!
//! 平台：只实现 macOS（无头/Windows 侧给「不支持」的**可读**回落，不是死按钮）。
//!
//! 判据（本模块单测钉住的都是纯函数）：参数拼装 [`capture_args`] / 结局判定 [`interpret`] /
//! PNG 头校验 [`looks_like_png`] / 引导文案 [`permission_guidance`] / 临时文件名 [`temp_png_path`]。
//! 真正「按下去能截到图」要在真机上肉眼验（合成点击打不进 app，见交接文档 §1）——**未验**。

use std::path::{Path, PathBuf};

/// 功能区那颗钮传给 Rust 的功能名（`ui/app.slint` 的 `composer-todo("截图")`）。
/// 两边逐字一致才能落到截屏分支（有守卫测试钉住）。
pub const ACTION_NAME: &str = "截图";

/// 系统截屏工具。用绝对路径：GUI 进程的 `PATH` 是 launchd 给的（不含 `/usr/sbin`），
/// 写 `screencapture` 会变成「命令找不到」——那是**静默**失败里最难查的一种。
///
/// `cfg(macos)`：它只被 macOS 分支用；非 macOS 留着就是**私有未用项**，会点亮 Windows
/// 门禁的 `dead_code`（`pub` 项不报，私有常量报——本模块其余都是 `pub`，只有它是私有）。
#[cfg(target_os = "macos")]
const SCREENCAPTURE: &str = "/usr/sbin/screencapture";

/// 一次截屏请求的结局。
///
/// `Captured` / `Cancelled` / `NeedsPermission` 只由 macOS 的 [`capture`] 构造；非 macOS 的
/// 回落只会返回 `Failed`。所以非 macOS 下这三个变体是「必然无人构造」，windows clippy
/// `-D warnings` 会以 dead_code 判红（v3.2.4 CI）。用 `cfg_attr` 而不是把变体整块 cfg 掉：
/// 枚举是平台无关的协议面，保留全部变体能让调用方（附件/状态行）在两种平台上写同一套
/// match，少一堆 `cfg` 分支。
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// 截到了图，落在给的临时路径（调用方负责落地进附件目录 / 清理）。
    Captured(PathBuf),
    /// 用户取消（Esc / 右键 / 选了空区域）：**不是错误**，如实说一声就行。
    Cancelled,
    /// 缺「屏幕录制」授权：已经触发过系统请求（弹框/亮设置项），引导文案见
    /// [`permission_guidance`]。
    NeedsPermission,
    /// 真失败（工具不存在 / 非零退出 / 文件不完整）：带可读原因。
    Failed(String),
}

/// `screencapture` 的参数（纯函数）：`-i` 交互式（选区 **或** 选窗，Esc 取消）、
/// `-x` 不播快门声（对话窗里放一声「咔嚓」是噪音），最后是目标文件。
#[cfg(target_os = "macos")]
pub fn capture_args(out: &Path) -> Vec<String> {
    vec![
        "-i".to_string(),
        "-x".to_string(),
        out.to_string_lossy().into_owned(),
    ]
}

/// 临时 PNG 路径。带 pid：同一秒里连点两次（或另一份 ABB 在跑）不会互相覆盖。
#[cfg(target_os = "macos")]
pub fn temp_png_path(dir: &Path, ts: u64, pid: u32) -> PathBuf {
    dir.join(format!("abb-screenshot-{ts}-{pid}.png"))
}

/// PNG 头（`\x89PNG\r\n\x1a\n`）：用来挡住「工具退出码 0 但写了半张图」这种假成功。
#[cfg(target_os = "macos")]
pub fn looks_like_png(bytes: &[u8]) -> bool {
    bytes.len() > 8 && bytes.starts_with(&[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a])
}

/// 判定一次 `screencapture` 调用的结局（纯函数，单测钉住）。
///
/// 口径：**没有文件就不许说成功**。
/// - 文件存在且像 PNG ⇒ `Captured`（用户取消只会留下「没有文件」，不会留下半张图）；
/// - 没有文件：退出码非零且 stderr 里有可读信息 ⇒ `Failed`（把 stderr 原样带出来，
///   不吞掉系统给的原因）；否则 ⇒ `Cancelled`（`screencapture -i` 取消时既可能留空文件
///   也没有明确 stderr，**如实当成取消而不是失败**——它不是错误，报成错误会让用户以为坏了）。
#[cfg(target_os = "macos")]
pub fn interpret(exit_ok: bool, stderr: &str, out: &Path) -> Outcome {
    match std::fs::read(out) {
        Ok(bytes) if looks_like_png(&bytes) => Outcome::Captured(out.to_path_buf()),
        Ok(_) => Outcome::Failed(format!(
            "截屏文件不完整（{}）：没拿到 PNG 数据",
            out.display()
        )),
        Err(_) => {
            let err = stderr.trim();
            if !exit_ok && !err.is_empty() {
                Outcome::Failed(format!("screencapture 退出异常：{err}"))
            } else {
                Outcome::Cancelled
            }
        }
    }
}

/// 缺权限时的引导文案（纯函数）。
///
/// 三件事必须说全，缺一句用户就卡住：**要哪项权限**、**去哪儿开**、**开完要不要重启**
/// （TCC 对已运行进程的授权不是立刻生效的，这是最常见的一句「我明明勾了」）。
#[cfg(target_os = "macos")]
pub fn permission_guidance() -> String {
    "截屏需要「屏幕录制」权限：系统设置 → 隐私与安全性 → 屏幕录制 → 勾选 ABB。\
     （刚勾上时系统常常要重启一次 ABB 才认，勾完再点一次 ✂ 即可。）"
        .to_string()
}

/// 截一张图（平台分派）。成功时图落在系统临时目录，**调用方负责**落地进附件目录并清理。
#[cfg(target_os = "macos")]
pub fn capture() -> Outcome {
    // 先探权限（只探不弹）：没授权就**不拍**——拍出来是张缺窗口的错图，比明说更糟。
    if !crate::permreq::screen_recording_granted() {
        // 顺手触发一次系统授权请求（弹框 / 亮出设置项）：那是唯一的授权入口，
        // 不点它就永远停在「没授权」。
        crate::permreq::request_screen_recording();
        return Outcome::NeedsPermission;
    }
    let out = temp_png_path(
        &std::env::temp_dir(),
        crate::chrono_lite::unix_secs(),
        std::process::id(),
    );
    let args = capture_args(&out);
    let res = std::process::Command::new(SCREENCAPTURE)
        .args(&args)
        .output();
    match res {
        Ok(o) => {
            let stderr = String::from_utf8_lossy(&o.stderr);
            let outcome = interpret(o.status.success(), &stderr, &out);
            if !matches!(outcome, Outcome::Captured(_)) {
                // 取消是常态、不该刷日志，但「非零退出」要留痕：将来真出问题时只剩这条线索。
                if !o.status.success() {
                    crate::log!(
                        "[shot] screencapture 退出码={:?} stderr={}",
                        o.status.code(),
                        stderr.trim()
                    );
                }
            }
            outcome
        }
        Err(e) => Outcome::Failed(format!("跑不起来 {SCREENCAPTURE}：{e}")),
    }
}

/// 非 macOS：没有这条路（Windows 的截屏 API 与 macOS 完全不同，不在本卡范围）。
/// 给**可读**回落而不是死按钮：点下去会在状态行说明原因。
#[cfg(not(target_os = "macos"))]
pub fn capture() -> Outcome {
    Outcome::Failed("截屏目前只有 macOS 实现（本平台暂不支持）。".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 参数口径：交互式选区/选窗 + 静音 + 目标文件在最后（`screencapture` 的位置参数）。
    #[cfg(target_os = "macos")]
    #[test]
    fn capture_args_are_interactive_quiet_and_end_with_the_target() {
        let p = Path::new("/tmp/x.png");
        assert_eq!(
            capture_args(p),
            vec!["-i".to_string(), "-x".to_string(), "/tmp/x.png".to_string()]
        );
        // 必须是绝对路径且落在目标文件上——`-i` 与 `-x` 谁在前无所谓，但目标必须是**它之后**
        // 唯一的位置参数（多一个位置参数 screencapture 会当成第二个输出文件）。
        let args = capture_args(p);
        assert_eq!(args.iter().filter(|a| !a.starts_with('-')).count(), 1);
        assert_eq!(args.last().unwrap(), &"/tmp/x.png".to_string());
    }

    /// 临时文件名带 pid：同一秒连点两次不互相覆盖（否则第二张会覆盖第一张，
    /// 而第一张可能已经被 `land_attachments` 复制走了——看起来是「截了两张只进来一张」）。
    #[cfg(target_os = "macos")]
    #[test]
    fn temp_paths_are_unique_per_second_and_process() {
        let d = Path::new("/tmp");
        assert_eq!(temp_png_path(d, 100, 1), temp_png_path(d, 100, 1));
        assert_ne!(temp_png_path(d, 100, 1), temp_png_path(d, 101, 1));
        assert_ne!(temp_png_path(d, 100, 1), temp_png_path(d, 100, 2));
        assert!(temp_png_path(d, 100, 1).to_string_lossy().ends_with(".png"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn png_sniff_rejects_short_and_foreign_files() {
        assert!(looks_like_png(b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR"));
        assert!(!looks_like_png(b""), "空文件不是 PNG");
        assert!(
            !looks_like_png(b"\x89PNG\r\n\x1a\n"),
            "只有 8 字节头 = 没数据"
        );
        assert!(!looks_like_png(b"not a png at all"));
        assert!(
            !looks_like_png(b"\xff\xd8\xff\xe0jpegjpeg"),
            "JPEG 不算截屏产物"
        );
    }

    /// 结局判定表——**没有文件就不许说成功**（这是本模块最重要的一条判据）。
    #[cfg(target_os = "macos")]
    #[test]
    fn interpret_never_reports_success_without_a_png() {
        let dir = std::env::temp_dir().join(format!("abb-shot-t-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let png = dir.join("ok.png");
        std::fs::write(&png, b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR....").unwrap();
        assert_eq!(interpret(true, "", &png), Outcome::Captured(png.clone()));

        // 有的没有 + 非零退出 + stderr ⇒ 失败，且**把系统原因带出来**（不吞）。
        let missing = dir.join("missing.png");
        match interpret(
            false,
            "screencapture: cannot run two interactive captures",
            &missing,
        ) {
            Outcome::Failed(msg) => {
                assert!(msg.contains("cannot run two interactive captures"), "{msg}");
            }
            other => panic!("非零退出应报失败，实际 {other:?}"),
        }
        // 没有文件 + 退出码 0 ⇒ 取消（用户按了 Esc），**不是**错误。
        assert_eq!(interpret(true, "", &missing), Outcome::Cancelled);
        assert_eq!(
            interpret(false, "", &missing),
            Outcome::Cancelled,
            "没有 stderr 的失败痕迹 ⇒ 按取消处理（不编一个错误原因）"
        );
        // 文件在但不完整 ⇒ 失败（挡住「退出码 0 但写了半张图」的假成功）。
        let half = dir.join("half.png");
        std::fs::write(&half, b"\x89PNG").unwrap();
        assert!(matches!(interpret(true, "", &half), Outcome::Failed(_)));

        let _ = std::fs::remove_file(&png);
        let _ = std::fs::remove_file(&half);
        let _ = std::fs::remove_dir(&dir);
    }

    /// 引导文案三要素：哪项权限 / 去哪儿开 / 开完可能要重启。
    #[cfg(target_os = "macos")]
    #[test]
    fn guidance_names_permission_location_and_restart() {
        let g = permission_guidance();
        for needle in ["屏幕录制", "系统设置", "隐私与安全性", "重启"] {
            assert!(g.contains(needle), "引导文案缺「{needle}」：{g}");
        }
    }

    /// 平台回落：非 macOS 要**可读**（不是空串、也不是死按钮）。
    #[cfg(not(target_os = "macos"))]
    #[test]
    fn non_macos_capture_explains_instead_of_failing_silently() {
        match capture() {
            Outcome::Failed(msg) => assert!(msg.contains("macOS"), "{msg}"),
            other => panic!("非 macOS 应给可读回落，实际 {other:?}"),
        }
    }
}
