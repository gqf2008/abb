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

/// 把 `.pop` 面板的锚点回调接上（生产 `run()` 里同一套）：几何（两个功能钮的窗口坐标矩形 +
/// 窗口尺寸）由 `.slint` 报来，落位/尺寸全在 `syschat::pop_anchor` / `more_pop_h`。
fn wire_pop(ui: &SystemChatWindow) {
    let cw = ui.as_weak();
    ui.on_pop_anchor_changed(move |emoji_btn, more_btn, win_w, win_h| {
        if let Some(w) = cw.upgrade() {
            apply_pop_anchors(&w, emoji_btn, more_btn, win_w, win_h);
        }
    });
}

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
///
/// v11：这里顺带接上**分割条接线**（生产 `run()` 里同一套）：不接的话 `convo-h` 会停在
/// `.slint` 的默认值，量出来的几何就不是生产布局（输入区会被撑到剩余的全高）。
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
    let _ = wire_split(&ui);
    wire_pop(&ui);
    ui.show().expect("show 失败");
    ui.window().set_size(slint::PhysicalSize::new(W, h));
    let win = windows.borrow().last().expect("平台没发窗口").clone();
    (ui, win, model)
}

/// 把分割条的四个回调接到一个新建的 [`SplitWiring`] 上（与生产 `run()` 同一套接线），
/// 返回句柄供测试驱动/断言。
fn wire_split(ui: &SystemChatWindow) -> Rc<SplitWiring> {
    let split = Rc::new(SplitWiring::new());
    let sw = split.clone();
    let cw = ui.as_weak();
    ui.on_split_band_changed(move |band| {
        if let Some(w) = cw.upgrade() {
            sw.on_band(&w, band);
        }
    });
    let sw = split.clone();
    let cw = ui.as_weak();
    ui.on_split_drag_to(move |pointer_rel| {
        if let Some(w) = cw.upgrade() {
            sw.drag_to(&w, pointer_rel);
        }
    });
    let sw = split.clone();
    ui.on_split_drag_end(move || {
        sw.drag_end();
    });
    split
}

/// 角色行**直接用生产构造点**（卡 `abb-wording-role-label-kind-20261013`）：身份（是否助手）
/// 是生产里的 `role_assistant` 字段，不是角色名文案 ⇒ 这里也必须按同一口径给，
/// 否则量到的是另一条渲染分支（角色名与身份在本台里再对不上号）。
/// 一行消息块：与生产 `ui.rs::block_row` / `ui.rs::think_row` 同口径的最小构造
///（kind 驱动渲染分支）。
///
/// 卡 `abb-v11-message-actions`：`selectable` / `plain` 必须与生产**同一判据**
///（`mdblocks::selectable_plain`）——这块头上量的是布局，判据抄错就等于量了另一条分支。
fn row(kind: &str, text: &str) -> SysChatBlockRow {
    // 思考块在生产里走 `think_row`：正文是**纯文本**（不是 markdown）⇒ 恒可选中、无样式可言。
    // `selectable_plain` 不管 `think`（GUI 自有的 kind），所以这里显式对齐生产。
    let plain = if kind == "think" {
        Some(text.to_string())
    } else {
        crate::mdblocks::selectable_plain(kind, text)
    };
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
        table_rows: slint::ModelRc::from(Rc::new(slint::VecModel::from(Vec::<TableRow>::new()))),
        col_ws: slint::ModelRc::from(Rc::new(slint::VecModel::from(Vec::<f32>::new()))),
        src: "".into(),
        alt: "".into(),
        img: slint::Image::default(),
        img_ok: false,
        img_aspect: 0.0,
        img_w: 0.0,
        img_h: 0.0,
        flow: slint::ModelRc::from(Rc::new(slint::VecModel::from(Vec::<ConvoFlowPiece>::new()))),
        flow_h: 0.0,
        flow_size: 13.0,
        math_off: slint::ModelRc::from(Rc::new(slint::VecModel::from(Vec::<i32>::new()))),
        math_tex: slint::ModelRc::from(Rc::new(slint::VecModel::from(
            Vec::<slint::SharedString>::new(),
        ))),
        role_assistant: false,
        think_expanded: false,
        think_live: false,
        think_elapsed: "".into(),
        selectable: plain.is_some(),
        plain: plain.unwrap_or_default().into(),
        turn: -1,
    }
}

/// 助手轮动作行（与生产 `ui.rs::acts_row` 同口径）：只带轮序号，正文空。
/// `turn < 0` = 「刚跑完的那一轮」（历史下标由 Rust 在点击时解析）。
fn acts_row(turn: i32) -> SysChatBlockRow {
    let mut r = row("acts", "");
    r.turn = turn;
    r
}

/// 多行整表：`rows[0]` 作表头（与 `mdblocks` 的产出同口径）；走**生产**的整表构造点
///（列宽/行高只有拿到整表才算得出来）。
fn table_of(rows: &[&[&str]]) -> SysChatBlockRow {
    let bs: Vec<crate::mdblocks::MdBlock> = rows
        .iter()
        .enumerate()
        .map(|(i, cs)| {
            let mut b = crate::mdblocks::MdBlock::new("table");
            b.header = i == 0;
            b.cells = cs.iter().map(|s| s.to_string()).collect();
            b
        })
        .collect();
    super::table_row(&bs)
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
    /// 与另一帧的差异像素数（`PremultipliedRgbaColor` 没有 `PartialEq`，只能逐通道比）。
    fn diff_px(&self, other: &Frame) -> usize {
        assert_eq!((self.w, self.h), (other.w, other.h), "两帧尺寸必须一致");
        self.buf
            .iter()
            .zip(other.buf.iter())
            .filter(|(p, q)| (p.red, p.green, p.blue, p.alpha) != (q.red, q.green, q.blue, q.alpha))
            .count()
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
        role_row("你", false),
        row("p", &format!("{LONG}{LONG}")),
        role_row("怹", true),
        row("h2", "小标题：为什么会长"),
        row("p", &format!("{LONG}{LONG}{LONG}")),
        row("list", "第一项（短）"),
        row("list", &long_list_item),
        row("list", "第三项"),
        row("quote", &format!("引用块：{LONG}{LONG}")),
        row("code", &code),
        table_of(&[
            &["列一", "列二"],
            &["一段比较长的单元格内容，用来撑出表格行的高度", "b"],
        ]),
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

/// E2 回归守卫：思考块**默认折叠**，展开后内容变高——证明 `.slint` 的 `kind == "think"`
/// 分支真的参与布局（不是只编译过），且 `think-expanded` 字段驱动它。
#[test]
fn think_block_collapsed_by_default_and_expands() {
    let long = "推理过程：".to_string() + &"先看输入再查文档最后定结论。".repeat(24);
    let mut think = row("think", &long);
    think.think_elapsed = "3.2s".into();
    let collapsed = vec![role_row("怹", true), think.clone(), row("p", "正文")];
    let (ui, win, model) = setup(collapsed, TALL_H);
    let _ = settle(&ui, &win, TALL_H);
    let h_collapsed = ui.get_dbg_content_h();

    // 默认必须折叠：折叠高度不得包含正文推理（至少比展开矮一截）。
    let mut expanded = think;
    expanded.think_expanded = true;
    model.set_row_data(1, expanded);
    let _ = settle(&ui, &win, TALL_H);
    let h_expanded = ui.get_dbg_content_h();
    assert!(
        h_expanded > h_collapsed + 20.0,
        "展开后的思考块必须占更多高度（折叠 {h_collapsed} vs 展开 {h_expanded}）：\
         折叠态不应渲染推理正文"
    );
}

// ───────── 卡 `abb-v11-message-actions`：消息可选中 + 助手轮动作行的无头守卫 ─────────

/// 生产口径的正文块（无行内样式 ⇒ 走只读 `TextInput`，可拖选 + ⌘C）。
fn selectable_p(text: &str) -> SysChatBlockRow {
    let r = row("p", text);
    assert!(
        r.selectable,
        "这段文本无行内样式，生产必须判定为可选中：{text:?}"
    );
    r
}

/// 换渲染元素**不改版式**：同一块内容分别走「可选中」分支（只读 `TextInput`）与旧分支
/// （`p`/`h2`/`list` → `StyledText`，`code` → `Text`），**高度与最底墨迹必须一致**。
///
/// 为什么必须量：换元素是本卡的核心动作。若两者排版规则不同（行高、内边距、换行宽度、
/// 竖排对齐），用户看到的是「消息整体变形」，而不是「白得可选中」。
///
/// 判别力（负向，已实测）：删掉 `ConvoSelectableText` 的 `wrap: word-wrap`
/// （`TextInput.wrap` 的默认值是 `no-wrap`）⇒ 长段落不换行 ⇒ 本测试立刻红。
/// 对照组里必须留**多行代码块**：它的换行是真的靠 `\n` 断行，最容易被「压成一行」写错。
#[test]
fn selectable_blocks_render_same_height_as_legacy_element() {
    let long = format!("{LONG}{LONG}");
    let code = (0..12)
        .map(|i| {
            format!("echo \"第 {i} 行：一条比较长的命令行，用来制造代码块内的换行与多行高度\"")
        })
        .collect::<Vec<_>>()
        .join("\n");
    // 波浪号：`row()` 的入参是**解析后的块文本**（= 生产 `block_row` 的入参），所以这里先过一遍
    // `parse`。`~` 曾被 `push_escaped` 漏掉，解析后应当已被转义成 `\~`（下面断言）。
    let tilde = {
        let src = format!("约 3~4 元、~/bin、a~b：{long}");
        let b = crate::mdblocks::parse(&src);
        assert_eq!(b.len(), 1, "用例前提：这段应解析成单块");
        assert_eq!(b[0].kind, "p");
        assert!(
            b[0].text.contains(r"\~"),
            "解析后字面量波浪号该已转义，否则判据之外会静默出错：{:?}",
            b[0].text
        );
        b[0].text.clone()
    };
    // `min_h` = 该块的**正面控制**：内容必须真的多行（否则「两边一样高」是空的）。
    let cases: [(&str, String, f32); 5] = [
        ("p", long.clone(), 100.0),
        ("h2", long.clone(), 100.0),
        ("list", long.clone(), 100.0),
        ("code", code, 180.0),
        // 波浪号：解析时**转义**成 `\~`，只有 Slint 认这个转义才不会漏出反斜杠
        //（评审 finding：`~` 曾是 `push_escaped` 的漏网元字符）。
        ("p", tilde, 100.0),
    ];
    // 量一条分支：`true` = 生产口径（可选中），`false` = 旧分支（`selectable` 关掉）。
    let measure = |kind: &str, text: &str, selectable: bool| {
        let mut r = row(kind, text);
        if selectable {
            assert!(r.selectable, "{kind} 无行内样式，生产必须判定为可选中");
        } else {
            r.selectable = false;
        }
        let (ui, win, _) = setup(vec![role_row("怹", true), r], TALL_H);
        let f = settle(&ui, &win, TALL_H);
        (ui.get_dbg_content_h(), f)
    };
    for (kind, text, min_h) in cases {
        let (h_new, f_new) = measure(kind, &text, true);
        let (h_old, f_old) = measure(kind, &text, false);
        let diff = f_new.diff_px(&f_old);
        println!(
            "{kind}: 可选中 content.h={h_new} / 旧分支 {h_old} / 差异像素 {diff} / 帧内墨迹行数 {}",
            (0..TALL_H)
                .filter(|&y| !f_new.ink_cols(y, 4, W - 4).is_empty())
                .count()
        );
        assert!(
            (h_new - h_old).abs() <= 1.0,
            "{kind} 换渲染元素不该改高度：TextInput={h_new} vs 旧分支={h_old}"
        );
        assert_eq!(
            diff, 0,
            "{kind} 换渲染元素必须**逐像素**一致（差异 {diff} 像素）：这是「观感零变化」的直接证据"
        );
        // 注：这里**不**拿「最底墨迹行号」当判据——它恒等于窗底 chrome 那一行（两分支都是
        // 2587），是**恒真**断言（独立评审 finding）。逐帧像素差是严格更强的判据，故用它。
        assert!(
            h_new >= min_h,
            "{kind} 正面控制：内容应真的多行（量到 {h_new}，期望 ≥ {min_h}）"
        );
    }
}

/// 有行内样式的块**必须继续走 `StyledText`**（`TextInput` 渲染不了粗体/链接/行内码/删除线）——
/// 这是 B 批已交付能力的回归守卫：宁可不可选中，也不许把样式画没。
///
/// 断言口径：入参一律取 `parse` 之后的**块文本**（= 生产 `block_row` 的入参），
/// 所以这条同时覆盖「解析器的转义 → `styled()` 渲染」这一整段。
///
/// 判别力（负向）：让 `selectable_plain` 忽略行内样式（恒 `Some`）⇒ 第一段断言红；
/// 让它漏认 `~`（评审 finding 的原样）⇒ 删除线那例红（实测两分支差异 920 像素）。
#[test]
fn styled_paragraph_stays_on_styled_path_and_still_renders() {
    // 每种样式各一例：粗体/行内码/链接/删除线（单波浪与双波浪都会被解析器重发成 `~~`）。
    // `bool` = 该例的解析结果是否**必须**与原文不同（含字面量波浪号 ⇒ 应被转义）。
    // 只有为 true 的例子才能真正验到「转义不许漏出反斜杠」——其余例子的参考帧与生产帧
    // 是同一份输入，属于「画得出来」的检查。
    for (md, must_differ_from_source) in [
        (
            "这段有 **粗体**、`行内码` 与 [链接](https://example.com)。",
            false,
        ),
        ("这段有 ~~删除线~~。", false),
        ("这段有 ~单波浪删除线~。", false),
        ("这段有 **粗体** 与 ~/bin 与 3~4 元。", true),
    ] {
        let b = crate::mdblocks::parse(md);
        assert_eq!(b.len(), 1, "用例前提：单段（{md:?}）");
        if must_differ_from_source {
            assert!(
                b[0].text.contains(r"\~") && b[0].text != md,
                "该例必须真的把字面量波浪号转义掉（否则下面那次比较是空判据）：{:?}",
                b[0].text
            );
        }
        let r = row("p", &b[0].text);
        assert!(
            !r.selectable,
            "含行内样式的段落必须继续走 StyledText（否则样式会被 TextInput 画掉）：{md:?} → {:?}",
            b[0].text
        );
        let (ui, win, _) = setup(vec![role_row("怹", true), r], TALL_H);
        let f = settle(&ui, &win, TALL_H);
        assert!(
            f.lowest_ink_row(4, TALL_H - 4, 4, W - 4).is_some(),
            "走 StyledText 的段落仍必须画出来（不能被新的 if 分支吞掉）：{md:?}"
        );
        // 与「没有换来换去」的参考帧逐像素一致：参考帧直接喂**未转义**原文给 `StyledText`。
        // 不一致就说明我们新加进 `push_escaped` 的转义在渲染侧漏了（例如 `\~` 被当成字面量）。
        let reference = row("p", md);
        assert!(!reference.selectable, "参考帧也走 StyledText");
        let (ui_ref, win_ref, _) = setup(vec![role_row("怹", true), reference], TALL_H);
        let f_ref = settle(&ui_ref, &win_ref, TALL_H);
        assert_eq!(
            f.diff_px(&f_ref),
            0,
            "解析后的转义文本与原文必须渲染成同一张图（转义不许漏出反斜杠）：{md:?}"
        );
    }
}

/// 代码块恒可选中，且正文**逐字**保留——复制代码是本卡第一场景，
/// 而代码原文在解析时**不转义**（拿元字符判样式会误判，反解转义会吃掉反斜杠）。
#[test]
fn code_block_row_is_selectable_and_keeps_text_verbatim() {
    let code = "ls *.rs | grep _tmp\nC:\\Users\\x\\a.txt\necho `<b>[x]</b>`";
    let r = row("code", code);
    assert!(r.selectable, "代码块必须可选中");
    assert_eq!(
        r.plain.as_str(),
        code,
        "代码正文必须逐字保留（不掉反斜杠/不吞星号）"
    );
}

/// 动作行（`kind == "acts"`）真的参与布局（撑出高度），且「朗读」按钮的字面
/// 由 `speaking-turn` 驱动：命中本行 ⇒ 「⏹ 停止朗读」，否则 「🔊 朗读」。
///
/// 判别力（负向）：`.slint` 里把 `if b.kind == "acts"` 写成恒假 ⇒ 高度不增 ⇒ 红；
/// 把三元里的 `root.speaking-turn` 写成常量 ⇒ 两种情况像素相同 ⇒ 红。
#[test]
fn acts_row_draws_and_its_speak_label_tracks_speaking_turn() {
    const ASSISTANT_TURN: i32 = 1;
    let text = "一条短的助手回答。";
    let without = vec![role_row("怹", true), selectable_p(text)];
    let with = vec![
        role_row("怹", true),
        selectable_p(text),
        acts_row(ASSISTANT_TURN),
    ];

    let (ui0, win0, _) = setup(without, TALL_H);
    let _ = settle(&ui0, &win0, TALL_H);
    let h_without = ui0.get_dbg_content_h();

    let (ui, win, _) = setup(with, TALL_H);
    let f_idle = settle(&ui, &win, TALL_H);
    let h_with = ui.get_dbg_content_h();
    assert!(
        h_with > h_without + 20.0,
        "动作行必须真的占位（无动作行 {h_without} vs 有 {h_with}）"
    );

    // 「朗读」态：本行的轮号 = 正在朗读的轮号。
    ui.set_speaking_turn(ASSISTANT_TURN);
    let f_speaking = settle(&ui, &win, TALL_H);
    // 别的轮在朗读 ⇒ 本行仍是「🔊 朗读」（按钮不该全窗一起变）。
    ui.set_speaking_turn(ASSISTANT_TURN + 1);
    let f_other = settle(&ui, &win, TALL_H);
    ui.set_speaking_turn(-2);
    let f_back = settle(&ui, &win, TALL_H);

    assert!(
        f_idle.diff_px(&f_speaking) > 0,
        "朗读态必须改变这一行的像素（按钮字面变「⏹ 停止朗读」）"
    );
    assert!(
        f_idle.diff_px(&f_other) == 0,
        "别轮在朗读时本行不该变（否则说明三元没按轮号区分）"
    );
    assert!(
        f_idle.diff_px(&f_back) == 0,
        "停止朗读后必须回到原样（-2 = 没有朗读在跑）"
    );
}

/// 三个动作按钮真的**收得到点击**，且把**本行的轮序号**带出来（`copy_turn(1)`）。
///
/// 判据不靠坐标表：从像素上取动作行**第一个按钮的下边框**（它的墨迹宽度 = 按钮整宽），
/// 在它上方 4px 处按下/抬起 —— 无头台没有「用户点到哪儿」之外的输入方式，
/// 所以这条同时验证了 `.slint` 的命中路由与 `b.turn` 的传参。
///
/// 判别力（负向）：把按钮里的 `root.copy_turn(b.turn)` 改成常量（如 `-1`）⇒ 断言收到的轮号失败。
#[test]
fn acts_buttons_route_clicks_with_their_turn() {
    const ASSISTANT_TURN: i32 = 1;
    let rows = vec![
        role_row("怹", true),
        selectable_p("一条短的助手回答。"),
        acts_row(ASSISTANT_TURN),
    ];
    let (ui, win, _) = setup(rows, TALL_H);
    let hits: Rc<RefCell<Vec<(String, i32)>>> = Rc::new(RefCell::new(Vec::new()));
    {
        let h = hits.clone();
        ui.on_copy_turn(move |t| h.borrow_mut().push(("copy".into(), t)));
    }
    {
        let h = hits.clone();
        ui.on_regen_turn(move |t| h.borrow_mut().push(("regen".into(), t)));
    }
    {
        let h = hits.clone();
        ui.on_speak_turn(move |t| h.borrow_mut().push(("speak".into(), t)));
    }

    let f = settle(&ui, &win, TALL_H);
    let fy = ui.get_dbg_flick_y().round() as u32;
    let fh = ui.get_dbg_flick_h().round() as u32;
    // 动作行是内容最底部 ⇒ 整窗最底一行的墨迹就是第一个按钮的下边框。
    let y_border = f
        .lowest_ink_row(fy, fy + fh, 4, W - 4)
        .expect("正面控制失败：看不到动作行");
    let cols = groups(&f.ink_cols(y_border, 4, W - 4));
    let (bx0, bx1) = cols.first().copied().expect("下边框行应有墨迹列");
    assert!(
        bx1 - bx0 > 20,
        "判据构造失败：第一个按钮的下边框应有一条横向墨迹（量到 {}px）",
        bx1 - bx0
    );
    press_release(&win, ((bx0 + bx1) / 2) as f32, (y_border - 4) as f32);

    let got = hits.borrow().clone();
    assert_eq!(
        got,
        vec![("copy".to_string(), ASSISTANT_TURN)],
        "第一个按钮（⧉ 复制）必须把本行轮号带出来"
    );
}
/// 卡 d9 回归守卫：表情面板 /「更多」菜单**能真的渲染出来**，且锚点
/// （`composer.absolute-position`）不会触发绑定环 / 运行时 panic。
///
/// 判据：开/关两帧的**差异像素**必须存在，且差异区的最上一行在输入区**上方**
/// （面板确实画在输入框上方，不是把输入框改了）。两个面板各验一次。
///
/// 判别力（负向）：把 `.slint` 里的 `if root.emoji-open:` 写成恒假（或删掉面板）⇒ 差异为 0 ⇒ 红。
#[test]
fn emoji_and_more_panels_render_above_composer() {
    // 差异像素数 + 最上一行差异的差异区（`None` = 两帧完全一样）。
    fn frame_diff_top(a: &Frame, b: &Frame, h: u32) -> (usize, Option<u32>) {
        let mut n = 0usize;
        let mut top: Option<u32> = None;
        for y in 0..h {
            for x in 0..W {
                let (p, q) = (a.px(x, y), b.px(x, y));
                if (p.red, p.green, p.blue, p.alpha) != (q.red, q.green, q.blue, q.alpha) {
                    n += 1;
                    top = top.or(Some(y));
                }
            }
        }
        (n, top)
    }

    let (ui, win, _) = setup(
        vec![role_row("怹", true), row("p", "面板渲染守卫")],
        SHORT_H,
    );
    // 按生产 `refresh_emoji_panel` 的口径灌内容（分类 + 一整页格子）。
    ui.set_emoji_cats(slint::ModelRc::from(Rc::new(slint::VecModel::from(vec![
        slint::SharedString::from("常用"),
        slint::SharedString::from("手势"),
    ]))));
    ui.set_emoji_cells(slint::ModelRc::from(Rc::new(slint::VecModel::from(
        crate::emoji::page_cells(0, 0)
            .into_iter()
            .map(slint::SharedString::from)
            .collect::<Vec<_>>(),
    ))));
    ui.set_emoji_cat_index(0);
    ui.set_emoji_page_label("1/2".into());
    let closed = settle(&ui, &win, SHORT_H);

    ui.set_emoji_open(true);
    let emoji_open = settle(&ui, &win, SHORT_H);
    let (n_emoji, top_emoji) = frame_diff_top(&closed, &emoji_open, SHORT_H);
    assert!(
        n_emoji > 500,
        "打开表情面板没有画出差异像素（{n_emoji}）：面板没渲染出来"
    );
    let top_emoji = top_emoji.expect("差异区应有最上一行");
    assert!(
        top_emoji < SHORT_H - 150,
        "表情面板的最上一行在 y={top_emoji}（窗口高 {SHORT_H}）——面板没画在输入框上方"
    );
    ui.set_emoji_open(false);

    // 「更多」菜单：同样灌上档位与一条待发。
    ui.set_sched_preset_labels(slint::ModelRc::from(Rc::new(slint::VecModel::from(vec![
        slint::SharedString::from("5 分钟后"),
        slint::SharedString::from("30 分钟后"),
        slint::SharedString::from("今天 18:00"),
    ]))));
    ui.set_sched_items(slint::ModelRc::from(Rc::new(slint::VecModel::from(vec![
        slint::SharedString::from("10-14 18:00 · 待发测试"),
    ]))));
    ui.set_sched_count(1);
    ui.set_sched_cancel_all_label("取消全部定时（1）".into());
    let before_more = settle(&ui, &win, SHORT_H);
    ui.set_more_open(true);
    let more_open = settle(&ui, &win, SHORT_H);
    let (n_more, top_more) = frame_diff_top(&before_more, &more_open, SHORT_H);
    assert!(n_more > 500, "打开「更多」菜单没有画出差异像素（{n_more}）");
    let top_more = top_more.expect("差异区应有最上一行");
    assert!(
        top_more < SHORT_H - 150,
        "「更多」菜单的最上一行在 y={top_more}（窗口高 {SHORT_H}）——没画在输入框上方"
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
    // v11（卡 `abb-v11-resizable-split`）：消息区与输入区**之间**多一条 10px 分割条（稿 .split）。
    // v11c（owner 真机复查）：这一带只能有**一条**线——原来是
    // ① 分割条顶边 ② 中间 38×2 抓手 ③ 输入区自己的 border-top，三条叠在 10px 里。
    // 现在只留 ①（分割条顶边），所以 662 之后下一个整宽暗行**直接是窗口底边**。
    assert_eq!(
        g.get(3).copied(),
        Some((662, 663)),
        "分割条顶边线（稿 .split{{border-top:2px}}）；它就在消息区底边下，\
         且是本带唯一的一条线"
    );
    assert_eq!(
        g.get(4).copied(),
        Some((818, 819)),
        "输入区不许再画自己的顶边（v11c 单线口径）：662 之后下一个整宽暗行应是窗口底边。\
         输入区总高仍由 zone 自己的 padding 锁定：input zone 从 672 起，框内 10 + 文本区 66 \
         + 间距 8 + 功能区 30 + 10 = 124"
    );
    assert_eq!(
        g.len(),
        5,
        "整窗只应有 5 条整宽暗行（顶边框/标题栏底/状态栏底/分割条顶边/底边框），\
         多出来的就是「分割条又多画了线」；实际 {g:?}"
    );
    // 带里除了那条顶边、**一行黑描边都不能有**（抓手已不再画；输入区顶边也已删）。
    assert!(
        bbox(&f, 6, 664, MW - 6, 674, is_dark).is_none(),
        "10px 分割条带里除顶边线外还有墨迹（v11c 口径：只保留那一条线）"
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
    // v11c：输入区顶边线已删 ⇒ 不能再拿「整宽暗行」定位输入区（`g` 里已经在 662 之后直接到
    // 窗口底边），改用窗口自报的 `dbg-convo-zone-y`。
    let zone_top = ui.get_dbg_convo_zone_y().round() as u32;
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

    // 卡 `abb-v11-message-actions`：助手轮动作行（`⧉ 复制 / ⟳ 重新生成 / 🔊 朗读`）
    // 与朗读态（该行按钮变 `⏹ 停止朗读`）。用**生产口径的行构成**（助手轮正文 + 动作行）。
    let (ui, win, _m) = setup(
        vec![
            role_row("你", false),
            selectable_p("帮我把导出脚本改成按天分片。"),
            role_row("怹", true),
            selectable_p("改好了：按日期切分文件名，并在失败时留一份日志。"),
            row("code", "for d in $(seq 1 7); do\n  echo \"$d\"\ndone"),
            acts_row(-1),
        ],
        SHORT_H,
    );
    ui.window().set_size(slint::PhysicalSize::new(520, SHORT_H));
    apply_syschat_layout(&ui, 520.0);
    let f = settle_w(&win, 520, SHORT_H);
    write_png(&f, &dir.join("acts-idle.png"));
    ui.set_speaking_turn(-1);
    let f = settle_w(&win, 520, SHORT_H);
    write_png(&f, &dir.join("acts-speaking.png"));
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

// ─────────────────── D6：标题栏（三颗窗口按钮 / 拖动 / 控件条）的**命中路由**守卫 ───────────────────
//
// 「布局在、点了没反应」必须先把责任切开：是 **Slint 命中路由**没把按下送到按钮（
// `titlebar_window_buttons_receive_clicks` 等），还是送到了、而**平台动作**
// （miniaturize / zoom / drag_window）在无边框窗上是空操作（拖动那条只能给间接判据，见
// `titlebar_drag_is_not_counted_as_click`）。手段：事件注入
// （`Window::dispatch_event_with_result`，同 slint-pixel 的 `tests/title_bar_native_move.rs`）。

/// 标题栏三颗窗口按钮的**命中中心**（绝对窗口坐标，逻辑 px）。
///
/// 几何来源（不是估算）：根 `VerticalLayout` padding 2px ⇒ `PixelTitleBar` 占
/// x 2..w−2、y 2..36；其内部按钮组是 `x: parent.width − 106px; y: 4px; width: 102px;
/// height: 26px` ⇒ 绝对 y 6..32，绝对左缘 = 2 + (w − 4 − 106) = w − 108。
/// 3×30px 按钮 + 2×6px 间隔 = 102px 恰好铺满，`alignment: end` 无余量。
fn titlebar_button_centers(w: u32) -> [(&'static str, f32, f32); 3] {
    let left = (w - 108) as f32;
    let y = 19.0; // 6 + 26/2
    [
        ("min", left + 15.0, y),
        ("max", left + 30.0 + 6.0 + 15.0, y),
        ("close", left + 2.0 * (30.0 + 6.0) + 15.0, y),
    ]
}

fn press(win: &Rc<MinimalSoftwareWindow>, x: f32, y: f32) {
    use slint::platform::{PointerEventButton, WindowEvent};
    win.window()
        .dispatch_event_with_result(WindowEvent::PointerPressed {
            position: slint::LogicalPosition::new(x, y),
            button: PointerEventButton::Left,
        })
        .expect("派发 PointerPressed");
}

fn release(win: &Rc<MinimalSoftwareWindow>, x: f32, y: f32) {
    use slint::platform::{PointerEventButton, WindowEvent};
    win.window()
        .dispatch_event_with_result(WindowEvent::PointerReleased {
            position: slint::LogicalPosition::new(x, y),
            button: PointerEventButton::Left,
        })
        .expect("派发 PointerReleased");
}

fn press_release(win: &Rc<MinimalSoftwareWindow>, x: f32, y: f32) {
    press(win, x, y);
    release(win, x, y);
}

/// 建一个 520 宽的对话窗，把四个标题栏回调接到计数器上。
fn setup_with_click_counters() -> (
    SystemChatWindow,
    Rc<MinimalSoftwareWindow>,
    Rc<std::cell::RefCell<Vec<&'static str>>>,
) {
    const CW: u32 = 520;
    let (ui, win, _) = setup(vec![row("p", "占位")], SHORT_H);
    ui.set_session_labels(slint::ModelRc::from(Rc::new(slint::VecModel::from(vec![
        slint::SharedString::from("新会话"),
    ]))));
    ui.set_model_labels(slint::ModelRc::from(Rc::new(slint::VecModel::from(vec![
        slint::SharedString::from("deepseek-chat"),
    ]))));
    ui.set_session_index(0);
    ui.set_model_index(0);
    ui.window().set_size(slint::PhysicalSize::new(CW, SHORT_H));
    super::apply_syschat_layout(&ui, CW as f32);
    let log: Rc<std::cell::RefCell<Vec<&'static str>>> = Rc::new(std::cell::RefCell::new(vec![]));
    {
        let l = log.clone();
        ui.on_minimize(move || l.borrow_mut().push("min"));
    }
    {
        let l = log.clone();
        ui.on_toggle_maximize(move || l.borrow_mut().push("max"));
    }
    {
        let l = log.clone();
        ui.on_close_window(move || l.borrow_mut().push("close"));
    }
    {
        let l = log.clone();
        ui.on_drag_start(move || l.borrow_mut().push("drag"));
    }
    // 画一帧，确保布局/属性都已落地（事件注入前窗口必须已 realize）。
    let _ = settle_w(&win, CW, SHORT_H);
    (ui, win, log)
}

/// D6 主判据：标题栏三颗窗口按钮**必须收到点击**（Slint 命中路由层）。
#[test]
fn titlebar_window_buttons_receive_clicks() {
    let (_ui, win, log) = setup_with_click_counters();
    for (name, x, y) in titlebar_button_centers(520) {
        press_release(&win, x, y);
        assert_eq!(
            log.borrow().last().copied(),
            Some(name),
            "点 {name} 按钮（{x},{y}）没有触发对应回调，log={:?}",
            log.borrow()
        );
    }
    assert_eq!(
        log.borrow().len(),
        3,
        "三颗按钮应各触发一次，实际 {:?}",
        log.borrow()
    );
}

/// 对照：标题栏空白处（`WindowMoveArea` 覆盖区）按下**不应**触发 drag-start
/// （native-move 打开时上游刻意不发它，避免与平台移动重复）；这条同时说明空白处没有被
/// 控件条/缩放热区之类的东西吃掉成别的回调。
#[test]
fn titlebar_blank_area_is_window_move_area() {
    let (_ui, win, log) = setup_with_click_counters();
    // 标题文字右侧、控件条左侧的空白：520 宽下 strip_x≈194，取 x=170（避开 logo/标题墨迹与控件）。
    press_release(&win, 170.0, 19.0);
    assert_eq!(
        log.borrow().last().copied(),
        None,
        "标题栏空白处不该触发任何窗口控制回调（native-move 由平台处理），实际 {:?}",
        log.borrow()
    );
}

/// 控件条（①②③④）压在标题栏上层，它的命中面**不许**吃掉三颗窗口按钮的点击。
///
/// 判据（双向）：点窗口按钮 ⇒ 控件条回调**一次都不许响**；点 ①新建会话 ⇒ 它必须响。
/// 只看几何（`titlebar_strip_does_not_overlap_title_or_buttons`）会漏掉「条本身比画的宽
/// （例如旧别名 `HorizontalBox` 自带 8px padding）」这类**看不见的命中面**外扩。
#[test]
fn titlebar_strip_does_not_steal_window_button_clicks() {
    let (ui, win, log) = setup_with_click_counters();
    let news = Rc::new(std::cell::Cell::new(0usize));
    {
        let n = news.clone();
        ui.on_new_session(move || n.set(n.get() + 1));
    }
    // ① 点三颗窗口按钮：`new-session` 不许被顺带触发。
    for (name, x, y) in titlebar_button_centers(520) {
        press_release(&win, x, y);
        assert_eq!(
            news.get(),
            0,
            "点 {name} 按钮时控件条（①新建会话）也被触发了：命中面外扩到窗口按钮区了"
        );
    }
    // ② 正面控制：点 ①新建会话必须真的触发（否则「没被吃」只是因为条本身没命中面）。
    let strip_x = ui.get_titlebar_strip_x().round() as f32;
    assert!(strip_x > 0.0, "控件条没摆上（strip_x={strip_x}）");
    press_release(&win, strip_x + crate::syschat::TB_NEW_W / 2.0, 19.0);
    assert_eq!(
        news.get(),
        1,
        "点 ①新建会话 没触发 new-session（控件条自己不可点）"
    );
    assert_eq!(
        log.borrow().len(),
        3,
        "点控件条不该触发窗口控制回调，实际 {:?}",
        log.borrow()
    );
}

fn move_to(win: &Rc<MinimalSoftwareWindow>, x: f32, y: f32) {
    use slint::platform::WindowEvent;
    win.window()
        .dispatch_event_with_result(WindowEvent::PointerMoved {
            position: slint::LogicalPosition::new(x, y),
        })
        .expect("派发 PointerMoved");
}

/// D6 拖动判据：标题栏空白处「按住拖过阈值」必须由**窗口移动**这条路吃掉，不许退化成点击。
///
/// 判据是一对（单看一半没有判别力）：
/// - **对照 F**：同一位置两次普通点击 ⇒ `double-clicked` 恰好触发一次 `toggle-maximize`
///   （证明这个坐标上的按下**真的**进了 `PixelTitleBar` 的子树 —— 没有别的面把按压吃掉）；
/// - **手势 D**：两次「按下 → 拖过阈值（20px > 8px）→ 拖回原位 → 松开」⇒ `toggle-maximize`
///   **一次都不许响**。拖着走还被算成点击，就是「拖动没被平台移动层拦下」。
///
/// 为什么需要它：`PixelTitleBar` 的拖拽在 `native-move: true` 下走
/// `WindowMoveArea` → `WindowAdapterInternal::start_window_move` → winit `drag_window()`，
/// 而这条链在无头环境里没有可断言的出口（`TestingWindow::window_move_request_count()` 需要
/// `i-slint-backend-testing` 的 `internal` feature，本仓不引该依赖）。但 `WindowMoveArea`
/// 越阈值时返回 `Intercept` 会给已持有 grab 的子 `TouchArea` 发 `Exit`（子 `pressed` 被清）
/// ⇒ 这次手势不再计入点击序列 —— 这正是上游 `slint-pixel` 用的间接判据。
///
/// 判别力实测（2026-10-14，本用例在两种配置下各跑一次）：
/// - 生产配置（`native-move: true`）：D → `[]`（点击被拦下）、F → `["max"]`；
/// - 变异 `native-move: false`：D → `["drag","drag","max"]` **报红**（点击没被拦下）。
/// 反向也成立：把 `PixelWindowResize` 热区挪到标题栏上（或让控件条盖住空白区），
/// 对照 F 会先红（按压根本没进标题栏）。
#[test]
fn titlebar_drag_is_not_counted_as_click() {
    let (_ui, win, log) = setup_with_click_counters();
    let blank_x = 170.0f32; // 标题文字右侧、控件条左侧的空白（标题栏内、非按钮、非控件条）
    let y = 19.0f32;

    // 手势 D：按下 → 越阈值 → 拖回 → 松在原位（松开点落在标题栏内 ⇒ 没被拦下就是一"点击"）。
    // ⚠️ 四步必须是**独立的四次派发**：按下即松开只是普通点击，量不到任何东西。
    for _ in 0..2 {
        press(&win, blank_x, y);
        move_to(&win, blank_x + 20.0, y);
        move_to(&win, blank_x, y);
        release(&win, blank_x, y);
    }
    assert!(
        !log.borrow().contains(&"max"),
        "两次「按住拖动」被算成了点击（双击 ⇒ toggle-maximize 响了）：窗口移动没有被发起。\
         log={:?}",
        log.borrow()
    );
    assert!(
        log.borrow().is_empty(),
        "标题栏空白处的拖动不该触发任何窗口控制回调（native-move 由平台处理），实际 {:?}",
        log.borrow()
    );

    // 对照 F：同一位置的两次普通点击必须触发一次 toggle-maximize
    // —— 否则「拖动没被算成点击」只是因为按压压根没进标题栏（假绿）。
    press_release(&win, blank_x, y);
    press_release(&win, blank_x, y);
    assert_eq!(
        log.borrow().iter().filter(|k| **k == "max").count(),
        1,
        "对照失败：同位置两次普通点击应恰好触发一次 toggle-maximize，实际 log={:?}",
        log.borrow()
    );
}

/// 热区几何旁证（**非自管口径** = Windows/Linux）：热区确实会吃掉标题栏顶部那一条。
///
/// 几何：热区是绝对 y 0..6 的整宽条（`height: 6px`），而标题栏占 y 2..36 ⇒ 标题栏顶 4px
/// 归热区；左右两条 6px 整高条同理压在标题栏两端。热区按下即 `begin-resize(dir)` 并 grab，
/// 内容再也拿不到这次按压。
///
/// 这是一条**取舍**（不是 bug）：无边框窗的边角缩放与标题栏顶部那 4px 只能二选一。改前的
/// D6 结论是「macOS 别画热区、交给系统」，2026-10-15 实测更正为：**透明**无边框窗在 macOS
/// 拿不到系统边角缩放（取证见 `docs/syschat-handover.md` §5.1）⇒ 对话窗**恒画**热区、
/// 走自管缩放（见 `self_managed_hit_zones_route_press_with_pointer_coords`）；
/// 只有设置窗仍按平台关掉——它的热区接的是
/// `winit::Window::drag_resize_window` **直接返回 `NotSupported`**
///（winit 0.30.13 `src/platform_impl/macos/window_delegate.rs:1179`），
/// 在 macOS 上「吃按压 + 什么都不做」。
/// Rust 侧口径由 `ui::tests::resize_hit_areas_follow_platform_rule` 守着。
#[test]
fn resize_hot_zone_eats_titlebar_top_band() {
    let (ui, win, _log) = setup_with_click_counters();
    let dirs = Rc::new(std::cell::RefCell::new(Vec::<String>::new()));
    {
        let d = dirs.clone();
        ui.on_begin_resize(move |dir| d.borrow_mut().push(dir.to_string()));
    }
    ui.set_resize_hit_areas(true);
    let _ = settle_w(&win, 520, SHORT_H);
    // 标题栏绝对 y 2..36；y=4 落在热区的 north 条（0..6）里。
    press(&win, 170.0, 4.0);
    release(&win, 170.0, 4.0);
    assert_eq!(
        dirs.borrow().as_slice(),
        ["north"],
        "标题栏顶部那条没有落到 north 缩放热区上（{dirs:?}）——本用例的几何前提变了，         请重新核对标题栏与热区的落位"
    );
}

/// **自管口径**（= 对话窗在 macOS 上的生产口径）必须把「方向 + 按下时的窗口内指针坐标」
/// 交给 Rust，而不是发系统 `begin-resize`。
///
/// 为什么这条必须存在：自管缩放的**锚点**就取自这组坐标——只报方向的老口径会让自管循环
/// 等第一次 `CursorMoved` 才补锚（快甩手时窗口边恒定落后指针十几像素，不是抖动）。
/// 它与 `resize_hot_zone_eats_titlebar_top_band`（非自管）互为对照：同一套几何、两条路由，
/// 由 `resize-self-managed` 二选一，**不得同时触发**。
///
/// 无头可跑的理由：热区是 `.slint` 里的 `ResizeZone`，`self.absolute-position + mouse-x/y`
/// 在最小软件窗适配器里同样成立；needs 真机的那部分（winit 屏幕坐标 + `set_size`）由
/// `syschat::SelfResize` 的纯逻辑用例覆盖。
///
/// 与 `v11c_resize_zones_hit_all_edges_and_corners`（同一套几何、扫 8 个方向）的分工：那条钉
/// 「八个方向的**路由**（自管/系统二选一）+ ±1 的窗口坐标」；本条钉**精确相等**与**反向对照**
/// ——内容区中部不得被吞（那条只取 8 个边/角点，扫不到「整窗 overlay」这类回归）。变异自验
/// （2026-10-15）：①`ResizeZone` 改成一律 `root.begin(dir)` ⇒ 两条都红；②只把 `south-east`
/// 角块缩到 4×4 并贴到最角上（按下点 (513,613) 不再落块内）⇒ **只有本条红**（那条的角点
/// (517,617) 仍在块内）——这正是本条不可省的理由。
#[test]
fn self_managed_hit_zones_route_press_with_pointer_coords() {
    const CW: u32 = 520;
    const CH: u32 = 620;
    let (ui, win, _log) = setup_with_click_counters();
    ui.window().set_size(slint::PhysicalSize::new(CW, CH));
    super::apply_syschat_layout(&ui, CW as f32);
    let got = Rc::new(std::cell::RefCell::new(Vec::<(String, f32, f32)>::new()));
    {
        let g = got.clone();
        ui.on_self_resize_begin(move |dir, x, y| g.borrow_mut().push((dir.to_string(), x, y)));
    }
    let system = Rc::new(std::cell::RefCell::new(Vec::<String>::new()));
    {
        let s = system.clone();
        ui.on_begin_resize(move |dir| s.borrow_mut().push(dir.to_string()));
    }
    ui.set_resize_hit_areas(true);
    ui.set_resize_self_managed(true);
    let _ = settle_w(&win, CW, CH);

    // 右下角（`south-east`，14×14 块：x 506..520 / y 606..620）：按下点 (513,613) 应原样上报。
    press(&win, 513.0, 613.0);
    release(&win, 513.0, 613.0);
    assert_eq!(
        got.borrow().as_slice(),
        [("south-east".to_string(), 513.0, 613.0)],
        "右下角按下没有落到自管缩放热区，或坐标没有如实上报（{got:?}）"
    );
    assert!(
        system.borrow().is_empty(),
        "自管口径下不许再发系统 begin-resize（{system:?}）——两条路由必须二选一"
    );

    // 右边条（`east`，6px 整高：x 514..520）：内容区中部按下也不得被吞。
    press(&win, 517.0, 300.0);
    release(&win, 517.0, 300.0);
    assert_eq!(
        got.borrow().as_slice(),
        [
            ("south-east".to_string(), 513.0, 613.0),
            ("east".to_string(), 517.0, 300.0)
        ],
        "右边条按下没有落到 east 热区（{got:?}）"
    );

    // 反向对照：内容区中部**不是**热区（否则整窗都被吃掉，输入框点不动）。
    press(&win, 260.0, 300.0);
    release(&win, 260.0, 300.0);
    assert_eq!(
        got.borrow().len(),
        2,
        "内容区中部不该被缩放热区吃掉（{got:?}）"
    );
}

/// **设置窗**的缩放热区（2026-10-15 扩到设置窗）：同一套几何、两条路由由
/// `resize-self-managed` 二选一，**不得同时触发**。
///
/// 为什么设置窗也要有一条（而不只靠源码守卫）：设置窗此前是「macOS 不画热区」——
/// 换成「恒画 + 自管」以后，最重要的判据是**按下真的落到热区、坐标真的如实上报**
/// （自管缩放的锚点就取自它；只报方向的老口径要等第一次 `CursorMoved` 才补锚）。
/// 源码守卫只看得到「接线在」，看不到「几何/路由对不对」。
///
/// 与对话窗那条（`self_managed_hit_zones_route_press_with_pointer_coords`）同构；
/// 窗口尺寸用设置窗自己的（980×680，见 `.slint` 的 `preferred-*`）。
///
/// 这也是**本仓第一条能渲染设置窗的用例**，所以它顺带钉住了 `ImageIcon` 的整图裁剪
/// （`ui/app.slint` 里那两条 `source-clip-*`）：设置窗里的图标全是 `ImageIcon`（`Image` 当
/// 组件根 + 动态 `source`），裁剪退回 0×0 时 `i-slint-core::fit()` 出 NaN ⇒ 上游 software
/// 渲染器直接在 `settle_w` 里 panic。判据就是「能不能渲染」本身。
///
/// 判别力（负向，变异自验 2026-10-15：逐条单发、每处都编译通过 + 目标用例 FAILED + 失败文本
/// 指向变异点）：① `ImageIcon` 的两条整图裁剪删掉 ⇒ 本用例红（euclid `size.rs:335` unwrap
/// panic）；② 设置窗 overlay 换回 `PixelWindowResize` ⇒ 红（`self-resize-begin` 永不触发）；
/// ③ `self-managed` 写死 `false` ⇒ 红（按下没有实现）；④ `south-east` 角块 14×14 → 4×4
/// ⇒ 红（(973,673) 不再落块内）。接线类四处打在源码守卫上（`resize_hit_areas_follow_platform_rule`
/// / `settings_window_min_constants_match_slint`：热区开关回「macOS 不画」、钩子删 `CursorMoved`、
/// 下界改用对话窗的、`SETTINGS_MIN_W` 漂 10px ⇒ 各自那条红）。
#[test]
fn settings_self_managed_hit_zones_route_press_with_pointer_coords() {
    const SW: u32 = 980;
    const SH: u32 = 680;
    let windows = WINDOWS.with(|w| w.clone());
    let _ = slint::platform::set_platform(Box::new(HeadlessPlatform {
        windows: windows.clone(),
    }));
    let ui = SettingsWindow::new().expect("SettingsWindow 建窗失败");
    let got = Rc::new(std::cell::RefCell::new(Vec::<(String, f32, f32)>::new()));
    {
        let g = got.clone();
        ui.on_self_resize_begin(move |dir, x, y| g.borrow_mut().push((dir.to_string(), x, y)));
    }
    let system = Rc::new(std::cell::RefCell::new(Vec::<String>::new()));
    {
        let s = system.clone();
        ui.on_begin_resize(move |dir| s.borrow_mut().push(dir.to_string()));
    }
    ui.set_resize_hit_areas(true);
    ui.set_resize_self_managed(true);
    ui.show().expect("设置窗 show 失败");
    ui.window().set_size(slint::PhysicalSize::new(SW, SH));
    let win = windows.borrow().last().expect("平台没发窗口").clone();
    let _ = settle_w(&win, SW, SH);

    // 右下角（`south-east`，14×14：x 966..980 / y 666..680）：按下点原样上报（含窗口坐标）。
    press(&win, 973.0, 673.0);
    release(&win, 973.0, 673.0);
    assert_eq!(
        got.borrow().as_slice(),
        [("south-east".to_string(), 973.0, 673.0)],
        "设置窗右下角按下没有落到自管缩放热区，或坐标没有如实上报（{got:?}）"
    );
    assert!(
        system.borrow().is_empty(),
        "自管口径下不许再发系统 begin-resize（{system:?}）——两条路由必须二选一"
    );

    // 左边条（`west`，6px 整高）：中部按按下也不得被吞。
    press(&win, 2.0, 300.0);
    release(&win, 2.0, 300.0);
    assert_eq!(
        got.borrow().as_slice(),
        [
            ("south-east".to_string(), 973.0, 673.0),
            ("west".to_string(), 2.0, 300.0)
        ],
        "设置窗左边条按下没有落到 west 热区（{got:?}）"
    );

    // 反向对照：内容区中部**不是**热区（否则整窗都被吃掉，各页控件点不动）。
    press(&win, 500.0, 300.0);
    release(&win, 500.0, 300.0);
    assert_eq!(
        got.borrow().len(),
        2,
        "设置窗内容区中部不该被缩放热区吃掉（{got:?}）"
    );

    // 系统口径（Windows/Linux 那条路）：同一个窗、只把开关翻过去 ⇒ 必须改发 `begin-resize`
    // 且**不再**发自管回调（否则两个平台的行为会各错一半）。
    ui.set_resize_self_managed(false);
    let _ = settle_w(&win, SW, SH);
    press(&win, 973.0, 673.0);
    release(&win, 973.0, 673.0);
    assert_eq!(
        system.borrow().as_slice(),
        ["south-east".to_string()],
        "关掉自管后，右下角必须回到系统 `begin-resize`（{system:?}）"
    );
    assert_eq!(
        got.borrow().len(),
        2,
        "关掉自管后不该再出自管回调（{got:?}）"
    );
}

/// D5 端到端：`Focused(false)` → 延时 → 判定「焦点已离开本进程」→ 窗口真的被隐藏。
///
/// 为什么能在无头环境里跑整条链：本进程（`cargo test` 的测试二进制）不是一个激活态的 App，
/// `platform::app_is_active()` 会返回 `false`（实测：`[NSApp isActive]` 在测试进程里为 false，
/// 且调用安全——`sharedApplication` 只取单例，不起 UI），所以「失焦 ⇒ 应隐藏」这一支在测试里
/// 就是**正面路径**；反面（焦点还在本进程 ⇒ 不隐藏）由 `ui::tests::blur_hide_*` 的纯函数用例覆盖。
///
/// 依赖真实墙钟：延时常量 `BLUR_HIDE_DELAY_MS` 变了本用例自动跟随（多睡 3 倍再判定），
/// 不用固定 sleep 去猜实现细节（同 `LESSON_节流类测试断言须随真实墙钟缩放`）。
#[test]
fn blur_hides_dialog_window_end_to_end() {
    // 注意：`Focused` 是 **winit** 的事件（Slint 的事件枚举里没有它）——
    // 失焦隐藏接的是 winit 的窗口事件钩子（`Window::on_winit_window_event`）。
    use slint::winit_030::winit::event::WindowEvent;
    let (ui, win, _) = setup(vec![row("p", "占位")], SHORT_H);
    assert!(ui.window().is_visible(), "前置控制失败：窗口没 show 上");

    let timer = slint::Timer::default();
    let empty_bubble: slint::Weak<AnchorBubble> = slint::Weak::default();
    let empty_settings: slint::Weak<SettingsWindow> = slint::Weak::default();
    let cw = ui.as_weak();
    // 朗读器：起一个 `/bin/sleep` 当在跑的朗读（不真出声），验「失焦隐藏也把朗读停掉」。
    let speaker: Rc<RefCell<crate::platform::Speaker>> =
        Rc::new(RefCell::new(crate::platform::Speaker::new()));
    assert!(
        speaker.borrow_mut().spawn_with("/bin/sleep", "30"),
        "前置控制失败：起不了替身朗读进程"
    );
    ui.set_speaking_turn(3);
    assert!(
        super::handle_syschat_focus_event(
            &WindowEvent::Focused(false),
            &timer,
            &cw,
            &empty_bubble,
            &empty_settings,
            &speaker,
        ),
        "Focused(false) 应被失焦隐藏接线消费"
    );

    // 焦点又回来：待执行的隐藏必须被取消（否则「点一下别处再点回来」会把窗口藏掉）。
    super::handle_syschat_focus_event(
        &WindowEvent::Focused(true),
        &timer,
        &cw,
        &empty_bubble,
        &empty_settings,
        &speaker,
    );
    std::thread::sleep(std::time::Duration::from_millis(
        super::BLUR_HIDE_DELAY_MS * 3,
    ));
    slint::platform::update_timers_and_animations();
    assert!(
        ui.window().is_visible(),
        "Focused(true) 之后待执行的隐藏没被取消：窗口被藏掉了"
    );
    assert!(
        speaker.borrow_mut().is_speaking(),
        "隐藏被取消时朗读不该被停掉（只有真的隐藏才停）"
    );

    // 再失焦：这次必须真的隐藏。
    super::handle_syschat_focus_event(
        &WindowEvent::Focused(false),
        &timer,
        &cw,
        &empty_bubble,
        &empty_settings,
        &speaker,
    );
    std::thread::sleep(std::time::Duration::from_millis(
        super::BLUR_HIDE_DELAY_MS * 3,
    ));
    slint::platform::update_timers_and_animations();
    assert!(
        !ui.window().is_visible(),
        "失焦（且焦点离开本进程）后窗口没有隐藏：D5 的延时/判定链断了"
    );
    // 三条隐藏路径共用 `hide_syschat`：失焦这条也必须停朗读并复位按钮态
    //（独立评审 finding：原来只有 ✕ 那条停了朗读）。
    assert!(
        !speaker.borrow_mut().is_speaking(),
        "失焦隐藏必须把在跑的朗读停掉（否则窗口没了、声音还在放）"
    );
    assert_eq!(
        ui.get_speaking_turn(),
        -2,
        "隐藏后朗读态必须复位（否则下次开窗还显示「⏹ 停止朗读」）"
    );
    let _ = win;
}

// ─────────────────── v11（卡 `abb-v11-panels-align`）：两面板 + 用量条的对稿守卫 ───────────────────

/// 出图目录（同 D2/D3 口径；**不写工作区**）。
const SHOTS: &str = "/Volumes/DataExt/tmp/syschat-shots";

/// 三个会话（含 tag）+ 两个模型（当前项 ✓ / 其它项 provider），灌进窗口。
/// 与生产 `ui::refresh_syschat_sel` 同口径的数据形状（tag 由 `syschat::session_tag` 生成）。
fn seed_panels(ui: &SystemChatWindow) {
    // v11（卡 `abb-v11-session-archive`）：面板文案（标题/footer/两行额外项）由 Rust 写。
    // 这里用**生产同一个纯函数**（`syschat::session_panel_text`）算，而不是另抄一份字面量——
    // 抄一份就会与生产漂移，出图也证明不了生产面板长什么样。
    let copy = crate::syschat::session_panel_text(false, 0);
    ui.set_session_panel_copy(SessionPanelCopy {
        heading: copy.heading.into(),
        foot: copy.foot.into(),
        extra_text: copy.extra_text.into(),
        extra_tag: copy.extra_tag.into(),
        extra2_text: copy.extra2_text.into(),
        extra2_tag: copy.extra2_tag.into(),
    });
    ui.set_session_labels(slint::ModelRc::from(Rc::new(slint::VecModel::from(vec![
        slint::SharedString::from("新会话"),
        slint::SharedString::from("把架构图做成网页"),
        slint::SharedString::from("排查 deepseek 端点"),
    ]))));
    ui.set_session_rows(slint::ModelRc::from(Rc::new(slint::VecModel::from(vec![
        SysChatMenuRow {
            text: "新会话".into(),
            tag: "昨天 23:12".into(),
            tick: false,
        },
        SysChatMenuRow {
            text: "把架构图做成网页".into(),
            tag: "当前".into(),
            tick: false,
        },
        SysChatMenuRow {
            text: "排查 deepseek 端点".into(),
            tag: "今天 00:40".into(),
            tick: false,
        },
    ]))));
    ui.set_model_labels(slint::ModelRc::from(Rc::new(slint::VecModel::from(vec![
        slint::SharedString::from("deepseek-chat"),
        slint::SharedString::from("claude-sonnet-4.6"),
        slint::SharedString::from("gpt-5.1"),
    ]))));
    ui.set_model_rows(slint::ModelRc::from(Rc::new(slint::VecModel::from(vec![
        SysChatMenuRow {
            text: "deepseek-chat".into(),
            tag: "".into(),
            tick: true,
        },
        SysChatMenuRow {
            text: "claude-sonnet-4.6".into(),
            tag: "anthropic".into(),
            tick: false,
        },
        SysChatMenuRow {
            text: "gpt-5.1".into(),
            tag: "openai".into(),
            tick: false,
        },
    ]))));
    ui.set_session_index(1);
    ui.set_model_index(0);
}

/// v11 验收：两面板的**锚点/尺寸/结构**与用量条的**结构**都可以机读断言（不靠肉眼）。
///
/// 判据来源：
/// - 锚点：`syschat::menu_anchor`（右缘跟钮右缘对齐、顶边在钮下缘 6px、靠窗边内移）；
/// - 面板高度：稿 `.menu` = 标题(28) + 每行 30 + 归档行 30 + footer；实现按内容自然高
///   （这里用**像素上的 accent 行**与 footer 墨迹交叉校验，不信自报值）；
/// - 用量条：稿 `.meter{width:56px;height:9px}`，内条 = 内容盒 52px。
#[test]
fn v11_panels_anchor_and_usage_meter_match_design() {
    const H: u32 = 820;
    const W520: u32 = 520;
    let (ui, win, _m) = setup(vec![row("p", "占位")], H);
    seed_panels(&ui);
    // **注入**用量（空档）：`apply_syschat_usage` 不读盘，所以这条断言与开发机的真实
    // `~/.agent-bridge` / `config.json` 无关（E3 落地时踩过：读源那半会把断言的期望值
    // 变成「取决于本机有没有会话 / 有没有这个供应商」）。
    super::apply_syschat_usage(&ui, None, None);
    ui.window().set_size(slint::PhysicalSize::new(W520, H));
    apply_syschat_layout(&ui, W520 as f32);

    // ① 锚点：与钮的矩形对齐（钮矩形由同一套 `titlebar_plan` 算术给出）。
    let plan = crate::syschat::titlebar_plan(W520 as f32, true);
    let (bx, bw) = crate::syschat::titlebar_session_btn(&plan).expect("520 宽下会话钮应画");
    let (mbx, mbw) = crate::syschat::titlebar_model_btn(&plan).expect("520 宽下模型 chip 应画");
    let sx = ui.get_session_menu_x();
    let sy = ui.get_session_menu_y();
    let mxx = ui.get_model_menu_x();
    let myy = ui.get_model_menu_y();
    // 会话钮在 520 宽下已靠窗右侧（右缘 252）⇒ 右对齐会把面板推出窗（x=2 < 6），
    // 于是被夹到 6px 起（稿 `.menu` 的「靠边内移」；纯函数用例覆盖未夹取的右对齐）。
    assert_eq!(
        sx,
        (bx + bw - 250.0).max(6.0),
        "会话面板应右对齐（靠窗边则夹到 6px）"
    );
    assert_eq!(mxx + 250.0, mbx + mbw, "模型面板右缘应与模型 chip 右缘对齐");
    assert_eq!(sy, 38.0, "面板顶边 = 钮下缘(6+26) + 6");
    assert_eq!(myy, 38.0, "两个面板同一行锚定");
    assert!(
        sx >= 6.0 && sx + 250.0 <= W520 as f32 - 6.0,
        "面板不许出窗：x={sx}"
    );

    // ② 会话面板：展开后量真实几何（自报值 + 像素上的 accent 行交叉校验）。
    ui.set_session_menu_open(true);
    let f = settle_w(&win, W520, H);
    assert_eq!(ui.get_dbg_session_menu_w(), 250.0, "面板宽 = 稿的 250px");
    let h = ui.get_dbg_session_menu_h();
    assert!((150.0..400.0).contains(&h), "面板高度不合理：{h}");
    // 当前行（index=1）在像素上是 accent 底：面板内容盒宽 250−4=246、行高 30、
    // 位置 = 锚点 y + padding 2 + 标题 28 + 第 1 行 30。
    let acc =
        bbox(&f, 0, 40, 300, 300, is_accent).expect("当前会话项没有 accent 底（稿 .item.on）");
    assert_eq!(
        acc.0,
        sx as u32 + 2,
        "accent 行左缘应贴内容盒（面板描边内侧 2px）"
    );
    assert_eq!(acc.2 - acc.0 + 1, 246, "accent 行宽 = 面板宽 − 左右描边");
    assert_eq!(
        acc.1,
        sy as u32 + 60,
        "accent 行 y = 锚点 + padding2 + 标题28 + 1 行30"
    );
    assert_eq!(acc.3 - acc.1 + 1, 30, "行高 30px");
    // footer 文案（第 3 行 + 归档行之后再有一段墨迹）——防止「面板把 footer 切掉」。
    assert!(
        bbox(
            &f,
            sx as u32,
            sy as u32 + 150,
            sx as u32 + 250,
            sy as u32 + h.ceil() as u32,
            is_dark
        )
        .is_some(),
        "面板下半部没有 footer 墨迹（footer 缺失或被切）"
    );
    // 行数变少 ⇒ 面板变矮（高度真的跟内容走，不是写死的）。
    ui.set_session_rows(slint::ModelRc::from(Rc::new(slint::VecModel::from(vec![
        SysChatMenuRow {
            text: "只有一条".into(),
            tag: "当前".into(),
            tick: false,
        },
    ]))));
    let _ = settle_w(&win, W520, H);
    assert!(
        ui.get_dbg_session_menu_h() < h,
        "行数减少后面板没变矮（高度没跟内容走）"
    );
    seed_panels(&ui);
    let _ = settle_w(&win, W520, H);

    // ③ 模型面板：当前项 ✓ + 其它项 provider（结构用 accent 行 + footer 断言）。
    ui.set_session_menu_open(false);
    ui.set_model_menu_open(true);
    let f2 = settle_w(&win, W520, H);
    let macc = bbox(&f2, 0, 40, 520, 300, is_accent).expect("当前模型项没有 accent 底");
    assert_eq!(macc.0, mxx as u32 + 2, "模型面板 accent 行左缘");
    assert_eq!(macc.2 - macc.0 + 1, 246, "模型面板 accent 行宽");
    assert_eq!(
        macc.1,
        myy as u32 + 30,
        "模型面板 accent 行 = 锚点 + padding2 + 标题28（第 1 行）"
    );
    // 当前模型行**不再有右侧 provider**（右侧留给 ✓）——✓ 是墨色，故该行右侧应有墨迹。
    assert!(
        bbox(
            &f2,
            mxx as u32 + 200,
            myy as u32 + 30,
            mxx as u32 + 248,
            myy as u32 + 60,
            is_dark
        )
        .is_some(),
        "当前项右侧没有 ✓（或右侧 tag 未清空）"
    );

    // ④ 用量条结构（稿 `.tok` + `.meter`）：56×9；E3 之前无数据源 ⇒ 内条 0 宽。
    ui.set_model_menu_open(false);
    let f = settle_w(&win, W520, H);
    assert_eq!(ui.get_dbg_usage_meter_w(), 56.0, "进度条宽 = 稿的 56px");
    assert_eq!(ui.get_dbg_usage_meter_h(), 9.0, "进度条高 = 稿的 9px");
    assert_eq!(
        ui.get_dbg_usage_fill_w(),
        0.0,
        "无数据源时内条为 0（不伪造）"
    );
    assert_eq!(ui.get_usage_text(), "上下文 —/— · —");
    assert_eq!(ui.get_usage_level(), 0);

    // ④c **有数时的几何与档位**（E3 起的真实值接缝）：内条宽 = 52px × 百分比，
    // 档位按 70/90 阈值换色。这里量的是**渲染出来的属性**，不是 Rust 算式——
    // 算式本身另有 `usage_text_percent_and_level_follow_design` 单测钉住。
    // 75% ⇒ 内条 39px、橙档；93.75% ⇒ 红档；越限夹到满宽。
    super::apply_syschat_usage(&ui, Some(96_000), Some(128_000));
    assert_eq!(ui.get_usage_text(), "上下文 96K/128K · 75.0%");
    assert_eq!(ui.get_usage_level(), 1, "75% 应是橙档");
    assert_eq!(ui.get_usage_meter_fill(), 39.0, "52px × 75% = 39px");
    super::apply_syschat_usage(&ui, Some(120_000), Some(128_000));
    assert_eq!(ui.get_usage_level(), 2, "93.75% 应是红档");
    assert_eq!(ui.get_usage_meter_fill(), 48.75, "52px × 93.75% = 48.75px");
    super::apply_syschat_usage(&ui, Some(300_000), Some(128_000));
    assert_eq!(
        ui.get_usage_meter_fill(),
        52.0,
        "越限时内条夹在内容盒宽（百分比本身也夹在 100）"
    );
    // 只知上限、不知用量：还要给「上限知道」的那半句话，但**不许**画出内条（不伪造 0% 之外的进度）。
    super::apply_syschat_usage(&ui, None, Some(128_000));
    assert_eq!(ui.get_usage_text(), "上下文 —/128K · —");
    assert_eq!(ui.get_usage_meter_fill(), 0.0);
    assert_eq!(ui.get_usage_level(), 0);
    // 复位成空档，下面的像素断言与出图口径不变。
    super::apply_syschat_usage(&ui, None, None);

    // ② 第二行「模型 · 状态」仍在（稿 `.status .st2 .l2`）。
    assert!(ui.get_dbg_status_second_line(), "状态行第二行丢了");

    // ④b **垂直居中**（v11c，owner 真机复查）。
    // 现象：用量条贴在 38px 状态行的最顶上（实测条身 y 38..46，而行中心是 57，偏上 ~14px）。
    // 根因：`HorizontalLayout` 的横轴对齐默认 `stretch`，而本元素有固定 `height: 9px`
    // ⇒ `solve_box_layout_ortho` 把不伸缩的元素放在**容器顶边**（`padding.begin`）；
    // 旁边的两行文本各自 `vertical-alignment: center` 且铺满行高，所以看着就是「条子偏上」。
    // 修法：该元素 `cross-axis-self-alignment: center`（不动整行的对齐，免得回退位 chip 跟着变）。
    // 判据：像素上量**条子的真实几何**（暗描边包围盒）与状态行的中心对齐。不依赖自报 y
    // （`dbg-usage-meter-*` 只有宽高）。
    let full_rows: Vec<u32> = (0..H)
        .filter(|&y| (10..W520 - 10).all(|x| is_dark(f.px(x, y))))
        .collect();
    let g = groups(&full_rows);
    let status_top = g[1].1 + 1; // 标题栏底分隔线的下一行
    let status_bottom = g[2].1 + 1; // 状态行底分隔线的下一行（开区间）
    let (mx0, mx1) = (W520 - 70, W520 - 10); // 右边距 12 + 条宽 56（含描边）⇒ 只有条子
                                             // y 上界取**状态行底分隔线的首行**（`g[2].0`）：那条线是整宽的，把它算进来包围盒就会
                                             // 变成 60×23（描边盒不再成立）。
    let meter = bbox(&f, mx0, status_top, mx1, g[2].0, is_dark)
        .expect("状态行右侧没找到用量条的描边（位置判据失效）");
    assert_eq!(
        meter.2 - meter.0 + 1,
        56,
        "用量条宽 = 56px（描边盒）；实际 {:?}",
        meter
    );
    let meter_center = (meter.1 + meter.3 + 1) as f32 / 2.0;
    let row_center = (status_top + status_bottom) as f32 / 2.0;
    assert!(
        (meter_center - row_center).abs() <= 1.5,
        "用量条应垂直居中：条中心 {meter_center}，状态行中心 {row_center}\n         （条实量 {meter:?}，行 {status_top}..{status_bottom}）"
    );
}

/// v11 行为：两面板的**开合**——点钮展开、点面板外收起、**再点钮也收起**
/// （稿 v11 第 1 条；`PopupWindow` 的 close-on-click-outside 做不到「再点收起」，见组件注释）。
#[test]
fn v11_menu_panels_open_and_close_on_outside_or_second_click() {
    const W520: u32 = 520;
    let (ui, win, _m) = setup(vec![row("p", "占位")], SHORT_H);
    seed_panels(&ui);
    ui.window()
        .set_size(slint::PhysicalSize::new(W520, SHORT_H));
    apply_syschat_layout(&ui, W520 as f32);
    let _ = settle_w(&win, W520, SHORT_H);
    let plan = crate::syschat::titlebar_plan(W520 as f32, true);
    let (bx, bw) = crate::syschat::titlebar_session_btn(&plan).expect("会话钮");
    let (sx, sy) = (
        bx + bw / 2.0,
        crate::syschat::TB_BTN_Y + crate::syschat::TB_BTN_H / 2.0,
    );
    let (mbx, mbw) = crate::syschat::titlebar_model_btn(&plan).expect("模型 chip");
    let (mx, my) = (mbx + mbw / 2.0, sy);

    // 会话面板：开 → 再点收起 → 再开 → 点面板外收起。
    press_release(&win, sx, sy);
    assert!(ui.get_session_menu_open(), "点 🗂 应展开会话面板");
    press_release(&win, sx, sy);
    assert!(
        !ui.get_session_menu_open(),
        "再点 🗂 应收起面板（不是只重开）"
    );
    press_release(&win, sx, sy);
    assert!(ui.get_session_menu_open(), "第三次点应再次展开");
    press_release(&win, 300.0, 400.0);
    assert!(!ui.get_session_menu_open(), "点面板外应收起");

    // 模型面板：同理；另外两者互斥（开会话面板后点模型 chip 应只剩模型面板）。
    press_release(&win, mx, my);
    assert!(ui.get_model_menu_open(), "点模型 chip 应展开模型面板");
    press_release(&win, mx, my);
    assert!(!ui.get_model_menu_open(), "再点模型 chip 应收起");
    // 互斥口径（与 d9 的表情/更多一致）：面板开着时点**另一个**chip，那一下被 backdrop 吃掉
    // ⇒ 只收起（等于「点外部收起」）；再点一次才展开另一个。这不是 bug，是同一套 backdrop 口径。
    press_release(&win, sx, sy);
    assert!(ui.get_session_menu_open(), "展开会话面板");
    press_release(&win, mx, my);
    assert!(
        !ui.get_session_menu_open() && !ui.get_model_menu_open(),
        "面板开着时点另一个 chip 应先收起（backdrop 吃掉那一下）"
    );
    press_release(&win, mx, my);
    assert!(ui.get_model_menu_open(), "再点模型 chip 才展开模型面板");
}

/// 卡 `abb-v11-session-archive`：会话面板的**第二行**（「🗄 已归档（N）…」）真的画出来了。
///
/// 判据用高度差（同数据下把 `extra2_text` 置空 ⇒ 面板正好矮一行 30px）。这是无头台能自动断言
/// 的那一半——点击路径（点开面板 → 点「已归档」→ 原地换成归档列表）走的是 Rust 回调，
/// 而无头台不接生产回调，那部分由 `syschat`/`ui` 的纯逻辑单测钉住（见
/// `session_panel_text_normal_and_archive_views`、`session_menu_rows_mark_current_only_when_current_passed`）。
///
/// 判别力（负向）：把 `MenuPanel` 里那段 `if root.extra-text2 != ""` 删掉 ⇒ 高度差为 0，本条红。
#[test]
fn v11_session_panel_renders_archive_entry_row() {
    const W520: u32 = 520;
    let (ui, win, _m) = setup(vec![row("p", "占位")], SHORT_H);
    seed_panels(&ui);
    ui.window()
        .set_size(slint::PhysicalSize::new(W520, SHORT_H));
    apply_syschat_layout(&ui, W520 as f32);
    ui.set_session_menu_open(true);
    let _ = settle_w(&win, W520, SHORT_H);
    let with_row = ui.get_dbg_session_menu_h();
    assert!(with_row > 0.0, "面板几何不可用：{with_row}");

    // 只摘掉第二行，其余字段照抄生产纯函数的结果。
    let mut copy = crate::syschat::session_panel_text(false, 0);
    copy.extra2_text = String::new();
    ui.set_session_panel_copy(SessionPanelCopy {
        heading: copy.heading.into(),
        foot: copy.foot.into(),
        extra_text: copy.extra_text.into(),
        extra_tag: copy.extra_tag.into(),
        extra2_text: copy.extra2_text.into(),
        extra2_tag: copy.extra2_tag.into(),
    });
    let _ = settle_w(&win, W520, SHORT_H);
    let without_row = ui.get_dbg_session_menu_h();
    assert_eq!(
        with_row - without_row,
        30.0,
        "「🗄 已归档（N）…」那一行没画出来（{with_row} → {without_row}）；行高应为 30px"
    );
}

/// v11（卡 `abb-v11-resizable-split`）：分割条的**位置/拖动/夹取/待落盘**。
///
/// 判据分三层（自报值 → 布局真值 → 像素）：
/// 1. **位置**：分割条夹在消息区底边与输入区顶边**之间**（三者首尾相接），高 = 稿的 10px；
///    默认比例（哨兵 1.0）下输入区正好取下限 ⇒ 输入框贴着窗口底部、消息区拿满剩余；
/// 2. **拖动**：按住分割条往上拖 ⇒ 指针到哪 divider 就到哪（divider 顶边 = 指针 y）、
///    两区高度按比例重分、松手后**待落盘比例**正好是屏幕上那一份，且不会重复写盘；
/// 3. **夹取**：往上/往下拖过头分别被内容区下限（96）与输入区下限夹住；待发附件行出现
///    （输入区下限 +76）时消息区让位，输入框仍贴底。
#[test]
fn v11_split_draggable_between_convo_and_composer() {
    const W520: u32 = 520;
    const H: u32 = 820;
    let (ui, win, _m) = setup(fixture(), H);
    let split = wire_split(&ui); // 重新接一遍：本用例要拿句柄断言「待落盘比例」
    ui.window().set_size(slint::PhysicalSize::new(W520, H));
    apply_syschat_layout(&ui, W520 as f32);
    let _ = settle_w(&win, W520, H);

    let band = (H - 2) as f32 - ui.get_dbg_flick_y();
    println!(
        "split: flick y={} h={} split y={} h={} zone y={} h={} band={band}",
        ui.get_dbg_flick_y(),
        ui.get_dbg_flick_h(),
        ui.get_dbg_split_y(),
        ui.get_dbg_split_h(),
        ui.get_dbg_convo_zone_y(),
        ui.get_dbg_convo_zone_h(),
    );
    // ── 1. 位置：三块首尾相接、分割条 10px、默认比例下输入区在下限 ──
    assert_eq!(
        ui.get_dbg_flick_bottom(),
        ui.get_dbg_split_y(),
        "分割条顶边应紧贴消息区底边"
    );
    assert_eq!(
        ui.get_dbg_split_h(),
        crate::syschat::SPLIT_HANDLE_H,
        "分割条高 = 稿的 10px"
    );
    assert_eq!(
        ui.get_dbg_split_y() + ui.get_dbg_split_h(),
        ui.get_dbg_convo_zone_y(),
        "输入区顶边应紧贴分割条底边"
    );
    assert_eq!(
        ui.get_dbg_flick_h() + ui.get_dbg_split_h() + ui.get_dbg_convo_zone_h(),
        band,
        "三块之和 = 可分配带（消息区 + 分割条 + 输入区 没有第四块）"
    );
    assert_eq!(
        ui.get_dbg_convo_zone_h(),
        crate::syschat::SPLIT_COMPOSER_MIN_H,
        "默认比例（哨兵 1.0）= 输入区取下限 ⇒ 分割条紧贴输入区上沿（与稿的默认观感一致）"
    );
    // 输入框贴底：zone 内 padding-bottom 10 ⇒ 框底 = zone 底 − 10（拖动前后都必须成立）
    assert_eq!(
        ui.get_dbg_composer_y() + ui.get_dbg_composer_h(),
        ui.get_dbg_convo_zone_h() - 10.0,
        "输入框应贴着输入区底边（拖动变大时它的底边不动）"
    );
    // 默认比例下输入框/文本区就是它们的自然高（稿 `.area{min-height:66px}`）
    assert_eq!(ui.get_dbg_area_h(), 66.0);
    assert_eq!(ui.get_dbg_composer_h(), 124.0);
    assert_eq!(
        ui.get_convo_h(),
        ui.get_dbg_flick_h(),
        "Rust 写的 convo-h 必须就是布局实际用的高（没被布局改写）"
    );

    // 像素侧（v11c 单线口径，owner 真机复查）：分割条这一带**只有一条线**——顶上那 2px 的
    // 整宽线。中间不再画抓手（旧稿 `.split i`），输入区也不再画自己的顶边（两条都删了）。
    let f = settle_w(&win, W520, H);
    let split_y = ui.get_dbg_split_y() as u32;
    for y in [split_y, split_y + 1] {
        assert_eq!(
            f.ink_cols(y, 6, W520 - 6).len(),
            (W520 - 12) as usize,
            "分割条顶边线应逐列铺满（y={y}）"
        );
    }
    assert!(
        bbox(&f, 6, split_y + 2, W520 - 6, split_y + 12, is_dark).is_none(),
        "分割条带里除顶上那 2px 外还有墨迹（抓手/输入区顶边都要去掉）"
    );

    // ── 2. 拖动：divider 跟手（顶边 = 指针 y）──
    let sy = ui.get_dbg_split_y() + 5.0; // 分割条中间那一点的 y
    let cx = W520 as f32 / 2.0;
    // 2a. 按下即松（没移动）：比例不变 ⇒ 不许产生待落盘（不然每次点一下都写盘）
    press(&win, cx, sy);
    release(&win, cx, sy);
    assert_eq!(ui.get_convo_h(), ui.get_dbg_flick_h());
    assert_eq!(split.take_pending(), None, "没改比例就不该登记落盘");

    // 2b. 按住往上拖 62px（-62）⇒ divider 顶边落在指针处
    let target = sy - 62.0;
    press(&win, cx, sy);
    move_to(&win, cx, target);
    release(&win, cx, target);
    let expect_h = target - ui.get_dbg_flick_y();
    assert_eq!(
        ui.get_convo_h(),
        expect_h,
        "divider 应跟手：指针相对消息区顶边的 y 就是新的内容区高"
    );
    assert_eq!(ui.get_dbg_split_y(), target, "分割条顶边应落在指针处");
    assert_eq!(ui.get_dbg_flick_h(), expect_h, "布局必须用上新的内容区高");
    assert_eq!(
        ui.get_dbg_composer_y() + ui.get_dbg_composer_h(),
        ui.get_dbg_convo_zone_h() - 10.0,
        "拖动后输入框仍贴底"
    );
    // 「输入区变高」在屏幕上必须是**输入框本身变高**（不只是分割条移位）：多出来的高度归
    // 输入框 + 文本区，这样拖动看得见效果。
    assert!(
        ui.get_dbg_area_h() > 66.0 && ui.get_dbg_composer_h() > 124.0,
        "拖动后输入框/文本区应变高（area={} composer={}）",
        ui.get_dbg_area_h(),
        ui.get_dbg_composer_h()
    );
    // 待落盘比例 = 屏幕上这一份（去抖定时器取走一次；再取就没有了）
    let want = crate::syschat::split_ratio(band, expect_h);
    let got = split.take_pending().expect("拖动改过比例 ⇒ 应有待落盘");
    assert!(
        (got - want).abs() < 1e-6,
        "待落盘比例应与所见一致（{got} vs {want}）"
    );
    assert_eq!(split.take_pending(), None, "取走一次后不应再有待落盘");

    // ── 3. 夹取：两头拖过头都被下限夹住 ──
    // 往下拖过头 ⇒ 输入区下限（消息区给到最大）
    let sy2 = ui.get_dbg_split_y() + 5.0;
    press(&win, cx, sy2);
    move_to(&win, cx, 5000.0);
    release(&win, cx, 5000.0);
    let band = (H - 2) as f32 - ui.get_dbg_flick_y();
    assert_eq!(
        ui.get_convo_h(),
        band - crate::syschat::SPLIT_HANDLE_H - crate::syschat::SPLIT_COMPOSER_MIN_H,
        "往下拖过头应被输入区下限夹住"
    );
    assert_eq!(
        ui.get_dbg_convo_zone_h(),
        crate::syschat::SPLIT_COMPOSER_MIN_H,
        "输入区到下限就不许再小"
    );
    // 往上拖过头 ⇒ 内容区下限
    let sy3 = ui.get_dbg_split_y() + 5.0;
    press(&win, cx, sy3);
    move_to(&win, cx, 0.0);
    release(&win, cx, 0.0);
    assert_eq!(
        ui.get_convo_h(),
        crate::syschat::SPLIT_CONVO_MIN_H,
        "往上拖过头应被内容区下限夹住"
    );
    assert!(
        ui.get_dbg_convo_zone_h() >= crate::syschat::SPLIT_COMPOSER_MIN_H,
        "输入区不得低于下限（消息区先让位）"
    );

    // ── 4. 悬停（没按下）的指针移动不得改任何东西 ──
    let before = ui.get_convo_h();
    let sy4 = ui.get_dbg_split_y() + 5.0;
    move_to(&win, cx, sy4 + 40.0);
    assert_eq!(ui.get_convo_h(), before, "悬停不是拖动，不许动 divider");

    // ── 5. 待发附件行出现（输入区下限 +76）⇒ 消息区让位、输入框仍贴底 ──
    ui.set_attach_count(3);
    split.set_composer_min(&ui, crate::syschat::split_composer_min_h(3));
    let _ = settle_w(&win, W520, H);
    let cmin = crate::syschat::split_composer_min_h(3);
    assert_eq!(cmin, crate::syschat::SPLIT_COMPOSER_MIN_H + 76.0);
    assert!(
        ui.get_composer_h() >= cmin,
        "输入区高应不低于（随附件行抬高的）下限（composer_h={} cmin={cmin}）",
        ui.get_composer_h()
    );
    assert!(
        ui.get_dbg_convo_zone_h() >= cmin,
        "待发附件行不许被压扁（输入区至少 {}，实际 {}）",
        cmin,
        ui.get_dbg_convo_zone_h()
    );
    assert!(
        ui.get_convo_h() <= band - crate::syschat::SPLIT_HANDLE_H - cmin + 0.5,
        "输入区下限抬高后消息区应让位（convo={}，上限={}）",
        ui.get_convo_h(),
        band - crate::syschat::SPLIT_HANDLE_H - cmin
    );
    assert_eq!(
        ui.get_dbg_composer_y() + ui.get_dbg_composer_h(),
        ui.get_dbg_convo_zone_h() - 10.0,
        "有待发附件时输入框仍贴底"
    );
    assert_eq!(
        ui.get_dbg_flick_h() + ui.get_dbg_split_h() + ui.get_dbg_convo_zone_h(),
        band,
        "三块之和仍等于可分配带（没有溢出/重叠）"
    );

    // ── 6. 最小窗（340×380，此时状态行会多出回落位的 chip）也必须自洽 ──
    // 这一档专门盯「可分配带」的算式：它由窗高减去固定构件得出，窄窗/矮窗下若有构件长高
    // （状态行回落行、标题栏换档）就会露馅——三块之和必须仍等于真实带高。
    const W340: u32 = 340;
    const H_MIN: u32 = 380;
    ui.set_attach_count(0);
    split.set_composer_min(&ui, crate::syschat::split_composer_min_h(0));
    ui.window().set_size(slint::PhysicalSize::new(W340, H_MIN));
    apply_syschat_layout(&ui, W340 as f32);
    let _ = settle_w(&win, W340, H_MIN);
    let band_min = (H_MIN - 2) as f32 - ui.get_dbg_flick_y();
    assert_eq!(
        ui.get_dbg_flick_h() + ui.get_dbg_split_h() + ui.get_dbg_convo_zone_h(),
        band_min,
        "340×380 下三块之和应等于可分配带（band_min={band_min}）"
    );
    assert!(
        ui.get_dbg_convo_zone_h() >= crate::syschat::SPLIT_COMPOSER_MIN_H,
        "最小窗下输入区也不得低于下限（zone={}）",
        ui.get_dbg_convo_zone_h()
    );
    assert!(
        ui.get_dbg_flick_h() >= crate::syschat::SPLIT_CONVO_MIN_H,
        "最小窗下消息区也不得低于下限（flick={}）",
        ui.get_dbg_flick_h()
    );
}

/// v11（卡 `abb-v11-popup-anchor`）：表情 /「更多」面板**贴着对应按钮上方**（而不是旧实现的
/// 「锚在输入框顶边」⇒ 底边离按钮 100+px）。三层判据：
/// 1. 自报值：`面板右缘 == 按钮右缘`、`按钮顶边 − 面板底边 == 6px`、两个面板各贴自己的按钮；
/// 2. 像素：开/关两帧的差异包围盒**整个落在按钮上方**、右缘与按钮右缘同列；
/// 3. 靠窗边（340 宽）时整体内移，仍不出窗。
///
/// 判别力（负向）：把 `pop_anchor` 的 x 改成左对齐、或 y 改回「输入框顶边 − h − 6」⇒ 1/2 红。
#[test]
fn v11_pop_panels_anchor_above_their_buttons() {
    // 差异包围盒（x0,y0,x1,y1），`None` = 两帧一样。宽高取**帧自己的**（本用例按窗宽渲两档：
    // 520 / 340，不能拿模块常量 `W`）。
    fn diff_bbox(a: &Frame, b: &Frame) -> Option<(u32, u32, u32, u32)> {
        let mut acc: Option<(u32, u32, u32, u32)> = None;
        for y in 0..a.h {
            for x in 0..a.w {
                let (p, q) = (a.px(x, y), b.px(x, y));
                if (p.red, p.green, p.blue, p.alpha) != (q.red, q.green, q.blue, q.alpha) {
                    acc = Some(match acc {
                        None => (x, y, x, y),
                        Some((a, b2, c, d)) => (a.min(x), b2.min(y), c.max(x), d.max(y)),
                    });
                }
            }
        }
        acc
    }

    for (window_w, tag) in [(520_u32, "520"), (340_u32, "340")] {
        let (ui, win, _m) = setup(vec![role_row("怹", true), row("p", "面板锚点")], SHORT_H);
        ui.set_emoji_cats(slint::ModelRc::from(Rc::new(slint::VecModel::from(vec![
            slint::SharedString::from("常用"),
        ]))));
        ui.set_emoji_cells(slint::ModelRc::from(Rc::new(slint::VecModel::from(
            crate::emoji::page_cells(0, 0)
                .into_iter()
                .map(slint::SharedString::from)
                .collect::<Vec<_>>(),
        ))));
        ui.set_emoji_page_label("1/1".into());
        ui.set_sched_preset_labels(slint::ModelRc::from(Rc::new(slint::VecModel::from(vec![
            slint::SharedString::from("5 分钟后"),
            slint::SharedString::from("30 分钟后"),
            slint::SharedString::from("今天 18:00"),
        ]))));
        ui.window()
            .set_size(slint::PhysicalSize::new(window_w, SHORT_H));
        apply_syschat_layout(&ui, window_w as f32);
        let closed = settle_w(&win, window_w, SHORT_H);

        for (which, btn_x, btn_y, btn_w, pop_x, pop_y, pop_w, pop_h) in [
            (
                "表情",
                ui.get_dbg_emoji_btn_x(),
                ui.get_dbg_emoji_btn_y(),
                ui.get_dbg_emoji_btn_w(),
                ui.get_emoji_pop_x(),
                ui.get_emoji_pop_y(),
                ui.get_emoji_pop_w(),
                ui.get_emoji_pop_h(),
            ),
            (
                "更多",
                ui.get_dbg_more_btn_x(),
                ui.get_dbg_more_btn_y(),
                ui.get_dbg_more_btn_w(),
                ui.get_more_pop_x(),
                ui.get_more_pop_y(),
                ui.get_more_pop_w(),
                ui.get_more_pop_h(),
            ),
        ] {
            println!(
                "{tag} 宽 / {which}：btn=({btn_x},{btn_y},{btn_w}) pop=({pop_x},{pop_y},{pop_w},{pop_h})"
            );
            // 有空间 ⇒ 右对齐；靠窗边放不下 ⇒ 整体内移（两种口径都要能过）
            let right_aligned = btn_x + btn_w - pop_w >= crate::syschat::POP_MARGIN as f32;
            // ① 落位 = 纯函数 `syschat::pop_anchor` 逐字算出（接线不许自己改算术）
            assert_eq!(
                (pop_x, pop_y),
                {
                    let (x, y) = crate::syschat::pop_anchor(
                        (btn_x as i32, btn_y as i32, btn_w as i32, 24),
                        (pop_w as i32, pop_h as i32),
                        (window_w as i32, SHORT_H as i32),
                    );
                    (x as f32, y as f32)
                },
                "{tag}/{which}：面板落位与 pop_anchor 不一致"
            );
            // ② 右缘与按钮右缘对齐；只有**靠窗边放不下**时才整体内移到留白处
            let want_right = btn_x + btn_w;
            if right_aligned {
                assert_eq!(
                    pop_x + pop_w,
                    want_right,
                    "{tag}/{which}：有空间时面板右缘应与按钮右缘对齐"
                );
            } else {
                assert_eq!(
                    pop_x,
                    crate::syschat::POP_MARGIN as f32,
                    "{tag}/{which}：靠窗边放不下 ⇒ 应整体内移到留白处"
                );
                // 内移后的实际效果：面板整体落在窗内（靠窗边也不许出窗）
                assert!(
                    pop_x + pop_w <= window_w as f32 - crate::syschat::POP_MARGIN as f32,
                    "{tag}/{which}：内移后仍出窗（右缘 {}）",
                    pop_x + pop_w
                );
            }
            // ③ 底边距按钮顶边 6px（上方空间够时；不够时才让位给「不出窗」）
            if pop_y > crate::syschat::POP_MARGIN as f32 {
                assert_eq!(
                    btn_y - (pop_y + pop_h),
                    crate::syschat::POP_GAP as f32,
                    "{tag}/{which}：面板底边距按钮顶边应为 6px"
                );
            }
            // ② 不出窗（6px 留白）
            assert!(
                pop_x >= crate::syschat::POP_MARGIN as f32
                    && pop_y >= crate::syschat::POP_MARGIN as f32
                    && pop_x + pop_w <= window_w as f32 - crate::syschat::POP_MARGIN as f32
                    && pop_y + pop_h <= SHORT_H as f32 - crate::syschat::POP_MARGIN as f32,
                "{tag}/{which}：面板出窗了 pop=({pop_x},{pop_y},{pop_w},{pop_h})"
            );
            // ③ 像素：开面板后差异区整个在按钮上方、右缘与按钮右缘同列
            if which == "表情" {
                ui.set_emoji_open(true);
            } else {
                ui.set_more_open(true);
            }
            let open = settle_w(&win, window_w, SHORT_H);
            if which == "更多" {
                // 「更多」面板 y 随待发条数变：置 0 条时默认高由 Rust 给（`more_pop_h(0)`）。
                assert_eq!(pop_h, crate::syschat::more_pop_h(0) as f32);
            }
            let bb = diff_bbox(&closed, &open)
                .unwrap_or_else(|| panic!("{tag}/{which}：面板没渲染出来（两帧一样）"));
            assert!(
                (bb.3 as f32) < btn_y,
                "{tag}/{which}：面板底边（像素 {}) 压到按钮顶边（{btn_y}）了",
                bb.3
            );
            // 像素上的面板右缘 = 锚点算出的右缘（接线/布局/渲染三方一致）
            assert!(
                (bb.2 as f32 + 1.0 - (pop_x + pop_w)).abs() <= 2.0,
                "{tag}/{which}：像素右缘 {} 与锚点右缘 {} 不一致",
                bb.2 + 1,
                pop_x + pop_w
            );
            // 有空间的那一档：像素右缘还必须与**按钮右缘**对齐（负向判据就落在这条上）
            if right_aligned {
                assert!(
                    (bb.2 as f32 + 1.0 - (btn_x + btn_w)).abs() <= 2.0,
                    "{tag}/{which}：像素右缘 {} 与按钮右缘 {} 不对齐",
                    bb.2 + 1,
                    btn_x + btn_w
                );
            }
            if which == "表情" {
                ui.set_emoji_open(false);
            } else {
                ui.set_more_open(false);
            }
            let _ = settle_w(&win, window_w, SHORT_H);
        }
    }
}

/// v11（卡 `abb-v11-popup-anchor`）：表情 /「更多」面板的**开合**——点按钮展开、点面板外收起、
/// **再点按钮也收起**（稿 v11 第 1 条；`PopupWindow` 的 close-on-click-outside 在按下就关，
/// 做不到「再点收起」，见 `.slint` 里那段注释）。两个面板**互斥**。
#[test]
fn v11_pop_panels_open_and_close_on_button_and_outside() {
    const W520: u32 = 520;
    let (ui, win, _m) = setup(vec![row("p", "占位")], SHORT_H);
    ui.window()
        .set_size(slint::PhysicalSize::new(W520, SHORT_H));
    apply_syschat_layout(&ui, W520 as f32);
    let _ = settle_w(&win, W520, SHORT_H);
    // 两个功能钮的命中中心（24×24，由 `.slint` 报来）
    let emoji = (
        ui.get_dbg_emoji_btn_x() + ui.get_dbg_emoji_btn_w() / 2.0,
        ui.get_dbg_emoji_btn_y() + 12.0,
    );
    let more = (
        ui.get_dbg_more_btn_x() + ui.get_dbg_more_btn_w() / 2.0,
        ui.get_dbg_more_btn_y() + 12.0,
    );

    // 表情：开 → 再点收起 → 再点开 → 点外部收起
    press_release(&win, emoji.0, emoji.1);
    assert!(ui.get_emoji_open(), "点 😊 应展开表情面板");
    press_release(&win, emoji.0, emoji.1);
    assert!(!ui.get_emoji_open(), "再点 😊 应收起（不是只重开）");
    press_release(&win, emoji.0, emoji.1);
    assert!(ui.get_emoji_open(), "第三次点应再次展开");
    press_release(&win, 200.0, 200.0);
    assert!(!ui.get_emoji_open(), "点面板外应收起");

    // 更多：同理；且两者互斥（面板开着时点另一个按钮 ⇒ 先收起，再点才展开）
    press_release(&win, more.0, more.1);
    assert!(ui.get_more_open(), "点 ⋯ 应展开「更多」面板");
    press_release(&win, more.0, more.1);
    assert!(!ui.get_more_open(), "再点 ⋯ 应收起");
    press_release(&win, emoji.0, emoji.1);
    assert!(ui.get_emoji_open(), "展开表情面板");
    press_release(&win, more.0, more.1);
    assert!(
        !ui.get_emoji_open() && !ui.get_more_open(),
        "面板开着时点另一个按钮应先收起（整窗 backdrop 吃掉那一下）"
    );
    press_release(&win, more.0, more.1);
    assert!(ui.get_more_open(), "再点 ⋯ 才展开「更多」");
    assert!(!ui.get_emoji_open(), "两面板互斥");
}

/// v11c（卡 `abb-v11-resize-maximize-macos`）：无边框窗的 8 个缩放热区 + 自管缩放入口。
///
/// 两件事一起钉：
/// 1. **几何**：6px 边条 + 14px 角块的命中面都在（macOS 上这是**唯一**能缩放的入口），
///    且三颗窗口钮的**点击中心**都不在热区里（否则「— ⤢ ✕ 都要真能用」会被热区吃掉）；
/// 2. **分路**：`resize-self-managed: true` 时按下报 `self-resize-begin(dir, x, y)`，其中
///    `(x, y)` 必须是**窗口坐标**（Rust 拿它当缩放锚点；报相对热区的坐标会让窗口越拖越飘）；
///    `false` 时仍报 `begin-resize(dir)`（Windows/Linux 的系统 `drag_resize_window` 路径不变）。
///
/// 判别力（负向）：把角块尺寸改成 30×30（或把环存顺序改成先角后边）⇒ 三钮中心断言红；
/// 把 `begin-at` 的参数改成 `self.mouse-x`（漏掉绝对位置）⇒ 坐标断言红。
#[test]
fn v11c_resize_zones_hit_all_edges_and_corners() {
    const W520: u32 = 520;
    let h = SHORT_H;
    let (ui, win, _) = setup(vec![row("p", "占位")], h);
    ui.set_resize_hit_areas(true);
    ui.window().set_size(slint::PhysicalSize::new(W520, h));
    super::apply_syschat_layout(&ui, W520 as f32);
    let sys: Rc<std::cell::RefCell<Vec<String>>> = Rc::new(std::cell::RefCell::new(vec![]));
    {
        let s = sys.clone();
        ui.on_begin_resize(move |d| s.borrow_mut().push(d.to_string()));
    }
    let own: Rc<std::cell::RefCell<Vec<(String, f32, f32)>>> =
        Rc::new(std::cell::RefCell::new(vec![]));
    {
        let o = own.clone();
        ui.on_self_resize_begin(move |d, x, y| o.borrow_mut().push((d.to_string(), x, y)));
    }
    let _ = settle_w(&win, W520, h);

    // 8 个方向各按一次（取各自最靠边的一条/一角）。
    let cases: [(&str, f32, f32); 8] = [
        ("north", 170.0, 2.0),
        ("south", 170.0, h as f32 - 3.0),
        ("west", 2.0, 200.0),
        ("east", W520 as f32 - 3.0, 200.0),
        ("north-west", 2.0, 2.0),
        ("north-east", W520 as f32 - 3.0, 2.0),
        ("south-west", 2.0, h as f32 - 3.0),
        ("south-east", W520 as f32 - 3.0, h as f32 - 3.0),
    ];
    let want: Vec<&str> = cases.iter().map(|c| c.0).collect();

    // ⓪ 三颗窗口钮的点击中心不得落进热区（热区是整窗 overlay，压在标题栏两端）。
    for (name, bx, by) in titlebar_button_centers(W520) {
        let in_zone = (bx < 14.0 || bx > W520 as f32 - 14.0) && by < 14.0;
        assert!(
            !in_zone,
            "{name} 钮的点击中心 ({bx},{by}) 落在缩放热区里：三钮会点不动"
        );
    }

    // ① 系统路径（默认 `resize-self-managed: false`）：只报方向。
    for (_, x, y) in cases {
        press_release(&win, x, y);
    }
    assert_eq!(
        sys.borrow().as_slice(),
        want.as_slice(),
        "系统路径的方向报错"
    );
    assert!(
        own.borrow().is_empty(),
        "系统路径不许触发自管缩放：{:?}",
        own.borrow()
    );

    // ② 自管路径（macOS）：报方向 + 指针的**窗口坐标**。
    sys.borrow_mut().clear();
    ui.set_resize_self_managed(true);
    let _ = settle_w(&win, W520, h);
    for (_, x, y) in cases {
        press_release(&win, x, y);
    }
    let got = own.borrow().clone();
    assert_eq!(
        got.len(),
        8,
        "自管路径应报 8 次（八个热区都不能丢）：{got:?}"
    );
    for (i, (dir, x, y)) in cases.iter().enumerate() {
        let (d, gx, gy) = &got[i];
        assert_eq!(d, dir, "第 {i} 个热区方向错：{got:?}");
        assert!(
            (gx - x).abs() <= 1.0 && (gy - y).abs() <= 1.0,
            "热区报来的指针坐标应是**窗口坐标**：期望 ({x},{y})，实际 ({gx},{gy})"
        );
    }
    assert!(
        sys.borrow().is_empty(),
        "自管路径不许再触发系统路径：{:?}",
        sys.borrow()
    );
}

/// v11c（owner 真机复查）：历史会话 / 模型列表两个面板的**圆角被方形不透明底盖掉**。
///
/// 根因：`Rectangle` 的 `border-radius` 只裁**自己**的背景/描边，**不裁子元素**；而标题
/// （稿 `.menu h6`，`--bg` 底）与 footer（`.foot`，`--bg` 底）是铺满内容盒的方形 ⇒ 把面板
/// 上/下两角的弧（连同 2px 描边）盖住 ⇒ 观感就是「圆角变方」。
/// 修法：给这两块补上与面板**内圈**同半径的角（8 − 2 = 6，弧心重合）。
///
/// 判据（像素、可机读）：面板四个角上、**弧的笔迹所在**那个像素必须比同层不透明底的底色
/// **明显暗**（那就是描边没被遮住）；修前这四个点与底色逐位相同。
/// 底色不硬编码：从同一块里挑一个**没有字**的位置采样（换主题不会假红）。
///
/// 判别力（负向）：把标题或 footer 的 `border-*-radius: 6px` 删掉 ⇒ 本用例红（已逐条实测）。
#[test]
fn v11c_menu_panels_keep_rounded_corners() {
    const H: u32 = 820;
    const W520: u32 = 520;
    fn luma(p: PremultipliedRgbaColor) -> u32 {
        p.red as u32 + p.green as u32 + p.blue as u32
    }
    let (ui, win, _m) = setup(vec![row("p", "占位")], H);
    seed_panels(&ui);
    ui.window().set_size(slint::PhysicalSize::new(W520, H));
    apply_syschat_layout(&ui, W520 as f32);

    let mut checked = 0;
    for (name, open_session) in [("会话面板", true), ("模型面板", false)] {
        if open_session {
            ui.set_session_menu_open(true);
        } else {
            ui.set_session_menu_open(false);
            ui.set_model_menu_open(true);
        }
        let f = settle_w(&win, W520, H);
        let px = (if open_session {
            ui.get_session_menu_x()
        } else {
            ui.get_model_menu_x()
        }) as u32;
        let py = (if open_session {
            ui.get_session_menu_y()
        } else {
            ui.get_model_menu_y()
        }) as u32;
        let w = 250u32;
        let h = (if open_session {
            ui.get_dbg_session_menu_h()
        } else {
            ui.get_dbg_model_menu_h()
        }) as u32;
        assert!(
            h > 0 && px + w < W520,
            "{name} 几何不可用：({px},{py},w={w},h={h})"
        );

        // 底色采样点：标题条 / footer 的**右侧空白处**（文字宽 = 面板宽 − 22）。
        let head_bg = f.px(px + w - 6, py + 14);
        let foot_bg = f.px(px + w - 6, py + h - 8);
        assert!(
            luma(head_bg) > 600 && luma(foot_bg) > 600,
            "{name} 底色采样点不在（浅色）底上：head={head_bg:?} foot={foot_bg:?}"
        );

        // 四个角：笔迹像素必须明显暗于同层底色。
        let probes = [
            ("左上", px + 3, py + 2, head_bg),
            ("右上", px + w - 4, py + 2, head_bg),
            ("左下", px + 3, py + h - 4, foot_bg),
            ("右下", px + w - 4, py + h - 4, foot_bg),
        ];
        for (what, cx, cy, bg) in probes {
            let got = f.px(cx, cy);
            assert!(
                luma(got) + 60 < luma(bg),
                "{name}{what}角 ({cx},{cy}) 的圆角弧被方形不透明底盖住了：\
                 该点应是描边笔迹，实量 {got:?}（同层底色 {bg:?}）"
            );
            checked += 1;
        }
    }
    assert_eq!(checked, 8, "两个面板 x 四角 = 8 个采样点都得跑到");
}

/// v11c 出图（owner 真机复查四条的对稿判读用；默认不跑）：
/// `v11c-full.png` / `v11c-split-single-line.png` / `v11c-panel-corners.png` /
/// `v11c-panel-corner-tl.png` / `v11c-panel-corner-bl.png` / `v11c-status-meter.png` /
/// `v11c-resize-zones.png`（把 8 个热区用紫线描出来，看“三钮是不是被吃了”）。
#[test]
#[ignore = "出图用；默认不跑"]
fn dump_v11c_fixes_for_review() {
    const W520: u32 = 520;
    const H: u32 = 820;
    const MAGENTA: PremultipliedRgbaColor = PremultipliedRgbaColor {
        red: 255,
        green: 0,
        blue: 255,
        alpha: 255,
    };
    let dir = std::path::Path::new(SHOTS);
    std::fs::create_dir_all(dir).expect("建出图目录失败");
    let (ui, win, _m) = setup(fixture(), H);
    seed_panels(&ui);
    super::apply_syschat_usage(&ui, None, None);
    ui.set_draft("就按这三份素材做，说明放第二节…".into());
    ui.set_resize_hit_areas(true);
    ui.window().set_size(slint::PhysicalSize::new(W520, H));
    apply_syschat_layout(&ui, W520 as f32);
    let _ = settle_w(&win, W520, H);

    // ① 分割条：整窗 + 放大那条带（只应看到**一条**线）。
    let f = settle_w(&win, W520, H);
    write_png(&f, &dir.join("v11c-full.png"));
    let sy = ui.get_dbg_split_y() as u32;
    let band = crop(&f, 0, sy.saturating_sub(14), W520, 44);
    write_png(&band, &dir.join("v11c-split-single-line.png"));

    // ② 面板圆角：整面板 + 左上/左下角放大（能直接看出弧线是否完整）。
    ui.set_session_menu_open(true);
    let f2 = settle_w(&win, W520, H);
    write_png(&f2, &dir.join("v11c-panel-corners.png"));
    let (px, py) = (
        ui.get_session_menu_x() as u32,
        ui.get_session_menu_y() as u32,
    );
    let ph = ui.get_dbg_session_menu_h() as u32;
    write_png(
        &crop(&f2, px.saturating_sub(4), py.saturating_sub(4), 44, 44),
        &dir.join("v11c-panel-corner-tl.png"),
    );
    write_png(
        &crop(&f2, px.saturating_sub(4), py + ph - 40, 44, 44),
        &dir.join("v11c-panel-corner-bl.png"),
    );
    ui.set_session_menu_open(false);

    // ③ 状态行：用量条是否与两行文字同轴居中。
    let f3 = settle_w(&win, W520, H);
    write_png(
        &crop(&f3, 0, 30, W520, 56),
        &dir.join("v11c-status-meter.png"),
    );
    // ③b E3：**有数据**时的用量条（75% ⇒ 橙档；出图给 owner 看档位换色，非断言）。
    // 注：注入值（不读盘）——出图可复现，不随开发机真实会话变。
    ui.set_usage_text("上下文 96K/128K · 75.0%".into());
    ui.set_usage_tip(crate::syschat::context_usage_tip(Some(96_000), Some(128_000)).into());
    ui.set_usage_meter_fill(crate::syschat::USAGE_METER_INNER_W * 0.75);
    ui.set_usage_level(1);
    let f3b = settle_w(&win, W520, H);
    write_png(
        &crop(&f3b, 0, 30, W520, 56),
        &dir.join("v11c-status-meter-warn.png"),
    );

    // ④ 缩放热区示意：把 8 个热区（6px 边 + 14px 角）用紫线描在原图上。
    // 几何与 `.slint` 的 `WindowResizeZones` 同源；命中面本身由 `v11c_resize_zones_hit_all_edges_and_corners`
    // 断言（这里只是给 owner 看的示意）。
    let mut f4 = settle_w(&win, W520, H);
    let zones: [(u32, u32, u32, u32); 8] = [
        (0, 0, W520, 6),
        (0, H - 6, W520, 6),
        (0, 0, 6, H),
        (W520 - 6, 0, 6, H),
        (0, 0, 14, 14),
        (W520 - 14, 0, 14, 14),
        (0, H - 14, 14, 14),
        (W520 - 14, H - 14, 14, 14),
    ];
    for (zx, zy, zw, zh) in zones {
        let (x1, y1) = ((zx + zw - 1).min(W520 - 1), (zy + zh - 1).min(H - 1));
        for x in zx..=x1 {
            for y in [zy, y1] {
                f4.buf[(y * W520 + x) as usize] = MAGENTA;
            }
        }
        for y in zy..=y1 {
            for x in [zx, x1] {
                f4.buf[(y * W520 + x) as usize] = MAGENTA;
            }
        }
    }
    write_png(&f4, &dir.join("v11c-resize-zones.png"));
    println!(
        "v11c 出图完成：v11c-full.png / v11c-split-single-line.png / v11c-panel-corners.png / \
         v11c-panel-corner-tl.png / v11c-panel-corner-bl.png / v11c-status-meter.png / \
         v11c-resize-zones.png（split_y={sy} panel=({px},{py},h={ph})）"
    );
}

/// v11 出图（对稿判读用；默认不跑）：分割条的两种比例 + 表情/「更多」面板贴按钮。
#[test]
#[ignore = "出图用；默认不跑"]
fn dump_v11_split_and_pops_for_review() {
    const W520: u32 = 520;
    const H: u32 = 820;
    let dir = std::path::Path::new(SHOTS);
    std::fs::create_dir_all(dir).expect("建出图目录失败");
    let (ui, win, _m) = setup(fixture(), H);
    let split = wire_split(&ui);
    seed_panels(&ui);
    ui.set_emoji_cats(slint::ModelRc::from(Rc::new(slint::VecModel::from(vec![
        slint::SharedString::from("常用"),
        slint::SharedString::from("手势"),
    ]))));
    ui.set_emoji_cells(slint::ModelRc::from(Rc::new(slint::VecModel::from(
        crate::emoji::page_cells(0, 0)
            .into_iter()
            .map(slint::SharedString::from)
            .collect::<Vec<_>>(),
    ))));
    ui.set_emoji_page_label("1/2".into());
    ui.set_sched_preset_labels(slint::ModelRc::from(Rc::new(slint::VecModel::from(vec![
        slint::SharedString::from("5 分钟后"),
        slint::SharedString::from("30 分钟后"),
        slint::SharedString::from("今天 18:00"),
    ]))));
    ui.set_draft("就按这三份素材做，说明放第二节…".into());
    ui.window().set_size(slint::PhysicalSize::new(W520, H));
    apply_syschat_layout(&ui, W520 as f32);
    let _ = settle_w(&win, W520, H);

    // ① 默认比例（输入区在下限）：分割条紧贴输入区上沿。
    let f = settle_w(&win, W520, H);
    write_png(&f, &dir.join("v11-split-default.png"));
    // ② 拖动（往上拖 160px ⇒ 输入区变高）：输入框仍贴底。
    let cx = W520 as f32 / 2.0;
    let sy = ui.get_dbg_split_y() + 5.0;
    press(&win, cx, sy);
    move_to(&win, cx, sy - 160.0);
    release(&win, cx, sy - 160.0);
    let _ = settle_w(&win, W520, H);
    let f2 = settle_w(&win, W520, H);
    write_png(&f2, &dir.join("v11-split-dragged.png"));
    // ③ 表情面板（贴 😊 按钮上方）+ 「更多」面板（贴 ⋯ 按钮上方）。
    ui.set_emoji_open(true);
    let f3 = settle_w(&win, W520, H);
    write_png(&f3, &dir.join("v11-pop-emoji.png"));
    ui.set_emoji_open(false);
    ui.set_sched_count(1);
    ui.set_sched_items(slint::ModelRc::from(Rc::new(slint::VecModel::from(vec![
        slint::SharedString::from("10-14 18:00 · 待发测试"),
    ]))));
    ui.set_more_open(true);
    let f4 = settle_w(&win, W520, H);
    write_png(&f4, &dir.join("v11-pop-more.png"));
    // 只切输入区那一带（放大看分割条/抓手/输入框贴底）。
    let band = crop(&f2, 0, (H - 420) as u32, W520, 420);
    write_png(&band, &dir.join("v11-split-zone.png"));
    println!(
        "v11 出图完成：v11-split-default.png / v11-split-dragged.png / v11-pop-emoji.png / \n         v11-pop-more.png / v11-split-zone.png（convo_h={} ratio={}）",
        ui.get_convo_h(),
        split.drag.borrow().ratio()
    );
}

/// v11 出图：会话面板 / 模型面板 / 状态区（对稿判读用；默认不跑）。
#[test]
#[ignore = "出图用；默认不跑"]
fn dump_v11_panels_for_review() {
    const H: u32 = 820;
    const W520: u32 = 520;
    let dir = std::path::Path::new(SHOTS);
    std::fs::create_dir_all(dir).expect("建出图目录失败");
    let (ui, win, _m) = setup(fixture(), H);
    seed_panels(&ui);
    super::apply_syschat_usage(&ui, None, None);
    ui.set_draft("就按这三份素材做，说明放第二节…".into());
    ui.window().set_size(slint::PhysicalSize::new(W520, H));
    apply_syschat_layout(&ui, W520 as f32);

    ui.set_session_menu_open(true);
    let f = settle_w(&win, W520, H);
    let f_dbg = (
        ui.get_session_menu_x(),
        ui.get_session_menu_y(),
        ui.get_dbg_session_menu_w(),
        ui.get_dbg_session_menu_h(),
    );
    write_png(&f, &dir.join("v11-session-menu.png"));
    let session_accent = bbox(&f, 0, 40, 300, 300, is_accent);
    println!("session panel frame: dbg={f_dbg:?} accent(panel)={session_accent:?}");
    // 卡 `abb-v11-session-archive`：**归档视图**——同一面板原地换成归档列表，第一行变
    // 「← 返回会话列表」、不再画第二行（标题/footer 由 `syschat::session_panel_text` 给）。
    ui.set_session_rows(slint::ModelRc::from(Rc::new(slint::VecModel::from(vec![
        SysChatMenuRow {
            text: "把架构图做成网页".into(),
            tag: "昨天 23:12".into(),
            tick: false,
        },
        SysChatMenuRow {
            text: "排查 deepseek 端点".into(),
            tag: "今天 00:40".into(),
            tick: false,
        },
    ]))));
    let copy = crate::syschat::session_panel_text(true, 2);
    ui.set_session_panel_copy(SessionPanelCopy {
        heading: copy.heading.into(),
        foot: copy.foot.into(),
        extra_text: copy.extra_text.into(),
        extra_tag: copy.extra_tag.into(),
        extra2_text: copy.extra2_text.into(),
        extra2_tag: copy.extra2_tag.into(),
    });
    let f = settle_w(&win, W520, H);
    write_png(&f, &dir.join("v11-session-menu-archived.png"));
    println!("archived view frame: h={}", ui.get_dbg_session_menu_h());
    // 复位成普通视图（后面的模型面板那帧与本帧无关，但保持出图口径一致）。
    seed_panels(&ui);
    ui.set_session_menu_open(false);
    ui.set_model_menu_open(true);
    let f2 = settle_w(&win, W520, H);
    write_png(&f2, &dir.join("v11-model-menu.png"));
    println!(
        "model panel frame: h={} accent(panel)={:?}",
        ui.get_dbg_session_menu_h(),
        bbox(&f2, 0, 40, 520, 300, is_accent)
    );
    // 无面板帧（状态区）：用量条 + 两行状态。
    ui.set_model_menu_open(false);
    let f = settle_w(&win, W520, H);
    write_png(&f, &dir.join("v11-status.png"));
    // 只切状态行+面板区（放大看用量条与面板描边）。
    let top = crop(&f, 0, 0, W520, 260);
    write_png(&top, &dir.join("v11-titlebar-status.png"));
    println!(
        "v11 出图完成：v11-session-menu.png / v11-session-menu-archived.png / v11-model-menu.png / v11-status.png / \
         v11-titlebar-status.png（session_menu=({},{}), model_menu=({},{})）",
        ui.get_session_menu_x(),
        ui.get_session_menu_y(),
        ui.get_model_menu_x(),
        ui.get_model_menu_y()
    );
}

/// 从一帧里裁一块（x,y 为左上角；供对稿放大用）。
fn crop(f: &Frame, x: u32, y: u32, w: u32, h: u32) -> Frame {
    let mut buf = vec![PremultipliedRgbaColor::default(); (w * h) as usize];
    for row in 0..h {
        for col in 0..w {
            buf[(row * w + col) as usize] = f.px(x + col, y + row);
        }
    }
    Frame { buf, w, h }
}

/// 用**真实会话里那张表**（owner 反馈「表格渲染不对」的原文本）验解析与渲染。
#[test]
#[ignore = "出图/定性用；默认不跑"]
fn dump_real_table_for_review() {
    let dir = std::path::Path::new("/Volumes/DataExt/tmp/syschat-shots");
    std::fs::create_dir_all(dir).expect("建出图目录失败");
    let md = "理解成给上面的 upstream 整理成表格：\n\n\
| 语境 | upstream 指谁 | 相反方向 | 典型操作 |\n\
|---|---|---|---|\n\
| Git 分支跟踪 | 本地分支所跟踪的远程分支（如 `origin/main`） | downstream 分支（从你这里拉变更的） | `git push -u origin main`、`git branch -vv` |\n\
| 开源协作 | 项目官方原仓库（`git remote add upstream <官方地址>`） | 你的 fork（`origin`） | 从 upstream 同步、向 upstream 提 PR |\n\
| 数据流 / 业务链路 | 变更与数据的来源侧 | 承接结果、依赖你的一方 | 上游断供 → 下游受影响 |\n\n\
一句话判据：**谁流向我，谁就是 upstream**。";
    let blocks = crate::mdblocks::parse(md);
    for b in &blocks {
        println!(
            "kind={:<6} header={:<5} cells={:?}",
            b.kind, b.header, b.cells
        );
    }
    let mut rows = vec![row("role", "怹")];
    push_block_rows(&mut rows, blocks);
    let (ui, win, _m) = setup(rows, 640);
    ui.set_session_labels(slint::ModelRc::from(Rc::new(slint::VecModel::from(vec![
        slint::SharedString::from("新会话"),
    ]))));
    ui.set_model_labels(slint::ModelRc::from(Rc::new(slint::VecModel::from(vec![
        slint::SharedString::from("deepseek-flash"),
    ]))));
    ui.set_session_index(0);
    ui.set_model_index(0);
    ui.window().set_size(slint::PhysicalSize::new(520, 640));
    apply_syschat_layout(&ui, 520.0);
    let f = settle_w(&win, 520, 640);
    write_png(&f, &dir.join("REAL-table.png"));
}

// ───────── 表格整块化 / 列宽 / 图片高度（2026-10-15）─────────

/// 连续 `table` 块必须合成**一个**渲染行（列宽与行高只有拿到整表才算得出来）。
#[test]
fn table_blocks_merge_into_one_row_with_content_sized_columns() {
    use slint::Model as _;
    let bs = crate::mdblocks::parse("| 键 | 值 |\n|---|---|\n| a | 1 |\n| b | 2 |\n");
    let mut rows = Vec::new();
    push_block_rows(&mut rows, bs);
    assert_eq!(
        rows.iter().map(|r| r.kind.as_str()).collect::<Vec<_>>(),
        vec!["table"],
        "整表必须是一个渲染行（旧实现一行一个块 ⇒ 每行各自 Flickable）"
    );
    assert_eq!(rows[0].table_rows.row_count(), 3, "表头 + 2 数据行");
    let ws: Vec<f32> = rows[0].col_ws.iter().collect();
    assert_eq!(ws.len(), 2);
    assert!(
        ws.iter().all(|w| *w < 100.0),
        "短内容列宽要贴内容（旧实现 132px 起步 ⇒ 列间距巨大）：{ws:?}"
    );

    // 反向控制：长内容列必须更宽（否则上面那条可能只是「列宽恒小」白过）
    let bs = crate::mdblocks::parse(
        "| 键 | 值 |\n|---|---|\n| a | 一段很长的单元格内容用来把这一列撑宽 |\n",
    );
    let mut rows = Vec::new();
    push_block_rows(&mut rows, bs);
    let ws2: Vec<f32> = rows[0].col_ws.iter().collect();
    assert!(ws2[1] > ws[1], "长内容列应更宽：{ws2:?} vs {ws:?}");
}

/// 图片块必须把**自己渲染出来的高度**报给布局。
///
/// 旧实现：块是裸 `Rectangle` + `Image{height:200px}`，却只向布局报 ~30px ⇒ 图片后的内容
/// 被图片盖住、滞底也算不到底（owner 2026-10-15「图片没有正确渲染」的根因）。
#[test]
fn image_block_reports_rendered_height() {
    // 造一张 200×100 的真 PNG（宽高比 0.5），不依赖 `~/.agent-bridge` 里有没有示例图
    let dir = std::env::temp_dir().join(format!("abb-img-h-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("t.png");
    image::RgbaImage::from_pixel(200, 100, image::Rgba([10, 120, 200, 255]))
        .save(&path)
        .unwrap();

    let blocks = crate::mdblocks::parse(&format!("![t]({})", path.display()));
    assert_eq!(blocks.len(), 1);
    assert_eq!(blocks[0].kind, "image");
    // 三态都要对：不存在的路径不得谎报 `img-ok`（旧口径 `img-ok=false` 走「打开」卡）
    assert!(!block_row(crate::mdblocks::MdBlock::new("image")).img_ok);
    let r = block_row(blocks.into_iter().next().unwrap());
    assert!(r.img_ok, "图片应加载成功");
    assert!(
        (r.img_aspect - 0.5).abs() < 0.01,
        "宽高比应为 0.5（高/宽）：{}",
        r.img_aspect
    );

    let height_with = |rows: Vec<SysChatBlockRow>| {
        let (ui, _win, _m) = setup(rows, 900);
        ui.window().set_size(slint::PhysicalSize::new(520, 900));
        apply_syschat_layout(&ui, 520.0);
        ui.get_dbg_col_h()
    };
    let h_no = height_with(vec![role_row("怹", true)]);
    let h_img = height_with(vec![role_row("怹", true), r]);
    assert!(
        h_img - h_no > 120.0,
        "图片块应把渲染高度报给布局：无图 {h_no} / 有图 {h_img}（旧实现只差 ~30px）"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// 块级公式真排版出图（人工核对用；`AGENT_BRIDGE_HOME` 指到临时目录可避免写真实缓存）。
///
/// 为什么 `#[ignore]`：它经 `block_row` 走生产 `mathrender::typeset` ⇒ 会往
/// `<bridge>/syschat/math-cache/` **写 SVG**，而全量门禁的隔离守卫不允许测试写运行数据。
/// 排版逻辑本身的判据在 `mathrender::tests`（缓存目录可注入），这里只负责出图。
#[test]
#[ignore = "出图/定性用；默认不跑"]
fn dump_math_typeset_for_review() {
    let dir = std::path::Path::new("/Volumes/DataExt/tmp/syschat-shots");
    std::fs::create_dir_all(dir).expect("建出图目录失败");
    let md = "行内数学：质能方程 $E = mc^2$ 混排。\n\n\
块级公式：\n\n$$\n\\int_0^1 x^2 \\, dx = \\frac{1}{3}\n$$\n\n\
再来一个：\n\n$$\n\\sum_{i=1}^{n} i = \\frac{n(n+1)}{2}, \\quad \\sqrt{x^2+y^2}\n$$\n\n\
非法公式兜底：\n\n$$\n\\frac{a}{\n$$\n";
    let mut rows = vec![row("role", "怹")];
    push_block_rows(&mut rows, crate::mdblocks::parse(md));
    let (ui, win, _m) = setup(rows, 900);
    ui.set_session_labels(slint::ModelRc::from(Rc::new(slint::VecModel::from(vec![
        slint::SharedString::from("新会话"),
    ]))));
    ui.set_model_labels(slint::ModelRc::from(Rc::new(slint::VecModel::from(vec![
        slint::SharedString::from("m"),
    ]))));
    ui.set_session_index(0);
    ui.set_model_index(0);
    ui.window().set_size(slint::PhysicalSize::new(520, 900));
    apply_syschat_layout(&ui, 520.0);
    println!("col-h={}", ui.get_dbg_col_h());
    let f = settle_w(&win, 520, 900);
    write_png(&f, &dir.join("math-typeset.png"));
}

/// 行内公式混排出图（人工核对用；`AGENT_BRIDGE_HOME` 指到临时目录避免写真实缓存）。
///
/// 与 `dump_math_typeset_for_review` 同因 `#[ignore]`：它经生产 `block_row_w` 走
/// `inlineflow` + `mathrender::typeset`，会往 `<bridge>/syschat/math-cache/` 写 SVG。
#[test]
#[ignore = "出图/定性用；默认不跑"]
fn dump_inline_math_for_review() {
    let dir = std::path::Path::new("/Volumes/DataExt/tmp/syschat-shots");
    std::fs::create_dir_all(dir).expect("建出图目录失败");
    let md = "行内公式（应与文字同一基线、正常换行）：\n\n\
质能方程 $E = mc^2$ 说明质量可以转化为能量；常见的还有 $\\frac{a+b}{c}$、\
$\\sqrt{x^2+y^2}$、$\\sum_{i=1}^{n} i$ 等等，这几个应当能和文字排在同一行并自动换行。\n\n\
普通段落（对照，不受影响）：这里的文字走 Slint 原生排版，**粗体** 与 `代码` 都在。\n\n\
块级公式（对照）：\n\n$$\n\\int_0^1 x^2 \\, dx = \\frac{1}{3}\n$$";
    let mut rows = vec![row("role", "怹")];
    push_block_rows_w(&mut rows, crate::mdblocks::parse(md), 416.0);
    let (ui, win, _m) = setup(rows, 900);
    ui.set_session_labels(slint::ModelRc::from(Rc::new(slint::VecModel::from(vec![
        slint::SharedString::from("新会话"),
    ]))));
    ui.set_model_labels(slint::ModelRc::from(Rc::new(slint::VecModel::from(vec![
        slint::SharedString::from("m"),
    ]))));
    ui.set_session_index(0);
    ui.set_model_index(0);
    ui.window().set_size(slint::PhysicalSize::new(520, 900));
    apply_syschat_layout(&ui, 520.0);
    println!("col-h={}", ui.get_dbg_col_h());
    let f = settle_w(&win, 520, 900);
    write_png(&f, &dir.join("inline-math.png"));
}

/// `mathp` 块：按 Rust 算好的坐标**绝对定位**渲染文字片段与公式图。
///
/// 这条不碰文件系统（片段直接构造；公式用内存里的纯色图），所以不需要 `#[ignore]`：
/// 它守的是 `.slint` 的 `mathp` 分支本身（`flow-h` 有没有被外层布局认下来 + 片段真的画出来）。
#[test]
fn mathp_block_renders_flow_pieces_absolutely() {
    let solid = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(
        &[20u8, 20, 20, 255].repeat(12 * 12),
        12,
        12,
    );
    let mk = |flow_h: f32| {
        let mut r = row("mathp", "");
        r.flow_size = 13.0;
        r.flow_h = flow_h;
        r.flow = slint::ModelRc::from(Rc::new(slint::VecModel::from(vec![
            ConvoFlowPiece {
                is_math: false,
                text: "文字片段甲乙".into(),
                img: slint::Image::default(),
                x: 0.0,
                y: 0.0,
                w: 60.0,
                h: 16.0,
            },
            ConvoFlowPiece {
                is_math: true,
                text: "".into(),
                img: slint::Image::from_rgba8(solid.clone()),
                x: 70.0,
                y: 2.0,
                w: 12.0,
                h: 12.0,
            },
        ])));
        r
    };
    let height_with = |row: SysChatBlockRow| {
        let (ui, _win, _m) = setup(vec![role_row("怹", true), row], 700);
        ui.window().set_size(slint::PhysicalSize::new(520, 700));
        apply_syschat_layout(&ui, 520.0);
        ui.get_dbg_col_h()
    };
    // 判据：`flow-h` 必须真的驱动整段高（40 → 100 应恰好高 60；裸容器不会被绝对定位子项撑高，
    // 所以这条一旦回退成「由子项算高」就会红）。
    let h40 = height_with(mk(40.0));
    let h100 = height_with(mk(100.0));
    assert!(
        (h100 - h40 - 60.0).abs() < 4.0,
        "`mathp` 的 flow-h 必须驱动整段高：40 → {h40}，100 → {h100}"
    );

    // 出图：文字与那张图都必须真的画出来（墨迹存在）。
    let (ui, win, _m) = setup(vec![role_row("怹", true), mk(40.0)], 700);
    ui.window().set_size(slint::PhysicalSize::new(520, 700));
    apply_syschat_layout(&ui, 520.0);
    let f = settle_w(&win, 520, 700);
    let fy = ui.get_dbg_flick_y().round() as u32;
    let fh = ui.get_dbg_flick_h().round() as u32;
    let ink = f.lowest_ink_row(fy, fy + fh, 4, 519);
    assert!(ink.is_some(), "`mathp` 内容应有墨迹（文字/图至少画出一个）");
}

/// 【诊断用·临时】把 owner 的 markdown 全能示例整篇渲染出来，供与设计稿逐节比对。
#[test]
#[ignore = "诊断用；默认不跑"]
fn diag_demo_doc() {
    let dir = std::path::Path::new("/Volumes/DataExt/tmp/syschat-shots");
    std::fs::create_dir_all(dir).expect("建出图目录失败");
    let md = std::fs::read_to_string(
        "/Users/sqb/.agent-bridge/workspaces/syschat/md-demo/markdown-demo.md",
    )
    .expect("读示例文档失败");
    let mut rows = vec![row("role", "怹")];
    push_block_rows_w(&mut rows, crate::mdblocks::parse(&md), 416.0);
    let (ui, win, _m) = setup(rows, 8200);
    ui.set_session_labels(slint::ModelRc::from(Rc::new(slint::VecModel::from(vec![
        slint::SharedString::from("新会话"),
    ]))));
    ui.set_model_labels(slint::ModelRc::from(Rc::new(slint::VecModel::from(vec![
        slint::SharedString::from("m"),
    ]))));
    ui.set_session_index(0);
    ui.set_model_index(0);
    ui.window().set_size(slint::PhysicalSize::new(680, 8200));
    apply_syschat_layout(&ui, 680.0);
    println!("col-h={}", ui.get_dbg_col_h());
    let f = settle_w(&win, 680, 8200);
    write_png(&f, &dir.join("demo-doc.png"));
}

/// `svg_aspect`：`image` crate 读不了 SVG，自己从 `viewBox` / `width`·`height` 取比例。
/// 不这么做，SVG 图片块的 `img-aspect` 恒 0 ⇒ 只能拿 120px 兜底高（源多大多小都一个样）。
#[test]
fn svg_aspect_reads_viewbox_and_wh() {
    let dir = std::env::temp_dir().join(format!("abb-svg-aspect-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let a = dir.join("a.svg");
    std::fs::write(
        &a,
        r#"<svg xmlns="http://www.w3.org/2000/svg" width="520" height="300" viewBox="0 0 520 300"></svg>"#,
    )
    .unwrap();
    let asp = svg_aspect(&a).expect("应能读出比例");
    assert!((asp - 300.0 / 520.0).abs() < 0.001, "viewBox 比例：{asp}");
    // 只有 width/height（带单位）也能读
    let b = dir.join("b.svg");
    std::fs::write(&b, r#"<svg width="100px" height="25px"></svg>"#).unwrap();
    assert!((svg_aspect(&b).unwrap() - 0.25).abs() < 0.001);
    // 非 SVG 不该走这条（位图由 `image::image_dimensions` 负责）
    let c = dir.join("c.png");
    image::RgbaImage::from_pixel(4, 8, image::Rgba([0, 0, 0, 255]))
        .save(&c)
        .unwrap();
    assert!(svg_aspect(&c).is_none(), "非 SVG 应返回 None");
    let _ = std::fs::remove_dir_all(&dir);
}

/// 【诊断用·临时】按设计稿 v12 的消息样例渲染一段（含图）+ 字形核对行。
#[test]
#[ignore = "诊断用；默认不跑"]
fn diag_design_body() {
    let dir = std::path::Path::new("/Volumes/DataExt/tmp/syschat-shots");
    let _ = std::fs::create_dir_all(dir);
    let img = crate::mdblocks::parse("![指数与多项式增长对比](md-demo/assets/demo.svg)");
    let mut rows = vec![
        role_row("怹", true),
        think_row(
            "用户要把架构图做成 HTML 页面。需要：① 把图存成文件 ② 配说明。",
            "3.2s",
            false,
            false,
        ),
        row("h2", "好的，已生成页面"),
        row("p", "要点："),
        row("list", "图片走 `figure`、自适应宽度"),
        row("quote", "单文件即开即用。"),
    ];
    for b in img {
        rows.push(block_row_w(b, 416.0));
    }
    // 字形核对：设计稿的 `⧉`/`⟳` vs 备选 `❐`/`↻`（Menlo 有）vs emoji
    rows.push(row("p", "字形：❏▣⊞▤⇉⇄⇅⎘⧉⿻⿰◰▢▥⧫⬚⬛⊟⊞↻✂⧉ ⏹ ⟳ ▸ ▾ ⧉ ❐ 📋"));
    rows.push(acts_row(0));
    let (ui, win, _m) = setup(rows, 1200);
    ui.set_session_labels(slint::ModelRc::from(Rc::new(slint::VecModel::from(vec![
        slint::SharedString::from("新会话"),
    ]))));
    ui.set_model_labels(slint::ModelRc::from(Rc::new(slint::VecModel::from(vec![
        slint::SharedString::from("m"),
    ]))));
    ui.set_session_index(0);
    ui.set_model_index(0);
    ui.window().set_size(slint::PhysicalSize::new(520, 1200));
    apply_syschat_layout(&ui, 520.0);
    let f = settle_w(&win, 520, 1200);
    write_png(&f, &dir.join("APP-body.png"));
}
