//! Markdown → 渲染块解析（系统对话的「完整 markdown」支持）。
//!
//! ## 为什么自绘而不是用 Slint 内置
//!
//! 实核（`i-slint-compiler-1.18.0/builtin_elements.rs:1246-1256`）：Slint 内置的
//! `StyledText` **明确不支持**标题/表格/引用/图片/分隔线，且 Slint **没有 audio/video
//! 元素**。所以「完整 markdown + 媒体」必须在 Rust 侧把 markdown 拆成**块**，再由 GUI
//! 逐块布局；块内的**行内样式**（粗体/斜体/行内码/链接/删除线）交给 `StyledText` 渲染。
//!
//! 本模块只做「markdown → [`MdBlock`]」，纯函数、无 IO，可独立单测。
//!
//! ## 块切分口径
//!
//! - 标题 `#`/`##`/`###` → `h1`/`h2`/`h3`（更深层级归到 `h3`，视觉上不再加深）；
//! - 段落 → `p`；
//! - 围栏/缩进代码块 → `code`（带语言标注，正文原样、**不转义**）；
//! - 引用 → `quote`（内容按行内 markdown 渲染）；
//! - 无序/有序列表 → `list`（`level` = 嵌套深度，`order` = 序号，无序为 0）；
//! - 任务列表 → `task`（`checked`）；
//! - 表格 → 每行一个 `table` 块（`cells` 为单元格的行内 markdown，`header` 标表头行）；
//! - 分隔线 → `hr`；
//! - 图片/媒体（`![..](src)` 或 `[..](src)` 且扩展名是媒体）→ `image`/`audio`/`video`，
//!   恒为**独立块**（Slint 无法把图片排进文字流），切成块后其前后的文字各自成段。
//!
//! 未知/不支持的节点（HTML、脚注、数学）退化为文本，**绝不吞内容**。

use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};

/// 一个渲染块。字段取并集（GUI 按 `kind` 取用）。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct MdBlock {
    /// `h1|h2|h3|p|list|task|code|quote|table|hr|image|audio|video`
    pub kind: String,
    /// 行内 markdown 文本（h*/p/list/task/quote/table 用）；`code` 为代码原文。
    pub text: String,
    /// 列表缩进层级（0 起）；非列表为 0。
    pub level: i32,
    /// 有序列表序号（从 1 起）；无序/非列表为 0。
    pub order: i32,
    /// 任务列表是否勾选。
    pub checked: bool,
    /// 代码块语言标注。
    pub lang: String,
    /// 表格行单元格（行内 markdown）。
    pub cells: Vec<String>,
    /// 表格该行是否表头。
    pub header: bool,
    /// 图片/媒体地址（本地路径或 `http(s)`）；`alt` 为替代文本/展示名。
    pub src: String,
    pub alt: String,
}

impl MdBlock {
    fn new(kind: &str) -> Self {
        Self {
            kind: kind.to_string(),
            text: String::new(),
            level: 0,
            order: 0,
            checked: false,
            lang: String::new(),
            cells: Vec::new(),
            header: false,
            src: String::new(),
            alt: String::new(),
        }
    }
}

/// 媒体扩展名 → 块类型（大小写不敏感，忽略 `?query`/`#frag`）。
fn media_kind_for(src: &str) -> Option<&'static str> {
    let path = src.split(['?', '#']).next().unwrap_or(src);
    let ext = path.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    match ext.as_str() {
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "svg" | "avif" | "ico" => Some("image"),
        "mp3" | "wav" | "m4a" | "aac" | "ogg" | "flac" | "opus" => Some("audio"),
        "mp4" | "mov" | "webm" | "m4v" | "mkv" | "avi" => Some("video"),
        _ => None,
    }
}

/// 行内文本转义：块内行内 markdown 是**我们自己重建**的，故把元字符转义，
/// 避免普通文本里的 `*`/`_`/`` ` ``/`[` 被当成样式。
fn push_escaped(t: &str, out: &mut String) {
    for c in t.chars() {
        if matches!(c, '\\' | '`' | '*' | '_' | '[' | ']' | '<' | '>') {
            out.push('\\');
        }
        out.push(c);
    }
}

/// 行内 markdown → Slint `StyledText`（**Rust 侧**解析）。
///
/// 为什么不在 `.slint` 里用 `@markdown("{x}")`（卡 abb-syschat-b-markdown 评审 B1/B2，2026-10-13）：
/// Slint 的插值模板要求字面量以 `\{` 起，`"{x}"` 会被当**编译期常量 markdown**
/// （生成代码里是 `parse_markdown("{b.text}", &[])`），渲染成字面量；即便写成 `\{x}`，
/// 插值实参也只按**纯文本**注入（`StringToStyledText`），行内样式仍不生效。
/// 所以必须在这里把真实值解析成 StyledText，GUI 直绑 `text: b.styled`。
pub fn styled(text: &str) -> slint::StyledText {
    slint::StyledText::from_markdown(text)
        .unwrap_or_else(|_| slint::StyledText::from_plain_text(text))
}

/// 反解转义（媒体 alt/src 展示用）。
fn unescape(s: &str) -> String {
    s.replace("\\*", "*")
        .replace("\\_", "_")
        .replace("\\[", "[")
        .replace("\\]", "]")
        .replace("\\`", "`")
        .replace('\\', "")
}

/// 落盘当前文本块（空文本丢弃；`hr` 例外）。段落若「只是一条媒体链接」，转成媒体块。
fn flush_cur(blocks: &mut Vec<MdBlock>, cur: &mut Option<MdBlock>) {
    if let Some(mut b) = cur.take() {
        b.text = b.text.trim().to_string();
        if b.text.is_empty() {
            return;
        }
        if b.kind == "p" {
            if let Some(m) = media_from_text(&b.text) {
                blocks.push(m);
                return;
            }
        }
        blocks.push(b);
    }
}

/// 「整段只是一条媒体」→ 媒体块：`![alt](src)`（图片）或 `[alt](src)` 且 src 扩展名是媒体。
fn media_from_text(text: &str) -> Option<MdBlock> {
    let t = text.trim();
    let (alt, src, forced_image) = if let Some(rest) = t.strip_prefix("![") {
        let i = rest.find("](")?;
        (&rest[..i], rest[i + 2..].strip_suffix(')')?, true)
    } else {
        let rest = t.strip_prefix('[')?;
        let i = rest.find("](")?;
        (&rest[..i], rest[i + 2..].strip_suffix(')')?, false)
    };
    let kind = if forced_image {
        media_kind_for(src).unwrap_or("image")
    } else {
        media_kind_for(src)?
    };
    let mut b = MdBlock::new(kind);
    b.src = dest_strip_title(src);
    let alt = unescape(alt.trim());
    b.alt = if alt.is_empty() {
        "image".to_string()
    } else {
        alt
    };
    Some(b)
}

/// 解析 markdown 为渲染块序列。**绝不返回 Err**：异常一律退化为文本块。
pub fn parse(markdown: &str) -> Vec<MdBlock> {
    if markdown.trim().is_empty() {
        return Vec::new();
    }
    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_TABLES);
    opts.insert(Options::ENABLE_STRIKETHROUGH);
    opts.insert(Options::ENABLE_TASKLISTS);
    let parser = Parser::new_ext(markdown, opts);

    let mut blocks: Vec<MdBlock> = Vec::new();
    let mut cur: Option<MdBlock> = None;
    // 行内容器闭合栈（`*`/`**`/`~~`/`](url)`）
    let mut closes: Vec<String> = Vec::new();
    // 代码块 (lang, buf)
    let mut code: Option<(String, String)> = None;
    // 上下文栈：当前文本块类型（"p" / "h1" / "h2" / "h3" / "quote" / "list"）
    let mut ctx: Vec<&'static str> = Vec::new();
    // 列表栈：每层 (ordered, next_ordinal)
    let mut lists: Vec<(bool, u64)> = Vec::new();
    let mut pending_task: Option<bool> = None;
    // 图片（可能带 alt）
    let mut image: Option<(String, String)> = None;
    // 表格
    let mut cell = String::new();
    let mut cells: Vec<String> = Vec::new();
    let mut in_table = false;
    let mut head_row = false;

    // 确保 cur 存在（按当前上下文类型建块）：取**最内层的非 p 容器**（引用/列表项里的
    // 段落应保留 quote/list 类型，否则 `> 引用` 会被当成普通段落）。
    macro_rules! ensure_cur {
        () => {
            if cur.is_none() {
                let kind = ctx
                    .iter()
                    .rev()
                    .find(|k| **k != "p")
                    .copied()
                    .unwrap_or("p");
                let mut b = MdBlock::new(kind);
                if kind == "list" {
                    if let Some(done) = pending_task {
                        b.kind = "task".to_string();
                        b.checked = done;
                    }
                }
                cur = Some(b);
            }
        };
    }

    for ev in parser {
        match ev {
            Event::Start(tag) => match tag {
                Tag::Paragraph => ctx.push("p"),
                Tag::Heading { level, .. } => ctx.push(match level {
                    HeadingLevel::H1 => "h1",
                    HeadingLevel::H2 => "h2",
                    _ => "h3",
                }),
                Tag::BlockQuote(_) => {
                    flush_cur(&mut blocks, &mut cur);
                    ctx.push("quote");
                }
                Tag::CodeBlock(kind) => {
                    flush_cur(&mut blocks, &mut cur);
                    let lang = match kind {
                        CodeBlockKind::Fenced(l) => l.trim().to_string(),
                        CodeBlockKind::Indented => String::new(),
                    };
                    code = Some((lang, String::new()));
                }
                Tag::List(start) => lists.push((start.is_some(), start.unwrap_or(0))),
                Tag::Item => {
                    flush_cur(&mut blocks, &mut cur);
                    ctx.push("list");
                    pending_task = None;
                }
                Tag::Table(_) => in_table = true,
                Tag::TableHead => {
                    head_row = true;
                    cells.clear();
                }
                Tag::TableRow => {
                    head_row = false;
                    cells.clear();
                }
                Tag::TableCell => cell.clear(),
                Tag::Emphasis if !in_table => {
                    ensure_cur!();
                    cur.as_mut().unwrap().text.push('*');
                    closes.push("*".into());
                }
                Tag::Strong if !in_table => {
                    ensure_cur!();
                    cur.as_mut().unwrap().text.push_str("**");
                    closes.push("**".into());
                }
                Tag::Strikethrough if !in_table => {
                    ensure_cur!();
                    cur.as_mut().unwrap().text.push_str("~~");
                    closes.push("~~".into());
                }
                Tag::Link { dest_url, .. } if !in_table => {
                    ensure_cur!();
                    cur.as_mut().unwrap().text.push('[');
                    closes.push(format!("]({dest_url})"));
                }
                // 表格内的图片：降级为单元格里的 alt 文字（否则会打断表格，评审 N2）
                Tag::Image { dest_url, .. } if !in_table => {
                    flush_cur(&mut blocks, &mut cur);
                    image = Some((dest_url.to_string(), String::new()));
                }
                _ => {}
            },
            Event::End(tag) => match tag {
                TagEnd::Paragraph | TagEnd::Heading(_) => {
                    ctx.pop();
                    flush_cur(&mut blocks, &mut cur);
                    closes.clear();
                }
                TagEnd::BlockQuote(_) => {
                    flush_cur(&mut blocks, &mut cur);
                    ctx.pop();
                }
                TagEnd::CodeBlock => {
                    if let Some((lang, buf)) = code.take() {
                        let mut b = MdBlock::new("code");
                        b.lang = lang;
                        b.text = buf.strip_suffix('\n').unwrap_or(&buf).to_string();
                        blocks.push(b);
                    }
                }
                TagEnd::List(_) => {
                    lists.pop();
                }
                TagEnd::Item => {
                    let mut b = match cur.take() {
                        Some(mut b) => {
                            b.text = b.text.trim().to_string();
                            b
                        }
                        None => MdBlock::new("list"),
                    };
                    if !b.text.is_empty() {
                        b.level = lists.len().saturating_sub(1) as i32;
                        if let Some((ordered, next)) = lists.last().copied() {
                            if ordered {
                                b.order = next as i32;
                                if let Some((_, n)) = lists.last_mut() {
                                    *n += 1;
                                }
                            }
                        }
                        blocks.push(b);
                    }
                    closes.clear();
                    ctx.pop();
                    pending_task = None;
                }
                TagEnd::Table => {
                    in_table = false;
                    head_row = false;
                }
                TagEnd::TableHead => {
                    push_table_row(&mut blocks, &mut cells, true);
                    head_row = false;
                }
                TagEnd::TableRow => push_table_row(&mut blocks, &mut cells, head_row),
                TagEnd::TableCell => {
                    cells.push(cell.trim().to_string());
                    cell.clear();
                }
                TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough | TagEnd::Link
                    if !in_table =>
                {
                    if let Some(c) = closes.pop() {
                        ensure_cur!();
                        cur.as_mut().unwrap().text.push_str(&c);
                    }
                }
                TagEnd::Image => {
                    if let Some((src, alt)) = image.take() {
                        let kind = media_kind_for(&src).unwrap_or("image");
                        let mut m = MdBlock::new(kind);
                        let alt = unescape(alt.trim());
                        m.alt = if alt.is_empty() {
                            "image".to_string()
                        } else {
                            alt
                        };
                        m.src = dest_strip_title(&src);
                        blocks.push(m);
                    }
                }
                _ => {}
            },
            Event::Text(t) => {
                if let Some((_, buf)) = code.as_mut() {
                    buf.push_str(&t);
                } else if let Some((_, alt)) = image.as_mut() {
                    alt.push_str(&t);
                } else if in_table {
                    // 表格单元格按**纯文本**渲染（GUI 用 Text），故不过行内 markdown、不转义
                    cell.push_str(&t);
                } else {
                    ensure_cur!();
                    push_escaped_block(&t, cur.as_mut().unwrap());
                }
            }
            Event::Code(c) => {
                if code.is_none() {
                    if in_table {
                        cell.push_str(&c);
                    } else {
                        ensure_cur!();
                        let t = &mut cur.as_mut().unwrap().text;
                        t.push('`');
                        push_escaped(&c, t);
                        t.push('`');
                    }
                }
            }
            Event::SoftBreak => {
                if !in_table {
                    ensure_cur!();
                    cur.as_mut().unwrap().text.push(' ');
                }
            }
            Event::HardBreak => {
                if !in_table {
                    ensure_cur!();
                    cur.as_mut().unwrap().text.push('\n');
                }
            }
            Event::Rule => {
                flush_cur(&mut blocks, &mut cur);
                blocks.push(MdBlock::new("hr"));
            }
            Event::TaskListMarker(done) => {
                pending_task = Some(done);
                if let Some(c) = cur.as_mut() {
                    c.kind = "task".to_string();
                    c.checked = done;
                }
            }
            // HTML / 脚注 / 数学：退化为文本（绝不吞）
            Event::Html(h) | Event::InlineHtml(h) => {
                if let Some((_, buf)) = code.as_mut() {
                    buf.push_str(&h);
                } else if in_table {
                    push_escaped(&h, &mut cell);
                } else {
                    ensure_cur!();
                    push_escaped_block(&h, cur.as_mut().unwrap());
                }
            }
            Event::FootnoteReference(r) => {
                if !in_table {
                    ensure_cur!();
                    let t = &mut cur.as_mut().unwrap().text;
                    t.push('[');
                    push_escaped(&r, t);
                    t.push(']');
                }
            }
            Event::InlineMath(m) | Event::DisplayMath(m) => {
                if !in_table {
                    ensure_cur!();
                    let t = &mut cur.as_mut().unwrap().text;
                    t.push('`');
                    push_escaped(&m, t);
                    t.push('`');
                }
            }
        }
    }

    flush_cur(&mut blocks, &mut cur);
    if !cells.is_empty() {
        push_table_row(&mut blocks, &mut cells, head_row);
    }
    blocks
}

/// 在 `MdBlock` 上追加转义文本（宏 `ensure_cur!` 后调用）。
fn push_escaped_block(t: &str, b: &mut MdBlock) {
    push_escaped(t, &mut b.text);
}

/// 表格行落盘。
fn push_table_row(blocks: &mut Vec<MdBlock>, cells: &mut Vec<String>, header: bool) {
    if cells.is_empty() {
        return;
    }
    let mut b = MdBlock::new("table");
    b.cells = std::mem::take(cells);
    b.header = header;
    blocks.push(b);
}

/// 去掉链接目标里的 title 部分（`url "title"` → `url`）。
///
/// **不能**用 `split_whitespace().next()`：pulldown-cmark 已把 `<...>` 角括号剥掉，
/// 带空格的文件名（macOS 截图常见）会被当成 title 切掉（评审 N1）。只切「空格 + 引号」。
fn dest_strip_title(dest: &str) -> String {
    match dest.find(" \"") {
        Some(i) => dest[..i].to_string(),
        None => dest.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(bs: &[MdBlock]) -> Vec<&str> {
        bs.iter().map(|b| b.kind.as_str()).collect()
    }

    #[test]
    fn empty_and_plain() {
        assert!(parse("").is_empty());
        assert!(parse("   \n  ").is_empty());
        let b = parse("你好，世界");
        assert_eq!(kinds(&b), vec!["p"]);
        assert_eq!(b[0].text, "你好，世界");
    }

    #[test]
    fn headings_levels() {
        let b = parse("# 一级\n\n## 二级\n\n### 三级\n\n#### 四级");
        assert_eq!(kinds(&b), vec!["h1", "h2", "h3", "h3"]);
        assert_eq!(b[0].text, "一级");
        assert_eq!(b[3].text, "四级", "四级归到 h3");
    }

    #[test]
    fn inline_styles_roundtrip_to_markdown() {
        let b = parse("这是 **粗** 与 *斜* 与 ~~删~~ 与 `码` 与 [链接](https://x.y)");
        assert_eq!(b.len(), 1);
        let t = &b[0].text;
        assert!(t.contains("**粗**"), "{t}");
        assert!(t.contains("*斜*"), "{t}");
        assert!(t.contains("~~删~~"), "{t}");
        assert!(t.contains("`码`"), "{t}");
        assert!(t.contains("[链接](https://x.y)"), "{t}");
    }

    #[test]
    fn text_metachars_are_escaped() {
        let b = parse("2 * 3 = 6，数组 a[0]");
        assert_eq!(b.len(), 1);
        assert!(b[0].text.contains("\\*"), "必须转义：{}", b[0].text);
        assert!(b[0].text.contains("a\\[0\\]"), "必须转义：{}", b[0].text);
    }

    #[test]
    fn fenced_code_keeps_lang_and_body_unmodified() {
        let b = parse("```rust\nfn main() { /* 不转义 * */ }\n```");
        assert_eq!(kinds(&b), vec!["code"]);
        assert_eq!(b[0].lang, "rust");
        assert!(b[0].text.contains("/* 不转义 * */"), "{}", b[0].text);
        assert!(!b[0].text.contains('\\'), "代码正文不得转义：{}", b[0].text);
    }

    #[test]
    fn unclosed_fence_degrades_to_code() {
        let b = parse("```\nlet x = 1;");
        assert_eq!(kinds(&b), vec!["code"]);
        assert!(b[0].text.contains("let x = 1;"));
    }

    #[test]
    fn lists_nested_ordered_and_task() {
        let b = parse("- a\n- b\n  - b1\n\n1. one\n2. two\n\n- [x] done\n- [ ] todo");
        let ks = kinds(&b);
        assert!(ks.contains(&"list"), "{ks:?}");
        assert!(ks.contains(&"task"), "{ks:?}");
        let nested = b.iter().find(|x| x.text == "b1").unwrap();
        assert_eq!(nested.level, 1, "嵌套层级");
        assert_eq!(b.iter().find(|x| x.text == "one").unwrap().order, 1);
        assert_eq!(b.iter().find(|x| x.text == "two").unwrap().order, 2);
        let task = b.iter().find(|x| x.kind == "task").unwrap();
        assert!(task.checked, "任务列表勾选态");
        assert!(
            b.iter().any(|x| x.kind == "task" && !x.checked),
            "未完成任务应在：{ks:?}"
        );
    }

    #[test]
    fn blockquote_and_rule() {
        let b = parse("> 引用一行\n\n---\n");
        assert_eq!(kinds(&b), vec!["quote", "hr"]);
        assert!(b[0].text.contains("引用一行"));
    }

    #[test]
    fn table_rows_and_header() {
        let b = parse("| A | B |\n|---|---|\n| 1 | 2 |\n| 3 | 4 |\n");
        let rows: Vec<&MdBlock> = b.iter().filter(|x| x.kind == "table").collect();
        assert_eq!(rows.len(), 3, "表头 + 两行：{:?}", kinds(&b));
        assert!(rows[0].header, "首行是表头");
        assert_eq!(rows[0].cells, vec!["A", "B"]);
        assert_eq!(rows[1].cells, vec!["1", "2"]);
        assert_eq!(rows[2].cells, vec!["3", "4"]);
    }

    #[test]
    fn image_becomes_image_block() {
        let b = parse("![架构图](assets/arch.png)");
        assert_eq!(kinds(&b), vec!["image"]);
        assert_eq!(b[0].src, "assets/arch.png");
        assert_eq!(b[0].alt, "架构图");
    }

    #[test]
    fn image_inside_paragraph_splits_blocks() {
        let b = parse("前文 ![图](a.png) 后文");
        assert_eq!(kinds(&b), vec!["p", "image", "p"], "{:?}", kinds(&b));
        assert_eq!(b[0].text, "前文");
        assert_eq!(b[2].text, "后文");
    }

    #[test]
    fn http_and_media_links_classified() {
        assert_eq!(
            kinds(&parse("![网图](https://example.com/a.webp)")),
            vec!["image"]
        );
        assert_eq!(
            kinds(&parse("[语音](https://example.com/v.wav)")),
            vec!["audio"]
        );
        assert_eq!(
            kinds(&parse("[视频](https://example.com/demo.mp4)")),
            vec!["video"]
        );
        assert_eq!(
            kinds(&parse("[普通链接](https://example.com/page)")),
            vec!["p"],
            "非媒体链接仍是段落"
        );
    }

    #[test]
    fn image_with_title_and_alt_text() {
        let b = parse("![我的 图](a.png \"标题\")");
        assert_eq!(kinds(&b), vec!["image"]);
        assert_eq!(b[0].src, "a.png", "title 不得混进 src");
        assert_eq!(b[0].alt, "我的 图");
    }

    #[test]
    fn crlf_and_long_line() {
        let b = parse("第一行\r\n第二行");
        assert_eq!(b.len(), 1);
        assert!(b[0].text.contains("第一行"));
        let long = "字".repeat(5000);
        let b = parse(&long);
        assert_eq!(b[0].text.chars().count(), 5000, "超长单行不截断");
    }

    #[test]
    fn media_kind_extensions() {
        assert_eq!(media_kind_for("a.PNG"), Some("image"));
        assert_eq!(media_kind_for("a/b.mp4?x=1"), Some("video"));
        assert_eq!(media_kind_for("a.m4a#t=3"), Some("audio"));
        assert_eq!(media_kind_for("a.txt"), None);
    }

    /// 评审 B1/B2 的负向守卫：行内 markdown 必须在 **Rust 侧**真解析，
    /// 不得退化成把模板串/纯文本交给 Slint。
    #[test]
    fn styled_parses_markdown_not_template() {
        let s = styled("**粗** 与 `码`");
        assert_ne!(
            s,
            slint::StyledText::from_plain_text("**粗** 与 `码`"),
            "行内样式未生效（与纯文本相同）"
        );
        assert_ne!(
            s,
            slint::StyledText::from_markdown("{b.text}").unwrap(),
            "不得退化成编译期模板串"
        );
    }

    #[test]
    fn path_with_spaces_not_truncated() {
        let b = parse("![截图](<my file.png>)");
        assert_eq!(kinds(&b), vec!["image"]);
        assert_eq!(b[0].src, "my file.png", "带空格路径不得被 title 规则截断");
        // title 仍要剥掉
        let b = parse("![图](a.png \"标题\")");
        assert_eq!(b[0].src, "a.png");
    }

    #[test]
    fn image_in_table_degrades_to_alt_text() {
        let b = parse("| 图 |\n|---|\n| ![alt](a.png) |\n");
        let rows: Vec<&MdBlock> = b.iter().filter(|x| x.kind == "table").collect();
        assert_eq!(rows.len(), 2, "表格不应被图片打断：{:?}", kinds(&b));
        assert_eq!(rows[1].cells, vec!["alt"], "表内图片降级为 alt 文本");
    }
}
