//! HTML → Markdown（系统对话「HTML 渲染支持」）。
//!
//! ## 为什么是「转成 markdown」而不是「自己出一套块」
//!
//! 本仓的渲染管线是「markdown → [`crate::mdblocks::MdBlock`] 块 → GUI 逐块布局」（表格/公式/
//! 媒体/代码选择都在那里）。HTML 要「像 markdown 一样」渲染，最省、最一致的做法就是把 HTML
//! **翻译成 markdown 文本**再喂给同一条管线——而不是并排写第二套块解析器（那会立刻漂移）。
//!
//! ## 覆盖范围（**只做结构与文本级样式**，不做 CSS 布局）
//!
//! 支持：`h1..h6`、`p`、`br`、`hr`、`ul/ol/li`（含嵌套）、`blockquote`、`pre/code`（成围栏）、
//! `table/thead/tbody/tr/td/th`（成 markdown 表格，`th` 行当表头）、`img`、`a`、
//! `b/strong`、`i/em`、`s/del/strike`、`code`，实体（`&amp; &lt; &gt; &quot; &#39; &nbsp;`）。
//!
//! **明确不做**（也不该假装做）：CSS 布局（`display/flex/position/grid`）、`style` 属性里的
//! 视觉样式、`iframe/canvas/svg` 绘制、脚本。`<style>/<script>/<head>` 的内容**整段丢弃**
//! （否则会把 CSS 源码当正文画出来）；其它不认识的标签**只丢标签、保留文字**（绝不吞内容）。
//!
//! ## 与 Slint 的关系（立项时核实过）
//!
//! Slint 的 `Text.text` 是 `styled-text`，文档写明「CommonMark markup with additional HTML
//! tags for styling」（`i-slint-compiler-1.17.1/builtins.slint:745`）——它**只**支持行内样式类
//! 标签，**不支持**标题/表格/引用/图片（本仓 `mdblocks` 顶注已实核过），所以「拿 HTML 直接丢给
//! Slint」并不能得到「像 markdown 一样」的效果，这也是本条走块管线的理由。

/// 块级标签：出现在**行首**时，把这一段裸 HTML 交给 [`html_to_markdown`]。
///
/// 为什么不把行内标签（`<b>`/`<code>`/`<a>`）也算进来：markdown 正文里夹一个 `<b>重点</b>`
/// 是常见写法，pulldown 会把它当 `InlineHtml` 原样带进行内渲染；在这里抢着转换反而会改动
/// 既有排版（且那类标签 Slint 自己就认）。
const BLOCK_TAGS: &[&str] = &[
    "html",
    "body",
    "main",
    "header",
    "footer",
    "article",
    "section",
    "div",
    "figure",
    "figcaption",
    "table",
    "thead",
    "tbody",
    "tr",
    "ul",
    "ol",
    "dl",
    "li",
    "p",
    "pre",
    "blockquote",
    "details",
    "summary",
    "fieldset",
    "form",
    "nav",
    "aside",
    // 这几个是「看起来不像块、但必须整段收走」的：`<hr>` 不收会漏成转义文本，
    // `<style>/<script>/<head>` 里的内容不收会把 CSS/JS 源码当正文画出来（实测抓到）。
    "hr",
    "style",
    "script",
    "meta",
    "link",
    "title",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
];

/// 把回复里**成块的裸 HTML** 就地转成 markdown（行内标签不动）。
///
/// 判据很保守：只有**行首**是 [`BLOCK_TAGS`] 里某个标签（`<div …>` / `</div>` 都算）才进转换，
/// 并且按「同名标签的配对深度」收集整段（深度归零即结束；兜底最多 400 行、缺闭合时**原样保留**）。
/// 这样一条普通中文短句里出现 `<b>` 绝不会被卷进来。
pub fn convert_html_blocks(md: &str) -> String {
    if !md.contains('<') {
        return md.to_string();
    }
    let mut out = String::new();
    let mut lines: std::iter::Peekable<std::str::Lines<'_>> = md.lines().peekable();
    while let Some(line) = lines.next() {
        let tag = match block_tag(line) {
            Some(tag) => tag,
            None => {
                out.push_str(line);
                out.push('\n');
                continue;
            }
        };
        let mut buf = String::from(line);
        let mut depth = depth_delta(line, &tag);
        let mut n = 0usize;
        while depth > 0 && n < 400 {
            let Some(next) = lines.next() else { break };
            depth += depth_delta(next, &tag);
            buf.push('\n');
            buf.push_str(next);
            n += 1;
        }
        if depth > 0 {
            // 没配到闭合：**原样保留**（宁可显示源码，也不能吞掉用户/模型写的内容）。
            out.push_str(&buf);
            out.push('\n');
        } else {
            out.push_str(html_to_markdown(&buf).trim_end());
            out.push_str("\n\n");
        }
    }
    out
}

/// 行首的块级标签名（小写）。`<!doctype html>` 也认（整行丢弃）。
fn block_tag(line: &str) -> Option<String> {
    let t = line.trim_start();
    if t.len() < 2 || !t.starts_with('<') {
        return None;
    }
    if t.starts_with("<!--") {
        return None;
    }
    if t[1..].eq_ignore_ascii_case("!doctype html>")
        || t.to_ascii_lowercase().starts_with("<!doctype")
    {
        return Some("!doctype".to_string());
    }
    let name = t[1..]
        .trim_start_matches('/')
        .split(|c: char| c.is_whitespace() || c == '>' || c == '/')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    if name.is_empty() {
        return None;
    }
    if BLOCK_TAGS.contains(&name.as_str()) {
        Some(name)
    } else {
        None
    }
}

/// **空元素**（没有闭合标签）：对配对深度恒为 0。
///
/// 为什么单列：`<hr>` 被数成 +1 时，收集循环会一直等 `</hr>` 等不到，最后走「缺闭合 ⇒
/// 原样保留」⇒ `<hr>` 直接漏成转义文本（实测抓到：`["h2","p","p"]` 里就是没有 `hr`）。
const VOID_TAGS: &[&str] = &[
    "hr", "br", "img", "meta", "link", "input", "source", "area", "col",
];

/// 该行对 `tag` 的配对深度贡献（同名的开标签 +1、闭标签 −1；空元素/自闭合不算）。
fn depth_delta(line: &str, tag: &str) -> i32 {
    if tag == "!doctype" || VOID_TAGS.contains(&tag) {
        return 0;
    }
    let mut delta = 0;
    let lower = line.to_ascii_lowercase();
    let mut from = 0usize;
    while let Some(pos) = lower[from..].find('<') {
        let at = from + pos;
        let rest = &lower[at..];
        // 只数**标签名精确等于** tag 的（`<divx>` 不算）。
        for (prefix, sign) in [("</", -1), ("<", 1)] {
            let Some(after) = rest.strip_prefix(prefix) else {
                continue;
            };
            let name: String = after
                .split(|c: char| c.is_whitespace() || c == '>' || c == '/')
                .next()
                .unwrap_or("")
                .to_string();
            if name == tag {
                // 自闭合（`<div/>`）不改变深度。
                let head = &rest[..rest.find('>').unwrap_or(rest.len()).min(rest.len())];
                if prefix == "<" && head.ends_with('/') {
                    break;
                }
                delta += sign;
                break;
            }
        }
        from = at + 1;
        if from >= lower.len() {
            break;
        }
    }
    delta
}

/// HTML 片段 → Markdown 文本（结构 + 文本级样式；见模块注释的范围表）。
pub fn html_to_markdown(html: &str) -> String {
    let mut out = String::new();
    // 行内样式：开标签 → markdown 记法；闭标签 → 对应的收尾。
    let mut open: Vec<String> = Vec::new();
    // 列表栈：true = 有序；同时记「本层已出几条」（有序列表编号要递增）。
    let mut lists: Vec<(bool, u32)> = Vec::new();
    // 表格：行 × 格 + 该行是否表头
    let mut rows: Vec<(Vec<String>, bool)> = Vec::new();
    let mut row: Vec<String> = Vec::new();
    let mut cell = String::new();
    let mut in_cell = false;
    let mut header_row = false;
    // `<style>/<script>/<head>` 内容整段丢弃
    let mut skip: Option<String> = None;
    // `<pre>` 里的内容要原样进围栏（再被行内标签处理就错了）
    let mut pre: Option<String> = None;

    let mut i = 0usize;
    let bytes = html.as_bytes();
    while i < html.len() {
        if bytes[i] != b'<' {
            let next = html[i..].find('<').map(|p| i + p).unwrap_or(html.len());
            let text = decode_entities(&html[i..next]);
            if skip.is_some() {
                // `<style>/<script>/<head>` 里的**文字**也要丢（只丢标签会把 CSS 源码当正文画出来）。
            } else if let Some(buf) = pre.as_mut() {
                buf.push_str(&text);
            } else if in_cell {
                cell.push_str(&text);
            } else {
                out.push_str(&text);
            }
            i = next;
            continue;
        }
        // 注释
        if html[i..].starts_with("<!--") {
            match html[i..].find("-->") {
                Some(end) => {
                    i += end + 3;
                    continue;
                }
                None => break,
            }
        }
        // doctype / 处理指令：丢
        if html[i..].starts_with("<!") || html[i..].starts_with("<?") {
            match html[i..].find('>') {
                Some(end) => {
                    i += end + 1;
                    continue;
                }
                None => break,
            }
        }
        let Some(rel) = html[i..].find('>') else {
            break;
        };
        let raw = &html[i + 1..i + rel];
        i += rel + 1;
        let closing = raw.trim_start().starts_with('/');
        let body = raw.trim_start().trim_start_matches('/');
        let name = body
            .trim_start_matches('/')
            .split(|c: char| c.is_whitespace() || c == '/')
            .next()
            .unwrap_or("")
            .to_ascii_lowercase();
        // 跳过区（style/script/head）
        if let Some(tag) = skip.clone() {
            if closing && name == tag {
                skip = None;
            }
            continue;
        }
        let self_closing = raw.trim_end().ends_with('/');
        match name.as_str() {
            "style" | "script" | "head" | "title" | "meta" | "link" => {
                if !closing && !self_closing {
                    skip = Some(name.clone());
                }
            }
            "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                if closing {
                    out.push_str("\n\n");
                } else {
                    let level = name[1..].parse::<usize>().unwrap_or(1).min(3);
                    out.push_str("\n\n");
                    out.push_str(&"#".repeat(level));
                    out.push(' ');
                }
            }
            "p" | "div" | "section" | "article" | "main" | "body" | "html" | "header"
            | "footer" | "nav" | "aside" | "figure" | "figcaption" | "fieldset" | "form"
            | "details" | "summary" | "dl" | "dd" => {
                out.push_str("\n\n");
            }
            "br" => out.push_str("  \n"),
            "hr" => out.push_str("\n\n---\n\n"),
            "blockquote" => {
                if closing {
                    out.push_str("\n\n");
                } else {
                    out.push_str("\n\n> ");
                }
            }
            "ul" | "ol" => {
                if closing {
                    lists.pop();
                    out.push('\n');
                    if lists.is_empty() {
                        out.push('\n');
                    }
                } else {
                    lists.push((name == "ol", 0));
                    out.push('\n');
                }
            }
            "li" => {
                if closing {
                    out.push('\n');
                } else {
                    let depth = lists.len().saturating_sub(1);
                    out.push_str(&"  ".repeat(depth));
                    match lists.last_mut() {
                        Some((true, n)) => {
                            *n += 1;
                            out.push_str(&format!("{}. ", n));
                        }
                        _ => out.push_str("- "),
                    }
                }
            }
            "pre" => {
                if closing {
                    if let Some(buf) = pre.take() {
                        out.push_str("\n\n```\n");
                        out.push_str(buf.trim_end_matches('\n'));
                        out.push_str("\n```\n\n");
                    }
                } else if pre.is_none() && !self_closing {
                    pre = Some(String::new());
                }
            }
            "table" => {
                if closing {
                    out.push_str(&table_to_markdown(&rows));
                    rows.clear();
                } else {
                    out.push_str("\n\n");
                }
            }
            "thead" | "tbody" | "tfoot" => {
                if closing {
                    out.push('\n');
                }
            }
            "tr" => {
                if closing {
                    // ⚠️ 这里**不要**再补一格：单元格已在 `</td>/<th>` 收过，
                    // 补一次会每行多出一列空的（表格立刻错位，实测抓到）。
                    cell.clear();
                    in_cell = false;
                    if !row.is_empty() {
                        rows.push((std::mem::take(&mut row), header_row));
                    }
                    header_row = false;
                } else {
                    header_row = true; // 只要出现 th 才算表头（下面会置 false）
                    row.clear();
                }
            }
            "td" | "th" => {
                if closing {
                    cell = cell.trim().to_string();
                    row.push(cell.clone());
                    cell.clear();
                    in_cell = false;
                } else {
                    in_cell = true;
                    if name == "td" {
                        header_row = false;
                    }
                }
            }
            "img" => {
                let src = attr(body, "src").unwrap_or_default();
                if !src.is_empty() {
                    let alt = attr(body, "alt").unwrap_or_default();
                    let md = format!("![{alt}]({src})");
                    if in_cell {
                        cell.push_str(&md);
                    } else {
                        out.push_str(&md);
                    }
                }
            }
            "a" => {
                if closing {
                    if let Some(href) = open.pop() {
                        let text = format!("]({href})");
                        if in_cell {
                            cell.push_str(&text);
                        } else {
                            out.push_str(&text);
                        }
                    } else {
                        let text = format!("]({})", "".to_string());
                        if in_cell {
                            cell.push_str(&text);
                        } else {
                            out.push_str(&text);
                        }
                    }
                } else {
                    let href = attr(body, "href").unwrap_or_default();
                    // href 为空 ⇒ 不写成链接（否则 `[]()` 会毁掉排版），只留文字。
                    if href.is_empty() {
                        open.push(String::new()); // 占位：闭标签时不再写 `](…)`
                    } else {
                        let text = format!("[");
                        if in_cell {
                            cell.push_str(&text);
                        } else {
                            out.push_str(&text);
                        }
                        open.push(href);
                    }
                }
            }
            "b" | "strong" => wrap(&mut out, &mut cell, in_cell, &mut open, "**", closing),
            "i" | "em" => wrap(&mut out, &mut cell, in_cell, &mut open, "*", closing),
            "s" | "del" | "strike" => wrap(&mut out, &mut cell, in_cell, &mut open, "~~", closing),
            "code" => {
                if pre.is_none() {
                    wrap(&mut out, &mut cell, in_cell, &mut open, "`", closing);
                }
            }
            "u" | "ins" => {} // 无 markdown 记法：只丢标签、留文字
            _ => {}           // 其它标签：丢标签，保留内部文字（绝不吞内容）
        }
    }
    // 表格还没收尾（缺 `</table>`）：也照样出（宁可多给一张表，不能丢内容）。
    if !rows.is_empty() {
        out.push_str(&table_to_markdown(&rows));
    }
    out
}

/// 行内样式包装：开标签写前缀、闭标签写同前缀（markdown 的 `**`/`*`/`~~`/`` ` `` 都是对称的）。
fn wrap(
    out: &mut String,
    cell: &mut String,
    in_cell: bool,
    open: &mut Vec<String>,
    marker: &str,
    closing: bool,
) {
    if closing {
        open.pop();
    } else {
        open.push(marker.to_string());
    }
    if in_cell {
        cell.push_str(marker);
    } else {
        out.push_str(marker);
    }
}

/// 「行 × (格, 是否表头行)」→ markdown 表格文本。
fn table_to_markdown(rows: &[(Vec<String>, bool)]) -> String {
    if rows.is_empty() {
        return String::new();
    }
    let cols = rows.iter().map(|(r, _)| r.len()).max().unwrap_or(0).max(1);
    let mut s = String::from("\n\n");
    for (i, (cells, _)) in rows.iter().enumerate() {
        s.push('|');
        for c in 0..cols {
            let text = cells.get(c).map(String::as_str).unwrap_or("");
            s.push(' ');
            s.push_str(&text.replace('|', "\\|"));
            s.push_str(" |");
        }
        s.push('\n');
        if i == 0 {
            s.push('|');
            for _ in 0..cols {
                s.push_str(" --- |");
            }
            s.push('\n');
        }
    }
    s.push('\n');
    s
}

/// 取标签体里的属性值（`src="…"` / `src='…'` / `src=…`）。
fn attr(tag_body: &str, name: &str) -> Option<String> {
    let lower = tag_body.to_ascii_lowercase();
    let mut from = 0usize;
    while let Some(pos) = lower[from..].find(name) {
        let at = from + pos;
        let before_ok = at == 0
            || tag_body[..at]
                .chars()
                .next_back()
                .map(|c| c.is_whitespace() || c == '<')
                .unwrap_or(true);
        let after = lower[at + name.len()..].trim_start();
        if before_ok {
            if let Some(rest) = after.strip_prefix('=') {
                let rest = rest.trim_start();
                let quote = rest.chars().next();
                let value = match quote {
                    Some('"') | Some('\'') => {
                        let q = quote.unwrap();
                        rest[1..].split(q).next().unwrap_or("").to_string()
                    }
                    _ => rest
                        .split(|c: char| c.is_whitespace() || c == '>')
                        .next()
                        .unwrap_or("")
                        .to_string(),
                };
                if !value.is_empty() {
                    return Some(decode_entities(&value));
                }
            }
        }
        from = at + name.len();
        if from >= lower.len() {
            break;
        }
    }
    None
}

/// 常见实体解码（够用即可；其余原样保留）。
fn decode_entities(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    s.replace("&nbsp;", " ")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&apos;", "'")
        .replace("&mdash;", "—")
        .replace("&ndash;", "–")
        .replace("&hellip;", "…")
        .replace("&amp;", "&")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mdblocks::parse;

    /// 标题/段落/分隔线：走既有块类型（= 「像 markdown 一样」）。
    #[test]
    fn headings_paragraphs_and_rules_become_markdown_blocks() {
        let html = "<h2>标题</h2>\n<p>第一段</p>\n<hr>\n<p>第二段</p>";
        let blocks = parse(html);
        let kinds: Vec<&str> = blocks.iter().map(|b| b.kind.as_str()).collect();
        assert!(kinds.contains(&"h2"), "h2 应成标题块：{kinds:?}");
        assert_eq!(
            kinds.iter().filter(|k| **k == "p").count(),
            2,
            "两段应成两个段落块：{kinds:?}"
        );
        assert!(kinds.contains(&"hr"), "hr 应成分隔线：{kinds:?}");
        let title = blocks.iter().find(|b| b.kind == "h2").unwrap();
        assert_eq!(title.text, "标题");
    }

    /// 列表：嵌套 + 有序编号。
    #[test]
    fn lists_keep_nesting_and_ordering() {
        let html = "<ul><li>甲</li><li>乙<ul><li>乙一</li></ul></li></ul>\n<ol><li>一</li><li>二</li></ol>";
        let blocks = parse(html);
        let lists: Vec<_> = blocks.iter().filter(|b| b.kind == "list").collect();
        assert!(lists.len() >= 4, "应有 ≥4 个列表项：{lists:?}");
        assert_eq!(lists[0].text, "甲");
        assert_eq!(lists[0].order, 0, "无序列表 order=0");
        let nested = lists.iter().find(|b| b.text == "乙一").expect("嵌套项应在");
        assert_eq!(nested.level, 1, "嵌套层级应为 1：{nested:?}");
        let ordered: Vec<_> = lists.iter().filter(|b| b.order > 0).collect();
        assert_eq!(ordered.len(), 2, "有序列表两项：{ordered:?}");
        assert_eq!(ordered[0].order, 1);
        assert_eq!(ordered[1].order, 2);
    }

    /// 表格：第一行（th）当表头，`|` 转义。
    #[test]
    fn table_with_header_becomes_markdown_table() {
        let html = "<table><tr><th>键</th><th>值</th></tr><tr><td>a</td><td>1</td></tr><tr><td>b|c</td><td>2</td></tr></table>";
        let blocks = parse(html);
        let rows: Vec<_> = blocks.iter().filter(|b| b.kind == "table").collect();
        assert_eq!(rows.len(), 3, "表头 + 两行数据：{rows:?}");
        assert!(rows[0].header, "第一行应为表头");
        assert_eq!(rows[0].cells, vec!["键".to_string(), "值".to_string()]);
        assert!(!rows[1].header);
        assert_eq!(rows[1].cells, vec!["a".to_string(), "1".to_string()]);
        assert_eq!(
            rows[2].cells[0], "b|c",
            "单元格里的竖线要还原（转义只发生在给 markdown 的中间文本里）"
        );
    }

    /// `<pre>` 成代码围栏；行内 `<code>` 成行内码。
    #[test]
    fn pre_becomes_fence_and_inline_code_stays() {
        let html = "<pre>fn main() {\n  let x = 1;\n}</pre>\n<p>看这个 <code>foo()</code> 调用</p>";
        let blocks = parse(html);
        let code = blocks
            .iter()
            .find(|b| b.kind == "code")
            .expect("应有代码块");
        assert!(code.text.contains("let x = 1;"), "代码原文应保留：{code:?}");
        let p = blocks.iter().find(|b| b.kind == "p").expect("应有段落");
        assert!(
            p.text.contains("`foo()`"),
            "行内 code 应成行内码：{:?}",
            p.text
        );
    }

    /// `style`/`script` 的**内容**整段丢弃（CSS 源码不能当正文画出来）。
    #[test]
    fn style_and_script_content_is_dropped() {
        let html = "<style>body{color:red}</style>\n<script>alert(1)</script>\n<p>正文</p>";
        let blocks = parse(html);
        let all: String = blocks.iter().map(|b| b.text.clone()).collect();
        assert!(!all.contains("color:red"), "CSS 不该出现：{all:?}");
        assert!(!all.contains("alert"), "脚本不该出现：{all:?}");
        assert!(all.contains("正文"), "正文要保留：{all:?}");
    }

    /// **行内标签不触发转换**：普通句子里夹 `<b>` 的既有排版不许被改动。
    #[test]
    fn inline_html_in_a_sentence_is_left_alone() {
        let md = "他有 <b>重点</b>，还有 <code>x</code> 和 <a href=\"https://a.b\">链接</a>。";
        assert_eq!(
            convert_html_blocks(md),
            format!("{md}\n"),
            "行内标签不该被当块转换（保序、保字面）"
        );
    }

    /// 缺配对的闭合标签：**原样保留**（宁可显示源码，不能吞内容）。
    #[test]
    fn unclosed_html_block_falls_back_to_source() {
        let md = "<div class=\"x\">\n没闭合\n";
        let out = convert_html_blocks(md);
        assert!(out.contains("没闭合"), "内容绝不能丢：{out:?}");
    }

    /// 链接与图片、实体解码。
    #[test]
    fn links_images_and_entities() {
        let html =
            "<p>看图 <img src=\"./a.png\" alt=\"图\"> 并 <a href=\"https://a.b\">点这里</a></p>";
        let blocks = parse(html);
        let img = blocks
            .iter()
            .find(|b| b.kind == "image")
            .expect("应有图片块");
        assert_eq!(img.src, "./a.png");
        assert_eq!(img.alt, "图");
        let all: String = blocks.iter().map(|b| b.text.clone()).collect();
        assert!(
            all.contains("[点这里](https://a.b)"),
            "链接应成 markdown：{all:?}"
        );
        // 实体解码：`&lt;C&gt;` 解成 `<C>` 后会被当**行内 HTML**（既有口径：字面保留、转义显示），
        // 所以只断言「& 与 < > 都没被当实体漏出」。
        let ent = parse("<p>A &amp; B &lt;C&gt;</p>");
        let text: String = ent.iter().map(|b| b.text.clone()).collect();
        assert!(text.contains("A & B"), "实体应解码：{ent:?}");
        assert!(!text.contains("&amp;"), "不该漏出实体源码：{ent:?}");
    }

    /// ```html 围栏**直接渲染**（与裸 HTML 同口径），其它语言的围栏仍是代码块。
    #[test]
    fn html_fence_renders_but_other_fences_stay_code() {
        let md = "```html\n<h3>小标题</h3>\n<ul><li>项</li></ul>\n```\n\n```rust\nlet x = 1;\n```";
        let blocks = parse(md);
        assert!(
            blocks.iter().any(|b| b.kind == "h3" && b.text == "小标题"),
            "html 围栏里的标题要渲染出来：{blocks:?}"
        );
        assert!(
            blocks.iter().any(|b| b.kind == "list" && b.text == "项"),
            "html 围栏里的列表要渲染出来：{blocks:?}"
        );
        assert!(
            blocks
                .iter()
                .any(|b| b.kind == "code" && b.text.contains("let x = 1;")),
            "非 html 围栏必须仍是代码块：{blocks:?}"
        );
    }

    /// `<!doctype html>` 开头的整页 HTML：结构与文字都要出来。
    #[test]
    fn full_document_renders_structure() {
        let html = "<!doctype html>\n<html><head><style>h1{color:red}</style></head>\n<body>\n<h1>报告</h1>\n<p>结论 A</p>\n<table><tr><th>项</th></tr><tr><td>x</td></tr></table>\n</body></html>";
        let blocks = parse(html);
        let kinds: Vec<&str> = blocks.iter().map(|b| b.kind.as_str()).collect();
        assert!(kinds.contains(&"h1"), "{kinds:?}");
        assert!(kinds.contains(&"p"), "{kinds:?}");
        assert!(kinds.contains(&"table"), "{kinds:?}");
        assert_eq!(
            kinds.iter().filter(|k| **k == "code").count(),
            0,
            "不该出现源码块：{kinds:?}"
        );
    }
}
