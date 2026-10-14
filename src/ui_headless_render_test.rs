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

/// 单列上的暗行（墨迹）行号（升序）——量控件边框盒用。
fn dark_rows(f: &Frame, x: u32, y0: u32, y1: u32) -> Vec<u32> {
    (y0..y1).filter(|&y| is_dark(f.px(x, y))).collect()
}

/// 把升序的连续行/列号按「连续段」分组（找 2px 分隔线、控件边框段用）。
fn groups(v: &[u32]) -> Vec<(u32, u32)> {
    let mut out: Vec<(u32, u32)> = Vec::new();
    for &x in v {
        match out.last_mut() {
            Some(last) if x == last.1 + 1 => last.1 = x,
            _ => out.push((x, x)),
        }
    }
    out
}

/// 主色（暖黄 `#ffc61a`）判据：发送按钮的 accent 底。
/// 预乘后不透明像素 = 原色；只用于量 bbox 的**形状**（不比较精确色值）。
fn is_accent(p: PremultipliedRgbaColor) -> bool {
    p.alpha > 200 && p.red >= 220 && p.green >= 150 && p.blue <= 120
}

/// 区域 `[x0,x1) × [y0,y1)` 里满足判据的像素包围盒（含端点）；空则 `None`。
fn bbox(
    f: &Frame,
    x0: u32,
    y0: u32,
    x1: u32,
    y1: u32,
    pred: impl Fn(PremultipliedRgbaColor) -> bool,
) -> Option<(u32, u32, u32, u32)> {
    let mut acc: Option<(u32, u32, u32, u32)> = None;
    for y in y0..y1 {
        for x in x0..x1 {
            if pred(f.px(x, y)) {
                acc = Some(match acc {
                    None => (x, y, x, y),
                    Some((a, b, c, d)) => (a.min(x), b.min(y), c.max(x), d.max(y)),
                });
            }
        }
    }
    acc
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

/// 同 [`settle`]，但帧宽可指定（默认 `settle` 固定用 `W`；出图要按真实窗宽渲）。
fn settle_w(window: &Rc<MinimalSoftwareWindow>, w: u32, h: u32) -> Frame {
    for _ in 0..3 {
        slint::platform::update_timers_and_animations();
        let mut buf = vec![PremultipliedRgbaColor::default(); (w * h) as usize];
        window.request_redraw();
        window.draw_if_needed(|r| {
            r.render(&mut buf, w as usize);
        });
    }
    slint::platform::update_timers_and_animations();
    let mut buf = vec![PremultipliedRgbaColor::default(); (w * h) as usize];
    window.request_redraw();
    window.draw_if_needed(|r| {
        r.render(&mut buf, w as usize);
    });
    Frame { buf, w, h }
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
    // 控件条右缘：按**实际画出来的**项宽 + 项间距算（换档时某些项会让位到状态行，
    // 所以不能再用「四项都在」的固定值去量）。
    // ①新建会话是 `x >= 0` 时恒画的一项（`titlebar_plan` 保证），但它的宽不写进窗口属性，
    // 所以这里用 Rust 侧常量。
    let items: Vec<(f32, &str)> = [
        (crate::syschat::TB_NEW_W, "①新建会话"),
        (ui.get_titlebar_session_w(), "②历史会话"),
        (ui.get_titlebar_model_w(), "③模型"),
        (ui.get_titlebar_settings_w(), "④⚙"),
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
        "500px 下 ④⚙ 应留在标题栏（settings_w={}）",
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
    // 四项（①新建会话 ②历史会话 ③模型 ④⚙）+ 3 个 6px 间隔。
    let right = ui.get_titlebar_strip_x().round() as u32
        + crate::syschat::TB_NEW_W.round() as u32
        + ui.get_titlebar_session_w().round() as u32
        + ui.get_titlebar_model_w().round() as u32
        + ui.get_titlebar_settings_w().round() as u32
        + 18;
    let gear_left = right - ui.get_titlebar_settings_w().round() as u32;
    assert!(
        (gear_left..right).any(|x| (6..32).any(|y| is_dark(f.px(x, y)))),
        "④⚙ 那一格没有墨迹（齿轮没画出来）"
    );
}

/// **v10 对稿锁**：稿子 CSS 里的五项硬尺寸，全部从**渲染像素**上量出来。
///
/// | 稿子（唯一事实源 `docs/design/syschat-dialog.html`） | 本用例量的量 |
/// |---|---|
/// | `.tb-btn{height:26px}`（标题栏一行、垂直居中） | ①新建会话钮的上下描边行 |
/// | `.status{min-height:38px}` | 标题栏底边 vs 状态栏底边两条分隔线的行距 |
/// | `.composer-zone{padding:10px}` | 输入框左右描边到窗口内沿的距离 |
/// | `.send{height:30px;padding:0 14px}` | 发送钮描边盒高 / 『发送』墨迹的左右留白 |
/// | `.fnbar{gap:4px}` + `.crow{align-items:center}` | 功能区总宽（5×24+4×4）与它相对发送钮的位置/中心线 |
///
/// 为什么量像素而不是读属性：这些量就是「布局是否真的按稿子落位」的最终事实——属性可能对，
/// 但被布局压缩/错位（本卡就修过一处：状态栏 `height:38px` 被 VerticalLayout 压到 25px、第二行被裁）。
///
/// 判别力（负向）：
/// 1. `min-height: 38px` 退回 `height: 38px` ⇒ 分隔线行距变 25 ⇒ 状态栏那条红；
/// 2. `.send` 高度或内边距改掉 ⇒ 发送钮那两条红；
/// 3. 功能区少画一枚钮（或 gap 改掉）⇒ 总宽 ≠ 136 ⇒ 功能区那条红。
#[test]
fn v10_metrics_match_design() {
    const MW: u32 = 520;
    const MH: u32 = 820;
    let (ui, win, _m) = setup(fixture(), MH);
    ui.set_session_labels(slint::ModelRc::from(Rc::new(slint::VecModel::from(vec![
        slint::SharedString::from("新会话"),
    ]))));
    ui.set_model_labels(slint::ModelRc::from(Rc::new(slint::VecModel::from(vec![
        slint::SharedString::from("deepseek-chat"),
    ]))));
    ui.set_session_index(0);
    ui.set_model_index(0);
    // 有草稿 ⇒ 发送钮 opacity = 1.0（预乘色不透明，accent 判据干净）。
    ui.set_draft("占位草稿".into());
    ui.window().set_size(slint::PhysicalSize::new(MW, MH));
    super::apply_syschat_layout(&ui, MW as f32);
    let f = settle_w(&win, MW, MH);

    println!(
        "dbg: composer=x{} y{} w{} h{} / area y{} h{} / crow y{} h{} / fnbar x{} y{} w{} h{} / send x{} y{} w{} h{}",
        ui.get_dbg_composer_x(),
        ui.get_dbg_composer_y(),
        ui.get_dbg_composer_w(),
        ui.get_dbg_composer_h(),
        ui.get_dbg_area_y(),
        ui.get_dbg_area_h(),
        ui.get_dbg_crow_y(),
        ui.get_dbg_crow_h(),
        ui.get_dbg_fnbar_x(),
        ui.get_dbg_fnbar_y(),
        ui.get_dbg_fnbar_w(),
        ui.get_dbg_fnbar_h(),
        ui.get_dbg_send_x(),
        ui.get_dbg_send_y(),
        ui.get_dbg_send_w(),
        ui.get_dbg_send_h(),
    );

    // ── 1. 标题栏：控件条最左那格（①新建会话）= 26px 高、与窗口按钮同线、在 34px 标题栏里居中 ──
    let strip_x = ui.get_titlebar_strip_x();
    assert!(strip_x > 0.0, "标题栏控件条没摆上（strip_x={strip_x}）");
    let strip_x = strip_x.round() as u32;
    // 扫 y=3..36：y=0,1 是窗口外描边、y=36,37 是标题栏底部分隔线（都跨整宽）——都不属于控件条。
    let rows = dark_rows(&f, strip_x + 13, 3, 36);
    let g = groups(&rows);
    let top = g.first().map(|r| r.0);
    let bottom = g.last().map(|r| r.1);
    assert_eq!(
        top,
        Some(6),
        "控件条顶缘应在 y=6（= 根 padding 2 + PixelTitleBar 内按钮 y:4，与窗口按钮同线）"
    );
    assert_eq!(bottom, Some(31), "控件条底缘应在 y=31");
    assert_eq!(
        bottom.unwrap() - top.unwrap() + 1,
        26,
        "标题栏项高应为 26px（稿 .tb-btn{{height:26px}}）"
    );
    // 垂直居中：标题栏是 y 2..36（上游 34px）⇒ 上下留白各 4px。
    assert_eq!(
        top.unwrap() - 2,
        36 - (bottom.unwrap() + 1),
        "控件条在标题栏里应垂直居中"
    );

    // ── 2. 状态栏 38px：整宽分隔线只可能来自窗描边/标题栏底边/状态栏底边/输入区顶边/窗底边 ──
    let full: Vec<u32> = (0..MH)
        .filter(|&y| (10..MW - 10).all(|x| is_dark(f.px(x, y))))
        .collect();
    let g = groups(&full);
    assert_eq!(g.first().copied(), Some((0, 1)), "窗口顶部 2px 描边");
    assert_eq!(
        g.get(1).copied(),
        Some((36, 37)),
        "标题栏底部分隔线（稿 .titlebar{{border-bottom:2px}}）"
    );
    assert_eq!(g.get(2).copied(), Some((74, 75)), "状态栏底部分隔线");
    assert_eq!(
        g[2].0 - g[1].0,
        38,
        "状态栏高度应为 38px（稿 .status{{min-height:38px}}）；被布局压缩会变小"
    );
    assert_eq!(
        g.get(3).copied(),
        Some((672, 673)),
        "输入区顶部分隔线（稿 .composer-zone{{border-top:2px}}）——也锁住输入框总高：\
         818 − 10（zone 下 padding）− 124（框内：10 + 文本区 66 + 间距 8 + 功能区 30 + 10）\
         − 10（zone 上 padding）− 2（顶部分隔线）= 672"
    );

    // ── 3. 输入区：zone padding 10px（左右一致）──
    // `composer.x` 是相对**zone 的 HorizontalLayout** 的坐标 ⇒ 它就等于 zone padding。
    assert_eq!(
        ui.get_dbg_composer_x(),
        10.0,
        "输入框左内边距应为 10px（稿 .composer-zone{{padding:10px}}）"
    );
    assert_eq!(
        ui.get_dbg_composer_w(),
        MW as f32 - 4.0 - 20.0,
        "输入框宽 = 窗宽 − 根 padding 4 − zone padding 20（左右一致，稿 v9 第 3 条）"
    );
    // composer 内部内容距**外沿** = 10px（稿 border 2 + padding 8；Slint 的 border-width 不占布局）。
    assert_eq!(
        ui.get_dbg_area_y(),
        10.0,
        "输入框内容距外沿应为 10px（稿 .composer{{border:2px;padding:8px}}）"
    );
    assert_eq!(
        ui.get_dbg_area_h(),
        66.0,
        "文本区高应为 66px（稿 .area{{min-height:66px}}）"
    );
    assert_eq!(
        ui.get_dbg_composer_h(),
        10.0 + 66.0 + 8.0 + 30.0 + 10.0,
        "输入框总高 = padding 10 + 文本区 66 + 间距 8 + 功能区 30 + padding 10 = 124"
    );
    // 绝对位置交叉验证（像素）：composer 左边框落在 x=12（根 padding 2 + zone 10）。
    // accent 在消息区也有（引用块强调条 / 图片卡），所以只在 composer 区域内找发送钮。
    let zone_top = g[3].0;
    let send_a =
        bbox(&f, 0, zone_top, MW, MH, is_accent).expect("找不到发送按钮的 accent 底（正面控制）");
    let send_cy = (send_a.1 + send_a.3) / 2;
    let left_border = (2..MW)
        .find(|&x| is_dark(f.px(x, send_cy)))
        .expect("找不到 composer 左边框");
    // 上界取 `MW-3`：右边 x=518,519 是窗口自己的 2px 外描边（跨全高，不算 composer 的边）。
    let right_border = (2..MW - 3)
        .rev()
        .find(|&x| is_dark(f.px(x, send_cy)))
        .expect("找不到 composer 右边框");
    assert_eq!(
        left_border - 2,
        10,
        "输入框左内边距应为 10px（稿 .composer-zone{{padding:10px}}）"
    );
    assert_eq!(
        MW - 3 - right_border,
        10,
        "输入框右内边距应为 10px（左右一致，稿 v9 第 3 条）"
    );

    // ── 4. 发送钮：描边盒高 30px、贴右内沿、『发送』居中、内边距 ≈14px ──
    assert_eq!(
        ui.get_dbg_send_h(),
        30.0,
        "发送按钮高应为 30px（稿 v10 .send{{height:30px}}，v9 前是 36）"
    );
    assert_eq!(
        ui.get_dbg_send_x() + ui.get_dbg_send_w(),
        ui.get_dbg_composer_w() - 2.0 * 10.0,
        "发送钮应贴在输入框右内沿（右边距 = composer padding 10）"
    );
    let send_cx = (send_a.0 + send_a.2) / 2;
    let srows = dark_rows(&f, send_cx, send_a.1 - 4, send_a.3 + 5);
    let sg = groups(&srows);
    let send_top = sg.first().unwrap().0;
    let send_bottom = sg.last().unwrap().1;
    assert_eq!(
        send_bottom - send_top + 1,
        30,
        "发送按钮高（像素量）应为 30px"
    );
    // accent 底只在 2px 描边以内 ⇒ 整盒 = accent 包围盒外扩 2px。
    assert_eq!(send_top, send_a.1 - 2, "发送钮上描边应在 accent 底外 2px");
    assert_eq!(
        send_bottom,
        send_a.3 + 2,
        "发送钮下描边应在 accent 底外 2px"
    );
    let send_box_l = send_a.0 - 2;
    let send_box_r = send_a.2 + 2;
    let t = bbox(
        &f,
        send_a.0 + 3,
        send_a.1 + 3,
        send_a.2 - 2,
        send_a.3 - 2,
        is_dark,
    )
    .expect("发送钮里应有『发送』墨迹（正面控制）");
    let pad_l = t.0 - send_box_l;
    let pad_r = send_box_r - t.2;
    assert_eq!(
        pad_l, pad_r,
        "『发送』应居中（左右留白 {pad_l} vs {pad_r}）"
    );
    assert!(
        (12..=20).contains(&pad_l),
        "发送钮内边距应 ≈14px（稿 .send{{padding:0 14px}}），实测 {pad_l}"
    );

    // ── 5. 功能区：整体靠右、总宽 136px、与发送钮同一水平线 ──
    assert_eq!(
        ui.get_dbg_fnbar_w(),
        5.0 * 24.0 + 4.0 * 4.0,
        "功能区总宽应为 5×24 + 4×4 = 136px（稿 .fnbar{{gap:4px}} + .ib{{width:24px}}）"
    );
    assert_eq!(ui.get_dbg_fnbar_h(), 24.0, "图标钮高应为 24px（稿 .ib）");
    assert_eq!(
        ui.get_dbg_crow_h(),
        30.0,
        "功能区一行的高 = 发送钮 30px（两者同一行）"
    );
    // 同一条水平线（稿 .crow align-items:center）：24px 的图标在 30px 行里上下各留 3px。
    assert_eq!(
        ui.get_dbg_fnbar_y(),
        3.0,
        "图标钮应在 30px 行里垂直居中（上下各 3px）"
    );
    assert_eq!(ui.get_dbg_send_y(), 0.0, "发送钮撑满该行高度（30px）");
    // 靠右：功能区与发送钮的间距 = 8px，左侧留出大段占空（稿里的 grow 块）。
    assert_eq!(
        ui.get_dbg_fnbar_x() + ui.get_dbg_fnbar_w() + 8.0,
        ui.get_dbg_send_x(),
        "功能区与发送钮间距应为 8px（稿 .crow{{gap:8px}}）"
    );
    assert!(
        ui.get_dbg_fnbar_x() >= 100.0,
        "功能区应整体靠右（左侧占空 {}px）",
        ui.get_dbg_fnbar_x()
    );
    // 像素交叉验证：整行行带里的暗列 = 五枚图标的左右描边 ⇒ 首尾之间正好 136px。
    let cols: Vec<u32> = (left_border + 3..send_box_l)
        .filter(|&x| (send_top..=send_bottom).any(|y| is_dark(f.px(x, y))))
        .collect();
    let fnbar_l = *cols.first().expect("找不到功能区左缘（正面控制）");
    let fnbar_r = *cols.last().expect("找不到功能区右缘");
    assert_eq!(
        fnbar_r - fnbar_l + 1,
        5 * 24 + 4 * 4,
        "功能区总宽（像素量）应为 136px"
    );
    assert_eq!(
        send_box_l - fnbar_r - 1,
        8,
        "功能区与发送钮间距（像素量）应为 8px"
    );
    // 像素交叉验证：首枚图标钮高 24px，且垂直中心与发送钮一致。
    // 取**中心列**（不是左边框列）：图标钮有 6px 圆角，最外侧列只覆盖中间十几像素。
    let irows = dark_rows(&f, fnbar_l + 12, send_top - 8, send_bottom + 9);
    let ig = groups(&irows);
    let itop = ig.first().unwrap().0;
    let ibottom = ig.last().unwrap().1;
    assert_eq!(ibottom - itop + 1, 24, "功能区图标钮高（像素量）应为 24px");
    assert_eq!(
        (itop + ibottom) / 2,
        send_cy,
        "图标钮与发送钮应同一条水平线（垂直居中）"
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
    // 默认已改成 520×560（见 `run_gui` 的显式 set_size）——出图按真实默认宽度来。
    ui.window().set_size(slint::PhysicalSize::new(520, SHORT_H));
    apply_syschat_layout(&ui, 520.0);
    let f = settle_w(&win, 520, SHORT_H);
    write_png(&f, &dir.join("short.png"));
    // 滚到底（滞底）——owner 截图里出问题的那种状态。
    ui.set_scroll_epoch(1);
    let f = settle_w(&win, 520, SHORT_H);
    write_png(&f, &dir.join("bottom.png"));

    // 「正在输入 + ⏹ 停止」这一行（E1）也要能对稿。
    ui.set_busy(true);
    let f = settle_w(&win, 520, SHORT_H);
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
    let f = settle_w(&win, 520, SHORT_H);
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

/// 整窗出图（520×820，含完整输入区与底部圆角），供对稿判读。
#[test]
#[ignore = "出图用；默认不跑"]
fn dump_full_window() {
    let dir = std::path::Path::new("/Volumes/DataExt/tmp/syschat-shots");
    std::fs::create_dir_all(dir).expect("建出图目录失败");
    let (ui, win, _m) = setup(fixture(), 820);
    ui.set_session_labels(slint::ModelRc::from(Rc::new(slint::VecModel::from(vec![
        slint::SharedString::from("新会话"),
    ]))));
    ui.set_model_labels(slint::ModelRc::from(Rc::new(slint::VecModel::from(vec![
        slint::SharedString::from("deepseek-chat"),
    ]))));
    ui.set_session_index(0);
    ui.set_model_index(0);
    // 有草稿 ⇒ 发送钮 opacity=1，PNG 上能量出 accent 底的完整形状（对稿用）。
    ui.set_draft("就按这三份素材做，说明放第二节…".into());
    ui.window().set_size(slint::PhysicalSize::new(520, 820));
    apply_syschat_layout(&ui, 520.0);
    let f = settle_w(&win, 520, 820);
    write_png(&f, &dir.join("full.png"));
    // 只切输入区（对稿时放大看 titlebar/fnbar 的尺寸，不用每次都去看整窗）。
    println!(
        "v10 出图：full.png（520×820）；标题栏 strip_x={} session={} model={} settings={}",
        ui.get_titlebar_strip_x(),
        ui.get_titlebar_session_w(),
        ui.get_titlebar_model_w(),
        ui.get_titlebar_settings_w()
    );
}
