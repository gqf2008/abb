//! **行内公式混排**：把「一段含 `$…$` 的文字」排成「文字 + 公式图」的同一行流。
//!
//! ## 为什么必须自己排
//!
//! Slint 的文字流里**放不进图片**（`Text`/`StyledText` 只有字形，没有内联对象；
//! 实核 `i-slint-core/textlayout.rs`、`styled_text.rs`，1.18 仍如此）。所以只要一段话里
//! 既有文字又有公式，就只能整段自己排——这就是本模块存在的原因。
//!
//! ## 为什么用 `parley`
//!
//! **Slint 自己的文字排版就是 `parley`**（`third_party/i-slint-core/textlayout/sharedparley.rs`，
//! `shared-parley` feature），而 `parley` 0.11 原生支持 **inline box**
//! （`InlineBox { id, index, width, height }` → 布局后给 `PositionedInlineBox { x, y, .. }`）。
//! 于是我们和 Slint 用**同一个排版引擎、同一套字体（fontique 解析系统字体）**：断行与基线天然一致，
//! 不会出现「图对不上文字基线」那类经典翻车。⇒ 也**不需要新增任何编译单元**（parley/fontique/
//! swash/skrifa 都已在 i-slint-core 的依赖树里，与 `pulldown-cmark` 同一情况）。
//!
//! ## 输出形态：坐标 + 元素（不是自己光栅化）
//!
//! 本模块只算**坐标**，不画像素：文字片段仍交给 Slint 的 `Text`（清晰、字体一致、emoji 回退一致），
//! 公式交给 `Image`（`mathrender` 出的 SVG）。`.slint` 侧按坐标**绝对定位**摆这些元素。
//! 换来两个已知取舍（owner 2026-10-15 已确认）：①这些段落**不能拖选/复制**（不再是流式文本）；
//! ②气泡宽度变化时要**重排**（调用方按新宽度重算，见 `ui::relayout_flow_rows`）。
//!
//! ## 失败绝不吞字
//!
//! 任一公式排版失败（非法 TeX）⇒ 整段返回 `None`，调用方回落成「等宽行内码」形态
//! （`mdblocks` 一直把行内码留在 `text` 里，正是为了这条退路）。

use std::path::Path;

use parley::layout::PositionedLayoutItem;
use parley::style::StyleProperty;
use parley::{FontContext, InlineBox, InlineBoxKind, LayoutContext};

/// 行内混排的一段（与 `.slint` 的 `ConvoFlowPiece` 一一对应）。
#[derive(Clone)]
pub struct FlowPiece {
    pub is_math: bool,
    /// 非公式：这一段的文字（一段 = 同一行内同一字体的连续文字）。
    pub text: String,
    /// 公式：排版好的图（`is_math` 为假时是空图）。
    pub image: slint::Image,
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

/// 一段话的混排结果。
#[derive(Clone)]
pub struct FlowLayout {
    pub pieces: Vec<FlowPiece>,
    /// 整段高（px）——调用方必须把它报给外层布局（裸容器不会被绝对定位的子项撑高）。
    pub height: f32,
    pub font_size: f32,
}

/// 排一段「文字 + 行内公式」。
///
/// `math` 是 `(字节偏移, TeX 原文)`，偏移相对 `text`（由 `mdblocks` 在解析时记下，见 `MdBlock::math`）。
/// `width` 是可用宽度（px，= `convo-text-width`）；`cache` 是公式 SVG 的缓存目录。
///
/// 返回 `None` = 有公式排不出来（调用方回落），或这段没有公式。
pub fn layout(
    text: &str,
    math: &[(usize, String)],
    width: f32,
    font_size: f32,
    cache: &Path,
    align: u8,
) -> Option<FlowLayout> {
    if math.is_empty() {
        return None;
    }
    layout_common(text, math, width, font_size, cache, align)
}

/// **单元格**用：没有公式也要排（表格要靠它拿行高与对齐），并支持列对齐。
///
/// `text` 是单元格原文（**含公式的 TeX 源码**，`mdblocks` 的表内口径就是直接拼原文）；本函数
/// 会先把各公式的 TeX 从文本里**剪掉**再排（否则内联图与源码会重复出现），剪不了/排不出来
/// （非法 TeX）就回落成「把原文当纯文本排」——**绝不吞字**。
/// `align`：`0` 左 / `1` 中 / `2` 右（来自表格的 `|:---|:--:|--:|`）。
pub fn layout_cell(
    text: &str,
    math: &[(usize, String)],
    width: f32,
    font_size: f32,
    cache: &Path,
    align: u8,
) -> Option<FlowLayout> {
    if math.is_empty() {
        return layout_common(text, &[], width, font_size, cache, align);
    }
    let mut out = String::new();
    let mut adjusted: Vec<(usize, String)> = Vec::with_capacity(math.len());
    let mut last = 0usize;
    for (off, tex) in math {
        let off = (*off).min(text.len());
        if off < last || !text.is_char_boundary(off) {
            // 偏移不合法（不应该发生）：别猜，直接走纯文本回落。
            return layout_common(text, &[], width, font_size, cache, align);
        }
        let end = (off + tex.len()).min(text.len());
        out.push_str(&text[last..off]);
        adjusted.push((out.len(), tex.clone()));
        last = end;
    }
    out.push_str(&text[last..]);
    layout_common(&out, &adjusted, width, font_size, cache, align)
        .or_else(|| layout_common(text, &[], width, font_size, cache, align))
}

/// [`layout`] 与 [`layout_cell`] 的公共实现（`math` 可为空 = 纯文本排版）。
fn layout_common(
    text: &str,
    math: &[(usize, String)],
    width: f32,
    font_size: f32,
    cache: &Path,
    align: u8,
) -> Option<FlowLayout> {
    // 空文本**但有公式**也要排（整格只有一个公式时，剪掉 TeX 后文本就是空的）。
    if text.trim().is_empty() && math.is_empty() {
        return None;
    }
    // 进程内 memo：**流式每个分片都重建整段**，表格更是每格一次 ⇒ 不缓存就是每个 tick 重排一遍
    // （owner 反馈的「GUI 打开很慢」）。键含宽度/字号/对齐——换个宽度就是要重排。
    let key = FlowKey {
        text: text.to_string(),
        math: math.to_vec(),
        width: width.to_bits(),
        font: font_size.to_bits(),
        align,
    };
    if let Some(hit) = MEMO.with(|m| m.borrow().get(&key).cloned()) {
        return hit;
    }
    let v = layout_uncached(text, math, width, font_size, cache, align);
    MEMO.with(|m| {
        let mut g = m.borrow_mut();
        if g.len() > 512 {
            g.clear();
        }
        g.insert(key, v.clone());
    });
    v
}

/// [`layout_common`] 的缓存键（宽度/字号按位比较 ⇒ 精确命中，不会因浮点近似而错配）。
#[derive(PartialEq, Eq, Hash, Clone)]
struct FlowKey {
    text: String,
    math: Vec<(usize, String)>,
    width: u32,
    font: u32,
    align: u8,
}

thread_local! {
    /// 复用的 parley 上下文（**不要在每次排版里 new**，见 `layout_uncached` 的注释）。
    static CTX: std::cell::RefCell<Option<(FontContext, LayoutContext<()>)>> =
        const { std::cell::RefCell::new(None) };

    static MEMO: std::cell::RefCell<std::collections::HashMap<FlowKey, Option<FlowLayout>>> =
        std::cell::RefCell::new(std::collections::HashMap::new());
}

fn layout_uncached(
    text: &str,
    math: &[(usize, String)],
    width: f32,
    font_size: f32,
    cache: &Path,
    align: u8,
) -> Option<FlowLayout> {
    let width = width.max(1.0);
    let font_size = font_size.max(1.0);

    // 1) 每个公式排成图；**任一失败就整段回落**（半段公式比全不排更糟）。
    //    字号 = 正文同字号，否则行内公式会比周围字大一号。
    let mut boxes = Vec::with_capacity(math.len());
    for (_, tex) in math {
        boxes.push(crate::mathrender::typeset_sized_in(
            tex,
            cache,
            font_size as f64,
        )?);
    }

    // 2) parley 排版：整段文字 + 每个公式一个 in-flow inline box。
    //
    // ⚠️ **`FontContext`/`LayoutContext` 必须复用**（thread-local）：`FontContext::new()` 要建
    // fontique 的字体集合、首次还要扫系统字体——**每次调用都 new 一次就等于「每个单元格
    // 重扫一遍字体库」**，实测那是「GUI 打开很慢」的主因（表格 4 张 ≈ 3.9s）。
    let mut pl = CTX.with(|c| {
        let mut g = c.borrow_mut();
        let (fcx, lcx) = g.get_or_insert_with(|| (FontContext::new(), LayoutContext::<()>::new()));
        let mut builder = lcx.ranged_builder(fcx, text, 1.0, true);
        builder.push_default(StyleProperty::FontSize(font_size));
        for (i, (off, _)) in math.iter().enumerate() {
            let m = &boxes[i];
            builder.push_inline_box(InlineBox {
                id: i as u64,
                kind: InlineBoxKind::InFlow,
                index: (*off).min(text.len()),
                width: m.w,
                // 只报「基线以上」：parley 把 box 底边贴上基线（`y = baseline - height`），
                // 报整个高度会让行高多留一个下落部；下落部由我们下面按 `depth` 补。
                height: m.ascent,
            });
        }
        builder.build(text)
    });
    pl.break_all_lines(Some(width));
    // 列对齐（平文单元格靠它居中/右对齐；左对齐时是 no-op）。
    pl.align(
        match align {
            1 => parley::Alignment::Center,
            2 => parley::Alignment::Right,
            _ => parley::Alignment::Left,
        },
        parley::AlignmentOptions::default(),
    );

    // 3) 逐行取坐标，并按「文字 metrics ∪ 公式 metrics」自己算行高与基线。
    let mut pieces: Vec<FlowPiece> = Vec::new();
    let mut y = 0.0f32;
    for line in pl.lines() {
        let mut t_asc = 0.0f32;
        let mut t_desc = 0.0f32;
        let mut t_line_h = 0.0f32;
        let mut m_asc = 0.0f32;
        let mut m_desc = 0.0f32;
        for item in line.items() {
            match item {
                PositionedLayoutItem::GlyphRun(g) => {
                    let m = g.run().metrics();
                    t_asc = t_asc.max(m.ascent);
                    t_desc = t_desc.max(m.descent);
                    t_line_h = t_line_h.max(m.line_height);
                }
                PositionedLayoutItem::InlineBox(ib) => {
                    let m = &boxes[ib.id as usize];
                    m_asc = m_asc.max(m.ascent);
                    m_desc = m_desc.max(m.depth);
                }
            }
        }
        let ascent = t_asc.max(m_asc);
        let descent = t_desc.max(m_desc);
        let line_h = t_line_h.max(ascent + descent).max(1.0);
        let baseline = y + ascent;

        for item in line.items() {
            match item {
                PositionedLayoutItem::GlyphRun(g) => {
                    let run = g.run();
                    let range = run.text_range();
                    let s = text.get(range).unwrap_or("");
                    if s.is_empty() {
                        continue;
                    }
                    let asc = run.metrics().ascent;
                    let desc = run.metrics().descent;
                    let advance = g.advance();
                    pieces.push(FlowPiece {
                        is_math: false,
                        text: s.to_string(),
                        image: slint::Image::default(),
                        x: g.offset(),
                        y: baseline - asc,
                        w: advance,
                        h: asc + desc,
                    });
                }
                PositionedLayoutItem::InlineBox(ib) => {
                    let m = &boxes[ib.id as usize];
                    pieces.push(FlowPiece {
                        is_math: true,
                        text: String::new(),
                        image: m.image.clone(),
                        x: ib.x,
                        // 图顶放在「基线 − 图内基线以上高」⇒ 图内基线正好落在文字基线上，
                        // 下落部（`depth`）自然伸到基线以下。
                        y: baseline - m.ascent,
                        w: m.w,
                        h: m.h,
                    });
                }
            }
        }
        y += line_h;
    }

    if pieces.is_empty() {
        return None;
    }
    Some(FlowLayout {
        pieces,
        height: y.max(1.0),
        font_size,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_cache(tag: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("abb-flow-{tag}-{}", std::process::id()))
    }

    /// 基线对齐的**结构性**判据：公式块与同行文字块**垂直相交**，且公式顶不低于文字顶
    /// （公式比文字高：带上标/分式时它会往上长，但绝不能整体沉到文字下面）。
    ///
    /// 为什么用「相交」而不是「顶对齐」：基线对齐下两者顶/底都不相等，只有基线相同；
    /// 这里用它们必须互相跨越对方的中腰来钉住「没被顶到行外面去」。
    #[test]
    fn math_box_sits_on_the_text_baseline() {
        let cache = tmp_cache("baseline");
        let _ = std::fs::remove_dir_all(&cache);
        let text = "质能方程 E=mc^2 表明质量与能量等价。";
        let off = "质能方程 ".len();
        let l =
            layout(text, &[(off, "E=mc^2".to_string())], 400.0, 13.0, &cache, 0).expect("应能排版");
        let math = l.pieces.iter().find(|p| p.is_math).expect("应有一个公式块");
        let words: Vec<&FlowPiece> = l.pieces.iter().filter(|p| !p.is_math).collect();
        assert!(!words.is_empty(), "公式前后都应有文字");
        let same_line: Vec<&&FlowPiece> = words
            .iter()
            .filter(|w| w.y < math.y + math.h && math.y < w.y + w.h)
            .collect();
        assert!(
            !same_line.is_empty(),
            "公式必须与同一行的文字共存：math=[{:.1},{:.1}) words={:?}",
            math.y,
            math.y + math.h,
            words.iter().map(|w| (w.y, w.y + w.h)).collect::<Vec<_>>()
        );
        for w in &same_line {
            // 公式顶必须在同行文字的**上半行**内（没被整体沉下去）；允许 1px 的亚像素差：
            // 公式基线以上高与文字 ascent 不一定相等（`E=mc^2` 比文字矮一点）。
            assert!(
                math.y <= w.y + w.h * 0.5,
                "公式不得整体沉到同行文字下方：math.y={:.1} word=[{:.1},{:.1})",
                math.y,
                w.y,
                w.y + w.h
            );
            assert!(
                math.y + math.h >= w.y + w.h * 0.5,
                "公式必须跨过文字中腰（同一基线）：math=[{:.1},{:.1}) word=[{:.1},{:.1})",
                math.y,
                math.y + math.h,
                w.y,
                w.y + w.h
            );
        }
        let _ = std::fs::remove_dir_all(&cache);
    }

    /// 断行：可用宽变窄 ⇒ 行数增加（宽度真的参与了排版，不是摆设）。
    #[test]
    fn narrower_width_breaks_into_more_lines() {
        let cache = tmp_cache("wrap");
        let _ = std::fs::remove_dir_all(&cache);
        let text = "这里是一段比较长的说明文字用来逼出换行效果并且中间夹着公式。";
        let off = "这里是一段比较长的说明文字用来逼出换行效果并且中间夹着".len();
        let math = vec![(off, r"\frac{a}{b}".to_string())];
        let wide = layout(text, &math, 600.0, 13.0, &cache, 0).expect("宽版应能排版");
        let narrow = layout(text, &math, 120.0, 13.0, &cache, 0).expect("窄版应能排版");
        assert!(
            narrow.height > wide.height,
            "窄宽度应更高（多行）：narrow={:.1} wide={:.1}",
            narrow.height,
            wide.height
        );
        let _ = std::fs::remove_dir_all(&cache);
    }

    /// 反例：非法 TeX ⇒ `None`（调用方据此回落成行内码，绝不吞字）。
    #[test]
    fn invalid_tex_falls_back() {
        let cache = tmp_cache("bad");
        let _ = std::fs::remove_dir_all(&cache);
        assert!(
            layout(
                "前 $\\frac{a}{$ 后",
                &[(2, r"\frac{a}{".to_string())],
                400.0,
                13.0,
                &cache,
                0
            )
            .is_none(),
            "非法公式必须回落"
        );
        let _ = std::fs::remove_dir_all(&cache);
    }

    /// 没有公式 ⇒ `None`（不参与混排）；整段只有一个公式（文本部分为空）⇒ 仍要排得出来。
    #[test]
    fn no_math_or_empty_is_not_flow() {
        let cache = tmp_cache("none");
        assert!(
            layout("普通句子", &[], 400.0, 13.0, &cache, 0).is_none(),
            "无公式的段落不该走混排"
        );
        let only = layout("", &[(0, "x".to_string())], 400.0, 13.0, &cache, 0)
            .expect("只有公式的段（文本为空）也要能排");
        assert!(only.pieces.iter().any(|p| p.is_math), "应有一个公式块");
        let _ = std::fs::remove_dir_all(&cache);
    }

    /// 片段文本必须能拼回原文（**除了被公式占掉的位置**）——防止坐标提取把字吞了/重复了。
    #[test]
    fn pieces_rebuild_the_plain_text() {
        let cache = tmp_cache("rebuild");
        let _ = std::fs::remove_dir_all(&cache);
        // 公式位置的字符在 `plain` 里本来就不存在（`$…$` 整段不进入 plain）
        let text = "甲乙丙  丁戊";
        let off = "甲乙丙 ".len();
        let l = layout(text, &[(off, "x^2".to_string())], 400.0, 13.0, &cache, 0).expect("排版");
        assert_eq!(l.pieces.iter().filter(|p| p.is_math).count(), 1);
        let rebuilt: String = l
            .pieces
            .iter()
            .filter(|p| !p.is_math)
            .map(|p| p.text.as_str())
            .collect();
        let norm = |s: &str| s.chars().filter(|c| !c.is_whitespace()).collect::<String>();
        assert_eq!(norm(&rebuilt), "甲乙丙丁戊", "拼回文本异常：{rebuilt:?}");
        let _ = std::fs::remove_dir_all(&cache);
    }
    /// 单元格混排：公式要**真排版**，且源码不得再当文字画一遍；非法 TeX 回落时源码不得丢。
    #[test]
    fn cell_math_is_typeset_without_duplicating_source() {
        let cache = tmp_cache("cell");
        let _ = std::fs::remove_dir_all(&cache);
        // 整格只有一个公式（`mdblocks` 的表内口径：TeX 直接拼在文本里）
        let l = layout_cell(
            "f(x)=1",
            &[(0, "f(x)=1".to_string())],
            120.0,
            12.0,
            &cache,
            0,
        )
        .expect("整格公式也要排得出来");
        assert_eq!(l.pieces.iter().filter(|p| p.is_math).count(), 1);
        let txt: String = l
            .pieces
            .iter()
            .filter(|p| !p.is_math)
            .map(|p| p.text.as_str())
            .collect();
        assert!(!txt.contains("f(x)"), "公式源码不得再画一遍：{txt:?}");

        // 文本 + 公式混排
        let cell = "增长 O(n^2) 很快";
        let off = "增长 ".len();
        let l2 = layout_cell(cell, &[(off, "O(n^2)".to_string())], 200.0, 12.0, &cache, 0)
            .expect("混排应能排");
        assert!(l2.pieces.iter().any(|p| p.is_math));
        let txt2: String = l2
            .pieces
            .iter()
            .filter(|p| !p.is_math)
            .map(|p| p.text.as_str())
            .collect();
        assert!(txt2.contains("增长") && txt2.contains("很快"));
        assert!(
            !txt2.contains("n^2"),
            "公式源码混排时也不该出现在文字里：{txt2:?}"
        );

        // 非法 TeX ⇒ 回落成纯文本，**源码不丢**
        let cell3 = "前 \\frac{a}{ 后";
        let l3 = layout_cell(
            cell3,
            &[(2, "\\frac{a}{".to_string())],
            200.0,
            12.0,
            &cache,
            0,
        );
        let txt3: String = l3
            .map(|l| {
                l.pieces
                    .iter()
                    .filter(|p| !p.is_math)
                    .map(|p| p.text.as_str())
                    .collect()
            })
            .unwrap_or_default();
        assert!(txt3.contains("frac"), "回落必须保留源码：{txt3:?}");
        let _ = std::fs::remove_dir_all(&cache);
    }
    /// 守卫：`FontContext` 必须**复用**（thread-local `CTX`），不许每次排版都 new。
    ///
    /// 为什么值得钉：`FontContext::new()` 会建 fontique 字体集、首次扫系统字体 ⇒ 每个单元格
    /// 重来一遍。实测 4 张表 3.9s → 复用后 0.5s（`diag_build_breakdown` 的读数），是
    /// owner 反馈「GUI 打开很慢」的主因。这条守卫让「改回每次 new」立刻红。
    #[test]
    fn font_context_is_reused_not_rebuilt_per_layout() {
        let src = include_str!("inlineflow.rs");
        // 拼接出「旧写法」再查，避免这条断言把自己的字面量也数进去。
        let legacy = ["let mut fcx = ", "Font", "Context::new();"].concat();
        assert!(
            !src.contains(&legacy),
            "不许在每次排版里新建 `FontContext`（要复用 thread-local CTX）"
        );
        assert!(
            src.contains("(FontContext::new(), LayoutContext::<()>::new())"),
            "必须走 thread-local `CTX` 的复用路径"
        );
    }
}
