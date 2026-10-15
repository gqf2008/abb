//! LaTeX → SVG 排版（**块级公式**）。KaTeX 的 Rust 移植 `ratex-*`，KaTeX 字体内嵌。
//!
//! 为什么值得加这个依赖（2026-10-15 owner 明确要求「处理 LaTeX」）：不开 `ENABLE_MATH` 时公式
//! 原样画出来；开了也只能降级成行内码（`\int`/`\frac` 仍是源码）。真排版必须有 TeX 引擎，而
//! 本仓能出图的只有两条路——把 TeX 变成 SVG 或 PNG。这里选 **SVG**：Slint 已依赖 resvg，
//! 矢量在高分屏更清晰，也省掉「Rust 侧 PNG 编码 → Slint 再解码」那一趟。
//!
//! **范围（务必如实理解）**：只排**块级**公式（`$$…$$` → `kind == "math"` 块）。
//! **行内**公式（`$x$`）仍是行内码——Slint 的文字流里放不进图片（`mdblocks` 顶注同结论），
//! 要做必须自建「文字 + 图片混排换行」的排版层。
//!
//! **成本**：`ratex-*` + `ratex-katex-fonts`（KaTeX TTF ~580K）进托盘二进制，编译单元增加。
//! 若哪天嫌重，正路是照 `crates/abb-chart` 的先例挪成独立包 + `$ABB_BIN` 子命令。
//!
//! **失败绝不吞内容**：任何一步出错（非法 TeX / 写盘失败 / Slint 加载失败）都返回 `None`，
//! 调用方回落成等宽 `Text` 显示原始 TeX——公式排不出来最多是「不好看」，不能变成「消息少了字」。

use std::path::{Path, PathBuf};

/// 排版结果：Slint 图片 + **自然显示尺寸**（逻辑 px，未缩放）+ 基线拆分。
pub struct MathImage {
    pub image: slint::Image,
    /// 自然宽（px）。过宽时由 `.slint` 等比缩到气泡内（见 `app.slint` 的 `math` 分支）。
    pub w: f32,
    /// 自然高（px）= `ascent + depth`。
    pub h: f32,
    /// **基线以上**的高度（px）：行内混排时用它把公式基线对齐到文字基线。
    pub ascent: f32,
    /// **基线以下**的高度（px，下落部）。
    pub depth: f32,
}

/// 公式渲染字号（px，单行/块级用）：贴着正文 13px 略大一点（独立成行的公式可略大）。
const FONT_PX: f64 = 15.0;
/// 四边留白（px）：给上下标/根号留溢出空间（给 0 会把 `\sqrt` 的横杠、`^2` 的顶切掉）。
const PAD_PX: f64 = 1.5;

/// 公式 SVG 的缓存目录（`~/.agent-bridge/syschat/math-cache/`）。
///
/// 缓存键 = 源码的 sha256（同一公式只排一次、只写一次盘）；文件名不含源码，避免非法字符。
pub fn cache_dir() -> PathBuf {
    crate::syschat::dir().join("math-cache")
}

/// 生产入口：按源码缓存成 SVG 文件，再交给 Slint 加载（resvg 按目标尺寸栅格化）。
pub fn typeset(tex: &str) -> Option<MathImage> {
    typeset_in(tex, &cache_dir())
}

/// 可注入缓存目录的入口（单测传临时目录——**绝不写真实 `~/.agent-bridge`**，否则全量门禁的
/// 隔离守卫会红）。用单行/块级的默认字号。
pub fn typeset_in(tex: &str, cache: &Path) -> Option<MathImage> {
    typeset_sized_in(tex, cache, FONT_PX)
}

/// [`typeset_in`] 的完整版：字号可指定（**行内混排**要跟正文同字号，见 `inlineflow`）。
pub fn typeset_sized_in(tex: &str, cache: &Path, font_px: f64) -> Option<MathImage> {
    let tex = tex.trim();
    let font_px = font_px.clamp(4.0, 200.0);
    if tex.is_empty() {
        return None;
    }
    // 解析失败（非法 TeX）⇒ None，调用方回落成等宽文本。
    let ast = ratex_parser::parser::parse(tex).ok()?;
    let lbox = ratex_layout::layout(&ast, &ratex_layout::LayoutOptions::default());
    let dl = ratex_layout::to_display_list(&lbox);
    let opts = ratex_svg::SvgOptions {
        font_size: font_px,
        padding: PAD_PX,
        embed_glyphs: true,
        ..Default::default()
    };
    let svg = ratex_svg::render_to_svg(&dl, &opts);
    // 基线拆分：`dl.height` = 基线以上，`dl.depth` = 基线以下（ratex 的 em 单位）。
    // 行内混排必须知道这两个值才能把公式贴到文字基线上。
    let ascent = (dl.height * font_px + PAD_PX).max(0.5) as f32;
    let depth = (dl.depth * font_px + PAD_PX).max(0.0) as f32;
    let w = (dl.width * font_px + 2.0 * PAD_PX).max(1.0) as f32;
    let h = ascent + depth;

    // 缓存键 = 源码 **+ 字号**：行内（13px）与块级（15px）同公式必须分开存，否则互相覆盖。
    let path = cache.join(format!(
        "{}-{}.svg",
        hash_hex(tex),
        (font_px * 100.0).round() as i64
    ));
    if !path.is_file() {
        std::fs::create_dir_all(cache).ok()?;
        // 先写临时文件再 rename：并发/中断时不会留下半截 SVG 被 Slint 缓存成坏图。
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, svg.as_bytes()).ok()?;
        std::fs::rename(&tmp, &path).ok()?;
    }
    let image = slint::Image::load_from_path(&path).ok()?;
    Some(MathImage {
        image,
        w,
        h,
        ascent,
        depth,
    })
}

/// 源码 → 文件名用的十六进制 sha256（复用 `syschat` 媒体缓存同款手法）。
fn hash_hex(s: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(s.as_bytes());
    format!("{:x}", h.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 能排版：常见公式都要出图且尺寸合理（宽/高 > 0、宽明显大于 0）。
    /// 目录注入到临时处，**不碰真实 `~/.agent-bridge`**。
    #[test]
    fn typesets_common_formulas_and_caches() {
        let dir = std::env::temp_dir().join(format!("abb-math-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        for tex in [
            r"E = mc^2",
            r"\int_0^1 x^2 \, dx = \frac{1}{3}",
            r"\frac{a+b}{c}",
            r"\sqrt{x^2+y^2}",
            r"\sum_{i=1}^{n} i = \frac{n(n+1)}{2}",
        ] {
            let m = typeset_in(tex, &dir).unwrap_or_else(|| panic!("公式排版失败：{tex}"));
            assert!(m.w > 0.0 && m.h > 0.0, "尺寸非法：{tex} → {}×{}", m.w, m.h);
        }
        // 第二次调用命中磁盘缓存（同源码 ⇒ 同文件），不重复写
        let files: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(Result::ok)
            .collect();
        assert_eq!(files.len(), 5, "每个公式一个缓存文件：{:?}", files.len());
        let before = files[0].path();
        let _ = typeset_in(r"E = mc^2", &dir).unwrap();
        let after: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(Result::ok)
            .collect();
        assert_eq!(after.len(), 5, "重复排版不应新增缓存文件");
        assert!(before.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 反例控制：非法 TeX / 空串 ⇒ `None`（调用方据此回落等宽文本，绝不吞内容）。
    #[test]
    fn invalid_tex_returns_none() {
        let dir = std::env::temp_dir().join(format!("abb-math-bad-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        assert!(typeset_in("   ", &dir).is_none(), "空公式应回落");
        assert!(
            typeset_in(r"\frac{a}{", &dir).is_none(),
            "括号不配对应回落（不得 panic、不得写文件）"
        );
        assert!(!dir.exists(), "回落路径不得创建缓存目录（副作用越小越好）");
    }
}
