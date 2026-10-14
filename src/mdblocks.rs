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
//! - 图片/媒体 → `image`/`audio`/`video`，恒为**独立块**（Slint 无法把图片排进文字流），
//!   切成块后其前后的文字各自成段。识别三种写法：
//!   1. markdown 图片/链接：`![alt](src)`、`[alt](src)`（src 扩展名是媒体）；
//!   2. **HTML 媒体标签**：`<img src=…>`、`<audio src=…>`、`<video src=…>`（含
//!      `<video><source src=…></video>` 的子元素形态）——D7 卡里 agent 正用这种形态；
//!   3. **裸媒体地址**：整段只有一个不含空白的 token 且扩展名是媒体（`https://…/a.mov`、
//!      `./assets/a.mp4`）。
//!
//! 未知/不支持的节点（其它 HTML、脚注、数学）退化为文本，**绝不吞内容**；但被识别为媒体
//! 标签的**开/闭标签本身**不再作为字面文本漏出（否则 `<audio …>` 成块后 `</audio>` 会
//! 显示成一行转义文本）。

use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};

/// 一个渲染块。字段取并集（GUI 按 `kind` 取用）。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct MdBlock {
    /// `h1|h2|h3|p|list|task|code|quote|table|hr|image|audio|video`
    pub kind: String,
    /// 行内 markdown 文本（[`inline_markdown_kind`] 那些 kind 用）；`code` 为代码原文；
    /// 结构化/媒体块（`table|hr|image|audio|video`）不用它（表格正文在 `cells`）。
    pub text: String,
    /// 列表缩进层级（0 起）；非列表为 0。
    pub level: i32,
    /// 有序列表序号（从 1 起）；无序/非列表为 0。
    pub order: i32,
    /// 任务列表是否勾选。
    pub checked: bool,
    /// 代码块语言标注。
    pub lang: String,
    /// 表格行单元格（**纯文本**：表内不过行内 markdown，见 [`parse`] 的 `in_table` 分支）。
    pub cells: Vec<String>,
    /// 表格该行是否表头。
    pub header: bool,
    /// 图片/媒体地址（本地路径或 `http(s)`）；`alt` 为替代文本/展示名。
    pub src: String,
    pub alt: String,
}

impl MdBlock {
    pub(crate) fn new(kind: &str) -> Self {
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
/// 避免普通文本里的 `*`/`_`/`` ` ``/`[`/`~` 被当成样式。
///
/// 覆盖口径：这里必须覆盖**解析器会重发的每一个元字符**——[`has_inline_markup`] 的判据
///（「未转义元字符 = 真有样式」）就建立在「字面量一定已转义」之上。漏一个就会在判据之外
/// 静默出错：`~` 曾漏在此列，于是 `~~删~~` 被当成「无样式」，GUI 换成只读 `TextInput`
/// 后把 `~~删~~` 当字面量画了出来（删除线没了；独立评审实测两分支差异 920 像素）。
/// 已实核 Slint 的 markdown 认 `\~`：`styled("3\\~4")` 与 `styled("3~4")` 逐像素一致。
fn push_escaped(t: &str, out: &mut String) {
    for c in t.chars() {
        if matches!(c, '\\' | '`' | '*' | '_' | '[' | ']' | '<' | '>' | '~') {
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

/// 该块的行内文本里**是否真有行内样式标记**（粗体/斜体/行内码/链接/原义 HTML）。
///
/// `push_escaped` 保证块内行内文本里**字面量元字符都已转义**（`\\` / `` \` `` / `\*` …），
/// 所以「出现**未被转义**的元字符」精确等价于「这段文本真有 markdown 行内样式」。
/// **只对「行内 markdown」形态的文本成立**（见 [`selectable_plain`]）；`code` 的正文不转义，
/// 拿它判样式会把 `ls *.rs` 误判成有样式。
fn has_inline_markup(escaped: &str) -> bool {
    let mut it = escaped.chars();
    while let Some(c) = it.next() {
        // 转义序列：整对跳过（`\*` 是字面星号，不是样式）
        if c == '\\' {
            let _ = it.next();
            continue;
        }
        if matches!(c, '`' | '*' | '_' | '[' | ']' | '<' | '>' | '~') {
            return true;
        }
    }
    false
}

/// 无行内样式块的**展示文本**（`push_escaped` 的逆运算：去掉转义用的反斜杠）。
///
/// **仅**对「行内 markdown」形态的文本成立——有样式时反斜杠里混着 markdown 标记的转义，
/// 直接反解会留下标记字符；对 `code` 的原文反解会吃掉代码里的反斜杠。调用点由
/// [`selectable_plain`] 前置条件保证。
fn plain(escaped: &str) -> String {
    let mut out = String::with_capacity(escaped.len());
    let mut it = escaped.chars();
    while let Some(c) = it.next() {
        if c == '\\' {
            if let Some(n) = it.next() {
                out.push(n);
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// 该 kind 的正文是**行内 markdown**：字面量元字符已被 [`push_escaped`] 转义，渲染走
/// [`styled`]（`ui::block_row` 只为这些 kind 算 `StyledText`——`code` 走原文、
/// `table|hr|image|audio|video` 不读它，见该处注释）。
///
/// 只在此处定义一次：判「要不要换成只读 `TextInput`」的 [`selectable_plain`] 与判
/// 「要不要算 `styled`」的 `ui::block_row` 必须同集合，否则要么白算要么白丢样式。
pub fn inline_markdown_kind(kind: &str) -> bool {
    matches!(kind, "h1" | "h2" | "h3" | "p" | "list" | "task" | "quote")
}

/// 该块的正文能否**换成只读 `TextInput` 渲染**（卡 `abb-v11-message-actions`；Slint 1.18
/// 的 `read-only` 仍可拖选并 ⌘C），返回可选的**展示文本**。
///
/// `None` = 保持原渲染分支。两条分支各有各的理由：
/// - `table`/`hr`/`image`/`audio`/`video` 是**结构化/媒体**块（多元素拼装或卡片），
///   换成单个 `TextInput` 只会把结构弄坏；GUI 自有的 `role`/`acts`/`think` 同理不由这里管。
/// - 有行内样式的块（`**粗体**`/`` `码` ``/链接）`TextInput` 渲染不了 ⇒ 继续走 `StyledText`，
///   不做视觉回退（B 批的行内样式是已交付能力）。
///
/// 文本形态有**两种**，混淆会真出错，故收敛在此处一处判定：
/// - **行内 markdown**（`h1`/`h2`/`h3`/`p`/`list`/`task`/`quote`）：字面量元字符已被
///   `push_escaped` 转义 ⇒「有未转义元字符」精确等价于「真有样式」；展示文本 = 反解转义。
/// - **原样文本**（`code`）：代码原文**不**转义 ⇒ 恒无行内样式，展示文本就是原文。
///   这是复制/朗读最要紧的一类块（`ls *.rs`、`C:\Users\x`），既不能按元字符判样式，
///   也不能反解转义。
pub fn selectable_plain(kind: &str, text: &str) -> Option<String> {
    if kind == "code" {
        return Some(text.to_string());
    }
    if !inline_markdown_kind(kind) {
        return None;
    }
    if has_inline_markup(text) {
        None
    } else {
        Some(plain(text))
    }
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

/// 媒体块的默认替代文本（无 `alt` 属性 / 空链接文字时）。
fn default_alt(kind: &str) -> String {
    match kind {
        "audio" => "音频",
        "video" => "视频",
        _ => "图片",
    }
    .to_string()
}

/// 「整段只是一条媒体」→ 媒体块：markdown 图片/链接，或**裸媒体地址**。
fn media_from_text(text: &str) -> Option<MdBlock> {
    let t = text.trim();
    if t.is_empty() {
        return None;
    }
    if let Some(m) = media_from_markdown(t) {
        return Some(m);
    }
    // 裸媒体地址：整段**只有一个不含空白的 token**，且它的扩展名是媒体。
    // 含空格的句子（`见 ./assets/a.mp4 文件`）是普通文本，不转块（避免吞正文）。
    if t.chars().any(char::is_whitespace) {
        return None;
    }
    // 文本块里的 `_`/`*` 等被 `push_escaped` 加了反斜杠，判类型/取值前先反解。
    let src = unescape(t);
    let kind = media_kind_for(&src)?;
    let mut b = MdBlock::new(kind);
    b.src = src;
    b.alt = default_alt(kind);
    Some(b)
}

/// markdown 图片/链接 → 媒体块：`![alt](src)`（图片）或 `[alt](src)` 且 src 扩展名是媒体。
fn media_from_markdown(text: &str) -> Option<MdBlock> {
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
        default_alt(kind)
    } else {
        alt
    };
    Some(b)
}

/// HTML 属性取值：从标签片段里取 `attr` 的值（双/单引号、无引号都认）。
///
/// 不用正则（不引依赖）；匹配时要求属性名左侧不是名字字符（避免 `data-src` 命中 `src`）。
fn html_attr(tag: &str, attr: &str) -> Option<String> {
    let lower = tag.to_ascii_lowercase();
    let bytes = lower.as_bytes();
    let mut from = 0usize;
    while let Some(rel) = lower[from..].find(attr) {
        let i = from + rel;
        let left_ok = i == 0 || !(bytes[i - 1].is_ascii_alphanumeric() || bytes[i - 1] == b'_');
        let after = bytes.get(i + attr.len()).copied();
        if left_ok
            && matches!(
                after,
                Some(b'=') | Some(b' ') | Some(b'\t') | Some(b'\n') | Some(b'\r') | Some(b'/')
            )
        {
            let rest = tag[i + attr.len()..].trim_start();
            if let Some(rest) = rest.strip_prefix('=') {
                let rest = rest.trim_start();
                let val = if let Some(r) = rest.strip_prefix('"') {
                    r.split('"').next().unwrap_or("")
                } else if let Some(r) = rest.strip_prefix('\'') {
                    r.split('\'').next().unwrap_or("")
                } else {
                    rest.split(|c: char| c.is_whitespace() || c == '>')
                        .next()
                        .unwrap_or("")
                };
                return Some(val.to_string());
            }
        }
        from = i + attr.len();
    }
    None
}

/// 该 HTML 片段是否是**媒体元素**的标签（含闭标签）。
///
/// 用于丢弃「成块后被消费掉的开/闭标签」——`<video …>` 成媒体块后，紧随的 `</video>`
/// 如果还走「HTML 退化为文本」就会在气泡里多出一行字面量。
fn is_media_html_tag(h: &str) -> bool {
    let t = h.trim_start();
    let t = t
        .strip_prefix("</")
        .or_else(|| t.strip_prefix('<'))
        .unwrap_or(t);
    let name: String = t
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric())
        .collect::<String>()
        .to_ascii_lowercase();
    matches!(name.as_str(), "img" | "audio" | "video" | "source")
}

/// HTML 片段 → 媒体块：`<img src>`→image、`<audio src>`→audio、`<video src>`→video；
/// `<video><source src></video>` 的 `<source>` 子元素按外层标签（或扩展名）定型。
///
/// 只认**带 `src` 的**媒体标签；找不到返回 `None`（调用方决定退回转义文本还是跳过）。
fn media_from_html(h: &str) -> Option<MdBlock> {
    let lower = h.to_ascii_lowercase();
    let wrap_video = lower.contains("<video");
    let wrap_audio = lower.contains("<audio");
    for tag in ["img", "audio", "video", "source"] {
        let needle = format!("<{tag}");
        let mut from = 0usize;
        while let Some(rel) = lower[from..].find(&needle) {
            let i = from + rel;
            let after = lower.as_bytes().get(i + needle.len()).copied();
            // 必须紧跟空白/`>`/`/`（排除 `<imgs>` 之类）
            if matches!(after, Some(c) if c.is_ascii_alphanumeric() || c == b'-') {
                from = i + needle.len();
                continue;
            }
            let end = lower[i..]
                .find('>')
                .map(|o| i + o + 1)
                .unwrap_or(lower.len());
            let tagstr = &h[i..end];
            if let Some(src) = html_attr(tagstr, "src") {
                let src = src.trim().to_string();
                if !src.is_empty() {
                    let kind = match tag {
                        "img" => "image",
                        "audio" => "audio",
                        "video" => "video",
                        _ if wrap_video => "video",
                        _ if wrap_audio => "audio",
                        _ => media_kind_for(&src).unwrap_or("video"),
                    };
                    let mut m = MdBlock::new(kind);
                    m.src = src;
                    let alt = html_attr(tagstr, "alt")
                        .map(|a| a.trim().to_string())
                        .unwrap_or_default();
                    m.alt = if alt.is_empty() {
                        default_alt(kind)
                    } else {
                        alt
                    };
                    return Some(m);
                }
            }
            from = end.max(i + 1);
        }
    }
    None
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
                            default_alt(kind)
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
            // HTML 媒体标签（`<img>/<audio>/<video>` 含 `<source>`）成独立媒体块
            // （D7 卡根因①：原先 HTML 一律转义 ⇒ agent 输出 `<img>`/`<audio>`/`<video>`
            // 永远成不了 image/audio/video 块）；其余 HTML / 脚注 / 数学退化为文本（绝不吞）。
            Event::Html(h) | Event::InlineHtml(h) => {
                if let Some((_, buf)) = code.as_mut() {
                    buf.push_str(&h);
                } else if in_table {
                    // 表内**一律不转义**（与上面 `Event::Text`/`Event::Code` 的 in_table 分支同口径）：
                    // 单元格由 GUI 用纯 `Text` 渲染，转义出来的反斜杠会**可见**地画进格子里
                    // （`| <b>x</b> |` 曾显示成 `\<b\>x\</b\>`）。
                    cell.push_str(&h);
                } else if let Some(m) = media_from_html(&h) {
                    // 与 markdown 图片同口径：切块前先落盘当前段落，保证块序正确
                    flush_cur(&mut blocks, &mut cur);
                    blocks.push(m);
                } else if is_media_html_tag(&h) {
                    // 被消费掉的媒体开/闭标签（如成块后的 `</video>`）不再漏成文本
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
/// **现状（2026-10-15 实核）**：这条分支在当前调用链上**取不到输入**——两个调用点拿到的
/// 都是 pulldown-cmark 的 `dest_url`（`Tag::Link`/`Tag::Image` 的字段）或由它重建的
/// `](dest_url)` 文本，而 0.13.4 的 `dest_url` **不含** title（title 在同一个 tag 的独立
/// `title` 字段里）。把整个函数换成 `dest.to_string()` 后没有任何行为用例变红。
/// 留着它是**防御性**的：成本近乎零，而上游哪天把 title 塞回 `dest_url`，
/// 「带空格的文件名」这类路径就会开始带上 `"标题"`。它的契约钉在单测
/// `dest_strip_title_pins_title_contract`（直接喂带 title 的串）——**不要**拿集成用例
/// `path_with_spaces_not_truncated` 充当它的判据（那里 title 早就被 pulldown 自己剥掉了）。
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
        // title 不得进 src：这条钉的是**整条解析链路**（title 由 pulldown 自己剥掉，
        // 见 `dest_strip_title` 注释），不是那个函数的判据。
        let b = parse("![图](a.png \"标题\")");
        assert_eq!(b[0].src, "a.png");
    }

    /// `dest_strip_title` 的**契约**（防御性分支，见函数注释）：调用链上取不到带 title 的输入，
    /// 所以直接喂串。变异点：删掉「空格 + 引号」那一支（`dest.find(\" \\\"\")` 恒 None）⇒ 本用例红。
    #[test]
    fn dest_strip_title_pins_title_contract() {
        assert_eq!(dest_strip_title("a.png \"标题\""), "a.png");
        // 不带引号的空格**不能**切（N1：macOS 截图名里带空格）
        assert_eq!(dest_strip_title("my file.png"), "my file.png");
        assert_eq!(dest_strip_title(""), "");
    }

    #[test]
    fn image_in_table_degrades_to_alt_text() {
        let b = parse("| 图 |\n|---|\n| ![alt](a.png) |\n");
        let rows: Vec<&MdBlock> = b.iter().filter(|x| x.kind == "table").collect();
        assert_eq!(rows.len(), 2, "表格不应被图片打断：{:?}", kinds(&b));
        assert_eq!(rows[1].cells, vec!["alt"], "表内图片降级为 alt 文本");
    }

    // ── D7：HTML 媒体标签 + 裸媒体地址（根因①）──

    /// `<img src>`（含相对路径）必须成 `image` 块，且 `alt`/`width` 属性正确分流。
    /// 变异点：把 `Event::Html` 分支的 `media_from_html` 判断去掉 ⇒ 本测红（退化成 `p`）。
    #[test]
    fn html_img_becomes_image_block() {
        let b = parse("<img src=\"./assets/cover.png\" alt=\"封面\" width=\"480\">");
        assert_eq!(kinds(&b), vec!["image"], "{:?}", kinds(&b));
        assert_eq!(b[0].src, "./assets/cover.png");
        assert_eq!(b[0].alt, "封面", "width 不得混进 alt/src");
        // 不带 alt 时回落默认替代文本
        let b = parse("<img src='a.png'>");
        assert_eq!(kinds(&b), vec!["image"]);
        assert_eq!(b[0].src, "a.png");
        assert_eq!(b[0].alt, "图片");
    }

    /// HTML 音视频必须成 `audio`/`video` 块（属性顺序无关，`controls` 不干扰）。
    #[test]
    fn html_audio_video_become_media_blocks() {
        let b = parse("<audio controls src=\"./assets/tone.mp3\"></audio>");
        assert_eq!(kinds(&b), vec!["audio"], "{:?}", kinds(&b));
        assert_eq!(b[0].src, "./assets/tone.mp3");
        assert_eq!(b[0].alt, "音频");

        let b = parse("<video width=\"640\" controls src=\"./assets/clip.mp4\"></video>");
        assert_eq!(kinds(&b), vec!["video"], "{:?}", kinds(&b));
        assert_eq!(b[0].src, "./assets/clip.mp4");
        assert_eq!(b[0].alt, "视频");
    }

    /// `<video><source src=…></video>` 子元素形态：`<source>` 按外层 `<video>` 定成 video。
    #[test]
    fn html_video_source_child_resolves_to_video() {
        let b = parse("<video controls><source src=\"a.mp4\" type=\"video/mp4\"></video>");
        let media: Vec<&MdBlock> = b
            .iter()
            .filter(|x| matches!(x.kind.as_str(), "audio" | "video" | "image"))
            .collect();
        assert_eq!(media.len(), 1, "恰好一个媒体块：{:?}", kinds(&b));
        assert_eq!(media[0].kind, "video");
        assert_eq!(media[0].src, "a.mp4");
        // 闭标签不得漏成字面文本块
        assert!(
            !b.iter().any(|x| x.text.contains("</video>")),
            "闭标签漏成文本：{:?}",
            b.iter().map(|x| &x.text).collect::<Vec<_>>()
        );
    }

    /// 媒体开/闭标签本身不得作为字面文本漏出（`</audio>` 不能显示成一行）。
    #[test]
    fn html_media_closing_tags_are_not_leaked() {
        let b = parse("<audio src=\"a.mp3\"></audio>");
        assert_eq!(kinds(&b), vec!["audio"], "{:?}", kinds(&b));
        let b = parse("<video controls><source src=\"a.mp4\"></video>\n\n后文");
        assert_eq!(kinds(&b), vec!["video", "p"], "{:?}", kinds(&b));
        assert_eq!(b[1].text, "后文");
    }

    /// 裸媒体地址（无 markdown 语法）：整段只有一个不含空白的 token 时成块。
    #[test]
    fn bare_media_links_become_blocks() {
        assert_eq!(kinds(&parse("https://x.y/a.mp4")), vec!["video"]);
        assert_eq!(kinds(&parse("https://x.y/a.mov?x=1")), vec!["video"]);
        assert_eq!(kinds(&parse("./assets/a.mp4")), vec!["video"]);
        assert_eq!(kinds(&parse("a.mp3")), vec!["audio"]);
        let b = parse("./assets/a.png");
        assert_eq!(kinds(&b), vec!["image"]);
        assert_eq!(b[0].src, "./assets/a.png", "裸地址 src 不得被转义");
        // 含空格的句子是正文，不得被吞成媒体块
        assert_eq!(kinds(&parse("见 ./assets/a.mp4 文件")), vec!["p"]);
        // 非媒体扩展名仍是段落
        assert_eq!(kinds(&parse("https://x.y/readme")), vec!["p"]);
    }

    /// 非媒体 HTML（`<div>` 等）仍走「退化为转义文本」路径，绝不吞内容。
    #[test]
    fn non_media_html_still_degrades_to_text() {
        let b = parse("<div>甲</div>");
        assert_eq!(kinds(&b), vec!["p"], "{:?}", kinds(&b));
        assert!(b[0].text.contains("div"), "HTML 不得被吞：{}", b[0].text);
    }

    /// 行内 HTML 媒体与 markdown 图片同口径：切出独立块，前后文字各自成段。
    #[test]
    fn html_media_inside_paragraph_splits_blocks() {
        let b = parse("前文 <img src=\"a.png\"> 后文");
        assert_eq!(kinds(&b), vec!["p", "image", "p"], "{:?}", kinds(&b));
        assert_eq!(b[0].text, "前文");
        assert_eq!(b[2].text, "后文");
    }

    /// HTML 媒体在表格内仍降级为单元格文本（不得打断表格布局）。
    #[test]
    fn html_media_in_table_degrades_to_cell_text() {
        let b = parse("| 图 |\n|---|\n| <img src=\"a.png\"> |\n");
        let rows: Vec<&MdBlock> = b.iter().filter(|x| x.kind == "table").collect();
        assert_eq!(rows.len(), 2, "表格不应被 HTML 图片打断：{:?}", kinds(&b));
        assert!(
            rows[1].cells[0].contains("img"),
            "单元格应保留 HTML 文本：{:?}",
            rows[1].cells
        );
    }

    /// B 批 O1：表内 HTML 与表内文本同口径——**不转义**（单元格由 GUI 用纯 `Text` 画，
    /// 转义出来的反斜杠会**可见**）。变异点：把 in_table 的 HTML 分支改回 `push_escaped`
    /// ⇒ 下面的 `!contains('\\')` 红。
    #[test]
    fn html_in_table_is_not_escaped() {
        let b = parse("| a |\n|---|\n| <b>x</b> |\n");
        let rows: Vec<&MdBlock> = b.iter().filter(|x| x.kind == "table").collect();
        assert_eq!(rows[1].cells, vec!["<b>x</b>"], "表内 HTML 应原样进单元格");
        assert!(
            !rows[1].cells.iter().any(|c| c.contains('\\')),
            "表内文本不得带转义反斜杠：{:?}",
            rows[1].cells
        );
        // 行内 HTML（`<b>` 是 Event::InlineHtml）与块级同口径
        assert_eq!(
            parse("| a |\n|---|\n| x <br> y |\n")[1].cells,
            vec!["x <br> y"]
        );
    }
    /// 卡 `abb-v11-message-actions`：只读 `TextInput` 只对**无行内样式**的块做替换，
    /// 判据必须精确——否则要么白丢样式（回退 B 批），要么白丢可选中能力。
    #[test]
    fn has_inline_markup_distinguishes_literal_metachars_from_real_styles() {
        // 纯文本：无标记
        assert!(!has_inline_markup("普通中文文本，含标点。"));
        // 字面量元字符（`push_escaped` 已转义）⇒ 不是样式
        assert!(!has_inline_markup(r"价格是 3 \* 4 元"));
        assert!(!has_inline_markup(r"下划线 a\_b 与反引号 \`x\`"));
        assert!(!has_inline_markup(r"尖括号 \<b\> 与方括号 \[x\]"));
        // 结尾一个转义的反斜杠：转义对消费掉后半，不应越界 panic
        assert!(!has_inline_markup(r"路径 C:\\"));
        // 真行内样式：未被转义的元字符
        assert!(has_inline_markup("这是 **粗体** 字"));
        assert!(has_inline_markup("行内码 `x = 1`"));
        assert!(has_inline_markup("_斜体_"));
        assert!(has_inline_markup("[链接](https://example.com)"));
        assert!(has_inline_markup("原义 <b>html</b>"));
        // 删除线：解析器把 `~~` 原样重发回 text（单 `~x~` 也会被重发成 `~~x~~`）
        assert!(has_inline_markup("这是 ~~删除线~~ 字"));
        assert!(has_inline_markup("~~单波浪也被重发成一对~~"));
        // 边界：转义的反斜杠 + 真星号 ⇒ 星号是**样式**（不是字面量）
        assert!(has_inline_markup(r"反斜杠 \\* 之后是斜体起点"));
    }

    /// `plain` 是 `push_escaped` 的逆运算（仅对无样式文本用）。
    #[test]
    fn plain_unescapes_literal_metachars_and_roundtrips_through_parse() {
        assert_eq!(plain(r"价格是 3 \* 4 元"), "价格是 3 * 4 元");
        assert_eq!(plain(r"路径 C:\\Users\\x"), r"路径 C:\Users\x");
        assert_eq!(plain("普通文本"), "普通文本");
        // 端到端往返：parse 把行内文本转义 ⇒ plain 解回来必须逐字等于原文
        // 只含**不构成 markdown 语法**的孤立元字符（孤立 `*` / 词内 `_` / 无链接的方括号）
        let src = "价格是 3 * 4 元；下划线 a_b；方括号 [x]";
        let b = parse(src);
        assert_eq!(b.len(), 1);
        assert_eq!(b[0].kind, "p");
        assert!(
            !has_inline_markup(&b[0].text),
            "纯文本段落的行内文本不应含未转义元字符（否则会白丢可选中）：{:?}",
            b[0].text
        );
        assert_eq!(plain(&b[0].text), src, "反解必须逐字回到原文");
    }

    /// 有样式的段落必须被判成「有样式」（GUI 继续走 StyledText，不做视觉回退）。
    #[test]
    fn parse_marks_styled_paragraph_as_having_inline_markup() {
        let b = parse("这是 **粗体** 与 [链接](https://example.com)。");
        assert_eq!(b.len(), 1);
        assert!(
            has_inline_markup(&b[0].text),
            "含粗体/链接的段落必须判为有行内样式：{:?}",
            b[0].text
        );
    }

    /// 卡 `abb-v11-message-actions` 评审 Critical 回归：**字面量波浪号**必须可选中、
    /// **删除线**必须留在 `StyledText`。
    ///
    /// 漏掉 `~` 时的实测后果（独立评审）：`~~删~~` 被判成「无行内样式」⇒ 走只读 `TextInput`
    /// ⇒ 用户看到字面量 `~~删~~`（删除线没了），两分支差异 920 像素。
    ///
    /// **`~a~`（单波浪成对）为什么也画删除线**（B 批 O4，2026-10-15 实核）：解析器
    /// `pulldown-cmark-0.13.4/src/parse.rs` 的 `MaybeEmphasis` 分支里，`~` 成对时若没开
    /// `ENABLE_SUBSCRIPT` 就归 `Strikethrough`；我们只开了 `ENABLE_STRIKETHROUGH`
    /// ⇒ `~a~` 与 `~~a~~` 解析后**同形**（都重发成 `~~a~~`）。这是解析器的口径，不是本仓取舍；
    /// 字面量波浪号则不受影响——`~` 要贴着非空白字符才成对，`~/a`、`a ~ b` 都不成对。
    #[test]
    fn tilde_is_escaped_when_literal_and_detected_when_a_style() {
        // 字面量波浪号：解析后已被转义 ⇒ 可选中，且展示文本逐字等于原文
        for src in [
            "约 3~4 元",
            "~/bin/abb",
            "a~b",
            "好的~",
            "see ~/x now",
            "a ~ b ~ c",
            "把文件放到 ~/a 和 ~/b",
        ] {
            let b = parse(src);
            assert_eq!(b.len(), 1, "{src:?}");
            assert!(
                !has_inline_markup(&b[0].text),
                "字面量波浪号不该被判成样式（{src:?} → {:?}）",
                b[0].text
            );
            assert_eq!(
                selectable_plain(&b[0].kind, &b[0].text).as_deref(),
                Some(src),
                "必须可选中且逐字保留（{src:?} → {:?}）",
                b[0].text
            );
        }
        // 删除线（`~~x~~` 与单波浪 `~x~` 都会被解析器重发成 `~~x~~`）⇒ 不可选中，留在 StyledText
        for src in ["这是 ~~删除线~~ 字", "~单波浪~"] {
            let b = parse(src);
            assert_eq!(b.len(), 1, "{src:?}");
            assert!(
                has_inline_markup(&b[0].text),
                "删除线必须判为有行内样式（{src:?} → {:?}）",
                b[0].text
            );
            assert_eq!(selectable_plain(&b[0].kind, &b[0].text), None, "{src:?}");
        }
        // 单波浪与双波浪**同形**（逐字钉住，免得后人以为两者渲染不同）
        assert_eq!(parse("~a~")[0].text, parse("~~a~~")[0].text);
        // 混合：有样式 + 字面量波浪号 ⇒ 走 StyledText，但文本里的 `~` 已转义（Slint 认 `\~`）
        let b = parse("**粗** 与 3~4");
        assert_eq!(selectable_plain(&b[0].kind, &b[0].text), None);
        assert!(
            b[0].text.contains(r"3\~4"),
            "字面量波浪号该被转义：{:?}",
            b[0].text
        );
    }

    /// 卡 `abb-v11-message-actions`：`code` 的正文是**代码原文**（解析时不转义），
    /// 所以它既不能被元字符判样式（`ls *.rs` 会被误判），也不能被反解（`C:\Users` 会掉反斜杠）。
    /// 这是「复制代码」这个最要紧的场景，必须是绿的。
    #[test]
    fn selectable_plain_keeps_code_verbatim() {
        // 逐字保留：星号、反斜杠、反引号、尖括号、方括号都不动
        for src in [
            "ls *.rs | grep _tmp",
            r"C:\Users\x\a.txt",
            "printf '%s\\n' \"a\"",
            "echo `<b>[x]</b>`",
        ] {
            assert_eq!(
                selectable_plain("code", src).as_deref(),
                Some(src),
                "代码原文必须逐字保留（不判样式、不反解）：{src:?}"
            );
        }
        // 端到端：围栏代码块走一遍 `parse`，块内文本仍是原文
        let md = "```sh\nls *.rs | grep _tmp\n```";
        let b = parse(md);
        assert_eq!(b.len(), 1);
        assert_eq!(b[0].kind, "code");
        assert_eq!(b[0].text, "ls *.rs | grep _tmp", "代码块正文不该被转义");
        assert_eq!(
            selectable_plain(&b[0].kind, &b[0].text).as_deref(),
            Some("ls *.rs | grep _tmp")
        );
    }

    /// 结构化/媒体块**不能**换成单个只读 `TextInput`（会把结构弄坏）⇒ 恒 `None`；
    /// 无样式的行内文本块恒 `Some`；有样式恒 `None`。三档都在一张表里钉住。
    #[test]
    fn selectable_plain_gates_by_kind_and_inline_style() {
        assert_eq!(selectable_plain("table", "a"), None);
        assert_eq!(selectable_plain("hr", ""), None);
        assert_eq!(selectable_plain("image", "a.png"), None);
        assert_eq!(selectable_plain("audio", "a.mp3"), None);
        assert_eq!(selectable_plain("video", "a.mp4"), None);
        // GUI 自有 kind（`parse` 不会产出）也一律不接管
        assert_eq!(selectable_plain("role", "你"), None);
        assert_eq!(selectable_plain("acts", ""), None);
        assert_eq!(selectable_plain("think", "想了一下"), None);
        // 无样式 ⇒ 可选中，且展示文本已反解
        assert_eq!(
            selectable_plain("p", r"价格 3 \* 4").as_deref(),
            Some("价格 3 * 4")
        );
        assert_eq!(selectable_plain("quote", "引用").as_deref(), Some("引用"));
        assert_eq!(selectable_plain("list", "项").as_deref(), Some("项"));
        // 有样式 ⇒ 保持 StyledText
        assert_eq!(selectable_plain("p", "这是 **粗体**"), None);
        assert_eq!(selectable_plain("h2", "看 `code`"), None);
    }
}
