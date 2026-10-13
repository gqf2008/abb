//! D2 无头渲染台：把**真实的 `SystemChatWindow`** 用 Slint 的软件渲染后端画进内存位图，
//! 用来对「消息区最后一行被输入区切断」「标题栏控件条压到标题文字」做**可断言**的回归守卫。
//!
//! 为什么可以这么干（不违反「不许跑 dev GUI」）：`MinimalSoftwareWindow` 是纯内存窗口适配器，
//! 不建系统窗口、不跑 `run_gui`，因此不会走到 `platform::heal_autostart()`（那条只在 GUI 启动
//! 路径里，会改写用户真实的 `~/Library/LaunchAgents/`）。本模块只在 `cargo test` 里编译运行，
//! 不写任何文件。Slint 的平台是**按线程**存的（`GLOBAL_CONTEXT` 是 thread_local），所以每个
//! 测试各自装一次软件后端即可。
//!
//! 判据依赖的观测面：`SystemChatWindow` 暴露的 `dbg-*` 只读属性（flick/col/content 的几何）。
//! 这些值在像素里量不出来（内容被裁剪后「真实高度」不可见），所以由窗口自己报出来。

use super::*;
use slint::platform::software_renderer::{
    MinimalSoftwareWindow, PremultipliedRgbaColor, RepaintBufferType,
};
use slint::platform::{Platform, PlatformError, WindowAdapter};

/// 默认窗宽（= `ui/app.slint` 的 `preferred-width`）与两条判据用的窗高。
const W: u32 = 460;
const SHORT_H: u32 = 620; // 消息区放不下一整段 ⇒ 走滚动路径
const TALL_H: u32 = 2600; // 放得下整段内容 ⇒ 量「真实渲染高度」

/// 每次建窗都发一个新的 `MinimalSoftwareWindow`：一个测试里要按不同窗高量两次。
struct HeadlessPlatform {
    windows: Rc<RefCell<Vec<Rc<MinimalSoftwareWindow>>>>,
}

impl Platform for HeadlessPlatform {
    fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, PlatformError> {
        let w = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
        self.windows.borrow_mut().push(w.clone());
        Ok(w)
    }
}

thread_local! {
    static WINDOWS: Rc<RefCell<Vec<Rc<MinimalSoftwareWindow>>>> = Rc::new(RefCell::new(Vec::new()));
}

/// 装后端 + 建窗 + 灌消息 + 定尺寸；返回窗口、平台句柄与 model（供追加/替换）。
fn setup(
    rows: Vec<SysChatBlockRow>,
    h: u32,
) -> (
    SystemChatWindow,
    Rc<MinimalSoftwareWindow>,
    Rc<slint::VecModel<SysChatBlockRow>>,
) {
    let windows = WINDOWS.with(|w| w.clone());
    let _ = slint::platform::set_platform(Box::new(HeadlessPlatform {
        windows: windows.clone(),
    }));
    let ui = SystemChatWindow::new().expect("SystemChatWindow 建窗失败");
    let model: Rc<slint::VecModel<SysChatBlockRow>> = Rc::new(slint::VecModel::from(rows));
    ui.set_blocks(slint::ModelRc::from(model.clone()));
    ui.show().expect("show 失败");
    ui.window().set_size(slint::PhysicalSize::new(W, h));
    let win = windows.borrow().last().expect("平台没发窗口").clone();
    (ui, win, model)
}

/// 一行消息块：与生产 `ui.rs::block_row` 同口径的最小构造（kind 驱动渲染分支）。
fn row(kind: &str, text: &str) -> SysChatBlockRow {
    SysChatBlockRow {
        kind: kind.into(),
        text: text.into(),
        styled: crate::mdblocks::styled(text),
        level: 0,
        order: 0,
        checked: false,
        marker: if kind == "role" {
            "".into()
        } else {
            "•".into()
        },
        lang: if kind == "code" {
            "sh".into()
        } else {
            "".into()
        },
        cells: slint::ModelRc::from(Rc::new(slint::VecModel::from(
            Vec::<slint::SharedString>::new(),
        ))),
        header: false,
        src: "".into(),
        alt: "".into(),
        img: slint::Image::default(),
        img_ok: false,
    }
}

fn table_row(cells: &[&str]) -> SysChatBlockRow {
    let mut r = row("table", "");
    r.header = true;
    r.cells = slint::ModelRc::from(Rc::new(slint::VecModel::from(
        cells
            .iter()
            .map(|s| slint::SharedString::from(*s))
            .collect::<Vec<_>>(),
    )));
    r
}

/// 暗像素判据：正文/描边是墨黑 `#1c1a17`（RGB 28,26,23），面板底是 `#ffffff` / 暖米 `#fdf6e9`。
/// 阈值 300（≈ 每通道 100）：描边与正文算暗，白/暖米/暖黄底算亮。
/// （状态行文案用 `PixelTheme.dim`、输入区提示用更浅的灰 —— 都亮于阈值，不会被当成正文墨迹。）
fn is_dark(px: PremultipliedRgbaColor) -> bool {
    px.alpha > 0 && (px.red as u32 + px.green as u32 + px.blue as u32) < 300
}

struct Frame {
    buf: Vec<PremultipliedRgbaColor>,
    w: u32,
    #[allow(dead_code)] // 保留：帧高是画面的基本事实，便于排障时打印
    h: u32,
}

impl Frame {
    fn px(&self, x: u32, y: u32) -> PremultipliedRgbaColor {
        self.buf[(y * self.w + x) as usize]
    }
    /// 某行 `[x0, x1)` 之间的墨迹列（升序）。
    fn ink_cols(&self, y: u32, x0: u32, x1: u32) -> Vec<u32> {
        (x0..x1).filter(|&x| is_dark(self.px(x, y))).collect()
    }
    /// `[y0, y1)` 行带里最下面一行有墨迹的行号（无则 None）。
    fn lowest_ink_row(&self, y0: u32, y1: u32, x0: u32, x1: u32) -> Option<u32> {
        (y0..y1)
            .rev()
            .find(|&y| !self.ink_cols(y, x0, x1).is_empty())
    }
}

/// 渲一帧（先跑定时器/动画，再强制标脏后让渲染器画）。
/// `request_redraw` 不能省：`draw_if_needed` 在「没有脏区」时直接返回、不调闭包。
fn draw(w: u32, h: u32, window: &Rc<MinimalSoftwareWindow>) -> Frame {
    slint::platform::update_timers_and_animations();
    let mut buf = vec![PremultipliedRgbaColor::default(); (w * h) as usize];
    window.request_redraw();
    window.draw_if_needed(|r| {
        r.render(&mut buf, w as usize);
    });
    Frame { buf, w, h }
}

/// 滞底是**延迟夹取**：第一帧负责夹取，第二帧才是屏幕上看到的位置。
fn settle(ui: &SystemChatWindow, window: &Rc<MinimalSoftwareWindow>, h: u32) -> Frame {
    for _ in 0..3 {
        let _ = draw(W, h, window);
    }
    let _ = ui;
    draw(W, h, window)
}

/// 一段会换行的长中文正文（约 6 行）。
const LONG: &str = "这是一段会换行的长正文，用来把消息区撑高：怹在本机读写文件、执行命令，\
并把结果贴回这个对话框里，供你继续追问；重复若干次以制造多行高度。";

/// 一份覆盖全部块类型的会话（role/p/h2/list/quote/code/table + 末尾长段落）。
fn fixture() -> Vec<SysChatBlockRow> {
    let long_list_item = format!("列表项也要能换行：{LONG}{LONG}");
    let code = (0..12)
        .map(|i| {
            format!("echo \"第 {i} 行：一条比较长的命令行，用来制造代码块内的换行与多行高度\"")
        })
        .collect::<Vec<_>>()
        .join("\n");
    vec![
        row("role", "你"),
        row("p", &format!("{LONG}{LONG}")),
        row("role", "怹"),
        row("h2", "小标题：为什么会长"),
        row("p", &format!("{LONG}{LONG}{LONG}")),
        row("list", "第一项（短）"),
        row("list", &long_list_item),
        row("list", "第三项"),
        row("quote", &format!("引用块：{LONG}{LONG}")),
        row("code", &code),
        table_row(&["列一", "列二"]),
        table_row(&["一段比较长的单元格内容，用来撑出表格行的高度", "b"]),
        row(
            "p",
            &format!("末尾长段落（这一段的底部就是判据盯的位置）：{LONG}{LONG}"),
        ),
    ]
}

/// #4 回归守卫：**消息区最后一行不被输入区切断**，且布局相信的内容高度覆盖真实渲染。
///
/// 两个判据（都必须在修好后为绿、修之前为红）：
/// 1. **布局真实性**（高窗，内容不被裁剪）：`content.h` ≥ 渲染出来的内容高度。
///    旧实现里 `for b in root.blocks` 的每个子组件是被 `layout_item_info(Vertical, None)`
///    查询的（无横轴约束）⇒ 里面所有换行文本都按「整段一行」估高，`content-height` 远小于
///    真实高度（实测某长段落：估 96 / 真实 ~174）。
/// 2. **用户可见症状**（620 高窗，滚到底）：最后一行墨迹离消息区底边仍有 ≥6px。
///    content-height 偏小 ⇒ 滞底夹取按偏小的高度算 ⇒ 最后几行落在视口下方、被输入区盖住
///    （owner 截图里的「两层叠字 + 被输入框压住」）。
#[test]
fn convo_bottom_not_clipped_by_composer() {
    // ── 判据 1：布局相信的内容高度必须覆盖真实渲染 ──
    let (ui, win, _) = setup(fixture(), TALL_H);
    let f = settle(&ui, &win, TALL_H);
    let fy = ui.get_dbg_flick_y().round() as u32;
    let fh = ui.get_dbg_flick_h().round() as u32;
    let believed = ui.get_dbg_content_h().round() as u32;
    let last_ink = f
        .lowest_ink_row(fy, fy + fh, 4, W - 4)
        .unwrap_or_else(|| panic!("正面控制失败：消息区没有任何墨迹（渲染通路或字体没生效）"));
    let ink_offset = last_ink - fy; // 最后一行墨迹相对内容顶的偏移（不含底部 12px 内边距）
    println!(
        "高窗：believed content.h={believed} 渲染最底墨迹偏移={ink_offset}（内容顶 {fy}，最底墨迹 {last_ink}）"
    );
    assert!(
        believed >= ink_offset,
        "布局相信的内容高（{believed}）小于真实渲染（{ink_offset}）：换行文本被按「一行」估高 \
         ⇒ 滞底算不到底 + 块间挤在一起（叠字）。believed 应 ≥ 真实高度"
    );

    // ── 判据 2：滚到底后最后一行不被消息区底边切断 ──
    let (ui2, win2, _) = setup(fixture(), SHORT_H);
    ui2.set_scroll_epoch(1);
    let f2 = settle(&ui2, &win2, SHORT_H);
    let fy2 = ui2.get_dbg_flick_y().round() as u32;
    let fh2 = ui2.get_dbg_flick_h().round() as u32;
    let bottom = fy2 + fh2;
    let last2 = f2
        .lowest_ink_row(fy2, bottom, 4, W - 4)
        .unwrap_or_else(|| panic!("正面控制失败：消息区没有任何墨迹"));
    let clearance = bottom as i64 - last2 as i64;
    println!(
        "短窗：flick={fy2}..{bottom} content.h={} content.y={} 最底墨迹={last2} 余量={clearance}px",
        ui2.get_dbg_content_h(),
        ui2.get_dbg_content_y()
    );
    assert!(
        clearance >= 6,
        "滚到底后最后一行只剩 {clearance}px 余量（内容 {fy2}..{bottom}，最底墨迹 {last2}）：\
         行被输入区切掉了。content-height 偏小 ⇒ 滞底夹取没到底（底部 12px 内边距应是空白的）"
    );
}

/// #1 回归守卫：标题栏控件条与标题文字之间必须有间隔（不许压字），且整条不越过窗口按钮。
///
/// 依赖 Rust 侧已经把计划写进窗口（`titlebar-strip-x` 等）。本用例直接调
/// [`super::apply_syschat_titlebar_plan`] 的等价路径：窗口宽变化时由 `titlebar-width-changed`
/// 回调驱动，这里在 `set_size` 之后即可断言（Slint 的 `changed` 处理器在值变化时触发）。
#[test]
fn titlebar_strip_does_not_overlap_title_or_buttons() {
    let (ui, win, _) = setup(vec![row("p", "占位")], SHORT_H);
    // 让会话/模型下拉有内容（否则模型 chip 按计划不画）
    ui.set_session_labels(slint::ModelRc::from(Rc::new(slint::VecModel::from(vec![
        slint::SharedString::from("新会话"),
        slint::SharedString::from("把架构图做成网页并配上说明文档再打包发给同事"),
    ]))));
    ui.set_model_labels(slint::ModelRc::from(Rc::new(slint::VecModel::from(vec![
        slint::SharedString::from("deepseek-chat"),
    ]))));
    ui.set_session_index(0);
    ui.set_model_index(0);
    super::apply_syschat_layout(&ui, W as f32);
    let f = settle(&ui, &win, SHORT_H);
    let strip_x = ui.get_titlebar_strip_x();
    assert!(
        strip_x > 0.0,
        "标题栏控件条没被摆上（titlebar-strip-x={strip_x}）"
    );
    // 标题栏横行 = y 6..32（PixelTitleBar 内按钮组 y=4 + titlebar 顶 2）
    let strip_x = strip_x.round() as u32;
    let title_right = (4..strip_x)
        .rev()
        .find(|&x| (6..32).any(|y| is_dark(f.px(x, y))))
        .expect("标题栏左侧应有 logo/标题墨迹（正面控制）");
    assert!(
        strip_x - title_right >= 8,
        "控件条左缘 {strip_x} 与标题文字右缘 {title_right} 只隔 {}px：压到标题了",
        strip_x - title_right
    );
    // 控件条右缘：按**实际画出来的**项宽 + 项间距算（D2 起 460px 下 ③⚙ 会让位到状态行，
    // 所以不能再用「三项都在」的固定 +12 去量）。
    let items: Vec<(f32, &str)> = [
        (ui.get_titlebar_session_w(), "①会话"),
        (ui.get_titlebar_model_w(), "②模型"),
        (ui.get_titlebar_settings_w(), "③⚙"),
    ]
    .into_iter()
    .filter(|(w, _)| *w > 0.0)
    .collect();
    let strip_right = items.iter().map(|(w, _)| *w).sum::<f32>().round() as u32
        + strip_x
        + 6 * (items.len().saturating_sub(1)) as u32;
    // 右侧边界 = **整个按钮区**左缘（W − 4 − 106）− 8
    let zone_left = W - 4 - 106;
    assert!(
        strip_right <= zone_left - 8 + 2,
        "控件条右缘 {strip_right} 越过了按钮区左侧（应 ≤ {}）",
        zone_left - 8
    );
    // 正面控制：**每一个画出来的**项都得有墨迹（少画一个就是空壳）。
    let mut cursor = strip_x;
    for (w, name) in &items {
        let w = w.round() as u32;
        let ink = (cursor..cursor + w).any(|x| (6..32).any(|y| is_dark(f.px(x, y))));
        assert!(ink, "{name}（{w}px 格）里没有任何墨迹 ⇒ 空壳");
        cursor += w + 6;
    }
}

/// 宽到三件套都放得下时（500px），③⚙ 必须**在标题栏里**且真有墨迹（Path 手绘齿轮）。
/// 460px（默认）下它让位到状态行 —— 两条一起把「让位口径」与「齿轮画得出来」都钉住。
#[test]
fn titlebar_gear_renders_when_it_fits() {
    const WIDE: u32 = 500;
    let (ui, win, _) = setup(vec![row("p", "占位")], SHORT_H);
    ui.set_session_labels(slint::ModelRc::from(Rc::new(slint::VecModel::from(vec![
        slint::SharedString::from("新会话"),
    ]))));
    ui.set_model_labels(slint::ModelRc::from(Rc::new(slint::VecModel::from(vec![
        slint::SharedString::from("deepseek-chat"),
    ]))));
    ui.set_session_index(0);
    ui.set_model_index(0);
    ui.window()
        .set_size(slint::PhysicalSize::new(WIDE, SHORT_H));
    super::apply_syschat_layout(&ui, WIDE as f32);
    assert!(
        ui.get_titlebar_settings_w() > 0.0,
        "500px 下 ③⚙ 应留在标题栏（settings_w={}）",
        ui.get_titlebar_settings_w()
    );
    slint::platform::update_timers_and_animations();
    let mut buf = vec![PremultipliedRgbaColor::default(); (WIDE * SHORT_H) as usize];
    win.request_redraw();
    win.draw_if_needed(|r| {
        r.render(&mut buf, WIDE as usize);
    });
    let f = Frame {
        buf,
        w: WIDE,
        h: SHORT_H,
    };
    let right = ui.get_titlebar_strip_x().round() as u32
        + ui.get_titlebar_session_w().round() as u32
        + ui.get_titlebar_model_w().round() as u32
        + ui.get_titlebar_settings_w().round() as u32
        + 12;
    let gear_left = right - ui.get_titlebar_settings_w().round() as u32;
    assert!(
        (gear_left..right).any(|x| (6..32).any(|y| is_dark(f.px(x, y)))),
        "③⚙ 那一格没有墨迹（齿轮没画出来）"
    );
}

/// **对稿/评审出图**（默认 `#[ignore]`：不进四门禁的常规轮）。
///
/// 为什么需要它：对稿如果只靠真机截图，同一份代码在不同窗口位置/缩放/滚动相位下会出不同的图，
/// 没法当基准；这里用无头软件渲染把**真实的** `SystemChatWindow` 渲成 PNG，同样输入 ⇒ 同样像素。
/// 出图落在 `/Volumes/DataExt/tmp/syschat-shots/`（**不写**工作区，也不碰真实 `~/Library`）。
///
/// 跑法：`bash tools/check_test_isolation.sh --bin agent-bridge -- --ignored dump_frames`
#[test]
#[ignore = "出图用；默认不跑（不进四门禁的常规轮）"]
fn dump_frames_for_review() {
    let dir = std::path::Path::new("/Volumes/DataExt/tmp/syschat-shots");
    std::fs::create_dir_all(dir).expect("建出图目录失败");

    let (ui, win, _model) = setup(fixture(), SHORT_H);
    // 标题栏 ①②③ 只在有候选时才渲染 ⇒ 出图必须把候选灌上，否则量不到真实落位。
    ui.set_session_labels(slint::ModelRc::from(Rc::new(slint::VecModel::from(vec![
        slint::SharedString::from("新会话"),
        slint::SharedString::from("上周的排查"),
    ]))));
    ui.set_model_labels(slint::ModelRc::from(Rc::new(slint::VecModel::from(vec![
        slint::SharedString::from("deepseek-chat"),
        slint::SharedString::from("claude-sonnet-4.6"),
    ]))));
    ui.set_session_index(0);
    ui.set_model_index(0);
    // 标题栏落位是 Rust 侧算出来再写属性的（`apply_syschat_layout`）——出图必须走同一条路，
    // 否则 `titlebar-strip-x` 停在初值、①②③ 一个都不渲染（这正是「无头出图保真」的意义）。
    apply_syschat_layout(&ui, W as f32);
    let f = settle(&ui, &win, SHORT_H);
    write_png(&f, &dir.join("short.png"));
    // 滚到底（滞底）——owner 截图里出问题的那种状态。
    ui.set_scroll_epoch(1);
    let f = settle(&ui, &win, SHORT_H);
    write_png(&f, &dir.join("bottom.png"));

    // 「正在输入 + ⏹ 停止」这一行（E1）也要能对稿。
    ui.set_busy(true);
    let f = settle(&ui, &win, SHORT_H);
    write_png(&f, &dir.join("busy.png"));

    // 附件行（3+N chips，D 批）：四条待发 ⇒ 只显示 3 个 + `+1`。
    ui.set_pending_rows(slint::ModelRc::from(Rc::new(slint::VecModel::from(vec![
        PendingAttachRow {
            index: 0,
            name: "构思图.png".into(),
            is_image: true,
            thumb: slint::Image::default(),
            thumb_ok: false,
            ext: "PNG".into(),
        },
        PendingAttachRow {
            index: 1,
            name: "演示.mov".into(),
            is_image: false,
            thumb: slint::Image::default(),
            thumb_ok: false,
            ext: "MOV".into(),
        },
        PendingAttachRow {
            index: 2,
            name: "设计规范.pdf".into(),
            is_image: false,
            thumb: slint::Image::default(),
            thumb_ok: false,
            ext: "PDF".into(),
        },
    ]))));
    ui.set_attach_count(4);
    ui.set_pending_more(1);
    ui.set_pending_more_text("+1".into());
    let f = settle(&ui, &win, SHORT_H);
    write_png(&f, &dir.join("attachments.png"));
}

/// 预乘 RGBA → 直通 RGBA8 并补白底（窗口本身透明：`rgb + (255 - a)` 等价于铺白）。
fn write_png(f: &Frame, path: &std::path::Path) {
    let mut img = image::RgbaImage::new(f.w, f.h);
    for y in 0..f.h {
        for x in 0..f.w {
            let p = f.px(x, y);
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
    img.save(path).expect("写 PNG 失败");
}

/// 窄窗（340px，最小宽）出图：验「①②③ 不与窗口按钮相撞、且不放空壳」——
/// 这是真机 340px 实测出问题的那个宽度。出 `narrow340.png`。
#[test]
#[ignore = "出图用；默认不跑"]
fn dump_narrow_for_review() {
    let dir = std::path::Path::new("/Volumes/DataExt/tmp/syschat-shots");
    std::fs::create_dir_all(dir).expect("建出图目录失败");
    const NW: u32 = 340;
    let (ui, win, _m) = setup(fixture(), 500);
    ui.set_session_labels(slint::ModelRc::from(Rc::new(slint::VecModel::from(vec![
        slint::SharedString::from("新会话"),
        slint::SharedString::from("上周的排查"),
    ]))));
    ui.set_model_labels(slint::ModelRc::from(Rc::new(slint::VecModel::from(vec![
        slint::SharedString::from("deepseek-chat"),
    ]))));
    ui.set_session_index(0);
    ui.set_model_index(0);
    ui.window().set_size(slint::PhysicalSize::new(NW, 500));
    apply_syschat_layout(&ui, NW as f32);
    slint::platform::update_timers_and_animations();
    let mut buf = vec![PremultipliedRgbaColor::default(); (NW * 500) as usize];
    win.request_redraw();
    win.draw_if_needed(|r| {
        r.render(&mut buf, NW as usize);
    });
    let f = Frame { buf, w: NW, h: 500 };
    write_png(&f, &dir.join("narrow340.png"));
    println!(
        "340px: strip_x={} session={} model={} settings={}",
        ui.get_titlebar_strip_x(),
        ui.get_titlebar_session_w(),
        ui.get_titlebar_model_w(),
        ui.get_titlebar_settings_w()
    );
}
