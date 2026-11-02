//! 会话分享的**渲染层**：把 [`ShareTurn`] 序列画成一张长图（PNG）。
//!
//! ## 为什么必须在子进程里跑（不是「实现方便」）
//!
//! 这里用的是 `MinimalSoftwareWindow`（Slint 的纯内存窗口适配器），它需要先
//! `slint::platform::set_platform(…)`。而 **Slint 的平台是每线程一次**：
//!
//! - GUI 进程里那条线程的平台已经被 winit 占住（`run_gui` 装的），再装一次会失败；
//! - 所以分享按钮不能「在 GUI 线程里就地渲一张图」。
//!
//! 于是架构是：GUI 点按钮 → spawn 自身子命令（`syschat-share`）→ 子进程装软件后端渲染 →
//! 返回图片路径 → 主进程把它接进待发附件。本模块就是**子进程那一侧**。
//! （同一个理由在 `ui_headless_render_test.rs` 里也有：它在 `cargo test` 的独立进程里装平台。）
//!
//! ## 只出消息区
//!
//! 分享图不含标题栏、状态行、用量条、输入区、分割条与把手。做法是**先按内容真实高度撑开窗口**，
//! 再按 `.slint` 自报的 `dbg-convo-zone-*` 把消息区裁出来——不是猜几何，也不是截屏可视区。
//!
//! ## 尺寸上限是显式的
//!
//! 像素高有硬上限（见 [`MAX_HEIGHT`]）：超了**不静默丢内容**，而是把 `truncated` 标出来，
//! 由调用方在图末/提示里说明。理由与卡里的要求一致：宁可少画并写明，不能让人以为发全了。

// S2 只落地渲染层，接线（子命令 + GUI 按钮 + 附件通路）在 S3。
// 移除这行 allow 的条件是**真的接上了**，而不是「先关警告」。
#![allow(dead_code)]

use super::*;
use crate::syschat_share::ShareTurn;
use slint::platform::software_renderer::{
    MinimalSoftwareWindow, PremultipliedRgbaColor, RepaintBufferType,
};
use slint::platform::{Platform, PlatformError, WindowAdapter};
use std::path::{Path, PathBuf};
use std::rc::Rc;

/// 分享图宽度（逻辑 px）。取与对话框 `preferred-width` 同宽，块内边距/字号口径才与窗口一致。
pub(crate) const SHARE_WIDTH: u32 = crate::syschat::SYSCHAT_PREFERRED_WIDTH as u32;

/// 分享图像素高上限。
///
/// 取值理由（不是拍的）：`image` 编码 + Slint 软件位图都要按 w×h 分配，
/// 520×20000 ≈ 1040 万像素 ≈ 41 MB RGBA，已接近「一次会话能忍受的等待」；
/// 再大就分片，而分片会让「一张长图」这个需求本身失效。**超限要标 truncated**。
pub(crate) const MAX_HEIGHT: u32 = 20000;

/// 渲染结果。
#[derive(Debug, Clone)]
pub(crate) struct ShareRender {
    pub path: PathBuf,
    pub width: u32,
    pub height: u32,
    /// 内容真实高度（未截断时 == `height`）。
    pub content_height: u32,
    /// 是否因 [`MAX_HEIGHT`] 被截断。
    pub truncated: bool,
}

/// 平台适配器：只发内存窗口，不建系统窗口。
struct SharePlatform {
    window: Rc<std::cell::RefCell<Option<Rc<MinimalSoftwareWindow>>>>,
}

impl Platform for SharePlatform {
    fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, PlatformError> {
        let w = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
        *self.window.borrow_mut() = Some(w.clone());
        Ok(w)
    }
}

/// 把分享块铺成界面用的块行序列（**复用生产构造点** `ui::role_row` / `ui::block_row`）。
///
/// 复用而不是另写一份，是为了让公式/表格/图表/代码块走**界面同一条**渲染分支——
/// 另写一份必然漂移，而分享图与窗口不一致正是这张卡要避免的。
fn rows_of(share: &[ShareTurn], flow_w: f32) -> Vec<SysChatBlockRow> {
    let mut rows = Vec::new();
    for t in share {
        let assistant = t.is_assistant();
        // **生产构造点**：标签与靠边口径与 `rows_for_w` 逐字一致。
        let mut role = role_row(if assistant { "怹" } else { "你" }, assistant);
        role.align_right = !assistant;
        rows.push(role);
        // **必须走 `push_block_rows_w`，不能逐块 `block_row_w`**：表格在界面里是
        // 「连续多个 `kind == "table"` 块合成一行」（`table_row(&blocks[i..j])`），
        // 逐块调用会把表格拆散——实测表现为**表格整块变成空白**（第一版就踩了）。
        push_block_rows_w(&mut rows, t.blocks.clone(), flow_w, !assistant);
    }
    rows
}

/// 正文流宽：与 `apply_syschat_layout` 同一条公式（改布局时只动那个常量）。
fn flow_width(w: u32) -> f32 {
    (w as f32 - crate::syschat::CONVO_BLOCK_TEXT_INSET).max(crate::syschat::CONVO_TEXT_WIDTH_MIN)
}

/// 把一帧画成 RGBA（alpha 预乘 → 直通，与界面出图台同一套还原）。
fn into_image(buf: &[PremultipliedRgbaColor], w: u32, h: u32) -> image::RgbaImage {
    let mut img = image::RgbaImage::new(w, h);
    for y in 0..h {
        for x in 0..w {
            let p = buf[(y * w + x) as usize];
            let add = 255u32 - p.alpha as u32;
            img.put_pixel(
                x,
                y,
                image::Rgba([
                    (p.red as u32 + add).min(255) as u8,
                    (p.green as u32 + add).min(255) as u8,
                    (p.blue as u32 + add).min(255) as u8,
                    255,
                ]),
            );
        }
    }
    img
}

/// 渲染一张分享长图。
///
/// **调用前必须确保本线程没装过 Slint 平台**（正常路径 = 在独立子进程里调）。
pub(crate) fn render_share(share: &[ShareTurn], out: &Path) -> Result<ShareRender, String> {
    if crate::syschat_share::is_empty(share) {
        return Err("这个会话没有可分享的内容".to_string());
    }
    let slot: Rc<std::cell::RefCell<Option<Rc<MinimalSoftwareWindow>>>> =
        Rc::new(std::cell::RefCell::new(None));
    slint::platform::set_platform(Box::new(SharePlatform {
        window: slot.clone(),
    }))
    .map_err(|e| format!("装渲染后端失败（本进程可能已有 GUI，分享须在子进程里跑）：{e}"))?;

    let ui = SystemChatWindow::new().map_err(|e| format!("建窗失败：{e}"))?;
    let rows = rows_of(share, flow_width(SHARE_WIDTH));
    ui.set_blocks(slint::ModelRc::from(Rc::new(slint::VecModel::from(rows))));
    // 分享图不需要可交互的分割条/面板：它们的接线只影响拖拽，不影响几何。
    ui.show().map_err(|e| format!("show 失败：{e}"))?;
    let win = slot
        .borrow()
        .clone()
        .ok_or_else(|| "平台没有发出窗口".to_string())?;

    let w = SHARE_WIDTH;
    // 第一趟：在**固定参考高度** H0 上量三个量。
    //
    // 为什么固定 H0 而不是直接用量出来的高度：消息区会随窗高伸缩，用
    // 「MAX_HEIGHT − convo_h」反推 chrome 是**自成循环**的（实测把 truncated 误判成 true）。
    // 在 H0 上量一次得到「上 chrome」与「下 chrome」，两者都与内容高度无关：
    //   need_h = convo_y + 内容真实高度 + 下 chrome
    // 参考高度取上限：**必须给足**，否则 `flick`（消息区）高不够，内容会被裁掉。
    // 这里踩过一次：小样本（783px）看不出来，30 轮（16000px）时后面 15000px 全是空白。
    let h_ref = MAX_HEIGHT;
    ui.window().set_size(slint::PhysicalSize::new(w, h_ref));
    apply_syschat_layout(&ui, w as f32);
    // 消息区（`flick`）的高度由 Rust 写（`root.convo-h`）；默认只有 326px。
    // **必须撑到可用空间**，否则超过 326px 的内容一律画不出来。
    ui.set_convo_h(h_ref as f32);
    let _ = frame(&win, w, h_ref); // 让布局跑完一轮
    let content_h = ui.get_dbg_col_h();
    if content_h <= 0.0 {
        return Err("布局报出的内容高度为 0，无法出图".to_string());
    }
    // 消息区 = `flick`（**不是** `convo-zone`——那名字指的是输入区 `composer-zone`，
    // 第一版就踩了这个坑：裁出来的是输入框）。`dbg-flick-y` 是它在窗口里的绝对 y，
    // `dbg-col-h` 是内容真实高度（含 col 的 12px 上下 padding）。
    let convo_y = ui.get_dbg_flick_y().max(0.0);
    let convo_h = ui.get_dbg_flick_h().max(0.0);
    let bottom_chrome = (h_ref as f32 - convo_y - convo_h).max(0.0);
    let need_h = (convo_y + content_h + bottom_chrome).ceil() as u32;
    let final_h = need_h.clamp(1, MAX_HEIGHT);
    let truncated = need_h > MAX_HEIGHT;

    // 第二趟：按确定的高度重排一次，出图。
    ui.window().set_size(slint::PhysicalSize::new(w, final_h));
    apply_syschat_layout(&ui, w as f32);
    ui.set_convo_h(final_h as f32);
    let f = frame(&win, w, final_h);

    // 裁出消息区：y 从 convo_y 开始，高 = min(内容高, 可用高)。
    let crop_y = convo_y.round().max(0.0) as u32;
    let avail = final_h.saturating_sub(crop_y);
    let crop_h = (content_h.ceil() as u32).min(avail).max(1);
    let full = into_image(&f, w, final_h);
    let cropped =
        image::imageops::crop_imm(&full, 0, crop_y.min(final_h.saturating_sub(1)), w, crop_h)
            .to_image();

    if let Some(dir) = out.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("建目录失败：{e}"))?;
    }
    cropped.save(out).map_err(|e| format!("写 PNG 失败：{e}"))?;

    Ok(ShareRender {
        path: out.to_path_buf(),
        width: w,
        height: crop_h,
        content_height: content_h.ceil() as u32,
        truncated,
    })
}

/// 画一帧（含两次 `update_timers_and_animations`：Slint 的布局/夹取有延迟拍）。
fn frame(win: &Rc<MinimalSoftwareWindow>, w: u32, h: u32) -> Vec<PremultipliedRgbaColor> {
    let mut buf = vec![PremultipliedRgbaColor::default(); (w * h) as usize];
    for _ in 0..3 {
        slint::platform::update_timers_and_animations();
        win.request_redraw();
        win.draw_if_needed(|r| {
            r.render(&mut buf, w as usize);
        });
    }
    buf
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::syschat::Turn;

    /// 把 `AGENT_BRIDGE_HOME` 临时指到一个 temp 目录。
    ///
    /// **为什么必须**：公式渲染（`mathrender::typeset`）会往 `<bridge>/syschat/math-cache/`
    /// 写 SVG/META。全量门禁的隔离守卫不允许测试写运行数据——实测它抓到过一次
    /// （`math-cache/*.svg` 落在隔离 HOME 里）。出图台那边是靠 `#[ignore]` 规避的，
    /// 但分享的这几条**必须常跑**，所以只能显式换根。
    struct TempHome {
        _lock: std::sync::MutexGuard<'static, ()>,
        prev: Option<std::ffi::OsString>,
        dir: std::path::PathBuf,
    }

    fn probe_env_lock() -> std::sync::MutexGuard<'static, ()> {
        use std::sync::{Mutex, OnceLock};
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    impl TempHome {
        fn new(tag: &str) -> Self {
            let lock = probe_env_lock();
            let dir =
                std::env::temp_dir().join(format!("abb-share-home-{tag}-{}", std::process::id()));
            let _ = std::fs::create_dir_all(&dir);
            let prev = std::env::var_os("AGENT_BRIDGE_HOME");
            std::env::set_var("AGENT_BRIDGE_HOME", &dir);
            Self {
                _lock: lock,
                prev,
                dir,
            }
        }
    }

    impl Drop for TempHome {
        fn drop(&mut self) {
            match self.prev.take() {
                Some(v) => std::env::set_var("AGENT_BRIDGE_HOME", v),
                None => std::env::remove_var("AGENT_BRIDGE_HOME"),
            }
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn turn(role: &str, text: &str) -> Turn {
        Turn {
            role: role.into(),
            text: text.into(),
            thought: String::new(),
            ts: 0,
        }
    }

    #[test]
    fn empty_session_is_rejected_instead_of_writing_a_blank_png() {
        let dir = std::env::temp_dir().join(format!("abb-share-empty-{}", std::process::id()));
        let out = dir.join("x.png");
        let err = render_share(&[], &out).unwrap_err();
        assert!(err.contains("没有可分享"), "报因要能讲给用户听：{err}");
        assert!(!out.exists(), "拒绝时不该留下文件");
    }

    #[test]
    fn share_width_is_the_dialog_width_so_blocks_lay_out_identically() {
        // 分享图与窗口同宽，块内边距/字号口径才一致（不一致会让公式/表格换行位置不同）。
        assert_eq!(SHARE_WIDTH, crate::syschat::SYSCHAT_PREFERRED_WIDTH as u32);
    }

    /// 真的出一张图。
    ///
    /// **这条是承重的**：上面几条只测纯逻辑，「渲染管线能不能跑通」一条都没验。
    /// 本用例在测试进程自己的线程里装软件后端（`set_platform` 是每线程一次，
    /// cargo 每个用例各占一条线程，所以不冲突），跑完整管线并断言产物形状。
    #[test]
    fn renders_a_real_long_png_with_the_expected_geometry() {
        let _home = TempHome::new("render");
        let dir = std::env::temp_dir().join(format!("abb-share-render-{}", std::process::id()));
        let out = dir.join("share.png");
        // 造一段够长的会话：多轮 + 代码块 + 表格 + 块级公式，
        // 覆盖「长图」与「公式/表格要走同一条管线」两件事。
        let md = "### 小节\n\n| 列 | 值 |\n|:--|--:|\n| a | 1 |\n\n```rust\nlet x = 1;\n```\n\n$$\nE = mc^2\n$$\n";
        let turns: Vec<Turn> = (0..12)
            .map(|i| turn(if i % 2 == 0 { "user" } else { "assistant" }, md))
            .collect();
        let share = crate::syschat_share::share_blocks(&turns);
        let r = render_share(&share, &out).expect("分享出图失败");

        assert!(out.exists(), "没写出文件：{}", out.display());
        assert_eq!(r.width, SHARE_WIDTH);
        assert!(r.height > 0, "高度为 0");
        assert!(
            !r.truncated,
            "12 轮不该超过上限（实际 {}）",
            r.content_height
        );
        // 图的实际像素与报告一致（报告不实比没报告更坏）
        let img = image::open(&out).expect("解不开自己写的 PNG");
        assert_eq!(img.width(), r.width);
        assert_eq!(img.height(), r.height);
        // **内容确实被画进去了**：全白/全透明说明渲染器没干活。
        let mut non_blank = 0u32;
        for p in img.to_rgba8().pixels() {
            if p.0[0] < 200 || p.0[1] < 200 || p.0[2] < 200 {
                non_blank += 1;
            }
        }
        assert!(
            non_blank > 500,
            "出图像素几乎全白（non_blank={non_blank}）⇒ 内容没被画进去"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 表格必须整块出现在分享里。
    ///
    /// **回归守卫**：界面里表格是「连续多个 `kind == "table"` 块合成一行」，
    /// 逐块渲染会把表格拆散——实测表现为**表格整块空白**（第一版踩过）。
    /// 这里按"表格行的单元格数量"判，而不是"有没有 table 块"。
    #[test]
    fn table_becomes_one_row_with_all_cells() {
        let md = "| 列A | 列B |\n|:---|--:|\n| 左 | 右 |\n| 1 | 2 |";
        let share = crate::syschat_share::share_blocks(&[turn("assistant", md)]);
        let rows = rows_of(&share, flow_width(SHARE_WIDTH));
        let table_rows: Vec<_> = rows.iter().filter(|r| r.kind == "table").collect();
        assert_eq!(
            table_rows.len(),
            1,
            "连续 table 块必须合成**一行**，实际 {} 行",
            table_rows.len()
        );
        // 单元格挂在 `table_rows[].cells`（不是行的 `cells` 字段——那是给别的 kind 用的）。
        let trs = &table_rows[0].table_rows;
        assert!(
            trs.row_count() >= 3,
            "表头 + 2 数据行 至少 3 行，实际 {}",
            trs.row_count()
        );
        let n_cells: usize = (0..trs.row_count())
            .filter_map(|i| trs.row_data(i))
            .map(|r| r.cells.row_count())
            .sum();
        assert!(
            n_cells >= 6,
            "表头+2 行 × 2 列 至少 6 个单元格，实际 {n_cells}"
        );
    }

    /// 消息区必须被撑到内容高。
    ///
    /// **回归守卫**：`.slint` 里 `convo-h` 默认只有 326px（默认窗 520×560 的比例），
    /// 不显式撑开的话内容会被截在 326px 内——实测 12 轮只画出前两轮（第一版踩过）。
    /// 判据：长会话渲染出的图高必须远大于默认 convo-h。
    #[test]
    fn long_session_is_not_clipped_to_the_default_convo_height() {
        let _home = TempHome::new("tall");
        let dir = std::env::temp_dir().join(format!("abb-share-tall-{}", std::process::id()));
        let out = dir.join("tall.png");
        let md = "### 小节\n\n一段正文。\n\n```\ncode\n```\n";
        let turns: Vec<Turn> = (0..14).map(|_| turn("assistant", md)).collect();
        let share = crate::syschat_share::share_blocks(&turns);
        let r = render_share(&share, &out).expect("出图失败");
        // **判据必须落到像素上，不能只看报告的高度**：
        // `col-h` 报的是布局高，不撑开 `convo-h` 时它照样很大——第一版用 `r.height > 2000`
        // 做守卫，变异实测（删掉 set_convo_h）**逃逸**了，因为图高是按 `col-h` 算的、
        // 但**画出来的像素被 flick 的可视高度夹在 326px**。
        // 所以判据改成：图的下半部分必须有墨迹（被截断的话下半张是全白）。
        let img = image::open(&out).expect("解不开 PNG").to_rgba8();
        let (w, h) = (img.width(), img.height());
        let ink_in = |y0: u32, y1: u32| -> u32 {
            let mut n = 0;
            for y in y0..y1.min(h) {
                for x in 0..w {
                    let p = img.get_pixel(x, y).0;
                    if p[0] < 200 || p[1] < 200 || p[2] < 200 {
                        n += 1;
                    }
                }
            }
            n
        };
        let lower = ink_in(h / 2, h);
        assert!(
            lower > 200,
            "图的**下半张**几乎全白（{lower} 个墨点）⇒ 内容被夹在可视高度内了（14 轮={}px）",
            r.height
        );
        assert!(!r.truncated, "14 轮不该触发上限");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 裁出来的是**消息区**，不是输入区。
    ///
    /// **回归守卫（M3 变异实测逃逸过一次）**：`.slint` 里 `convo-zone` 那个名字指的是
    /// **输入区**（`composer-zone`），用它会裁出一张只有输入框、正文全丢的图。
    /// 第一版就是按名字猜的。
    ///
    /// 判据必须落到**内容**上：光数墨点不行——输入框自己也有边框和按钮，墨点够多。
    /// 这里用「长会话的下半部分要有墨迹」+「顶部不能是输入框的占位文案」两条一起判。
    #[test]
    fn crops_the_message_area_not_the_composer() {
        let _home = TempHome::new("crop");
        let dir = std::env::temp_dir().join(format!("abb-share-crop-{}", std::process::id()));
        let out = dir.join("crop.png");
        let md = "### 小节\n\n一段正文。\n\n```\ncode\n```\n";
        let turns: Vec<Turn> = (0..14).map(|_| turn("assistant", md)).collect();
        let share = crate::syschat_share::share_blocks(&turns);
        let r = render_share(&share, &out).expect("出图失败");
        let img = image::open(&out).expect("解不开 PNG").to_rgba8();
        let (w, h) = (img.width(), img.height());

        // ① 14 轮 ≈ 2600px；若裁成了输入区，高度会掉到 ~780px。
        assert!(
            h > 1500,
            "图只有 {h}px 高 ⇒ 裁到输入区了（14 轮内容约 2600px）"
        );
        // ② 顶部必须是正文/角色行，不能有输入框的占位文案区域特征：
        //    输入区在 520 宽下有明显的 2px 圆角边框（顶部有长水平线）。
        //    正文区顶部是角色行（短文字），横向没有贯穿整行的深色线。
        let dark_in_row = |y: u32| -> u32 {
            (0..w)
                .filter(|&x| {
                    let p = img.get_pixel(x, y).0;
                    p[0] < 160 && p[1] < 160 && p[2] < 160
                })
                .count() as u32
        };
        // 顶部 40 行里不应出现「几乎贯穿整行」的深色线（= 输入框上边框）。
        let has_border_like_row = (0..40.min(h)).any(|y| dark_in_row(y) > w / 2);
        assert!(
            !has_border_like_row,
            "图顶部有贯穿整行的深色线 ⇒ 裁到输入框了"
        );
        let _ = std::fs::remove_dir_all(&dir);
        let _ = r;
    }

    /// 长会话**必须整段画进图里**，不能只有开头一段有墨迹。
    ///
    /// **回归守卫（真出过一次）**：`flick`（消息区）高由 `root.convo-h` 写，默认只有
    /// 326px。不显式撑开时，**超过 326px 的内容一律画不出来**——30 轮那次的图是
    /// 16602px 高，但第一屏之后 16000px 全是空白。
    ///
    /// 为什么之前的守卫没抓住：小样本（783px）刚好没超过夹取线。**大样本才暴露的问题
    /// 必须用大样本守**，所以这条按 30 轮造。
    #[test]
    fn long_session_has_ink_all_the_way_down() {
        let _home = TempHome::new("inkdown");
        let dir = std::env::temp_dir().join(format!("abb-share-inkdown-{}", std::process::id()));
        let out = dir.join("inkdown.png");
        let md = "## 结论\n\n正文一段。\n\n| a | b |\n|--|--|\n| 1 | 2 |\n";
        let turns: Vec<Turn> = (0..30).map(|_| turn("assistant", md)).collect();
        let share = crate::syschat_share::share_blocks(&turns);
        let r = render_share(&share, &out).expect("出图失败");
        let img = image::open(&out).expect("解不开 PNG").to_rgba8();
        let (w, h) = (img.width(), img.height());
        assert!(
            h > 3000,
            "30 轮每轮约 200px，图高应远超 3000px（默认 convo-h=326px 的 10 倍），实际 {h}"
        );

        // 把图纵向切成 6 段，**每一段都必须有墨迹**。
        // 只查首尾不够：中间空掉同样是"内容丢了"。
        let ink_in = |y0: u32, y1: u32| -> u32 {
            let mut n = 0;
            for y in y0..y1.min(h) {
                for x in 0..w {
                    let p = img.get_pixel(x, y).0;
                    if p[0] < 200 || p[1] < 200 || p[2] < 200 {
                        n += 1;
                    }
                }
            }
            n
        };
        let seg = h / 6;
        for i in 0..6 {
            let n = ink_in(i * seg, (i + 1) * seg);
            assert!(
                n > 100,
                "第 {}/6 段（y {}..{}）只有 {n} 个墨点 ⇒ 该段内容没被画出来",
                i + 1,
                i * seg,
                (i + 1) * seg
            );
        }
        assert!(!r.truncated, "30 轮不该触发上限");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rows_carry_role_identity_not_just_text() {
        // 角色行必须带身份（`role_assistant`），否则分享图里的着色分支与窗口不同。
        let share =
            crate::syschat_share::share_blocks(&[turn("user", "问"), turn("assistant", "答")]);
        let rows = rows_of(&share, flow_width(SHARE_WIDTH));
        assert_eq!(
            rows.len(),
            4,
            "两轮 ⇒ 两条角色行 + 两条正文块：{}",
            rows.len()
        );
        assert!(!rows[0].role_assistant);
        assert!(rows[2].role_assistant);
    }
}
